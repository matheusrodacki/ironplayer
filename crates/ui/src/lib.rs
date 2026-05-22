//! Crate `ui` — Interface egui do IronPlayer.
//!
//! SPEC-UI-001 a SPEC-UI-006

pub mod panels;
pub mod state;
pub mod status_bar;

pub use state::{
    AppCommand, AppState, AspectRatioMode, AudioErrorSnapshot, AudioOperationalState,
    AudioStatusSnapshot, AudioTrackInfo, ConnectionState, TableEvent, TablesSnapshot,
};

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;

use crate::panels::metrics::MetricsPanel;
use crate::panels::tables::TablesPanel;
use crate::panels::video::VideoPanel;
use crate::status_bar::StatusBar;
use av::{Clock, MasterClock, VideoFrame, VideoRenderer};
use av::video_queue::{VideoQueue, PopResult};

// ---------------------------------------------------------------------------
// IronPlayerApp
// ---------------------------------------------------------------------------

/// Aplicação principal do IronPlayer.
///
/// Implementa `eframe::App` com layout de 3 colunas:
/// - Esquerda:  `VideoPanel` (≈40%)
/// - Centro:    análise — PIDs / Tables / Serviços (≈35%)
/// - Direita:   `MetricsPanel` (≈25%)
/// - Topo:      barra URL + Conectar / Desconectar
/// - Rodapé:    `StatusBar`
///
/// SPEC-UI-001
pub struct IronPlayerApp {
    /// Estado completo da UI — snapshot imutável atualizado a cada frame.
    state: AppState,
    /// Sender para enviar comandos ao backend via canal MPSC bounded.
    cmd_tx: Sender<AppCommand>,
    /// Conteúdo atual do campo de texto de URL.
    url_input: String,
    /// Painel central com abas PIDs / Tables / Serviços.
    tables_panel: TablesPanel,
    /// Painel direito com gráficos e log de erros.
    metrics_panel: MetricsPanel,
    /// Receptor de snapshots do pipeline (opcional).
    ///
    /// SPEC-UI-008
    snapshot_rx: Option<ts::aggregator::SnapshotReceiver>,
    /// Estado de conexão compartilhado com o command handler do pipeline.
    connection_rx: Option<Arc<RwLock<ConnectionState>>>,
    /// Snapshot compartilhado de métricas/estado operacional do áudio.
    audio_status_rx: Option<Arc<RwLock<AudioStatusSnapshot>>>,
    /// Serviço selecionado compartilhado com o command handler do pipeline.
    selected_service_rx: Option<Arc<RwLock<Option<u16>>>>,
    /// Eventos incrementais de tabelas PSI/SI vindos do `TableDispatcher`.
    table_events_rx: Option<Receiver<TableEvent>>,
    /// Receptor de frames de vídeo decodificados (FfmpegDecoder → UI).
    ///
    /// SPEC-AV-003
    video_frames_rx: Option<Receiver<VideoFrame>>,
    /// Fila de frames de vídeo ordenada por PTS com políticas drop/hold/resync.
    ///
    /// Substitui o pipeline best-effort de drenagem simples.
    ///
    /// SPEC-AV-VQ-001
    video_queue: VideoQueue,
    /// Clock master usado para sincronizar a exibição de frames de vídeo.
    ///
    /// Inicia como `MasterClock::Wall(0)` e é ancorado no PTS do primeiro
    /// frame recebido.
    ///
    /// SPEC-AV-VQ-001
    video_clock: MasterClock,
    /// Indica se o clock de vídeo já foi ancorado ao PTS do primeiro frame.
    video_clock_initialized: bool,
    /// Renderizador de vídeo: mantém textura GPU/CPU entre frames.
    ///
    /// SPEC-AV-003
    video_renderer: Option<VideoRenderer>,
    /// Dimensões do último frame renderizado `(width, height)` corrigidas por SAR.
    video_dims: Option<(u32, u32)>,
    /// Modo de aspect-ratio selecionado pelo usuário.
    ///
    /// Preferência puramente visual; padrão `Dar` usa o SAR sinalizado pelo stream.
    aspect_ratio_mode: AspectRatioMode,
    /// Timestamp do último snapshot usado para alimentar históricos de gráficos.
    last_metrics_snapshot_timestamp: Option<Instant>,
    /// Número de eventos de jitter PCR já incorporados ao histórico da UI.
    seen_pcr_jitter_records: usize,
}

