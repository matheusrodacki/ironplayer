//! Crate `ui-slint` — Interface Slint do IronPlayer (modo Broadcast).
//!
//! Substitui a UI egui (`crates/ui`). Consome os mesmos snapshots ao vivo
//! produzidos pelo pipeline em `src/main.rs` (canais + `Arc<RwLock>`), mapeia-os
//! para os modelos Slint e renderiza o vídeo via pipeline GPU (wgpu) ou CPU.

slint::include_modules!();

mod state;
mod video;

pub use state::{
    AppCommand, AppState, AspectRatioMode, AudioErrorSnapshot, AudioOperationalState,
    AudioStatusSnapshot, AudioTrackInfo, ConnectionState, HwAccelChoice, TableEvent, TablesSnapshot,
};

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use slint::{Color, ComponentHandle, Image, ModelRc, RenderingState, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use slint::wgpu_29::{wgpu, WGPUConfiguration};

use av::video_queue::PopResult;
use av::{Clock, MasterClock, VideoFrame, VideoQueue};
use ts::metrics::{AudioCodec, MetricsSnapshot, PidEntry, PidType, VideoCodec};
use ts::{Pid, StreamKind};

// ---------------------------------------------------------------------------
// Tipos de handle do pipeline
// ---------------------------------------------------------------------------

type SharedConn = Arc<RwLock<ConnectionState>>;
type SharedAudio = Arc<RwLock<AudioStatusSnapshot>>;
type SharedService = Arc<RwLock<Option<u16>>>;
type SharedPipeline = Arc<RwLock<ts::metrics::PipelineMetrics>>;
type SharedAudioClock = Arc<RwLock<Option<av::AudioClockHandle>>>;
type SharedMediaInfo = Arc<RwLock<ts::MediaInfoCodecSnapshot>>;

/// Pacote de handles do pipeline necessário para a UI.
pub struct PipelineHandles {
    pub cmd_tx: Sender<AppCommand>,
    pub snapshot_rx: ts::aggregator::SnapshotReceiver,
    pub conn_rx: SharedConn,
    pub audio_rx: SharedAudio,
    pub selected_service_rx: SharedService,
    pub table_events_rx: Receiver<TableEvent>,
    pub video_frames_rx: Receiver<VideoFrame>,
    pub pipeline_metrics_rx: SharedPipeline,
    pub audio_clock_rx: SharedAudioClock,
    pub media_info_rx: SharedMediaInfo,
    pub initial_url: String,
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

/// Inicia a janela Slint do IronPlayer e roda o event loop até o usuário fechar.
pub fn run(handles: PipelineHandles) -> Result<(), slint::PlatformError> {
    // Configura o backend wgpu (render zero-copy) antes de criar a janela.
    // Em falha, `gpu` é None e caímos no fallback CPU (femtovg/GL).
    let gpu = setup_wgpu_backend();

    let window = AppWindow::new()?;
    window.set_url(SharedString::from(handles.initial_url.as_str()));

    let selected_pid: Rc<RefCell<Option<Pid>>> = Rc::new(RefCell::new(None));

    // ── Callbacks → comandos ao backend ───────────────────────────────────
    {
        let cmd_tx = handles.cmd_tx.clone();
        window.on_connect(move |url| {
            let url = url.to_string();
            if !url.is_empty() {
                let _ = cmd_tx.try_send(AppCommand::Connect { url, iface: None });
            }
        });
    }
    {
        let cmd_tx = handles.cmd_tx.clone();
        window.on_disconnect(move || {
            let _ = cmd_tx.try_send(AppCommand::Disconnect);
        });
    }
    {
        let cmd_tx = handles.cmd_tx.clone();
        let selected = selected_pid.clone();
        window.on_select_pid(move |pid| {
            let pid = pid as Pid;
            *selected.borrow_mut() = Some(pid);
            let _ = cmd_tx.try_send(AppCommand::SelectPid { pid });
        });
    }
    {
        let cmd_tx = handles.cmd_tx.clone();
        window.on_select_service(move |service_id| {
            let _ = cmd_tx.try_send(AppCommand::SelectService {
                service_id: service_id as u16,
            });
        });
    }

    // ── Modo de render: GPU (zero-copy) ou CPU (fallback) ─────────────────
    let (render, gpu_bridge) = match gpu {
        Some((device, queue)) => match av::VideoRenderer::new(device, queue) {
            Ok(renderer) => {
                let hw_zc = renderer.supports_hw_zero_copy();
                av::set_gpu_zero_copy_enabled(hw_zc);
                tracing::info!(hw_zero_copy = hw_zc, "vídeo: pipeline GPU ativo");
                let bridge = Arc::new(GpuVideoBridge::new(renderer));
                (RenderMode::Gpu(bridge.clone()), Some(bridge))
            }
            Err(e) => {
                tracing::warn!(error = %e, "vídeo: VideoRenderer GPU falhou; fallback CPU");
                (spawn_cpu_worker(), None)
            }
        },
        None => (spawn_cpu_worker(), None),
    };

    // GPU: converte YUV→RGBA no `BeforeRendering` do Slint (mesmo ciclo de
    // apresentação), em vez de bloquear o timer da UI com `queue.submit`.
    if let Some(bridge) = gpu_bridge {
        let weak = window.as_weak();
        if let Err(e) = window.window().set_rendering_notifier(move |state, _api| {
            if !matches!(state, RenderingState::BeforeRendering) {
                return;
            }
            let Some(win) = weak.upgrade() else {
                return;
            };
            bridge.render_pending(&win);
        }) {
            tracing::warn!(error = ?e, "slint: set_rendering_notifier falhou");
        }
    }

    // ── Estado de polling (single-thread, na UI) ──────────────────────────
    let mut poller = Poller {
        snapshot_rx: handles.snapshot_rx,
        conn_rx: handles.conn_rx,
        audio_rx: handles.audio_rx,
        selected_service_rx: handles.selected_service_rx,
        table_events_rx: handles.table_events_rx,
        pipeline_metrics_rx: handles.pipeline_metrics_rx,
        media_info_rx: handles.media_info_rx,
        state: AppState::default(),
        last_snapshot_ts: None,
        seen_jitter: 0,
        selected_pid,
        tick: 0,
        video: VideoState::new(handles.video_frames_rx, handles.audio_clock_rx),
        render,
    };

    // Preenche já no primeiro tick.
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(16),
        move || {
            let Some(win) = weak.upgrade() else { return };
            poller.tick(&win);
        },
    );

    window.run()
}

// ---------------------------------------------------------------------------
// Ponte GPU: timing no timer, render no BeforeRendering do Slint
// ---------------------------------------------------------------------------

/// Estado compartilhado entre o timer (timing A/V) e o `RenderingNotifier`
/// (conversão YUV→RGBA na GPU, sincronizada com o ciclo de apresentação).
struct GpuVideoBridge {
    pending: Mutex<Option<VideoFrame>>,
    renderer: Mutex<av::VideoRenderer>,
}

impl GpuVideoBridge {
    fn new(renderer: av::VideoRenderer) -> Self {
        Self {
            pending: Mutex::new(None),
            renderer: Mutex::new(renderer),
        }
    }

    /// Enfileira o frame mais recente (descarta o anterior se ainda não exibido).
    fn set_pending(&self, frame: VideoFrame) {
        *self.pending.lock().expect("gpu bridge pending") = Some(frame);
    }

    /// Chamado em `RenderingState::BeforeRendering` — roda o shader e atualiza a `Image`.
    fn render_pending(&self, win: &AppWindow) {
        let frame = self.pending.lock().expect("gpu bridge pending").take();
        let Some(frame) = frame else {
            return;
        };
        let mut renderer = self.renderer.lock().expect("gpu bridge renderer");
        if let Some(tex) = renderer.render_to_texture(&frame) {
            match Image::try_from(tex) {
                Ok(img) => {
                    win.set_video_frame(img);
                    win.set_video_has_signal(true);
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "slint: Image::try_from(texture) falhou")
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Modo de render (GPU zero-copy / CPU fallback)
// ---------------------------------------------------------------------------

/// Estratégia de exibição de vídeo escolhida no boot.
enum RenderMode {
    /// GPU: shader YUV/NV12 → textura RGBA importada como `slint::Image`.
    Gpu(Arc<GpuVideoBridge>),
    /// CPU: thread worker converte para `SharedPixelBuffer` RGBA.
    Cpu {
        frame_tx: Sender<VideoFrame>,
        img_rx: Receiver<SharedPixelBuffer<Rgba8Pixel>>,
    },
}

/// Cria a stack wgpu (DX12) e instala o backend wgpu da Slint via
/// `require_wgpu_29(Manual)`. Devolve `device`/`queue` compartilhados para o
/// `VideoRenderer`, ou `None` se a GPU/wgpu não estiver disponível (→ fallback).
///
/// Usa o adapter de alta performance (mesmo GPU primário que o decoder D3D11VA),
/// pré-requisito para o compartilhamento de surface da Fase 2.
fn setup_wgpu_backend() -> Option<(Arc<wgpu::Device>, Arc<wgpu::Queue>)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        flags: wgpu::InstanceFlags::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        backend_options: wgpu::BackendOptions::default(),
        display: None,
    });
    let adapter = match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    })) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "wgpu: nenhum adapter DX12; fallback CPU");
            return None;
        }
    };
    let info = adapter.get_info();
    tracing::info!(adapter = %info.name, backend = ?info.backend, "wgpu: adapter selecionado");

    // Solicita `TEXTURE_FORMAT_NV12` quando o adapter suporta — habilita o
    // caminho zero-copy de hardware (Fase 2). Sem suporte, o flag fica desligado
    // e o decoder usa planos CPU.
    let nv12 = adapter.features() & wgpu::Features::TEXTURE_FORMAT_NV12;
    let (device, queue) = match pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("ironplayer-video"),
        required_features: nv12,
        required_limits: adapter.limits(),
        ..Default::default()
    })) {
        Ok(dq) => dq,
        Err(e) => {
            tracing::warn!(error = %e, "wgpu: request_device falhou; fallback CPU");
            return None;
        }
    };

    let device_for_renderer = device.clone();
    let queue_for_renderer = queue.clone();
    match slint::BackendSelector::new()
        .require_wgpu_29(WGPUConfiguration::Manual {
            instance,
            adapter,
            device,
            queue,
        })
        .select()
    {
        Ok(()) => {
            tracing::info!("slint: backend wgpu ativo (render zero-copy)");
            Some((
                Arc::new(device_for_renderer),
                Arc::new(queue_for_renderer),
            ))
        }
        Err(e) => {
            tracing::warn!(error = %e, "slint: require_wgpu_29 falhou; fallback CPU/GL");
            None
        }
    }
}

