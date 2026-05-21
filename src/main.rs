mod channels;
mod config;
mod ffmpeg_check;
mod table_dispatcher;

use bytes::Bytes;
use channels::BoundedSender;
use config::AppConfig;
use net::{
    ReceiverConfig, RtpStripper, StopHandle as NetStopHandle, StopToken as NetStopToken, StreamUrl,
    UdpReceiver,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use table_dispatcher::{DecodeCommand, DemuxCommand, PesCommand, TableDispatcher};
use ts::{
    aggregator::{
        AggregatorNetEvent, MetricsAggregator, StopHandle as MetricsStopHandle,
        StopToken as MetricsStopToken,
    },
    CompleteSection, SectionAssembler, SectionData, TsDemuxer,
};
use ui::IronPlayerApp;

// ── SenderGuard ───────────────────────────────────────────────────────────────

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
    let cfg = AppConfig::load_or_default();
    tracing::info!("IronPlayer iniciado");

    // 4. Cria todos os canais bounded do pipeline
    let ch = channels::AppChannels::create();

    // 5. Canais auxiliares para eventos RTP → MetricsAggregator
    let (rtp_events_tx, rtp_events_rx) = crossbeam_channel::bounded::<net::RtpEvent>(64);
    let (agg_net_tx, agg_net_rx) = crossbeam_channel::bounded::<AggregatorNetEvent>(64);
    let (demux_cmd_tx, demux_cmd_rx) = crossbeam_channel::bounded::<DemuxCommand>(64);
    let (pes_cmd_tx, pes_cmd_rx) = crossbeam_channel::bounded::<PesCommand>(64);
    let (decode_cmd_tx, decode_cmd_rx) = crossbeam_channel::bounded::<DecodeCommand>(16);

    // 6. Token de parada do MetricsAggregator
    let (metrics_stop_token, metrics_stop_handle): (MetricsStopToken, MetricsStopHandle) =
        MetricsStopToken::new();

    // Estado de conexão compartilhado entre cmd-handler e IronPlayerApp
    let conn_state: Arc<std::sync::RwLock<ui::ConnectionState>> =
        Arc::new(std::sync::RwLock::new(ui::ConnectionState::Idle));

    // Handle da conexão de rede atual; compartilhado entre cmd-handler e PipelineGuard
    let current_net_stop: Arc<Mutex<Option<NetStopHandle>>> = Arc::new(Mutex::new(None));

    // Serviço selecionado, compartilhado entre cmd-handler, TableDispatcher e UI.
    let selected_service: Arc<std::sync::RwLock<Option<u16>>> =
        Arc::new(std::sync::RwLock::new(None));

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
    let table_disp = TableDispatcher::new_with_auto_play(
        ch.complete_sections_rx,
        ch.table_events_tx,
        demux_cmd_tx,
        pes_cmd_tx,
        decode_cmd_tx,
        selected_service.clone(),
        true,
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
                                DemuxCommand::RegisterPmtPid(pid) => {
                                    demuxer.register_pmt_pid(pid);
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
    {
        let pes_packets_rx = ch.pes_packets_rx;
        let video_frames_tx = ch.video_frames_tx;
        let video_frames_rx_for_drop = ch.video_frames_rx.clone();
        let audio_frames_tx = ch.audio_frames_tx;
        handles.push(
            std::thread::Builder::new()
                .name("av-decode".into())
                .spawn(move || {
                    let mut decoder = match av::FfmpegDecoder::new() {
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

                    tracing::info!("av-decode: iniciado");

                    // Contador de erros de decode por PID para evitar log spam
                    // (canais de áudio com offset errado podem falhar 30x/s).
                    let mut decode_err_count: std::collections::HashMap<u16, u64> =
                        std::collections::HashMap::new();

                    loop {
                        // Drena comandos de controle (ex.: Reset ao trocar serviço).
                        while let Ok(command) = decode_cmd_rx.try_recv() {
                            match command {
                                DecodeCommand::Reset => {
                                    decoder.reset();
                                    tracing::info!(
                                        "av-decode: contextos resetados (troca de serviço)"
                                    );
                                }
                            }
                        }

                        match pes_packets_rx.recv_timeout(Duration::from_millis(20)) {
                            Ok(packet) => match decoder.decode(&packet) {
                                Ok(frames) => {
                                    for frame in frames {
                                        match frame {
                                            av::DecodedFrame::Video(vf) => {
                                                video_frames_tx
                                                    .try_send_latest(&video_frames_rx_for_drop, vf);
                                            }
                                            av::DecodedFrame::Audio(af) => {
                                                audio_frames_tx.try_send(af);
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
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
                            },
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

    // Thread: audio-out — SPEC-AV-004
    // Recebe AudioFrames do decoder e os entrega ao AudioOutput (WASAPI via cpal).
    // Inicialização lazy: AudioOutput é criado na primeira frame recebida, pois
    // sample_rate e channels só são conhecidos após decodificação.
    {
        let audio_frames_rx = ch.audio_frames_rx;
        let jitter_buffer_ms = cfg.player.jitter_buffer_ms as u32;
        let initial_volume = cfg.player.volume;
        handles.push(
            std::thread::Builder::new()
                .name("audio-out".into())
                .spawn(move || {
                    let mut audio_out: Option<av::AudioOutput> = None;
                    let mut rebuild_failures = 0u64;

                    loop {
                        if let Some(out) = audio_out.as_mut() {
                            if out.needs_rebuild() {
                                match out.rebuild_stream() {
                                    Ok(()) => {
                                        rebuild_failures = 0;
                                    }
                                    Err(e) => {
                                        rebuild_failures += 1;
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
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        };

                        // Lazy-init: cria AudioOutput na primeira frame (ou quando
                        // sample_rate/channels mudam, reiniciando o stream).
                        let needs_reinit = audio_out.as_ref().is_none_or(|out| {
                            out.sample_rate != frame.sample_rate || out.channels != frame.channels
                        });

                        if needs_reinit {
                            match av::AudioOutput::new(
                                frame.sample_rate,
                                frame.channels,
                                jitter_buffer_ms,
                            ) {
                                Ok(out) => {
                                    rebuild_failures = 0;
                                    out.set_volume(initial_volume);
                                    tracing::info!(
                                        sample_rate = frame.sample_rate,
                                        channels = frame.channels,
                                        "audio-out: AudioOutput inicializado"
                                    );
                                    audio_out = Some(out);
                                }
                                Err(e) => {
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
                        }
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
        let current_net_stop = current_net_stop.clone();
        let selected_service = selected_service.clone();
        let net_raw_tx = sender_guard.net_raw_tx.sender();
        let net_events_tx = ch.net_events_tx.sender();
        let receiver_cfg = ReceiverConfig {
            buf_size: cfg.network.udp_buffer_bytes,
            timeout_ms: cfg.network.timeout_ms,
        };

        std::thread::Builder::new()
            .name("cmd-handler".into())
            .spawn(move || {
                for cmd in cmd_rx.iter() {
                    match cmd {
                        ui::AppCommand::Connect { url, iface: _ } => {
                            // Para conexão anterior, se existir
                            if let Some(h) = current_net_stop.lock().unwrap().take() {
                                h.stop();
                            }

                            match StreamUrl::parse(&url) {
                                Err(e) => {
                                    tracing::error!(error = %e, url, "URL de stream inválida");
                                    *conn_state.write().unwrap() = ui::ConnectionState::Error {
                                        url,
                                        reason: e.to_string(),
                                    };
                                }
                                Ok(parsed_url) => {
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
                                                    *conn_state_t.write().unwrap() =
                                                        ui::ConnectionState::Idle;
                                                    tracing::info!("net-recv encerrado normalmente");
                                                }
                                                Err(e) => {
                                                    tracing::error!(error = %e, "net-recv encerrou com erro");
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
                            *conn_state.write().unwrap() = ui::ConnectionState::Idle;
                            tracing::info!("desconectado pelo usuário");
                        }
                        ui::AppCommand::SelectService { service_id } => {
                            *selected_service.write().unwrap() = Some(service_id);
                            tracing::info!(service_id, "serviço selecionado pelo usuário");
                        }
                        _ => {}
                    }
                }
            })
            .expect("falha ao criar thread cmd-handler");
    }

    // 15. Loop de UI via eframe
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
        ..Default::default()
    };

    eframe::run_native(
        "IronPlayer",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(IronPlayerAppWithPipeline {
                inner: IronPlayerApp::new(
                    cc,
                    cmd_tx,
                    Some(snapshot_rx),
                    Some(conn_state),
                    Some(selected_service),
                    Some(table_events_rx.clone()),
                    Some(video_frames_rx),
                ),
                guard,
            }))
        }),
    )
}