impl IronPlayerApp {
    /// Cria um novo `IronPlayerApp`.
    ///
    /// `snapshot_rx`: receptor de métricas do pipeline; `None` quando o
    /// pipeline ainda não foi iniciado (modo stand-alone / testes).
    ///
    /// `video_frames_rx`: receptor de `VideoFrame` decodificados; `None` em
    /// modo stand-alone. Quando `Some`, o renderer é inicializado em modo GPU
    /// (D3D11 via wgpu) se disponível, ou modo CPU como fallback.
    ///
    /// SPEC-UI-001 · SPEC-UI-008 · SPEC-AV-003
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cmd_tx: Sender<AppCommand>,
        snapshot_rx: Option<ts::aggregator::SnapshotReceiver>,
        connection_rx: Option<Arc<RwLock<ConnectionState>>>,
        audio_status_rx: Option<Arc<RwLock<AudioStatusSnapshot>>>,
        selected_service_rx: Option<Arc<RwLock<Option<u16>>>>,
        table_events_rx: Option<Receiver<TableEvent>>,
        video_frames_rx: Option<Receiver<VideoFrame>>,
    ) -> Self {
        // Inicializa VideoRenderer em modo GPU (D3D11) quando wgpu disponível,
        // ou em modo CPU como fallback. SPEC-AV-003 · SPEC-AV-003c
        let video_renderer = video_frames_rx.as_ref().map(|_| {
            if let Some(wgpu_state) = &cc.wgpu_render_state {
                VideoRenderer::new_gpu(
                    wgpu_state.device.clone(),
                    wgpu_state.queue.clone(),
                    wgpu_state.renderer.clone(),
                )
            } else {
                VideoRenderer::new_cpu(cc.egui_ctx.clone())
            }
        });

        Self {
            state: AppState::default(),
            cmd_tx,
            url_input: String::new(),
            tables_panel: TablesPanel::new(),
            metrics_panel: MetricsPanel::new(),
            snapshot_rx,
            connection_rx,
            audio_status_rx,
            selected_service_rx,
            table_events_rx,
            video_frames_rx,
            video_queue: VideoQueue::default(),
            video_clock: MasterClock::wall(0),
            video_clock_initialized: false,
            video_renderer,
            video_dims: None,
            aspect_ratio_mode: AspectRatioMode::default(),
            last_metrics_snapshot_timestamp: None,
            seen_pcr_jitter_records: 0,
        }
    }

    /// Retorna uma referência imutável ao estado atual da UI.
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Lê o `SnapshotReceiver` (se disponível) e atualiza `AppState`.
    ///
    /// Chamado no início de cada frame em `update()`.
    ///
    /// SPEC-UI-008
    fn poll_snapshot(&mut self) {
        let rx = match &self.snapshot_rx {
            Some(rx) => rx.clone(),
            None => return,
        };
        let snapshot = rx.borrow();
        let now = Instant::now();
        update_metric_histories_if_new_snapshot(
            &mut self.state,
            &snapshot,
            &mut self.last_metrics_snapshot_timestamp,
            &mut self.seen_pcr_jitter_records,
            now,
        );

        // Atualiza o snapshot de métricas.
        self.state.metrics = snapshot;

        // Atualiza o estado de conexão a partir do command handler.
        if let Some(conn_rx) = &self.connection_rx {
            if let Ok(state) = conn_rx.read() {
                self.state.connection = state.clone();
            }
        }

        if let Some(audio_rx) = &self.audio_status_rx {
            if let Ok(audio) = audio_rx.read() {
                self.state.audio = audio.clone();
            }
        }

        // Atualiza o serviço selecionado a partir do command handler.
        if let Some(svc_rx) = &self.selected_service_rx {
            if let Ok(svc) = svc_rx.read() {
                self.state.selected_service = *svc;
            }
        }
    }

    /// Drena eventos de tabela sem bloquear o frame da UI.
    ///
    /// SPEC-UI-008
    fn poll_table_events(&mut self) {
        let Some(rx) = self.table_events_rx.as_ref().cloned() else {
            return;
        };

        for event in rx.try_iter().take(512) {
            if matches!(event, TableEvent::Reset) {
                self.reset_stream_data();
                continue;
            }
            self.state.apply_table_event(event);
        }
    }

    fn reset_stream_data(&mut self) {
        self.state.reset_stream_data();
        self.metrics_panel.reset_stream_data();
        self.video_dims = None;
        self.last_metrics_snapshot_timestamp = None;
        self.seen_pcr_jitter_records = 0;

        // Drena o canal de entrada e limpa a fila PTS-ordenada.
        if let Some(rx) = &self.video_frames_rx {
            while rx.try_recv().is_ok() {}
        }
        self.video_queue.clear();
        self.video_clock = MasterClock::wall(0);
        self.video_clock_initialized = false;
    }

    /// Drena frames do canal de vídeo, insere na `VideoQueue` PTS-ordenada e
    /// faz upload do próximo frame pronto ao renderer.
    ///
    /// # Algoritmo
    ///
    /// 1. Drena até 16 frames do canal `video_frames_rx` e os insere na
    ///    `VideoQueue` com as políticas drop/hold/resync/wrap.
    /// 2. Na primeira chegada de frame com PTS definido, ancora o
    ///    `video_clock` no PTS do frame (`reset(anchor)`), garantindo que o
    ///    clock esteja alinhado com o início do stream.
    /// 3. Chama `pop_ready(clock.now_pts90())` para extrair o próximo frame
    ///    na janela de exibição:
    ///    - `Ready`: faz upload ao renderer.
    ///    - `Resync`: reseta o clock para `new_anchor` e faz upload.
    ///    - `TooEarly` / `Empty`: nenhuma ação.
    ///
    /// SPEC-AV-003 · SPEC-AV-VQ-001
    fn poll_video_frames(&mut self) {
        let rx = match &self.video_frames_rx {
            Some(r) => r.clone(),
            None => return,
        };

        // 1. Drena até 16 frames do canal e insere na fila PTS-ordenada.
        for frame in rx.try_iter().take(16) {
            // Ancora o clock no PTS do primeiro frame recebido.
            if !self.video_clock_initialized {
                if let Some(pts) = frame.pts {
                    self.video_clock.reset(pts as i64);
                    self.video_clock_initialized = true;
                }
            }
            self.video_queue.push(frame);
        }

        // 2. Extrai o próximo frame pronto para exibição.
        let clock_pts = self.video_clock.now_pts90();
        let ready_frame = match self.video_queue.pop_ready(clock_pts) {
            PopResult::Ready(f) => Some(f),
            PopResult::Resync { frame, new_anchor } => {
                // Resincroniza o clock ao novo âncora de PTS.
                self.video_clock.reset(new_anchor);
                Some(frame)
            }
            PopResult::TooEarly | PopResult::Empty => None,
        };

        // 3. Faz upload do frame ao renderer.
        if let Some(frame) = ready_frame {
            if let Some(renderer) = &mut self.video_renderer {
                match renderer.upload(&frame) {
                    Ok(()) => {
                        // Aplica o SAR para calcular as dimensões de exibição corretas.
                        // DAR = SAR * (w/h); mantemos w fixo e ajustamos h:
                        //   display_h = pixel_h * sar_den / sar_num
                        // Para 1920×540 com SAR 1:2 → display_h = 540*2/1 = 1080 → 16:9
                        let display_h = if frame.sar_num > 1 || frame.sar_den > 1 {
                            let h64 = frame.height as u64 * frame.sar_den as u64;
                            (h64 / frame.sar_num.max(1) as u64) as u32
                        } else {
                            frame.height
                        };
                        self.video_dims = Some((frame.width, display_h.max(1)));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "poll_video_frames: falha no upload do frame");
                    }
                }
            }
        }
    }
}