/// Sobe a thread worker de conversão CPU (YUV→RGBA) e devolve o modo CPU.
///
/// A conversão é cara; rodá-la na thread da UI bloquearia render/input. Canais
/// pequenos com descarte mantêm a latência baixa sob carga; a UI drena e usa
/// sempre o buffer mais recente.
fn spawn_cpu_worker() -> RenderMode {
    let (frame_tx, frame_rx) = crossbeam_channel::bounded::<VideoFrame>(2);
    let (img_tx, img_rx) = crossbeam_channel::bounded::<SharedPixelBuffer<Rgba8Pixel>>(2);
    std::thread::Builder::new()
        .name("slint-video-convert".into())
        .spawn(move || {
            for frame in frame_rx.iter() {
                if let Some(buf) = video::convert(&frame) {
                    let _ = img_tx.try_send(buf);
                }
            }
        })
        .expect("falha ao criar thread slint-video-convert");
    RenderMode::Cpu { frame_tx, img_rx }
}

// ---------------------------------------------------------------------------
// Poller
// ---------------------------------------------------------------------------

struct Poller {
    snapshot_rx: ts::aggregator::SnapshotReceiver,
    conn_rx: SharedConn,
    audio_rx: SharedAudio,
    selected_service_rx: SharedService,
    table_events_rx: Receiver<TableEvent>,
    pipeline_metrics_rx: SharedPipeline,
    media_info_rx: SharedMediaInfo,
    state: AppState,
    last_snapshot_ts: Option<Instant>,
    seen_jitter: usize,
    selected_pid: Rc<RefCell<Option<Pid>>>,
    tick: u64,
    video: VideoState,
    /// Estratégia de exibição (GPU zero-copy ou CPU via worker).
    render: RenderMode,
}

