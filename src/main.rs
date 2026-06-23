mod channels;
mod config;
mod ffmpeg_check;
mod table_dispatcher;

use bytes::Bytes;
use channels::BoundedSender;
use config::{AppConfig, HwAccelChoice};
use net::{
    ReceiverConfig, RtpStripper, StopHandle as NetStopHandle, StopToken as NetStopToken, StreamUrl,
    UdpReceiver,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use table_dispatcher::{DecodeCommand, DemuxCommand, PesCommand, TableCommand, TableDispatcher};
use ts::{
    aggregator::{
        AggregatorNetEvent, MetricsAggregator, StopHandle as MetricsStopHandle,
        StopToken as MetricsStopToken,
    },
    CompleteSection, SectionAssembler, SectionData, TsDemuxer,
};
use ui::IronPlayerApp;

// ── CLI parsing ───────────────────────────────────────────────────────────────

/// Argumentos de linha de comando reconhecidos pelo IronPlayer.
///
/// Faz parsing manual para evitar dependência de `clap`.  Suporta:
/// - `--hwaccel <auto|d3d11va|none>` (SPEC-CFG-HW-001)
/// - `--help` / `-h`
///
/// Valores ausentes mantêm o `HwAccelChoice` lido do `ironstream.toml`.
struct CliArgs {
    hwaccel_override: Option<HwAccelChoice>,
}

impl CliArgs {
    fn parse_from(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut hwaccel_override = None;
        let mut iter = args.into_iter().skip(1); // pula nome do binário
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    eprintln!(
                        "IronPlayer — uso:\n  \
                         ironplayer [--hwaccel auto|d3d11va|none]\n"
                    );
                    std::process::exit(0);
                }
                "--hwaccel" => {
                    let v = iter
                        .next()
                        .ok_or_else(|| "--hwaccel requer um valor".to_string())?;
                    hwaccel_override = Some(HwAccelChoice::parse_cli(&v)?);
                }
                other if other.starts_with("--hwaccel=") => {
                    let v = &other["--hwaccel=".len()..];
                    hwaccel_override = Some(HwAccelChoice::parse_cli(v)?);
                }
                other => {
                    return Err(format!("argumento desconhecido: '{other}' (use --help)"));
                }
            }
        }
        Ok(Self { hwaccel_override })
    }
}

/// Mantém os senders do pipeline vivos até o shutdown limpo.
///
/// Enquanto este guard existir, os canais permanecem abertos.  Ao ser dropado
/// em `IronPlayerApp::on_exit`, desencadeia o encerramento em cascata:
///
/// `net_raw` fecha → rtp-strip sai → `ts_raw` fecha → ts-demux sai →
/// `section_data` fecha → sec-asm sai → `complete_sections` fecha →
/// table-disp sai.
// Campos mantidos por RAII: nunca lidos, apenas dropados no shutdown.
#[allow(dead_code)]
struct SenderGuard {
    net_raw_tx: BoundedSender<Bytes>,
    section_data_tx: BoundedSender<SectionData>,
    complete_sections_tx: BoundedSender<CompleteSection>,
    ts_events_tx: BoundedSender<ts::TsEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioCommand {
    Reset,
}

// ── PipelineGuard ─────────────────────────────────────────────────────────────

/// Encapsula handles e recursos do pipeline para shutdown limpo.
///
/// Injetado em `IronPlayerApp` via `eframe::CreationContext` e recuperado
/// em `on_exit` para encerramento em cascata.
struct PipelineGuard {
    pipeline_handles: Vec<Option<std::thread::JoinHandle<()>>>,
    metrics_handle: Option<std::thread::JoinHandle<()>>,
    /// Handle da conexão de rede atual; compartilhado com o cmd-handler.
    current_net_stop: Arc<Mutex<Option<NetStopHandle>>>,
    metrics_stop_handle: Option<MetricsStopHandle>,
    _sender_guard: Option<SenderGuard>,
}

impl PipelineGuard {
    fn shutdown(&mut self) {
        tracing::info!("shutdown iniciado pelo eframe::App::on_exit");

        // 1. Para a conexão de rede ativa (se houver)
        if let Some(h) = self.current_net_stop.lock().unwrap().take() {
            h.stop();
        }
        if let Some(h) = self.metrics_stop_handle.take() {
            h.stop();
        }

        // 2. Dropa SenderGuard → fecha canal `net_raw` → cascata de encerramento
        drop(self._sender_guard.take());

        // 3. Join de todas as threads com budget total de 2 s
        let deadline = Instant::now() + Duration::from_secs(2);

        for handle_opt in self.pipeline_handles.iter_mut() {
            if let Some(handle) = handle_opt.take() {
                join_with_deadline(handle, deadline);
            }
        }
        if let Some(handle) = self.metrics_handle.take() {
            join_with_deadline(handle, deadline);
        }

        tracing::info!("shutdown limpo concluido");
    }
}

// ── IronPlayerAppExt ──────────────────────────────────────────────────────────

/// Wrapper que adiciona shutdown do pipeline ao `IronPlayerApp` do crate `ui`.
struct IronPlayerAppWithPipeline {
    inner: IronPlayerApp,
    guard: PipelineGuard,
}

impl eframe::App for IronPlayerAppWithPipeline {
    fn update(&mut self, ctx: &eframe::egui::Context, frame: &mut eframe::Frame) {
        self.inner.update(ctx, frame);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.inner.close_command_channel();
        self.guard.shutdown();
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Faz join de um `JoinHandle` respeitando um deadline global.
///
/// Spawna uma thread auxiliar para nao bloquear o deadline total.
/// Emite `WARN` se o deadline expirar antes da thread encerrar.
fn join_with_deadline(handle: std::thread::JoinHandle<()>, deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        tracing::warn!(
            thread = handle.thread().name().unwrap_or("<sem-nome>"),
            "timeout de shutdown esgotado -- thread pode estar orfa",
        );
        return;
    }

    let name = handle.thread().name().unwrap_or("<sem-nome>").to_owned();
    let (tx, rx) = crossbeam_channel::bounded::<()>(1);
    std::thread::spawn(move || {
        let _ = handle.join();
        let _ = tx.send(());
    });

    if rx.recv_timeout(remaining).is_err() {
        tracing::warn!(thread = %name, "thread nao encerrou no timeout de 2s");
    }
}

fn refresh_audio_status_from_output(
    audio_status: &Arc<std::sync::RwLock<ui::AudioStatusSnapshot>>,
    audio_out: &av::AudioOutput,
) {
    if let Ok(mut status) = audio_status.write() {
        status.sample_rate_hz = Some(audio_out.sample_rate);
        status.channels = Some(audio_out.channels);
        status.buffer_level = audio_out.buffer_level();
        status.errors.underruns = audio_out.underrun_count();
        status.errors.overruns = audio_out.overrun_count();
        status.state = if status.buffer_level >= 0.5 {
            ui::AudioOperationalState::Playing
        } else {
            ui::AudioOperationalState::Buffering
        };
    }
}

struct StreamResetTargets<'a> {
    selected_service: &'a Arc<std::sync::RwLock<Option<u16>>>,
    selected_audio_pid: &'a Arc<std::sync::RwLock<Option<u16>>>,
    table_cmd_tx: &'a crossbeam_channel::Sender<TableCommand>,
    agg_net_tx: &'a crossbeam_channel::Sender<AggregatorNetEvent>,
    demux_cmd_tx: &'a crossbeam_channel::Sender<DemuxCommand>,
    pes_cmd_tx: &'a crossbeam_channel::Sender<PesCommand>,
    decode_cmd_tx: &'a crossbeam_channel::Sender<DecodeCommand>,
    audio_cmd_tx: &'a crossbeam_channel::Sender<AudioCommand>,
}

fn reset_stream_routing(targets: StreamResetTargets<'_>) {
    if let Ok(mut service) = targets.selected_service.write() {
        *service = None;
    }
    if let Ok(mut audio_pid) = targets.selected_audio_pid.write() {
        *audio_pid = None;
    }
    if targets.table_cmd_tx.try_send(TableCommand::Reset).is_err() {
        tracing::warn!("canal table-control cheio — Reset descartado");
    }
    if targets
        .agg_net_tx
        .try_send(AggregatorNetEvent::Reset)
        .is_err()
    {
        tracing::warn!("canal metrics-control cheio — Reset descartado");
    }
    if targets.demux_cmd_tx.try_send(DemuxCommand::Reset).is_err() {
        tracing::warn!("canal demux-control cheio — Reset descartado");
    }
    if targets.pes_cmd_tx.try_send(PesCommand::Reset).is_err() {
        tracing::warn!("canal pes-control cheio — Reset descartado");
    }
    if targets
        .decode_cmd_tx
        .try_send(DecodeCommand::Reset)
        .is_err()
    {
        tracing::warn!("canal decode-control cheio — Reset descartado");
    }
    if targets.audio_cmd_tx.try_send(AudioCommand::Reset).is_err() {
        tracing::warn!("canal audio-control cheio — Reset descartado");
    }
}

fn bootstrap_d3d11_device(
    hwaccel_choice: HwAccelChoice,
    pipeline_metrics: &Arc<std::sync::RwLock<ts::metrics::PipelineMetrics>>,
) -> Option<std::sync::Arc<av::D3d11Device>> {
    let hwaccel_request_active = !matches!(hwaccel_choice, HwAccelChoice::None);

    #[cfg(windows)]
    {
        if !hwaccel_request_active {
            tracing::info!("hw: --hwaccel=none — bootstrap D3D11 ignorado; decode 100% CPU");
            return None;
        }

        match av::D3d11Device::new() {
            Ok(dev) => {
                tracing::info!(
                    adapter = dev.adapter_description(),
                    luid = dev.adapter_luid().as_u64(),
                    vendor_id = format!("{:#06x}", dev.vendor_id()),
                    hwaccel_choice = hwaccel_choice.label(),
                    "hw: D3d11Device bootstrap OK (Fase E)"
                );
                if let Ok(mut metrics) = pipeline_metrics.write() {
                    av::AdapterInfo::from_device(&dev).apply_to_metrics(&mut metrics);
                }
                Some(dev)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    hwaccel_choice = hwaccel_choice.label(),
                    "hw: D3d11Device bootstrap falhou — hwaccel desabilitado; usando decode CPU"
                );
                None
            }
        }
    }