fn update_metric_histories_if_new_snapshot(
    state: &mut AppState,
    snapshot: &ts::metrics::MetricsSnapshot,
    last_snapshot_timestamp: &mut Option<Instant>,
    seen_pcr_jitter_records: &mut usize,
    now: Instant,
) {
    if last_snapshot_timestamp.is_some_and(|timestamp| timestamp == snapshot.timestamp) {
        return;
    }
    *last_snapshot_timestamp = Some(snapshot.timestamp);

    let cutoff = now - Duration::from_secs(60);

    state
        .bitrate_history
        .push_back((snapshot.timestamp, snapshot.total_bitrate_kbps));
    while state
        .bitrate_history
        .front()
        .is_some_and(|(timestamp, _)| *timestamp < cutoff)
    {
        state.bitrate_history.pop_front();
    }

    let jitter_events = &snapshot.errors.pcr_jitter_events;
    if jitter_events.len() < *seen_pcr_jitter_records {
        *seen_pcr_jitter_records = 0;
        state.pcr_history.clear();
    }

    for record in jitter_events.iter().skip(*seen_pcr_jitter_records) {
        state
            .pcr_history
            .entry(record.pid)
            .or_default()
            .push_back(record.clone());
    }
    *seen_pcr_jitter_records = jitter_events.len();

    for history in state.pcr_history.values_mut() {
        while history
            .front()
            .is_some_and(|record| record.timestamp < cutoff)
        {
            history.pop_front();
        }
    }
    state.pcr_history.retain(|_, history| !history.is_empty());
}