impl Poller {
    fn tick(&mut self, win: &AppWindow) {
        self.tick = self.tick.wrapping_add(1);

        // Vídeo a cada tick (~60 Hz): resolve o timing; render GPU no notifier.
        match &self.render {
            RenderMode::Gpu(bridge) => {
                if let Some(frame) = self.video.poll() {
                    bridge.set_pending(frame);
                    win.window().request_redraw();
                }
            }
            // CPU: despacha p/ o worker e exibe o buffer convertido mais recente.
            RenderMode::Cpu { frame_tx, img_rx } => {
                if let Some(frame) = self.video.poll() {
                    let _ = frame_tx.try_send(frame);
                }
                let mut latest = None;
                while let Ok(buf) = img_rx.try_recv() {
                    latest = Some(buf);
                }
                if let Some(buf) = latest {
                    win.set_video_frame(Image::from_rgba8(buf));
                    win.set_video_has_signal(true);
                }
            }
        }

        // Métricas/tabelas a ~4 Hz (snapshots chegam a 1 Hz).
        let refresh_meta = self.tick % 15 == 0;
        self.poll_table_events();
        self.poll_snapshot();

        if refresh_meta {
            self.apply_to_window(win);
        }
        // Sempre atualiza timecode e status leves (baratos).
        self.apply_live(win);
    }

    fn poll_table_events(&mut self) {
        let events: Vec<TableEvent> = self.table_events_rx.try_iter().take(512).collect();
        for event in events {
            if matches!(event, TableEvent::Reset) {
                self.reset_stream();
                continue;
            }
            self.state.apply_table_event(event);
        }
    }

    fn reset_stream(&mut self) {
        self.state.reset_stream_data();
        self.last_snapshot_ts = None;
        self.seen_jitter = 0;
        self.video.reset();
    }

    fn poll_snapshot(&mut self) {
        let snapshot = self.snapshot_rx.borrow();
        let now = Instant::now();
        update_metric_histories_if_new_snapshot(
            &mut self.state,
            &snapshot,
            &mut self.last_snapshot_ts,
            &mut self.seen_jitter,
            now,
        );
        let pipeline = self.state.metrics.pipeline.clone();
        self.state.metrics = snapshot;
        self.state.metrics.pipeline = pipeline;

        if let Ok(c) = self.conn_rx.read() {
            self.state.connection = c.clone();
        }
        if let Ok(a) = self.audio_rx.read() {
            self.state.audio = a.clone();
        }
        if let Ok(s) = self.selected_service_rx.read() {
            self.state.selected_service = *s;
        }
        if let Ok(p) = self.pipeline_metrics_rx.read() {
            self.state.metrics.pipeline = p.clone();
        }
        if let Ok(m) = self.media_info_rx.read() {
            self.state.media_info = m.clone();
        }
        self.state.selected_pid = *self.selected_pid.borrow();
    }

