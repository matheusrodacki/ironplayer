//! Crate `ui` — Interface egui do IronPlayer.
//!
//! SPEC-UI-001 a SPEC-UI-006

pub mod panels;
pub mod state;
pub mod status_bar;

pub use state::{
    AppCommand, AppState, AudioErrorSnapshot, AudioOperationalState, AudioStatusSnapshot,
    AudioTrackInfo, ConnectionState, TableEvent, TablesSnapshot,
};

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;

use crate::panels::metrics::MetricsPanel;
use crate::panels::tables::TablesPanel;
use crate::panels::video::VideoPanel;
use crate::status_bar::StatusBar;
use av::{VideoFrame, VideoRenderer};

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
    /// Renderizador de vídeo: mantém textura GPU/CPU entre frames.
    ///
    /// SPEC-AV-003
    video_renderer: Option<VideoRenderer>,
    /// Dimensões do último frame renderizado `(width, height)`.
    video_dims: Option<(u32, u32)>,
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
            video_renderer,
            video_dims: None,
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
        let cutoff = now - Duration::from_secs(60);

        // Histórico de bitrate total (janela 60 s).
        self.state
            .bitrate_history
            .push_back((now, snapshot.total_bitrate_kbps));
        while self
            .state
            .bitrate_history
            .front()
            .is_some_and(|(t, _)| *t < cutoff)
        {
            self.state.bitrate_history.pop_front();
        }

        // Histórico de jitter PCR por PID (janela 60 s).
        for record in &snapshot.errors.pcr_jitter_events {
            let history = self.state.pcr_history.entry(record.pid).or_default();
            history.push_back(record.clone());
        }
        for history in self.state.pcr_history.values_mut() {
            while history.front().is_some_and(|r| r.timestamp < cutoff) {
                history.pop_front();
            }
        }

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
            self.state.apply_table_event(event);
        }
    }

    /// Drena frames de vídeo do canal e faz upload do mais recente ao renderer.
    ///
    /// Drena até 8 frames por frame de UI, retendo apenas o mais recente para
    /// evitar acúmulo. Segue o comportamento de drop-oldest definido em
    /// SPEC-CHAN-001 para o canal `video_frames`.
    ///
    /// SPEC-AV-003
    fn poll_video_frames(&mut self) {
        let rx = match &self.video_frames_rx {
            Some(r) => r.clone(),
            None => return,
        };

        // Drena até 8 frames, mantendo apenas o mais recente.
        let mut latest: Option<VideoFrame> = None;
        for frame in rx.try_iter().take(8) {
            latest = Some(frame);
        }

        if let Some(frame) = latest {
            if let Some(renderer) = &mut self.video_renderer {
                match renderer.upload(&frame) {
                    Ok(()) => {
                        self.video_dims = Some((frame.width, frame.height));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "poll_video_frames: falha no upload do frame");
                    }
                }
            }
        }
    }
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
                VideoPanel::show(ui, &self.state, video_texture, &self.cmd_tx);
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