impl eframe::App for IronPlayerApp {
    /// Atualiza e renderiza a interface a cada frame.
    ///
    /// SPEC-UI-001 · SPEC-UI-008
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ── Poll de métricas do pipeline ──────────────────────────────────
        self.poll_snapshot();
        self.poll_table_events();
        self.poll_video_frames();

        // eframe e' reactive por padrao -- so' repinta com interacao do
        // usuario. Para vídeo em tempo real precisamos de redraw contínuo:
        // pedimos repaint a ~60 Hz enquanto houver fluxo de video. Sem isso o
        // canal `video_frames` lota e o decoder fica gerando frames descartados.
        if self.video_frames_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }

        // ── Header: URL + botões Conectar / Desconectar ──────────────────────
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("URL:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.url_input)
                        .hint_text("udp://@239.1.1.1:1234")
                        .desired_width(400.0),
                );

                let can_connect = matches!(
                    self.state.connection,
                    ConnectionState::Idle | ConnectionState::Error { .. }
                );
                if ui
                    .add_enabled(can_connect, egui::Button::new("Conectar"))
                    .clicked()
                    && !self.url_input.is_empty()
                {
                    let _ = self.cmd_tx.try_send(AppCommand::Connect {
                        url: self.url_input.clone(),
                        iface: None,
                    });
                }

                let can_disconnect = matches!(
                    self.state.connection,
                    ConnectionState::Connected { .. } | ConnectionState::Connecting { .. }
                );
                if ui
                    .add_enabled(can_disconnect, egui::Button::new("Desconectar"))
                    .clicked()
                {
                    let _ = self.cmd_tx.try_send(AppCommand::Disconnect);
                }
            });
        });

        // ── Rodapé: StatusBar ─────────────────────────────────────────────────
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            StatusBar::show(ui, &self.state);
        });

        // ── Painel esquerdo: VideoPanel (≈40%) ───────────────────────────────
        let video_texture = self
            .video_renderer
            .as_ref()
            .and_then(|r| r.texture_id())
            .zip(self.video_dims);
        egui::SidePanel::left("video_panel")
            .resizable(true)
            .default_width(400.0)
            .show(ctx, |ui| {
                VideoPanel::show(
                    ui,
                    &self.state,
                    video_texture,
                    &self.cmd_tx,
                    &mut self.aspect_ratio_mode,
                );
            });

        // ── Painel direito: MetricsPanel (≈25%) ──────────────────────────────
        egui::SidePanel::right("metrics_panel")
            .resizable(true)
            .default_width(280.0)
            .show(ctx, |ui| {
                self.metrics_panel.show(ui, &self.state, &self.cmd_tx);
            });

        // ── Painel central: PIDs / Tables / Serviços (≈35%) ──────────────────
        egui::CentralPanel::default().show(ctx, |ui| {
            self.tables_panel.show(ui, &self.state, &self.cmd_tx);
        });
    }
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