    /// Atualiza propriedades caras (modelos) — chamado a ~4 Hz.
    fn apply_to_window(&self, win: &AppWindow) {
        let st = &self.state;

        // PID rows
        let rows: Vec<PidRow> = st
            .metrics
            .pid_table
            .iter()
            .map(|e| pid_row(e, st))
            .collect();
        win.set_pid_count(rows.len() as i32);
        win.set_pid_rows(ModelRc::new(VecModel::from(rows)));
        win.set_pid_total(SharedString::from(format!(
            "{:.1} Mbps",
            st.metrics.total_bitrate_kbps / 1000.0
        )));

        // Serviços (SDT)
        win.set_services(ModelRc::new(VecModel::from(build_services(st))));

        // Media info
        let (video_info, audio_info, res, caption) = build_media_info(st);
        win.set_video_info(ModelRc::new(VecModel::from(video_info)));
        win.set_audio_info(ModelRc::new(VecModel::from(audio_info)));
        win.set_video_res(SharedString::from(res));
        win.set_video_caption(SharedString::from(caption));

        // PSI/SI grade
        win.set_psi_rows(ModelRc::new(VecModel::from(build_psi_rows(st))));

        // Gráficos
        let (b_val, b_sub, b_area, b_line) = build_bitrate_chart(st);
        win.set_bitrate_value(SharedString::from(b_val));
        win.set_bitrate_sub(SharedString::from(b_sub));
        win.set_bitrate_area(SharedString::from(b_area));
        win.set_bitrate_line(SharedString::from(b_line));

        let (j_val, j_sub, j_line) = build_jitter_chart(st);
        win.set_jitter_value(SharedString::from(j_val));
        win.set_jitter_sub(SharedString::from(j_sub));
        win.set_jitter_line(SharedString::from(j_line));
    }

    /// Atualiza propriedades leves a cada tick (conexão, status bar, timecode).
    fn apply_live(&self, win: &AppWindow) {
        let st = &self.state;
        let (connected, connecting, label) = match &st.connection {
            ConnectionState::Idle => (false, false, "Desconectado".to_string()),
            ConnectionState::Connecting { url } => (false, true, format!("Conectando {url}")),
            ConnectionState::Connected { url, .. } => (true, false, format!("Conectado {url}")),
            ConnectionState::Error { reason, .. } => (false, false, format!("Erro: {reason}")),
        };
        win.set_connected(connected);
        win.set_connecting(connecting);
        win.set_conn_label(SharedString::from(label));
        win.set_live(connected);

        win.set_total_mbps(SharedString::from(format!(
            "{:.1}",
            st.metrics.total_bitrate_kbps / 1000.0
        )));
        win.set_cc_total(SharedString::from(format!(
            "{}",
            st.metrics.errors.total_cc_errors()
        )));
        win.set_buffer_pct(SharedString::from(format!(
            "{}%",
            (st.audio.buffer_level * 100.0).round() as i32
        )));
        win.set_audio_summary(SharedString::from(audio_summary(st)));
        win.set_hw_summary(SharedString::from(hw_summary(st)));

        win.set_timecode(SharedString::from(timecode(st)));
        if !connected {
            win.set_video_has_signal(false);
        }
    }
}

// ---------------------------------------------------------------------------
// VideoState — fila PTS-ordenada + clock (porte enxuto de crates/ui)
// ---------------------------------------------------------------------------

struct VideoState {
    rx: Receiver<VideoFrame>,
    audio_clock_rx: SharedAudioClock,
    queue: VideoQueue,
    clock: MasterClock,
    clock_init: bool,
    uses_audio: bool,
    adopted_audio_id: Option<usize>,
    pts_interval: Option<i64>,
    last_pts: Option<i64>,
}

impl VideoState {
    fn new(rx: Receiver<VideoFrame>, audio_clock_rx: SharedAudioClock) -> Self {
        Self {
            rx,
            audio_clock_rx,
            queue: VideoQueue::default(),
            clock: MasterClock::wall(0),
            clock_init: false,
            uses_audio: false,
            adopted_audio_id: None,
            pts_interval: None,
            last_pts: None,
        }
    }

    fn reset(&mut self) {
        while self.rx.try_recv().is_ok() {}
        self.queue.clear();
        self.clock = MasterClock::wall(0);
        self.clock_init = false;
        self.uses_audio = false;
        self.adopted_audio_id = None;
        self.pts_interval = None;
        self.last_pts = None;
        if let Ok(mut g) = self.audio_clock_rx.write() {
            *g = None;
        }
    }