    #[cfg(not(windows))]
    {
        let _ = hwaccel_request_active;
        let _ = pipeline_metrics;
        None
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() -> eframe::Result<()> {
    // 1. Init tracing
    //
    // O filtro default silencia bibliotecas de UI/GPU notoriamente barulhentas
    // (`wgpu`, `naga`, `winit`, `glutin`, `eframe`) em nível INFO; sem isso a
    // `Device::maintain: waiting for submission index N` é emitida a cada
    // submit GPU e satura o terminal, derrubando o FPS efetivo do player.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(
                    "info,wgpu=warn,wgpu_core=warn,wgpu_hal=error,naga=warn,\
                     winit=warn,glutin=warn,eframe=warn,egui_wgpu=warn",
                )
            }),
        )
        .init();

    // 2. Verifica DLLs FFmpeg (SPEC-AV-CHECK-001)
    if let Err(e) = ffmpeg_check::check_ffmpeg_compatibility() {
        tracing::error!(error = %e, "verificacao de DLLs FFmpeg falhou -- encerrando");
        eprintln!("\n[IronPlayer] ERRO: DLLs FFmpeg incompativeis ou nao encontradas\n");
        eprintln!("{e}\n");
        std::process::exit(1);
    }

    // 3. Carrega AppConfig (ironstream.toml ou defaults)
    let mut cfg = AppConfig::load_or_default();

    // 3.1 CLI override (SPEC-CFG-HW-001) — --hwaccel sobrescreve [player].hwaccel
    match CliArgs::parse_from(std::env::args()) {
        Ok(cli) => {
            if let Some(choice) = cli.hwaccel_override {
                tracing::info!(
                    cli = choice.label(),
                    cfg = cfg.player.hwaccel.label(),
                    "CLI: --hwaccel sobrescreve [player].hwaccel"
                );
                cfg.player.hwaccel = choice;
            }
        }
        Err(e) => {
            eprintln!("[IronPlayer] {e}");
            std::process::exit(2);
        }
    }

    tracing::info!(hwaccel = cfg.player.hwaccel.label(), "IronPlayer iniciado");

    // 4. Cria todos os canais bounded do pipeline
    let ch = channels::AppChannels::create();

    // 5. Canais auxiliares para eventos RTP → MetricsAggregator
    let (rtp_events_tx, rtp_events_rx) = crossbeam_channel::bounded::<net::RtpEvent>(64);
    let (agg_net_tx, agg_net_rx) = crossbeam_channel::bounded::<AggregatorNetEvent>(64);
    let (demux_cmd_tx, demux_cmd_rx) = crossbeam_channel::bounded::<DemuxCommand>(64);
    let (pes_cmd_tx, pes_cmd_rx) = crossbeam_channel::bounded::<PesCommand>(64);
    let (decode_cmd_tx, decode_cmd_rx) = crossbeam_channel::bounded::<DecodeCommand>(16);
    let (audio_cmd_tx, audio_cmd_rx) = crossbeam_channel::bounded::<AudioCommand>(16);

    // 6. Token de parada do MetricsAggregator
    let (metrics_stop_token, metrics_stop_handle): (MetricsStopToken, MetricsStopHandle) =
        MetricsStopToken::new();

    // Estado de conexão compartilhado entre cmd-handler e IronPlayerApp
    let conn_state: Arc<std::sync::RwLock<ui::ConnectionState>> =
        Arc::new(std::sync::RwLock::new(ui::ConnectionState::Idle));
    let audio_status: Arc<std::sync::RwLock<ui::AudioStatusSnapshot>> =
        Arc::new(std::sync::RwLock::new(ui::AudioStatusSnapshot::default()));
    if let Ok(mut status) = audio_status.write() {
        status.set_volume(cfg.player.volume);
    }

    // Handle da conexão de rede atual; compartilhado entre cmd-handler e PipelineGuard
    let current_net_stop: Arc<Mutex<Option<NetStopHandle>>> = Arc::new(Mutex::new(None));

    // Serviço selecionado, compartilhado entre cmd-handler, TableDispatcher e UI.
    let selected_service: Arc<std::sync::RwLock<Option<u16>>> =
        Arc::new(std::sync::RwLock::new(None));
    let selected_audio_pid: Arc<std::sync::RwLock<Option<u16>>> =
        Arc::new(std::sync::RwLock::new(None));
    let (table_cmd_tx, table_cmd_rx) = crossbeam_channel::bounded::<TableCommand>(32);

    // 7. Instancia MetricsAggregator
    let (metrics_agg, snapshot_rx) =
        MetricsAggregator::new(ch.ts_events_rx, ch.pcr_events_rx, agg_net_rx);

    // 8. Instancia TsDemuxer com rastreamento de PCR integrado (SPEC-TS-004b)
    let ts_demuxer = TsDemuxer::new(
        ch.section_data_tx.sender(),
        ch.pes_data_tx.sender(),
        ch.ts_events_tx.sender(),
    )
    .with_pcr_tracker(ch.pcr_events_tx.sender());

    // 9. Instancia SectionAssembler
    let section_asm =
        SectionAssembler::new(ch.complete_sections_tx.sender(), ch.ts_events_tx.sender());

    // 10. Instancia TableDispatcher (auto_play: seleciona o primeiro serviço
    // com A/V automaticamente; o usuário pode trocar via menu do VideoPanel).
    let table_disp = TableDispatcher::new_with_auto_play_and_control(
        ch.complete_sections_rx,
        ch.table_events_tx,
        demux_cmd_tx.clone(),
        pes_cmd_tx.clone(),
        decode_cmd_tx.clone(),
        selected_service.clone(),
        selected_audio_pid.clone(),
        audio_status.clone(),
        true,
        Some(table_cmd_rx),
    );
    let table_events_rx = ch.table_events_rx;

    // 11. SenderGuard -- mantem senders vivos ate o shutdown
    let sender_guard = SenderGuard {
        net_raw_tx: ch.net_raw_tx,
        section_data_tx: ch.section_data_tx,
        complete_sections_tx: ch.complete_sections_tx,
        ts_events_tx: ch.ts_events_tx,
    };

    // 12. Spawn das threads de backend (sem net-recv — iniciado pelo cmd-handler)
    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    // Thread: rtp-strip
    {
        let net_raw_rx = ch.net_raw_rx;
        let ts_raw_tx = ch.ts_raw_tx;
        // Clone antes de mover agg_net_tx para dentro do rtp-strip closure.
        let agg_net_tx_rtp = agg_net_tx.clone();
        handles.push(
            std::thread::Builder::new()
                .name("rtp-strip".into())
                .spawn(move || {
                    let mut stripper = RtpStripper::new(rtp_events_tx);
                    loop {
                        match net_raw_rx.recv() {
                            Ok(bytes) => {
                                let stripped = stripper.strip(bytes);
                                if !stripped.is_empty() {
                                    ts_raw_tx.try_send(stripped);
                                }
                            }
                            Err(_) => break,
                        }
                        while let Ok(evt) = rtp_events_rx.try_recv() {
                            let agg_evt = match evt {
                                net::RtpEvent::OutOfOrder { .. } => {
                                    AggregatorNetEvent::RtpOutOfOrder
                                }
                            };
                            let _ = agg_net_tx_rtp.try_send(agg_evt);
                        }
                    }
                })
                .expect("falha ao criar thread rtp-strip"),
        );
    }

    // Thread: net-events — converte NetEvent → AggregatorNetEvent e repassa ao MetricsAggregator.
    //
    // O canal `ch.net_events_rx` recebe eventos do UdpReceiver (Started, Timeout, Stopped).
    // Este bridge drena o canal para evitar backpressure; quando NetEvent::UdpBufferOverflow
    // for implementado no crate `net`, adicioná-lo aqui como `AggregatorNetEvent::UdpBufferOverflow`.
    {
        let net_events_rx = ch.net_events_rx;
        let _agg_net_tx_bridge = agg_net_tx.clone();
        handles.push(
            std::thread::Builder::new()
                .name("net-events".into())
                .spawn(move || {
                    for evt in net_events_rx.iter() {
                        match evt {
                            // TODO: quando net::NetEvent::UdpBufferOverflow for adicionado:
                            // net::NetEvent::UdpBufferOverflow { .. } =>
                            //     let _ = _agg_net_tx_bridge.try_send(AggregatorNetEvent::UdpBufferOverflow),
                            net::NetEvent::Started
                            | net::NetEvent::Timeout
                            | net::NetEvent::Stopped => {}
                        }
                    }
                })
                .expect("falha ao criar thread net-events"),
        );
    }

    // Thread: ts-demux
    {
        let ts_raw_rx = ch.ts_raw_rx;
        handles.push(
            std::thread::Builder::new()
                .name("ts-demux".into())
                .spawn(move || {
                    let mut demuxer = ts_demuxer;
                    for bytes in ts_raw_rx.iter() {
                        while let Ok(command) = demux_cmd_rx.try_recv() {
                            match command {
                                DemuxCommand::Reset => {
                                    demuxer.reset_dynamic_state();
                                }
                                DemuxCommand::RegisterPmtPid(pid) => {
                                    demuxer.register_pmt_pid(pid);
                                }
                                DemuxCommand::RegisterNitPid(pid) => {
                                    demuxer.register_nit_pid(pid);
                                }
                                DemuxCommand::RegisterAvPid(pid) => {
                                    demuxer.register_av_pid(pid);
                                }
                                DemuxCommand::DeregisterAvPid(pid) => {
                                    demuxer.deregister_av_pid(pid);
                                }
                            }
                        }
                        demuxer.process_chunk(&bytes);
                    }
                })
                .expect("falha ao criar thread ts-demux"),
        );
    }

    // Thread: pes-asm
    {
        let pes_data_rx = ch.pes_data_rx;
        let pes_packets_tx = ch.pes_packets_tx.sender();
        handles.push(
            std::thread::Builder::new()
                .name("pes-asm".into())
                .spawn(move || {
                    let mut asm = av::PesAssembler::new(pes_packets_tx);
                    loop {
                        while let Ok(command) = pes_cmd_rx.try_recv() {
                            match command {
                                PesCommand::Reset => {
                                    asm.reset();
                                }
                                PesCommand::RegisterPid { pid, codec } => {
                                    asm.register_pid(pid, codec);
                                }
                                PesCommand::DeregisterPid { pid } => {
                                    asm.deregister_pid(pid);
                                }
                            }
                        }

                        match pes_data_rx.recv_timeout(Duration::from_millis(10)) {
                            Ok(data) => asm.push(data.pid, data.pusi, data.data),
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        }
                    }
                })
                .expect("falha ao criar thread pes-asm"),
        );
    }

    // Thread: sec-asm
    {
        let section_data_rx = ch.section_data_rx;
        handles.push(
            std::thread::Builder::new()
                .name("sec-asm".into())
                .spawn(move || {
                    let mut asm = section_asm;
                    for data in section_data_rx.iter() {
                        if let Err(e) = asm.push(data) {
                            tracing::warn!(error = %e, "sec-asm: erro ao processar secao");
                        }
                    }
                })
                .expect("falha ao criar thread sec-asm"),
        );
    }

    // Thread: table-disp
    handles.push(
        std::thread::Builder::new()
            .name("table-disp".into())
            .spawn(move || {
                table_disp.run();
            })
            .expect("falha ao criar thread table-disp"),
    );

    // Thread: av-decode — SPEC-AV-002b
    // Recebe PesPackets, decodifica via FFmpeg e roteia VideoFrame/AudioFrame.

    // Arc compartilhado para expor métricas do pipeline de decode à UI.
    let pipeline_metrics_shared: std::sync::Arc<std::sync::RwLock<ts::metrics::PipelineMetrics>> =
        std::sync::Arc::new(std::sync::RwLock::new(
            ts::metrics::PipelineMetrics::default(),
        ));
    let pipeline_metrics_ui = std::sync::Arc::clone(&pipeline_metrics_shared);
    let d3d11_device_arc = bootstrap_d3d11_device(cfg.player.hwaccel, &pipeline_metrics_shared);

    {
        let pes_packets_rx = ch.pes_packets_rx;
        let video_frames_tx = ch.video_frames_tx;
        let video_frames_rx_for_drop = ch.video_frames_rx.clone();
        let audio_frames_tx = ch.audio_frames_tx;
        let audio_frames_rx_for_drop = ch.audio_frames_rx.clone();
        let audio_status = audio_status.clone();
        // Constrói CodecConfig a partir do DecoderConfig lido do ironstream.toml.
        //
        // Cap em 8 threads quando em auto-detect: com frame threading o decoder
        // FFmpeg buffera ~thread_count frames antes de emitir o primeiro, o que
        // dessincroniza o áudio (que toca imediatamente). Em máquinas com muitos
        // núcleos (16+) o ganho de throughput acima de 8 threads é marginal em
        // H.264/HEVC, mas a latência de pipeline cresce linearmente. O usuário
        // pode override via `[decoder] thread_count = N` no ironstream.toml.
        const AUTO_THREAD_CAP: u32 = 8;
        let codec_cfg = av::CodecConfig {
            thread_count: if cfg.decoder.thread_count == 0 {
                std::thread::available_parallelism()
                    .map(|n| (n.get() as u32).min(AUTO_THREAD_CAP))
                    .unwrap_or(1)
            } else {
                cfg.decoder.thread_count
            },
            thread_type: match cfg.decoder.thread_type {
                config::DecoderThreadType::Auto => av::ThreadType::Auto,
                config::DecoderThreadType::Frame => av::ThreadType::Frame,
                config::DecoderThreadType::Slice => av::ThreadType::Slice,
            },
            // Aplica overrides de perfil: fast → ativa skip_loop_filter + flag2_fast;
            // accurate → desativa ambos; default → usa valores individuais do TOML.
            skip_loop_filter: match cfg.decoder.profile {
                config::DecoderProfile::Fast => true,
                config::DecoderProfile::Accurate => false,
                config::DecoderProfile::Default => cfg.decoder.skip_loop_filter,
            },
            flag2_fast: match cfg.decoder.profile {
                config::DecoderProfile::Fast => true,
                config::DecoderProfile::Accurate => false,
                config::DecoderProfile::Default => cfg.decoder.flag2_fast,
            },
        };
        let pipeline_metrics_decode = std::sync::Arc::clone(&pipeline_metrics_shared);
        let d3d11_device_for_decode = d3d11_device_arc.clone();
        let initial_hwaccel_choice = cfg.player.hwaccel;
        handles.push(
            std::thread::Builder::new()
                .name("av-decode".into())
                .spawn(move || {
                    let mut decoder = match av::FfmpegDecoder::new_with_config(codec_cfg) {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::error!(
                                %e,
                                "av-decode: falha ao inicializar FfmpegDecoder; thread encerrando"
                            );
                            // Drena o canal para não bloquear o pipeline a montante.
                            for _ in pes_packets_rx.iter() {}
                            return;
                        }
                    };

                    match (initial_hwaccel_choice, d3d11_device_for_decode.as_ref()) {
                        (config::HwAccelChoice::None, _) => {
                            let _ = decoder.enable_hwaccel(av::HwAccelMode::Off);
                        }
                        (config::HwAccelChoice::Auto | config::HwAccelChoice::D3d11va, Some(dev)) => {
                            let _ = decoder.enable_hwaccel(av::HwAccelMode::D3d11Va(Arc::clone(dev)));
                        }
                        (config::HwAccelChoice::Auto | config::HwAccelChoice::D3d11va, None) => {
                            decoder.fallback_to_sw("D3D11 indisponível no bootstrap");
                        }
                    }

                    tracing::info!("av-decode: iniciado");

                    // Contador de erros de decode por PID para evitar log spam
                    // (canais de áudio com offset errado podem falhar 30x/s).
                    let mut decode_err_count: std::collections::HashMap<u16, u64> =
                        std::collections::HashMap::new();

                    // Rastreamento de latência de decode por PID de vídeo (janela deslizante).
                    let mut decode_times: std::collections::HashMap<
                        u16,
                        std::collections::VecDeque<f64>,
                    > = std::collections::HashMap::new();
                    const TIMING_WINDOW: usize = 100;
                    let mut pipeline_update_timer = std::time::Instant::now();

                    loop {
                        // Drena comandos de controle (ex.: Reset ao trocar serviço).
                        while let Ok(command) = decode_cmd_rx.try_recv() {
                            match command {
                                DecodeCommand::Reset => {
                                    decoder.reset();
                                    decode_times.clear();
                                    tracing::info!(
                                        "av-decode: contextos resetados (troca de serviço)"
                                    );
                                }
                                DecodeCommand::SetHwAccel { choice } => {
                                    match choice {
                                        config::HwAccelChoice::None => {
                                            let _ = decoder.enable_hwaccel(av::HwAccelMode::Off);
                                            decoder.reset_with_hw_state();
                                            decode_times.clear();
                                            tracing::info!(
                                                hwaccel = choice.label(),
                                                "av-decode: hwaccel desativado em runtime"
                                            );
                                        }
                                        config::HwAccelChoice::Auto
                                        | config::HwAccelChoice::D3d11va => {
                                            if let Some(dev) = d3d11_device_for_decode.as_ref() {
                                                let _ = decoder.enable_hwaccel(av::HwAccelMode::D3d11Va(Arc::clone(dev)));
                                                decoder.reset_with_hw_state();
                                                decode_times.clear();
                                                tracing::info!(
                                                    hwaccel = choice.label(),
                                                    "av-decode: hwaccel ativado em runtime"
                                                );
                                            } else {
                                                decoder.fallback_to_sw("D3D11 indisponível para runtime toggle");
                                                tracing::warn!(
                                                    hwaccel = choice.label(),
                                                    "av-decode: runtime toggle para hwaccel ignorado por falta de D3D11"
                                                );
                                            }
                                        }
                                    }
                                }
                                DecodeCommand::HandleDeviceRemoved => {
                                    decoder.fallback_to_sw("DXGI_ERROR_DEVICE_REMOVED");
                                    decoder.reset();
                                    decode_times.clear();
                                    tracing::warn!(
                                        "av-decode: device removed reportado pela UI; decoder rebaixado para SW"
                                    );
                                }
                            }
                        }

                        match pes_packets_rx.recv_timeout(Duration::from_millis(20)) {
                            Ok(packet) => {
                                let is_video = matches!(packet.codec, av::MediaCodec::Video(_));
                                let pid = packet.pid;
                                let t0 = std::time::Instant::now();
                                let decode_result = decoder.decode(&packet);
                                let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

                                if is_video {
                                    let w = decode_times.entry(pid).or_default();
                                    w.push_back(elapsed_ms);
                                    if w.len() > TIMING_WINDOW {
                                        w.pop_front();
                                    }
                                }

                                // Atualiza o Arc de métricas de pipeline a cada ~1 s.
                                if pipeline_update_timer.elapsed() >= Duration::from_secs(1) {
                                    if let Ok(mut m) = pipeline_metrics_decode.write() {
                                        m.decoder_threads_used = decoder.threads_used();
                                        m.deinterlacer_active = decoder.has_deinterlacer_active();
                                        m.hw_decode_active = decoder.is_hwaccel_active();
                                        m.hw_decode_codec = decoder.hw_decode_codec().map(str::to_owned);
                                        m.hw_decode_fallback_reason = decoder.fallback_reason().map(str::to_owned);
                                        m.hw_frame_pool_in_use = decoder.hw_frame_pool_in_use();
                                        m.decode_time_ms_p50.clear();
                                        m.decode_time_ms_p99.clear();
                                        for (&vpid, times) in &decode_times {
                                            if times.is_empty() {
                                                continue;
                                            }
                                            let mut sorted: Vec<f64> =
                                                times.iter().copied().collect();
                                            sorted.sort_by(|a, b| {
                                                a.partial_cmp(b)
                                                    .unwrap_or(std::cmp::Ordering::Equal)
                                            });
                                            let mid = sorted.len() / 2;
                                            m.decode_time_ms_p50.insert(vpid, sorted[mid]);
                                            let p99_idx = ((sorted.len() * 99) / 100)
                                                .min(sorted.len().saturating_sub(1));
                                            m.decode_time_ms_p99.insert(vpid, sorted[p99_idx]);
                                        }
                                    }
                                    pipeline_update_timer = std::time::Instant::now();
                                }

                                match decode_result {
                                    Ok(frames) => {
                                        for frame in frames {
                                            match frame {
                                                av::DecodedFrame::Video(vf) => {
                                                    video_frames_tx.try_send_latest(
                                                        &video_frames_rx_for_drop,
                                                        vf,
                                                    );
                                                }
                                                av::DecodedFrame::Audio(af) => {
                                                    audio_frames_tx.try_send_latest(
                                                        &audio_frames_rx_for_drop,
                                                        af,
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        if matches!(packet.codec, av::MediaCodec::Audio(_)) {
                                            if let Ok(mut status) = audio_status.write() {
                                                status.errors.decode_errors += 1;
                                                status.errors.last_error = Some(e.to_string());
                                                status.state = ui::AudioOperationalState::Error;
                                            }
                                        }
                                        let n = decode_err_count.entry(packet.pid).or_insert(0);
                                        *n += 1;
                                        // Loga o primeiro erro e depois a cada 200 ocorrencias
                                        // do mesmo PID, evitando saturar o terminal.
                                        if *n == 1 || (*n).is_multiple_of(200) {
                                            tracing::warn!(
                                                %e,
                                                pid = packet.pid,
                                                count = *n,
                                                "av-decode: erro ao decodificar PES"
                                            );
                                        }
                                    }
                                }
                            }
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        }
                    }

                    tracing::info!("av-decode: encerrado normalmente");
                })
                .expect("falha ao criar thread av-decode"),
        );
    }

    // Thread: video-drain removida (Task 2) — VideoRenderer integrado na UI.
    // `video_frames_rx` é entregue diretamente a `IronPlayerApp`.
    let video_frames_rx = ch.video_frames_rx;

    // Clock de áudio compartilhado entre audio-out e a UI.
    //
    // Quando `AudioOutput` é criado (lazy, na primeira frame de áudio),
    // a thread `audio-out` grava um `AudioClockHandle` aqui.  A UI lê o
    // handle em `poll_video_frames` e troca `video_clock` de `WallClock`
    // para `AudioClock`, sincronizando o vídeo ao relógio real WASAPI e
    // eliminando o drift causado pela latência do decoder multi-thread.
    let audio_clock_shared: Arc<std::sync::RwLock<Option<av::AudioClockHandle>>> =
        Arc::new(std::sync::RwLock::new(None));
    let audio_clock_for_audio_out = Arc::clone(&audio_clock_shared);
    let audio_clock_for_ui = Arc::clone(&audio_clock_shared);

    // Thread: audio-out — SPEC-AV-004
    // Recebe AudioFrames do decoder e os entrega ao AudioOutput (WASAPI via cpal).
    // Inicialização lazy: AudioOutput é criado na primeira frame recebida, pois
    // sample_rate e channels só são conhecidos após decodificação.
    {
        let audio_frames_rx = ch.audio_frames_rx;
        let jitter_buffer_ms = cfg.player.jitter_buffer_ms as u32;
        let initial_volume = cfg.player.volume;
        let audio_status = audio_status.clone();
        let audio_clock_tx = audio_clock_for_audio_out;
        let selected_audio_pid_rx = selected_audio_pid.clone();
        handles.push(
            std::thread::Builder::new()
                .name("audio-out".into())
                .spawn(move || {
                    let mut audio_out: Option<av::AudioOutput> = None;
                    let mut active_audio_pid: Option<u16> = None;
                    let mut clock_published = false;
                    let mut rebuild_failures = 0u64;

                    loop {
                        while let Ok(command) = audio_cmd_rx.try_recv() {
                            match command {
                                AudioCommand::Reset => {
                                    audio_out = None;
                                    active_audio_pid = None;
                                    clock_published = false;
                                    rebuild_failures = 0;
                                    while audio_frames_rx.try_recv().is_ok() {}
                                    if let Ok(mut guard) = audio_clock_tx.write() {
                                        *guard = None;
                                    }
                                    tracing::info!("audio-out: estado resetado e fila drenada");
                                }
                            }
                        }

                        if let Some(out) = audio_out.as_mut() {
                            if out.needs_rebuild() {
                                if let Ok(mut status) = audio_status.write() {
                                    status.state = ui::AudioOperationalState::Recovering;
                                }
                                match out.rebuild_stream() {
                                    Ok(()) => {
                                        rebuild_failures = 0;
                                        refresh_audio_status_from_output(&audio_status, out);
                                    }
                                    Err(e) => {
                                        rebuild_failures += 1;
                                        if let Ok(mut status) = audio_status.write() {
                                            status.errors.output_errors += 1;
                                            status.errors.last_error = Some(e.to_string());
                                            status.state = ui::AudioOperationalState::Error;
                                        }
                                        if rebuild_failures == 1 || rebuild_failures.is_multiple_of(20) {
                                            tracing::warn!(
                                                %e,
                                                retries = rebuild_failures,
                                                sample_rate = out.sample_rate,
                                                channels = out.channels,
                                                "audio-out: falha ao recriar AudioOutput; mantendo retry"
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        let frame = match audio_frames_rx.recv_timeout(Duration::from_millis(50)) {
                            Ok(frame) => frame,
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                                if let Some(out) = audio_out.as_ref() {
                                    refresh_audio_status_from_output(&audio_status, out);
                                }
                                continue;
                            }
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        };

                        let selected_audio_pid = selected_audio_pid_rx
                            .read()
                            .map(|g| *g)
                            .unwrap_or(None);
                        if selected_audio_pid.is_some_and(|pid| pid != frame.pid) {
                            tracing::debug!(
                                frame_pid = frame.pid,
                                selected_pid = ?selected_audio_pid,
                                "audio-out: frame de áudio obsoleto descartado"
                            );
                            continue;
                        }

                        // Lazy-init: cria AudioOutput na primeira frame (ou quando
                        // sample_rate/channels mudam, reiniciando o stream).
                        let pid_changed = active_audio_pid.is_some_and(|pid| pid != frame.pid);
                        let needs_reinit = audio_out.as_ref().is_none_or(|out| {
                            out.sample_rate != frame.sample_rate || out.channels != frame.channels
                        }) || pid_changed;

                        if needs_reinit {
                            if pid_changed {
                                audio_out = None;
                                clock_published = false;
                                if let Ok(mut guard) = audio_clock_tx.write() {
                                    *guard = None;
                                }
                                tracing::info!(
                                    old_pid = ?active_audio_pid,
                                    new_pid = frame.pid,
                                    "audio-out: trilha de áudio mudou; recriando saída e clock"
                                );
                            }
                            match av::AudioOutput::new(
                                frame.sample_rate,
                                frame.channels,
                                jitter_buffer_ms,
                            ) {
                                Ok(out) => {
                                    rebuild_failures = 0;
                                    out.set_volume(initial_volume);
                                    if let Ok(mut status) = audio_status.write() {
                                        status.set_volume(initial_volume);
                                        status.sample_rate_hz = Some(frame.sample_rate);
                                        status.channels = Some(frame.channels);
                                        status.buffer_level = 0.0;
                                        status.state = ui::AudioOperationalState::Buffering;
                                    }
                                    tracing::info!(
                                        sample_rate = frame.sample_rate,
                                        channels = frame.channels,
                                        "audio-out: AudioOutput inicializado"
                                    );
                                    active_audio_pid = Some(frame.pid);
                                    audio_out = Some(out);
                                }
                                Err(e) => {
                                    if let Ok(mut status) = audio_status.write() {
                                        status.errors.output_errors += 1;
                                        status.errors.last_error = Some(e.to_string());
                                        status.state = ui::AudioOperationalState::Error;
                                    }
                                    tracing::error!(
                                        %e,
                                        sample_rate = frame.sample_rate,
                                        channels = frame.channels,
                                        "audio-out: falha ao criar AudioOutput; frame descartado"
                                    );
                                    continue;
                                }
                            }
                        }

                        if let Some(ref out) = audio_out {
                            out.push_samples(&frame);
                            refresh_audio_status_from_output(&audio_status, out);

                            // Publica o AudioClockHandle somente quando o primeiro frame
                            // tiver PTS válido — AC-3 pode emitir frames iniciais sem PTS
                            // do decoder, mas com PTS no PES (via resolve_audio_pts).
                            if !clock_published {
                                if let Some(pts) = frame.pts {
                                    let anchor = pts as i64;
                                    if let Ok(mut guard) = audio_clock_tx.write() {
                                        *guard = Some(out.clock_handle(anchor));
                                        clock_published = true;
                                        tracing::debug!(
                                            anchor_pts = anchor,
                                            "audio-out: AudioClockHandle publicado"
                                        );
                                    }
                                }
                            }
                        }
                    }

                    if let Ok(mut status) = audio_status.write() {
                        status.reset_stream_runtime(ui::AudioOperationalState::Idle);
                    }

                    tracing::info!("audio-out: encerrado normalmente");
                })
                .expect("falha ao criar thread audio-out"),
        );
    }

    // Thread: metrics
    let metrics_handle = std::thread::Builder::new()
        .name("metrics".into())
        .spawn(move || {
            metrics_agg.run(metrics_stop_token);
        })
        .expect("falha ao criar thread metrics");

    tracing::info!(threads = handles.len() + 1, "pipeline de backend iniciado");

    // 13. Canal de comandos UI → pipeline
    let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<ui::AppCommand>(channels::CAP_APP_COMMANDS);

    // 14. Thread: cmd-handler — processa Connect/Disconnect da UI dinamicamente
    {
        let conn_state = conn_state.clone();
        let audio_status = audio_status.clone();
        let current_net_stop = current_net_stop.clone();
        let selected_service = selected_service.clone();
        let selected_audio_pid = selected_audio_pid.clone();
        let demux_cmd_tx = demux_cmd_tx.clone();
        let pes_cmd_tx = pes_cmd_tx.clone();
        let decode_cmd_tx = decode_cmd_tx.clone();
        let table_cmd_tx = table_cmd_tx.clone();
        let agg_net_tx = agg_net_tx.clone();
        let net_raw_tx = sender_guard.net_raw_tx.sender();
        let net_events_tx = ch.net_events_tx.sender();
        let receiver_cfg = ReceiverConfig {
            buf_size: cfg.network.udp_buffer_bytes,
            timeout_ms: cfg.network.timeout_ms,
        };

        let handle = std::thread::Builder::new()
            .name("cmd-handler".into())
            .spawn(move || {
                for cmd in cmd_rx.iter() {
                    match cmd {
                        ui::AppCommand::Connect { url, iface: _ } => {
                            // Para conexão anterior, se existir
                            if let Some(h) = current_net_stop.lock().unwrap().take() {
                                h.stop();
                            }
                            reset_stream_routing(StreamResetTargets {
                                selected_service: &selected_service,
                                selected_audio_pid: &selected_audio_pid,
                                table_cmd_tx: &table_cmd_tx,
                                agg_net_tx: &agg_net_tx,
                                demux_cmd_tx: &demux_cmd_tx,
                                pes_cmd_tx: &pes_cmd_tx,
                                decode_cmd_tx: &decode_cmd_tx,
                                audio_cmd_tx: &audio_cmd_tx,
                            });

                            match StreamUrl::parse(&url) {
                                Err(e) => {
                                    tracing::error!(error = %e, url, "URL de stream inválida");
                                    if let Ok(mut status) = audio_status.write() {
                                        status.reset_stream_runtime(ui::AudioOperationalState::Error);
                                        status.errors.last_error = Some(e.to_string());
                                    }
                                    *conn_state.write().unwrap() = ui::ConnectionState::Error {
                                        url,
                                        reason: e.to_string(),
                                    };
                                }
                                Ok(parsed_url) => {
                                    if let Ok(mut status) = audio_status.write() {
                                        status.reset_stream_runtime(ui::AudioOperationalState::Buffering);
                                        status.set_volume(cfg.player.volume);
                                    }
                                    *conn_state.write().unwrap() =
                                        ui::ConnectionState::Connecting { url: url.clone() };
                                    tracing::info!(url, "conectando...");

                                    let (stop_token, stop_handle) = NetStopToken::new();
                                    *current_net_stop.lock().unwrap() = Some(stop_handle);

                                    let receiver = UdpReceiver::new(
                                        parsed_url,
                                        net_raw_tx.clone(),
                                        net_events_tx.clone(),
                                        receiver_cfg.clone(),
                                    );
                                    let conn_state_t = conn_state.clone();
                                    let audio_status_t = audio_status.clone();
                                    let url_t = url.clone();

                                    std::thread::Builder::new()
                                        .name("net-recv".into())
                                        .spawn(move || {
                                            *conn_state_t.write().unwrap() =
                                                ui::ConnectionState::Connected {
                                                    url: url_t.clone(),
                                                    since: Instant::now(),
                                                };
                                            match receiver.run(stop_token) {
                                                Ok(()) => {
                                                    if let Ok(mut status) = audio_status_t.write() {
                                                        status.reset_stream_runtime(ui::AudioOperationalState::Idle);
                                                    }
                                                    *conn_state_t.write().unwrap() =
                                                        ui::ConnectionState::Idle;
                                                    tracing::info!("net-recv encerrado normalmente");
                                                }
                                                Err(e) => {
                                                    tracing::error!(error = %e, "net-recv encerrou com erro");
                                                    if let Ok(mut status) = audio_status_t.write() {
                                                        status.reset_stream_runtime(ui::AudioOperationalState::Error);
                                                        status.errors.last_error = Some(e.to_string());
                                                    }
                                                    *conn_state_t.write().unwrap() =
                                                        ui::ConnectionState::Error {
                                                            url: url_t,
                                                            reason: e.to_string(),
                                                        };
                                                }
                                            }
                                        })
                                        .expect("falha ao criar thread net-recv");
                                }
                            }
                        }
                        ui::AppCommand::Disconnect => {
                            if let Some(h) = current_net_stop.lock().unwrap().take() {
                                h.stop();
                            }
                            reset_stream_routing(StreamResetTargets {
                                selected_service: &selected_service,
                                selected_audio_pid: &selected_audio_pid,
                                table_cmd_tx: &table_cmd_tx,
                                agg_net_tx: &agg_net_tx,
                                demux_cmd_tx: &demux_cmd_tx,
                                pes_cmd_tx: &pes_cmd_tx,
                                decode_cmd_tx: &decode_cmd_tx,
                                audio_cmd_tx: &audio_cmd_tx,
                            });
                            if let Ok(mut status) = audio_status.write() {
                                status.reset_stream_runtime(ui::AudioOperationalState::Idle);
                            }
                            *conn_state.write().unwrap() = ui::ConnectionState::Idle;
                            tracing::info!("desconectado pelo usuário");
                        }
                        ui::AppCommand::SelectService { service_id } => {
                            if let Ok(mut status) = audio_status.write() {
                                status.sample_rate_hz = None;
                                status.channels = None;
                                status.buffer_level = 0.0;
                                status.state = ui::AudioOperationalState::Buffering;
                                status.errors.last_error = None;
                            }
                            if let Ok(mut audio_pid) = selected_audio_pid.write() {
                                *audio_pid = None;
                            }
                            if audio_cmd_tx.try_send(AudioCommand::Reset).is_err() {
                                tracing::warn!("canal audio-control cheio — Reset descartado");
                            }
                            *selected_service.write().unwrap() = Some(service_id);
                            tracing::info!(service_id, "serviço selecionado pelo usuário");
                        }
                        ui::AppCommand::SelectAudio { service_id, pid } => {
                            if let Ok(mut status) = audio_status.write() {
                                status.sample_rate_hz = None;
                                status.channels = None;
                                status.buffer_level = 0.0;
                                status.state = ui::AudioOperationalState::Buffering;
                                status.errors.last_error = None;
                            }
                            *selected_service.write().unwrap() = Some(service_id);
                            *selected_audio_pid.write().unwrap() = Some(pid);
                            if audio_cmd_tx.try_send(AudioCommand::Reset).is_err() {
                                tracing::warn!("canal audio-control cheio — Reset descartado");
                            }
                            tracing::info!(service_id, pid, "trilha de áudio selecionada pelo usuário");
                        }
                        ui::AppCommand::SetHwAccel { choice } => {
                            let cfg_choice: config::HwAccelChoice = choice.into();
                            if decode_cmd_tx
                                .try_send(DecodeCommand::SetHwAccel { choice: cfg_choice })
                                .is_err()
                            {
                                tracing::warn!(
                                    hwaccel = cfg_choice.label(),
                                    "cmd-handler: canal decode_cmd cheio; SetHwAccel descartado"
                                );
                            } else {
                                tracing::info!(
                                    hwaccel = cfg_choice.label(),
                                    "cmd-handler: SetHwAccel enviado ao decoder"
                                );
                            }
                        }
                        ui::AppCommand::GpuDeviceRemoved => {
                            if decode_cmd_tx
                                .try_send(DecodeCommand::HandleDeviceRemoved)
                                .is_err()
                            {
                                tracing::warn!(
                                    "cmd-handler: canal decode_cmd cheio; HandleDeviceRemoved descartado"
                                );
                            } else {
                                tracing::warn!(
                                    "cmd-handler: HandleDeviceRemoved enviado ao decoder"
                                );
                            }
                        }
                        _ => {}
                    }
                }
            })
            .expect("falha ao criar thread cmd-handler");
        handles.push(handle);
    }

    // Usa PowerPreference::HighPerformance para que wgpu e D3D11 selecionem
    // o mesmo adapter físico em sistemas com múltiplas GPUs (iGPU + dGPU).
    // A validação definitiva do LUID acontece no CreationContext abaixo.
    let wgpu_power_pref = if d3d11_device_arc.is_some() {
        eframe::wgpu::PowerPreference::HighPerformance
    } else {
        eframe::wgpu::PowerPreference::default()
    };

    // 16. Loop de UI via eframe
    let guard = PipelineGuard {
        pipeline_handles: handles.into_iter().map(Some).collect(),
        metrics_handle: Some(metrics_handle),
        current_net_stop,
        metrics_stop_handle: Some(metrics_stop_handle),
        _sender_guard: Some(sender_guard),
    };

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("IronPlayer")
            .with_inner_size([1280.0, 720.0]),
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            present_mode: eframe::wgpu::PresentMode::Fifo,
            power_preference: wgpu_power_pref,
            // Pede TEXTURE_FORMAT_16BIT_NORM para suportar upload R16Unorm dos
            // planos YUV 10-bit (HEVC Main10/Rext, comum em broadcast). Quando
            // o adapter não expõe a feature, cai silenciosamente para o default
            // — o renderer ainda funciona em streams 8-bit.
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

    // Clona `d3d11_device_arc` para uso dentro do closure (o Arc original pode
    // ser movido para o FfmpegDecoder na Fase B).
    let d3d11_for_cc = d3d11_device_arc.clone();
    let d3d11_for_ui = d3d11_device_arc;

    eframe::run_native(
        "IronPlayer",
        native_options,
        Box::new(move |cc| {
            // ── Fase A: validar LUID do adapter wgpu vs D3d11Device ───────────
            //
            // Compara o VendorId do adapter wgpu com o VendorId do ID3D11Device
            // para detectar mismatches em sistemas multi-GPU (Risco R4).
            // A comparação definitiva por LUID requer wgpu::hal que ainda não é
            // necessária na Fase A; VendorId é suficiente para validação inicial.
            if let Some(ref d3d_dev) = d3d11_for_cc {
                if let Some(rs) = cc.wgpu_render_state.as_ref() {
                    let wgpu_info = rs.adapter.get_info();
                    let d3d_vendor = d3d_dev.vendor_id();
                    let wgpu_vendor = wgpu_info.vendor;

                    if wgpu_vendor == d3d_vendor {
                        tracing::info!(
                            wgpu_adapter = %wgpu_info.name,
                            wgpu_vendor = format!("{:#06x}", wgpu_vendor),
                            d3d_vendor = format!("{:#06x}", d3d_vendor),
                            "hw: adapter wgpu e D3d11Device usam o mesmo vendor — LUID compatível (Fase A OK)"
                        );
                    } else {
                        tracing::warn!(
                            wgpu_adapter = %wgpu_info.name,
                            wgpu_vendor = format!("{:#06x}", wgpu_vendor),
                            d3d_vendor = format!("{:#06x}", d3d_vendor),
                            "hw: vendor mismatch entre wgpu e D3d11Device (possível multi-GPU Optimus) — Risco R4"
                        );
                    }
                }
            }

            let mut inner = IronPlayerApp::new(
                cc,
                cmd_tx,
                Some(snapshot_rx),
                Some(conn_state),
                Some(audio_status),
                Some(selected_service),
                Some(table_events_rx.clone()),
                Some(video_frames_rx),
                d3d11_for_ui.clone(),
            );
            inner.set_pipeline_metrics_rx(pipeline_metrics_ui);
            inner.set_audio_clock_rx(audio_clock_for_ui);
            inner.set_hwaccel_choice(cfg.player.hwaccel.into());
            Ok(Box::new(IronPlayerAppWithPipeline { inner, guard }))
        }),
    )
}