/// Inicia a janela principal do IronPlayer.
///
/// Cria um canal de comandos bounded, constrói `IronPlayerApp` e delega ao
/// `eframe::run_native`. Retorna `Err` se o subsistema gráfico falhar.
///
/// SPEC-UI-001
pub fn run(title: &str) -> eframe::Result {
    let (cmd_tx, _cmd_rx) = crossbeam_channel::bounded::<AppCommand>(64);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([1280.0, 720.0]),
        ..Default::default()
    };

    eframe::run_native(
        title,
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(IronPlayerApp::new(
                cc, cmd_tx, None, None, None, None, None, None,
            )))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ts::metrics::{ErrorSnapshot, MetricsSnapshot, PcrJitterRecord};

    #[test]
    fn spec_ui_008_metric_histories_ignore_repainted_snapshot() {
        let base_time = Instant::now();
        let mut state = AppState::default();
        let mut last_snapshot_timestamp = None;
        let mut seen_pcr_jitter_records = 0;

        let first_record = PcrJitterRecord {
            pid: 0x0111,
            timestamp: base_time,
            expected_us: 1_000,
            measured_us: 1_700,
        };
        let first_snapshot = MetricsSnapshot {
            total_bitrate_kbps: 32_000.0,
            errors: ErrorSnapshot {
                pcr_jitter_events: vec![first_record.clone()],
                ..ErrorSnapshot::default()
            },
            timestamp: base_time,
            ..MetricsSnapshot::default()
        };

        update_metric_histories_if_new_snapshot(
            &mut state,
            &first_snapshot,
            &mut last_snapshot_timestamp,
            &mut seen_pcr_jitter_records,
            base_time,
        );
        update_metric_histories_if_new_snapshot(
            &mut state,
            &first_snapshot,
            &mut last_snapshot_timestamp,
            &mut seen_pcr_jitter_records,
            base_time + Duration::from_millis(16),
        );

        assert_eq!(state.bitrate_history.len(), 1);
        assert_eq!(state.pcr_history[&0x0111].len(), 1);
        assert_eq!(seen_pcr_jitter_records, 1);

        let second_record = PcrJitterRecord {
            pid: 0x0111,
            timestamp: base_time + Duration::from_secs(1),
            expected_us: 2_000,
            measured_us: 2_800,
        };
        let second_snapshot = MetricsSnapshot {
            total_bitrate_kbps: 32_500.0,
            errors: ErrorSnapshot {
                pcr_jitter_events: vec![first_record, second_record],
                ..ErrorSnapshot::default()
            },
            timestamp: base_time + Duration::from_secs(1),
            ..MetricsSnapshot::default()
        };

        update_metric_histories_if_new_snapshot(
            &mut state,
            &second_snapshot,
            &mut last_snapshot_timestamp,
            &mut seen_pcr_jitter_records,
            base_time + Duration::from_secs(1),
        );

        assert_eq!(state.bitrate_history.len(), 2);
        assert_eq!(state.pcr_history[&0x0111].len(), 2);
        assert_eq!(seen_pcr_jitter_records, 2);

        let reset_snapshot = MetricsSnapshot {
            timestamp: base_time + Duration::from_secs(2),
            ..MetricsSnapshot::default()
        };

        update_metric_histories_if_new_snapshot(
            &mut state,
            &reset_snapshot,
            &mut last_snapshot_timestamp,
            &mut seen_pcr_jitter_records,
            base_time + Duration::from_secs(2),
        );

        assert!(state.pcr_history.is_empty());
        assert_eq!(seen_pcr_jitter_records, 0);
    }
}