    /// Resolve o timing (clock + fila) e devolve o próximo frame pronto para
    /// exibição, **sem** converter (a conversão roda no worker).
    fn poll(&mut self) -> Option<VideoFrame> {
        // 1. Adota/re-adota AudioClock.
        if let Ok(guard) = self.audio_clock_rx.try_read() {
            match guard.as_ref() {
                Some(handle) => {
                    let id = handle.id();
                    if self.adopted_audio_id != Some(id) {
                        self.clock = MasterClock::Audio(handle.clone());
                        self.adopted_audio_id = Some(id);
                        self.uses_audio = true;
                        self.clock_init = true;
                    }
                }
                None => {
                    if self.adopted_audio_id.is_some() {
                        self.adopted_audio_id = None;
                        self.uses_audio = false;
                    }
                }
            }
        }

        // 2. Drena frames do canal para a fila.
        let mut frames = Vec::new();
        for f in self.rx.try_iter().take(16) {
            frames.push(f);
        }
        for frame in frames {
            if !self.uses_audio && !self.clock_init {
                if let Some(pts) = frame.pts() {
                    self.clock.reset(pts as i64);
                    self.clock_init = true;
                }
            }
            if let Some(pts) = frame.pts() {
                let pts = pts as i64;
                if let Some(last) = self.last_pts {
                    let delta = pts - last;
                    if (90..=10_800).contains(&delta) {
                        self.pts_interval = Some(match self.pts_interval {
                            Some(prev) => (prev * 7 + delta) / 8,
                            None => delta,
                        });
                    }
                }
                self.last_pts = Some(pts);
            }
            self.queue.push(frame);
        }

        // 2b. Capacidade por tempo (~2,5 s).
        if let Some(interval) = self.pts_interval {
            const TARGET_HOLD_TICKS: i64 = 225_000;
            let target = (TARGET_HOLD_TICKS / interval.max(1))
                .clamp(av::VIDEO_QUEUE_CAPACITY as i64, 160) as usize;
            if target != self.queue.capacity() {
                self.queue.set_capacity(target);
            }
        }

        // 3. Extrai próximo frame pronto.
        let allow_resync = self.clock.audio_handle().is_none();
        let clock_pts = self.clock.now_pts90();
        match self.queue.pop_ready_with_resync(clock_pts, allow_resync) {
            PopResult::Ready(f) => Some(f),
            PopResult::Resync { frame, new_anchor } => {
                if allow_resync {
                    self.clock.reset(new_anchor);
                }
                Some(frame)
            }
            PopResult::TooEarly | PopResult::Empty => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Mapeamento AppState → modelos Slint
// ---------------------------------------------------------------------------

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb_u8(r, g, b)
}

fn pid_type_badge(t: &PidType) -> Option<(&'static str, Color)> {
    let blue = rgb(0x5a, 0xa0, 0xd0);
    let orange = rgb(0xe8, 0x94, 0x3a);
    let teal = rgb(0x57, 0xc0, 0x8a);
    let purple = rgb(0xa9, 0x8a, 0xd6);
    let faint = rgb(0x58, 0x64, 0x70);
    match t {
        PidType::Pat => Some(("PAT", blue)),
        PidType::Pmt => Some(("PMT", blue)),
        PidType::Nit => Some(("NIT", blue)),
        PidType::Sdt => Some(("SDT", blue)),
        PidType::Eit => Some(("EIT", blue)),
        PidType::Tdt => Some(("TDT", blue)),
        PidType::Bat => Some(("BAT", blue)),
        PidType::Pcr => Some(("PCR", purple)),
        PidType::Video { .. } => Some(("Vídeo", orange)),
        PidType::Audio { .. } => Some(("Áudio", teal)),
        PidType::NullPacket => Some(("NULL", faint)),
        PidType::Unknown => None,
    }
}

/// Classifica o PID para o badge, com fallbacks quando `pid_type` é `Unknown`:
/// 1) o próprio `pid_type`; 2) PID por número (null/PSI bem-conhecidos);
/// 3) `StreamKind` do probe Media Info (vídeo/áudio/dados).
fn classify_pid(e: &PidEntry, st: &AppState) -> (String, Color) {
    let blue = rgb(0x5a, 0xa0, 0xd0);
    let orange = rgb(0xe8, 0x94, 0x3a);
    let teal = rgb(0x57, 0xc0, 0x8a);
    let faint = rgb(0x58, 0x64, 0x70);

    if let Some((label, color)) = pid_type_badge(&e.pid_type) {
        return (label.to_string(), color);
    }
    // Fallback por número de PID.
    match e.pid {
        0x1FFF => return ("NULL".into(), faint),
        0x0000 => return ("PAT".into(), blue),
        0x0010 => return ("NIT".into(), blue),
        0x0011 => return ("SDT".into(), blue),
        0x0012 => return ("EIT".into(), blue),
        0x0014 => return ("TDT".into(), blue),
        _ => {}
    }
    // Fallback pelo probe Media Info.
    if let Some(kind) = st.media_info.get(e.pid).and_then(|ci| ci.kind) {
        return match kind {
            StreamKind::Video => ("Vídeo".into(), orange),
            StreamKind::Audio => ("Áudio".into(), teal),
            StreamKind::Data | StreamKind::Menu => ("Dados".into(), faint),
        };
    }
    ("—".into(), faint)
}

/// Label legível com fallback para o codec/kind do probe quando vazio.
fn pid_label(e: &PidEntry, st: &AppState) -> String {
    if !e.label.trim().is_empty() {
        return e.label.clone();
    }
    if let Some(ci) = st.media_info.get(e.pid) {
        if let Some(fmt) = &ci.format {
            return fmt.clone();
        }
        if let Some(kind) = ci.kind {
            return format!("{kind:?}");
        }
    }
    String::new()
}

fn pid_row(e: &PidEntry, st: &AppState) -> PidRow {
    let (kind, color) = classify_pid(e, st);
    PidRow {
        pid_dec: SharedString::from(format!("{}", e.pid)),
        pid_hex: SharedString::from(format!("0x{:04X}", e.pid)),
        kind: SharedString::from(kind),
        kind_color: color,
        label: SharedString::from(pid_label(e, st)),
        kbps: SharedString::from(format!("{:.1}", e.bitrate_kbps)),
        cc: SharedString::from(format!("{}", e.cc_errors)),
        pkts: SharedString::from(format!("{}", e.packet_count)),
        has_errors: e.cc_errors > 0,
        selected: st.selected_pid == Some(e.pid),
        pid: e.pid as i32,
    }
}

/// Lista de serviços a partir do SDT.
fn build_services(st: &AppState) -> Vec<ServiceRow> {
    let Some(sdt) = &st.tables.sdt else {
        return Vec::new();
    };
    sdt.services
        .iter()
        .map(|s| {
            let name = s
                .service_name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| format!("Serviço {}", s.service_id));
            let provider = s.provider_name.clone().unwrap_or_default();
            ServiceRow {
                id: SharedString::from(format!("{} (0x{:04X})", s.service_id, s.service_id)),
                name: SharedString::from(name),
                provider: SharedString::from(provider),
                running: SharedString::from(format!("{:?}", s.running_status)),
                // DVB: free_CA_mode = 1 → serviço sob controle de acesso (scrambled).
                scrambled: s.free_ca_mode,
                selected: st.selected_service == Some(s.service_id),
                service_id: s.service_id as i32,
            }
        })
        .collect()
}

fn info(key: &str, value: String, accent: bool) -> InfoRow {
    InfoRow {
        key: SharedString::from(key),
        value: SharedString::from(value),
        accent,
    }
}

fn video_codec_label(c: &VideoCodec) -> String {
    match c {
        VideoCodec::H264 => "H.264 / AVC".into(),
        VideoCodec::H265 => "H.265 / HEVC".into(),
        VideoCodec::Mpeg2 => "MPEG-2 Video".into(),
        VideoCodec::Unknown(t) => format!("Vídeo (0x{t:02X})"),
    }
}

fn audio_codec_label(c: &AudioCodec) -> String {
    match c {
        AudioCodec::Aac => "AAC".into(),
        AudioCodec::Ac3 => "AC-3".into(),
        AudioCodec::Eac3 => "E-AC-3".into(),
        AudioCodec::MpegAudio => "MPEG Audio".into(),
        AudioCodec::Unknown(t) => format!("Áudio (0x{t:02X})"),
    }
}

/// Constrói os blocos de Media Info (vídeo, áudio), resolução e caption do vídeo.
fn build_media_info(st: &AppState) -> (Vec<InfoRow>, Vec<InfoRow>, String, String) {
    // PID de vídeo principal, com fallbacks:
    // 1) maior bitrate entre PidType::Video;
    // 2) PID cujo probe Media Info reporta kind=Video;
    // 3) maior bitrate geral excluindo null packets (0x1FFF).
    let video_pid = st
        .metrics
        .pid_table
        .iter()
        .filter(|e| matches!(e.pid_type, PidType::Video { .. }))
        .max_by(|a, b| a.bitrate_kbps.total_cmp(&b.bitrate_kbps))
        .or_else(|| {
            st.metrics
                .pid_table
                .iter()
                .filter(|e| {
                    st.media_info
                        .get(e.pid)
                        .and_then(|ci| ci.kind)
                        .is_some_and(|k| matches!(k, StreamKind::Video))
                })
                .max_by(|a, b| a.bitrate_kbps.total_cmp(&b.bitrate_kbps))
        })
        .or_else(|| {
            st.metrics
                .pid_table
                .iter()
                .filter(|e| e.pid != 0x1FFF && !matches!(e.pid_type, PidType::NullPacket))
                .max_by(|a, b| a.bitrate_kbps.total_cmp(&b.bitrate_kbps))
        });

    let mut video = Vec::new();
    let mut res = "—".to_string();
    let mut caption = "sem sinal".to_string();
    if let Some(e) = video_pid {
        let ci = st.media_info.get(e.pid);
        // Codec: tipo da PMT, ou formato do probe (HEVC/AVC), ou genérico.
        let codec = match &e.pid_type {
            PidType::Video { codec } => video_codec_label(codec),
            _ => ci
                .and_then(|c| c.format.clone())
                .map(|f| match f.as_str() {
                    "HEVC" => "H.265 / HEVC".to_string(),
                    "AVC" => "H.264 / AVC".to_string(),
                    other => other.to_string(),
                })
                .unwrap_or_else(|| "Vídeo".into()),
        };
        video.push(info("Codec", codec.clone(), true));
        if let Some(ci) = ci {
            if let Some(p) = &ci.format_profile {
                video.push(info("Perfil / Nível", p.clone(), false));
            }
            if let (Some(w), Some(h)) = (ci.width, ci.height) {
                res = format!("{w} × {h}");
                video.push(info("Resolução", res.clone(), false));
            }
            if let Some(fr) = &ci.frame_rate {
                video.push(info("Frame rate", fr.clone(), false));
            }
            if let Some(a) = &ci.display_aspect_ratio {
                video.push(info("Aspecto", a.clone(), false));
            }
        }
        video.push(info(
            "Bitrate",
            format!("{:.1} Mbps", e.bitrate_kbps / 1000.0),
            false,
        ));
        video.push(info("PID", format!("{} (0x{:04X})", e.pid, e.pid), false));

        let fps = ci.and_then(|c| c.frame_rate.clone()).unwrap_or_default();
        let mbps = format!("{:.1} Mbps", e.bitrate_kbps / 1000.0);
        let parts: Vec<String> = [
            codec.clone(),
            if res != "—" { res.clone() } else { String::new() },
            fps,
            mbps,
        ]
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect();
        caption = parts.join(" · ");
    }

    // Áudio: trilha ativa, ou primeiro PID de áudio.
    let audio_pid = st
        .audio
        .active_track
        .as_ref()
        .map(|t| t.pid)
        .or_else(|| {
            st.metrics
                .pid_table
                .iter()
                .find(|e| matches!(e.pid_type, PidType::Audio { .. }))
                .map(|e| e.pid)
        });

    let mut audio = Vec::new();
    if let Some(pid) = audio_pid {
        let entry = st.metrics.pid_table.iter().find(|e| e.pid == pid);
        let codec = match entry.map(|e| &e.pid_type) {
            Some(PidType::Audio { codec }) => audio_codec_label(codec),
            _ => st
                .audio
                .active_track
                .as_ref()
                .map(|t| t.codec_label.clone())
                .unwrap_or_else(|| "—".into()),
        };
        audio.push(info("Codec", codec, true));
        if let Some(sr) = st.audio.sample_rate_hz {
            audio.push(info("Amostragem", format!("{:.1} kHz", sr as f64 / 1000.0), false));
        }
        if let Some(ch) = st.audio.channels.or(st.audio.source_channels) {
            audio.push(info("Canais", format!("{ch} ch"), false));
        }
        if let Some(br) = st.audio.encoded_bitrate_kbps.or(st.audio.stream_bitrate_kbps) {
            audio.push(info("Bitrate", format!("{br:.1} kbps"), false));
        }
        if let Some(lang) = st.audio.active_track.as_ref().and_then(|t| t.language.clone()) {
            audio.push(info("Idioma", lang, false));
        }
        audio.push(info("PID", format!("{pid} (0x{pid:04X})"), false));
    }

    (video, audio, res, caption)
}

/// Card PSI/SI a partir de presença na `TablesSnapshot`.
fn psi_card(name: &str, detail: &str, present: bool) -> PsiCard {
    PsiCard {
        name: SharedString::from(name),
        detail: SharedString::from(detail),
        present,
    }
}

fn build_psi_rows(st: &AppState) -> Vec<PsiRow> {
    let t = &st.tables;
    let cards = vec![
        psi_card("PAT", "MPEG · programas", t.pat.is_some()),
        psi_card("PMT", "MPEG · serviços", !t.pmts.is_empty()),
        psi_card("SDT", "DVB · atual", t.sdt.is_some()),
        psi_card("NIT", "DVB · rede", t.nit.is_some()),
        psi_card("EIT", "DVB · p/f", !t.eit_pf.is_empty()),
        psi_card("TDT / TOT", "DVB · UTC", t.tdt.is_some() || t.tot.is_some()),
        psi_card("BAT", "DVB · bouquet", t.bat.is_some()),
        psi_card("CAT", "MPEG · CA", t.cat.is_some()),
    ];

    cards
        .chunks(2)
        .map(|chunk| PsiRow {
            a: chunk[0].clone(),
            b: chunk.get(1).cloned().unwrap_or_else(|| psi_card("", "", false)),
            has_b: chunk.len() > 1,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Gráficos (path commands SVG no viewbox 0..100)
// ---------------------------------------------------------------------------

/// Gera comandos de linha "M x y L x y …" a partir de pontos (x,y) em 0..100.
fn line_path(points: &[(f32, f32)]) -> String {
    if points.is_empty() {
        return String::new();
    }
    let mut s = String::with_capacity(points.len() * 12);
    for (i, (x, y)) in points.iter().enumerate() {
        if i == 0 {
            s.push_str(&format!("M {x:.2} {y:.2}"));
        } else {
            s.push_str(&format!(" L {x:.2} {y:.2}"));
        }
    }
    s
}

/// Variante preenchida: fecha a linha até a base (y=100).
fn area_path(points: &[(f32, f32)]) -> String {
    if points.is_empty() {
        return String::new();
    }
    let mut s = line_path(points);
    let last_x = points.last().unwrap().0;
    let first_x = points.first().unwrap().0;
    s.push_str(&format!(" L {last_x:.2} 100 L {first_x:.2} 100 Z"));
    s
}

fn build_bitrate_chart(st: &AppState) -> (String, String, String, String) {
    let hist = &st.bitrate_history;
    let value = format!("{:.1} Mbps", st.metrics.total_bitrate_kbps / 1000.0);
    if hist.len() < 2 {
        return (value, String::new(), String::new(), String::new());
    }
    let now = Instant::now();
    let max = hist
        .iter()
        .map(|(_, v)| *v)
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let peak = max / 1000.0;
    let points: Vec<(f32, f32)> = hist
        .iter()
        .map(|(t, v)| {
            let age = now.duration_since(*t).as_secs_f64().min(60.0);
            let x = (1.0 - age / 60.0) * 100.0;
            // 8% de headroom no topo.
            let y = 100.0 - (v / max * 92.0);
            (x as f32, y as f32)
        })
        .collect();
    (
        value,
        format!("pico {peak:.1}"),
        area_path(&points),
        line_path(&points),
    )
}

fn build_jitter_chart(st: &AppState) -> (String, String, String) {
    // PID com mais registros de jitter (PCR principal).
    let hist = st
        .pcr_history
        .values()
        .max_by_key(|h| h.len());
    let Some(hist) = hist else {
        return ("±0".into(), String::new(), String::new());
    };
    if hist.is_empty() {
        return ("±0".into(), String::new(), String::new());
    }
    let now = Instant::now();
    // Escala ±500 µs em torno do centro (y=50).
    const SCALE_US: f64 = 500.0;
    let mut peak = 0.0_f64;
    let mut last_abs = 0.0_f64;
    let points: Vec<(f32, f32)> = hist
        .iter()
        .map(|r| {
            let jitter_us = (r.measured_us - r.expected_us) as f64;
            let abs = jitter_us.abs();
            if abs > peak {
                peak = abs;
            }
            last_abs = abs;
            let age = now.duration_since(r.timestamp).as_secs_f64().min(60.0);
            let x = (1.0 - age / 60.0) * 100.0;
            let norm = (jitter_us / SCALE_US).clamp(-1.0, 1.0);
            let y = 50.0 - norm * 48.0;
            (x as f32, y as f32)
        })
        .collect();
    (
        format!("±{:.0}", last_abs),
        format!("pico {peak:.0} µs"),
        line_path(&points),
    )
}

// ---------------------------------------------------------------------------
// Status bar / timecode helpers
// ---------------------------------------------------------------------------

fn audio_summary(st: &AppState) -> String {
    let codec = st
        .audio
        .active_track
        .as_ref()
        .map(|t| t.codec_label.clone())
        .unwrap_or_else(|| "—".into());
    let sr = st
        .audio
        .sample_rate_hz
        .map(|s| format!("{:.1} kHz", s as f64 / 1000.0))
        .unwrap_or_default();
    let ch = st
        .audio
        .channels
        .or(st.audio.source_channels)
        .map(|c| format!("{c}ch"))
        .unwrap_or_default();
    [codec, sr, ch]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

fn hw_summary(st: &AppState) -> String {
    let p = &st.metrics.pipeline;
    let mode = if p.hw_decode_active {
        "HW decode"
    } else {
        "CPU decode"
    };
    match &p.gpu_adapter_name {
        Some(name) => format!("{name} · {mode}"),
        None => mode.to_string(),
    }
}

/// Timecode derivado do tempo decorrido desde a conexão (HH:MM:SS:FF).
fn timecode(st: &AppState) -> String {
    let since = match &st.connection {
        ConnectionState::Connected { since, .. } => *since,
        _ => return "--:--:--:--".into(),
    };
    let elapsed = since.elapsed();
    let total = elapsed.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    // ~30 frames/s para o campo de frames (visual).
    let f = ((elapsed.subsec_millis() as f64 / 1000.0) * 30.0) as u32 % 30;
    format!("{h:02}:{m:02}:{s:02}:{f:02}")
}

// ---------------------------------------------------------------------------
// Históricos de métricas (porte de crates/ui)
// ---------------------------------------------------------------------------

fn update_metric_histories_if_new_snapshot(
    state: &mut AppState,
    snapshot: &MetricsSnapshot,
    last_snapshot_timestamp: &mut Option<Instant>,
    seen_jitter: &mut usize,
    now: Instant,
) {
    if last_snapshot_timestamp.is_some_and(|t| t == snapshot.timestamp) {
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
        .is_some_and(|(t, _)| *t < cutoff)
    {
        state.bitrate_history.pop_front();
    }

    let jitter_events = &snapshot.errors.pcr_jitter_events;
    if jitter_events.len() < *seen_jitter {
        *seen_jitter = 0;
        state.pcr_history.clear();
    }
    for record in jitter_events.iter().skip(*seen_jitter) {
        state
            .pcr_history
            .entry(record.pid)
            .or_insert_with(VecDeque::new)
            .push_back(record.clone());
    }
    *seen_jitter = jitter_events.len();

    for history in state.pcr_history.values_mut() {
        while history.front().is_some_and(|r| r.timestamp < cutoff) {
            history.pop_front();
        }
    }
    state.pcr_history.retain(|_, h| !h.is_empty());
}
