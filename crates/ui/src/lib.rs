//! Crate `ui` — Interface egui do IronPlayer.
//!
//! SPEC-UI-001 a SPEC-UI-006

pub mod panels;
pub mod state;
pub mod status_bar;

pub use state::{
    AppCommand, AppState, AspectRatioMode, AudioErrorSnapshot, AudioOperationalState,
    AudioStatusSnapshot, AudioTrackInfo, ConnectionState, HwAccelChoice, TableEvent,
    TablesSnapshot,
};

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;

use crate::panels::metrics::MetricsPanel;
use crate::panels::tables::TablesPanel;
use crate::panels::video::VideoPanel;
use crate::status_bar::StatusBar;
use av::video_queue::{PopResult, VideoQueue};
use av::{Clock, MasterClock, VideoFrame, VideoRenderer};

const PRIMARY_CONTENT_RATIO: f32 = 0.85;
const TABLES_WIDTH_RATIO: f32 = 0.30;

#[derive(Debug, Clone, Copy, PartialEq)]
struct DashboardLayout {
    top_height: f32,
    bottom_height: f32,
    left_width: f32,
    right_width: f32,
}

fn compute_dashboard_layout(available: egui::Vec2, spacing: egui::Vec2) -> DashboardLayout {
    let total_width = available.x.max(0.0);
    let total_height = available.y.max(0.0);

    let top_height = total_height * PRIMARY_CONTENT_RATIO;
    let bottom_height = (total_height - top_height).max(0.0);

    let row_width = (total_width - spacing.x).max(0.0);
    let left_width = row_width * TABLES_WIDTH_RATIO;
    let right_width = (row_width - left_width).max(0.0);

    DashboardLayout {
        top_height,
        bottom_height,
        left_width,
        right_width,
    }
}

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
    /// Indica se o clock de vídeo já foi ancorado (wall no primeiro PTS ou áudio).
    video_clock_initialized: bool,
    /// `true` quando `video_clock` usa `AudioClock` como master A/V.
    clock_uses_audio: bool,
    /// Id estável do `AudioClockHandle` atualmente adotado pela UI.
    ///
    /// Permite detectar quando a thread `audio-out` publica um handle novo
    /// (troca de serviço/trilha) e re-adotá-lo, em vez de continuar usando um
    /// handle obsoleto cujo contador de samples está congelado.
    adopted_audio_clock_id: Option<usize>,
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
    /// Métricas do pipeline de decodificação (decoder threads, deinterlacer,
    /// decode time p50/p99) compartilhadas com a thread `av-decode`.
    ///
    /// Preenchido via `set_pipeline_metrics_rx` após `new()`.
    ///
    /// SPEC-METRICS-PIPELINE-001
    pipeline_metrics_rx: Option<Arc<std::sync::RwLock<ts::metrics::PipelineMetrics>>>,
    /// Handle do clock de áudio publicado pela thread `audio-out`.
    ///
    /// Quando `Some`, `poll_video_frames` troca `video_clock` de
    /// `MasterClock::Wall` para `MasterClock::Audio`, fazendo o vídeo
    /// sincronizar contra o relógio real de reprodução WASAPI em vez de
    /// wall-clock, eliminando o drift causado pela latência do decoder
    /// multi-thread (frame threading).
    ///
    /// Preenchido via `set_audio_clock_rx` após `new()`.
    audio_clock_rx: Option<Arc<std::sync::RwLock<Option<av::AudioClockHandle>>>>,
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
        d3d11_device: Option<Arc<av::D3d11Device>>,
    ) -> Self {
        // Inicializa VideoRenderer em modo GPU (D3D11) quando wgpu disponível,
        // ou em modo CPU como fallback. SPEC-AV-003 · SPEC-AV-003c
        let video_renderer = video_frames_rx.as_ref().map(|_| {
            if let Some(wgpu_state) = &cc.wgpu_render_state {
                match d3d11_device.as_ref() {
                    Some(_d3d11_dev) => VideoRenderer::new_hw_gpu(
                        wgpu_state.device.clone(),
                        wgpu_state.target_format,
                    ),
                    None => VideoRenderer::new_gpu(
                        wgpu_state.device.clone(),
                        wgpu_state.queue.clone(),
                        wgpu_state.target_format,
                    ),
                }
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
            clock_uses_audio: false,
            adopted_audio_clock_id: None,
            video_renderer,
            video_dims: None,
            aspect_ratio_mode: AspectRatioMode::default(),
            last_metrics_snapshot_timestamp: None,
            audio_clock_rx: None,
            seen_pcr_jitter_records: 0,
            pipeline_metrics_rx: None,
        }
    }

    /// Associa o Arc compartilhado de métricas do pipeline ao app.
    ///
    /// Deve ser chamado logo após `new()`, antes do primeiro `update()`.
    ///
    /// SPEC-METRICS-PIPELINE-001
    pub fn set_pipeline_metrics_rx(
        &mut self,
        rx: Arc<std::sync::RwLock<ts::metrics::PipelineMetrics>>,
    ) {
        self.pipeline_metrics_rx = Some(rx);
    }

    /// Associa o `AudioClockHandle` compartilhado publicado por `audio-out`.
    ///
    /// Quando a thread `audio-out` cria (ou recria) o `AudioOutput`, grava
    /// um novo `AudioClockHandle` neste `Arc`; `poll_video_frames` lê o
    /// handle na primeira oportunidade e troca `video_clock` de
    /// `MasterClock::Wall` para `MasterClock::Audio`, sincronizando o vídeo
    /// ao relógio real de reprodução WASAPI.
    ///
    /// Deve ser chamado logo após `new()`, antes do primeiro `update()`.
    pub fn set_audio_clock_rx(&mut self, rx: Arc<std::sync::RwLock<Option<av::AudioClockHandle>>>) {
        self.audio_clock_rx = Some(rx);
    }

    pub fn set_hwaccel_choice(&mut self, choice: HwAccelChoice) {
        self.metrics_panel.set_hwaccel_choice(choice);
    }

    /// Fecha o canal de comandos para permitir shutdown em cascata do backend.
    ///
    /// SPEC-UI-001
    pub fn close_command_channel(&mut self) {
        let (replacement_tx, _replacement_rx) = crossbeam_channel::bounded(0);
        self.cmd_tx = replacement_tx;
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

        // Atualiza o snapshot de métricas preservando campos preenchidos pela UI.
        let pipeline = self.state.metrics.pipeline.clone();
        self.state.metrics = snapshot;
        self.state.metrics.pipeline = pipeline;

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
        // Volta ao wall clock; na próxima conexão o audio_clock_rx fornecerá
        // um novo AudioClockHandle e video_clock será trocado novamente.
        self.video_clock = MasterClock::wall(0);
        self.video_clock_initialized = false;
        self.clock_uses_audio = false;
        self.adopted_audio_clock_id = None;
        // Limpa o handle de áudio obsoleto para que poll_video_frames troque
        // para o novo handle da próxima sessão.
        if let Some(rx) = &self.audio_clock_rx {
            if let Ok(mut guard) = rx.write() {
                *guard = None;
            }
        }
        // av_sync_history é limpo via state.reset_stream_data() acima.
    }

    /// Drena frames do canal de vídeo, insere na `VideoQueue` PTS-ordenada e
    /// faz upload do próximo frame pronto ao renderer.
    ///
    /// # Algoritmo
    ///
    /// 1. Tenta trocar `video_clock` de `WallClock` para `AudioClock` assim que
    ///    a thread `audio-out` publicar um `AudioClockHandle`. Com áudio como
    ///    clock master, o vídeo é sincronizado contra o PTS real de reprodução
    ///    WASAPI, eliminando o drift causado pela latência do decoder multi-thread.
    /// 2. Drena até 16 frames do canal `video_frames_rx` e os insere na
    ///    `VideoQueue` com as políticas drop/hold/resync/wrap.
    /// 3. Fallback: se ainda sem AudioClock, ancora o `WallClock` no PTS do
    ///    primeiro frame de vídeo recebido (`reset(anchor)`).
    /// 4. Chama `pop_ready(clock.now_pts90())` para extrair o próximo frame
    ///    na janela de exibição:
    ///    - `Ready`: faz upload ao renderer.
    ///    - `Resync`: reseta o clock para `new_anchor` e faz upload.
    ///    - `TooEarly` / `Empty`: nenhuma ação.
    /// 5. Atualiza os campos de sincronização A/V em `state.metrics`
    ///    e amostra o offset no histórico de 60 s a ~1 Hz.
    ///
    /// SPEC-AV-003 · SPEC-AV-VQ-001 · SPEC-METRICS-SYNC-001
    fn poll_video_frames(&mut self, ctx: &egui::Context) {
        let rx = match &self.video_frames_rx {
            Some(r) => r.clone(),
            None => return,
        };

        // 1. Adota (ou re-adota) o AudioClockHandle publicado por audio-out.
        //    - Upgrade wall→áudio mesmo após o vídeo ter ancorado o WallClock.
        //    - Detecta substituição do handle (troca de serviço/trilha) via id
        //      estável: um handle recriado tem id distinto, então trocamos o
        //      clock em vez de travar num handle obsoleto (contador congelado).
        if let Some(rx) = &self.audio_clock_rx {
            if let Ok(guard) = rx.try_read() {
                match guard.as_ref() {
                    Some(handle) => {
                        let id = handle.id();
                        if self.adopted_audio_clock_id != Some(id) {
                            self.video_clock = MasterClock::Audio(handle.clone());
                            self.adopted_audio_clock_id = Some(id);
                            self.clock_uses_audio = true;
                            self.video_clock_initialized = true;
                            tracing::debug!(
                                clock_id = id,
                                "poll_video_frames: AudioClock adotado (A/V master)"
                            );
                        }
                    }
                    None => {
                        // Áudio em reinicialização (reset/troca de serviço): marca
                        // para re-adotar o próximo handle. Mantém o clock atual até
                        // lá (freeze breve de ~1 frame, sem salto de PTS).
                        if self.adopted_audio_clock_id.is_some() {
                            self.adopted_audio_clock_id = None;
                            self.clock_uses_audio = false;
                        }
                    }
                }
            }
        }

        // 2. Drena até 16 frames do canal e insere na fila PTS-ordenada.
        for frame in rx.try_iter().take(16) {
            // Fallback: ancora o WallClock no PTS do primeiro frame apenas sem
            // AudioClock (não sobrescreve após upgrade para áudio).
            if !self.clock_uses_audio && !self.video_clock_initialized {
                if let Some(pts) = frame.pts() {
                    self.video_clock.reset(pts as i64);
                    self.video_clock_initialized = true;
                }
            }
            self.video_queue.push(frame);
        }

        // 2. Extrai o próximo frame pronto para exibição.
        let clock_pts = self.video_clock.now_pts90();
        let allow_clock_resync = self.video_clock.audio_handle().is_none();
        let ready_frame = match self
            .video_queue
            .pop_ready_with_resync(clock_pts, allow_clock_resync)
        {
            PopResult::Ready(f) => Some(f),
            PopResult::Resync { frame, new_anchor } => {
                // Resincroniza o clock ao novo âncora de PTS.
                if allow_clock_resync {
                    self.video_clock.reset(new_anchor);
                }
                Some(frame)
            }
            PopResult::TooEarly | PopResult::Empty => None,
        };

        // 3. Faz upload do frame ao renderer.
        if let Some(frame) = ready_frame {
            if let Some(renderer) = &mut self.video_renderer {
                let (frame_w, frame_h, sar_num, sar_den) = (
                    frame.width(),
                    frame.height(),
                    frame.sar_num(),
                    frame.sar_den(),
                );
                let upload_result = match frame {
                    VideoFrame::Sw(ref yuv) => renderer.upload(yuv),
                    VideoFrame::Hw(hw) => renderer.upload_hw(hw),
                };
                match upload_result {
                    Ok(()) => {
                        // Atualiza métricas de GPU upload, colorspace e color_range.
                        // SPEC-METRICS-PIPELINE-001
                        self.state.metrics.pipeline.gpu_upload_bytes_per_sec =
                            renderer.gpu_upload_bytes_per_sec();
                        if let Some(cs) = renderer.current_colorspace_label() {
                            self.state.metrics.pipeline.colorspace = Some(cs.to_string());
                        }
                        if let Some(cr) = renderer.current_color_range_label() {
                            self.state.metrics.pipeline.color_range = Some(cr.to_string());
                        }
                        if let Some(shared) = &self.pipeline_metrics_rx {
                            if let Ok(mut metrics) = shared.write() {
                                metrics.gpu_upload_bytes_per_sec =
                                    renderer.gpu_upload_bytes_per_sec();
                                if let Some(cs) = renderer.current_colorspace_label() {
                                    metrics.colorspace = Some(cs.to_string());
                                }
                                if let Some(cr) = renderer.current_color_range_label() {
                                    metrics.color_range = Some(cr.to_string());
                                }
                            }
                        }
                        // Aplica o SAR para calcular as dimensões de exibição corretas.
                        // DAR = SAR * (w/h); mantemos w fixo e ajustamos h:
                        //   display_h = pixel_h * sar_den / sar_num
                        // Para 1920×540 com SAR 1:2 → display_h = 540*2/1 = 1080 → 16:9
                        let display_h = if sar_num > 1 || sar_den > 1 {
                            let h64 = frame_h as u64 * sar_den as u64;
                            (h64 / sar_num.max(1) as u64) as u32
                        } else {
                            frame_h
                        };
                        self.video_dims = Some((frame_w, display_h.max(1)));
                    }
                    Err(e) => {
                        if e.is_device_removed() {
                            self.handle_device_removed(ctx, &e);
                        }
                        tracing::warn!(error = %e, "poll_video_frames: falha no upload do frame");
                    }
                }
            }
        }

        // 4. Atualiza campos de sincronização A/V no MetricsSnapshot local.
        // Relê o clock aqui pois pode ter sido resetado pelo Resync acima.
        let current_clock_pts = self.video_clock.now_pts90();
        let sync_offset_ms: i32 = self
            .video_queue
            .front_pts()
            .map(|front_pts| {
                let diff_90 = front_pts - current_clock_pts;
                // Converte de 90 kHz para ms e clamp para i32.
                (diff_90 / 90).clamp(i32::MIN as i64, i32::MAX as i64) as i32
            })
            .unwrap_or(0);

        self.state.metrics.av_sync_offset_ms = sync_offset_ms;
        self.state.metrics.late_frames_dropped = self.video_queue.dropped_late;
        self.state.metrics.early_frames_held = self.video_queue.held_early;
        self.state.metrics.pts_discontinuities = self.video_queue.discontinuities;
        self.state.metrics.video_queue_depth = self.video_queue.len().min(u16::MAX as usize) as u16;

        // Amostra o offset no histórico a ~1 Hz (verifica se passou 1 s desde
        // a última amostra para evitar acumulação de 60 entradas/s).
        let now = Instant::now();
        let should_sample = match self.state.av_sync_history.back() {
            None => true,
            Some((t, _)) => now.duration_since(*t) >= Duration::from_secs(1),
        };
        if should_sample {
            self.state.av_sync_history.push_back((now, sync_offset_ms));
            let cutoff = now - Duration::from_secs(60);
            while self
                .state
                .av_sync_history
                .front()
                .is_some_and(|(t, _)| *t < cutoff)
            {
                self.state.av_sync_history.pop_front();
            }
        }
    }

    /// Copia as métricas de pipeline da thread `av-decode` para o snapshot local.
    ///
    /// Lê o `Arc<RwLock<PipelineMetrics>>` sem bloqueio (try_read); se a lock
    /// estiver ocupada, o frame é pulado sem afetar a UI.
    ///
    /// SPEC-METRICS-PIPELINE-001
    fn poll_pipeline_metrics(&mut self) {
        if let Some(rx) = &self.pipeline_metrics_rx {
            if let Ok(m) = rx.read() {
                self.state.metrics.pipeline = m.clone();
            }
        }
    }

    fn handle_device_removed(&mut self, _ctx: &egui::Context, error: &av::AvError) {
        if let Some(rx) = &self.video_frames_rx {
            while rx.try_recv().is_ok() {}
        }
        self.video_queue.clear();
        self.video_dims = None;
        self.state.metrics.video_queue_depth = 0;
        self.state.metrics.pipeline.hw_decode_active = false;
        self.state.metrics.pipeline.hw_decode_fallback_reason =
            Some("DXGI_ERROR_DEVICE_REMOVED".to_string());
        self.state.metrics.pipeline.tdr_recoveries =
            self.state.metrics.pipeline.tdr_recoveries.saturating_add(1);

        if let Some(renderer) = &mut self.video_renderer {
            let _ = renderer.fallback_to_software();
        }

        if let Some(shared) = &self.pipeline_metrics_rx {
            if let Ok(mut metrics) = shared.write() {
                metrics.hw_decode_active = false;
                metrics.hw_decode_fallback_reason = Some("DXGI_ERROR_DEVICE_REMOVED".to_string());
                metrics.tdr_recoveries = metrics.tdr_recoveries.saturating_add(1);
            }
        }

        let _ = self.cmd_tx.try_send(AppCommand::GpuDeviceRemoved);
        tracing::warn!(error = %error, "poll_video_frames: device D3D11 removido; fila drenada e fallback SW ativado");
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
        self.poll_pipeline_metrics();
        self.poll_video_frames(ctx);

        // eframe é reactive por padrão. Para vídeo em tempo real, pedimos
        // repaint reativo na chegada de cada frame (via ctx.request_repaint()
        // chamado no pipeline) em vez de polling fixo a 16 ms. O PresentMode::Fifo
        // garante sincronismo com vblank e evita tearing.
        if self.video_frames_rx.is_some() {
            ctx.request_repaint();
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

        // ── Dashboard principal: topo 90% (tabelas 30% + vídeo 70%),
        //    rodapé 10% com métricas em colunas ──────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| {
            let layout = compute_dashboard_layout(ui.available_size(), ui.spacing().item_spacing);

            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), layout.top_height),
                egui::Layout::left_to_right(egui::Align::Min),
                |ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(layout.left_width, layout.top_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            self.tables_panel.show(ui, &self.state, &self.cmd_tx);
                        },
                    );

                    ui.allocate_ui_with_layout(
                        egui::vec2(layout.right_width, layout.top_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            VideoPanel::show(
                                ui,
                                &self.state,
                                self.video_renderer.as_ref(),
                                self.video_dims,
                                &self.cmd_tx,
                                &mut self.aspect_ratio_mode,
                            );
                        },
                    );
                },
            );

            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), layout.bottom_height),
                egui::Layout::left_to_right(egui::Align::Min),
                |ui| {
                    self.metrics_panel
                        .show_columnar_strip(ui, &self.state, &self.cmd_tx);
                },
            );
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
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            present_mode: eframe::wgpu::PresentMode::Fifo,
            device_descriptor: std::sync::Arc::new(|adapter| {
                let base_limits = if adapter.get_info().backend == eframe::wgpu::Backend::Gl {
                    eframe::wgpu::Limits::downlevel_webgl2_defaults()
                } else {
                    eframe::wgpu::Limits::default()
                };

                let wanted = eframe::wgpu::Features::TEXTURE_FORMAT_16BIT_NORM;
                let required_features = wanted & adapter.features();

                eframe::wgpu::DeviceDescriptor {
                    label: Some("ironplayer wgpu device"),
                    required_features,
                    required_limits: eframe::wgpu::Limits {
                        max_texture_dimension_2d: 8192,
                        ..base_limits
                    },
                    memory_hints: eframe::wgpu::MemoryHints::default(),
                }
            }),
            ..Default::default()
        },
        ..Default::default()
    };

    eframe::run_native(
        title,
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(IronPlayerApp::new(
                cc, cmd_tx, None, None, None, None, None, None, None,
            )))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ts::metrics::{ErrorSnapshot, MetricsSnapshot, PcrJitterRecord};

    #[test]
    fn spec_ui_001_dashboard_layout_uses_requested_ratios() {
        let layout = compute_dashboard_layout(egui::vec2(1000.0, 800.0), egui::vec2(8.0, 8.0));

        assert_eq!(layout.top_height, 680.0);
        assert_eq!(layout.bottom_height, 120.0);
        assert_eq!(layout.left_width, 297.6);
        assert_eq!(layout.right_width, 694.4);
    }

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
