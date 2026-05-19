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
use std::time::{Duration, Instant};
use table_dispatcher::TableDispatcher;
use ts::{
    aggregator::{
        AggregatorNetEvent, MetricsAggregator, StopHandle as MetricsStopHandle,
        StopToken as MetricsStopToken,
    },
    CompleteSection, SectionAssembler, SectionData, TsDemuxer,
};

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

// ── IronPlayerApp ─────────────────────────────────────────────────────────────

/// App eframe que orquestra o pipeline IronPlayer e implementa shutdown limpo.
///
/// `on_exit` e invocado pelo eframe ao fechar a janela e executa:
/// 1. Sinaliza `NetStopHandle::stop()` e `MetricsStopHandle::stop()`
/// 2. Dropa o `SenderGuard` → encerramento em cascata dos canais
/// 3. Faz join de todas as threads com budget total de 2 s
struct IronPlayerApp {
    pipeline_handles: Vec<Option<std::thread::JoinHandle<()>>>,
    metrics_handle: Option<std::thread::JoinHandle<()>>,
    net_stop_handle: Option<NetStopHandle>,
    metrics_stop_handle: Option<MetricsStopHandle>,
    _sender_guard: Option<SenderGuard>,
}

impl IronPlayerApp {
    fn new(
        pipeline_handles: Vec<std::thread::JoinHandle<()>>,
        metrics_handle: std::thread::JoinHandle<()>,
        net_stop_handle: NetStopHandle,
        metrics_stop_handle: MetricsStopHandle,
        sender_guard: SenderGuard,
    ) -> Self {
        Self {
            pipeline_handles: pipeline_handles.into_iter().map(Some).collect(),
            metrics_handle: Some(metrics_handle),
            net_stop_handle: Some(net_stop_handle),
            metrics_stop_handle: Some(metrics_stop_handle),
            _sender_guard: Some(sender_guard),
        }
    }
}

impl eframe::App for IronPlayerApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        eframe::egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("IronPlayer");
            ui.label("Pipeline de backend ativo.");
        });
    }

    /// Aciona o shutdown limpo ao fechar a janela (SPEC-WIRING-SHUTDOWN).
    ///
    /// Sequencia:
    /// 1. Sinaliza parada dos workers ativos (`net-recv`, `metrics`)
    /// 2. Dropa `SenderGuard` → cascata de fechamento de canais
    /// 3. Join de todas as threads com budget total de 2 s
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        tracing::info!("shutdown iniciado pelo eframe::App::on_exit");

        // 1. Sinaliza parada dos workers com stop handles
        if let Some(h) = self.net_stop_handle.take() {
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
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

    // 5. URL de stream (primeiro argumento CLI, opcional)
    let stream_url: Option<StreamUrl> = std::env::args().nth(1).and_then(|url| {
        match StreamUrl::parse(&url) {
            Ok(u) => {
                tracing::info!(url, "URL de stream configurada");
                Some(u)
            }
            Err(e) => {
                tracing::error!(error = %e, "URL de stream invalida; pipeline de rede desativado");
                None
            }
        }
    });

    // 6. Canais auxiliares para eventos RTP → MetricsAggregator
    let (rtp_events_tx, rtp_events_rx) = crossbeam_channel::bounded::<net::RtpEvent>(64);
    let (agg_net_tx, agg_net_rx) = crossbeam_channel::bounded::<AggregatorNetEvent>(64);

    // 7. Tokens e handles de parada
    let (net_stop_token, net_stop_handle) = NetStopToken::new();
    let (metrics_stop_token, metrics_stop_handle): (MetricsStopToken, MetricsStopHandle) =
        MetricsStopToken::new();

    // 8. Instancia MetricsAggregator
    let (metrics_agg, _snapshot_rx) =
        MetricsAggregator::new(ch.ts_events_rx, ch.pcr_events_rx, agg_net_rx);

    // 9. Instancia TsDemuxer
    let ts_demuxer = TsDemuxer::new(
        ch.section_data_tx.sender(),
        ch.pes_data_tx.sender(),
        ch.ts_events_tx.sender(),
    );

    // 10. Instancia SectionAssembler
    let section_asm =
        SectionAssembler::new(ch.complete_sections_tx.sender(), ch.ts_events_tx.sender());

    // 11. Instancia TableDispatcher
    let table_disp = TableDispatcher::new(ch.complete_sections_rx, ch.table_events_tx);

    // 12. SenderGuard -- mantem senders vivos ate o shutdown
    let sender_guard = SenderGuard {
        net_raw_tx: ch.net_raw_tx,
        section_data_tx: ch.section_data_tx,
        complete_sections_tx: ch.complete_sections_tx,
        ts_events_tx: ch.ts_events_tx,
    };

    // 13. Spawn das threads de backend
    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    // Thread: net-recv (apenas se URL fornecida)
    if let Some(url) = stream_url {
        let receiver_cfg = ReceiverConfig {
            buf_size: cfg.network.udp_buffer_bytes,
            timeout_ms: cfg.network.timeout_ms,
        };
        let net_raw_tx = sender_guard.net_raw_tx.sender();
        let net_events_tx = ch.net_events_tx.sender();
        let stop = net_stop_token.clone();
        let receiver = UdpReceiver::new(url, net_raw_tx, net_events_tx, receiver_cfg);
        handles.push(
            std::thread::Builder::new()
                .name("net-recv".into())
                .spawn(move || {
                    if let Err(e) = receiver.run(stop) {
                        tracing::error!(error = %e, "net-recv encerrou com erro");
                    }
                })
                .expect("falha ao criar thread net-recv"),
        );
    }

    // Thread: rtp-strip
    {
        let net_raw_rx = ch.net_raw_rx;
        let ts_raw_tx = ch.ts_raw_tx;
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
                            let _ = agg_net_tx.try_send(agg_evt);
                        }
                    }
                })
                .expect("falha ao criar thread rtp-strip"),
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
                        demuxer.process_chunk(&bytes);
                    }
                })
                .expect("falha ao criar thread ts-demux"),
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

    // Thread: metrics
    let metrics_handle = std::thread::Builder::new()
        .name("metrics".into())
        .spawn(move || {
            metrics_agg.run(metrics_stop_token);
        })
        .expect("falha ao criar thread metrics");

    tracing::info!(threads = handles.len() + 1, "pipeline de backend iniciado");

    // 14. Loop de UI via eframe
    let app = IronPlayerApp::new(
        handles,
        metrics_handle,
        net_stop_handle,
        metrics_stop_handle,
        sender_guard,
    );

    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "IronPlayer",
        native_options,
        Box::new(|_cc| Ok(Box::new(app))),
    )
}
