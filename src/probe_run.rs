//! Supervisor do run de monitoração: abre feeds, engines, writer e snapshot.
//!
//! Um **run** é a sessão de monitoração como um todo; dentro dele há uma
//! sessão por feed (§6).  Este módulo é quem materializa o ciclo do §5.5:
//! entrar em Probe abre o run, sair fecha os arquivos e grava o resumo de
//! cada sessão.
//!
//! SPEC-PROBE-004 · SPEC-PROBE-011 · SPEC-PROBE-012 · SPEC-PROBE-014 ·
//! SPEC-PROBE-016 · SPEC-PROBE-020

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use probe::{
    render_run_report, writer_channel, FeedIdentity, FeedSnapshot, KeepAwake, ProbeConfig,
    ProbeEngine, ProbeRun, ProbeSnapshot, SeriesWindow, SystemClock, WriterHandle,
};

use crate::feed::{resolve_feeds, FeedPipeline};

/// Estado compartilhado do run, publicado para a UI a 1 Hz.
pub type SharedProbeSnapshot = Arc<RwLock<ProbeSnapshot>>;

/// Um run aberto.
struct ActiveRun {
    run: ProbeRun,
    feeds: Vec<FeedPipeline>,
    /// Snapshot por slot, escrito pelas threads de engine.
    slots: Vec<Arc<RwLock<Option<FeedSnapshot>>>>,
    stop: Arc<AtomicBool>,
    handles: Vec<std::thread::JoinHandle<()>>,
    writer_handle: Option<WriterHandle>,
    writer_thread: Option<std::thread::JoinHandle<()>>,
    /// Impede suspensão enquanto existir (SPEC-PROBE-012).
    _awake: KeepAwake,
    started: Instant,
}

/// Supervisor do modo Probe.
///
/// Vive na thread `cmd-handler`; a UI fala com ele por `AppCommand`.
pub struct ProbeRunner {
    cfg: ProbeConfig,
    receiver_cfg: net::ReceiverConfig,
    snapshot: SharedProbeSnapshot,
    thumbnails: ui_slint::SharedThumbnails,
    /// Janela dos gráficos escolhida na UI (SPEC-PROBE-010).
    window: Arc<AtomicUsize>,
    active: Option<ActiveRun>,
    sessions_root: PathBuf,
}

impl ProbeRunner {
    /// Cria o supervisor (sem abrir run).
    pub fn new(
        cfg: ProbeConfig,
        receiver_cfg: net::ReceiverConfig,
        snapshot: SharedProbeSnapshot,
        thumbnails: ui_slint::SharedThumbnails,
    ) -> Self {
        Self {
            cfg,
            receiver_cfg,
            snapshot,
            thumbnails,
            window: Arc::new(AtomicUsize::new(1)),
            active: None,
            sessions_root: probe::session::default_sessions_root(),
        }
    }

    /// `true` enquanto há um run gravando.
    pub fn is_running(&self) -> bool {
        self.active.is_some()
    }

    /// Troca a janela dos gráficos publicada no próximo snapshot.
    ///
    /// SPEC-PROBE-010
    pub fn set_window(&self, index: usize) {
        self.window
            .store(index.min(SeriesWindow::ALL.len() - 1), Ordering::Relaxed);
    }

    /// Abre um run com os feeds do `[probe]`.
    ///
    /// SPEC-PROBE-004 — cria uma pasta de sessão por feed sob o mesmo `run_id`.
    pub fn start(&mut self) -> Result<(), String> {
        if self.active.is_some() {
            return Ok(());
        }

        let specs = resolve_feeds(&self.cfg).map_err(|e| e.to_string())?;
        if specs.is_empty() {
            return Err(
                "nenhum feed configurado — adicione blocos [[probe.feeds]] no ironstream.toml"
                    .to_string(),
            );
        }

        let clock = Arc::new(SystemClock);
        let started_utc = chrono::Utc::now();

        // SPEC-PROBE-016 — a retenção roda **antes** de criar o run, para que
        // a pasta em curso nunca entre na varredura.
        let report = probe::retention::apply(
            &self.sessions_root,
            self.cfg.retention_days,
            self.cfg.max_disk_mb,
            None,
            SystemTime::now(),
        );
        if !report.removed.is_empty() {
            tracing::info!(
                removed = report.removed.len(),
                by_age = report.removed_by_age,
                by_size = report.removed_by_size,
                "probe: retenção aplicada"
            );
        }

        let run = ProbeRun::create(&self.sessions_root, started_utc)
            .map_err(|e| format!("não foi possível criar a pasta do run: {e}"))?;

        let (writer_handle, writer) = writer_channel(self.cfg.flush_interval());
        let writer_thread = std::thread::Builder::new()
            .name("probe-writer".into())
            .spawn(move || writer.run())
            .map_err(|e| format!("falha ao criar thread probe-writer: {e}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let mut feeds = Vec::with_capacity(specs.len());
        let mut slots = Vec::with_capacity(specs.len());
        let mut handles = Vec::with_capacity(specs.len() + 1);
        let mut feed_dirs = Vec::with_capacity(specs.len());

        for spec in specs {
            let slot = spec.slot;
            let dir = run
                .create_feed_dir(slot, &spec.url_text)
                .map_err(|e| format!("não foi possível criar a pasta do feed {slot}: {e}"))?;
            feed_dirs.push(
                dir.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string(),
            );

            let pipeline = FeedPipeline::spawn(spec.clone(), &self.cfg, self.receiver_cfg.clone());

            let mut meta = probe::session::new_session_meta(
                &run,
                slot,
                &spec.name,
                &spec.url_text,
                spec.fec,
                &self.cfg,
            );
            meta.so_rcvbuf_bytes = pipeline.shared.so_rcvbuf();
            meta.interface = self
                .receiver_cfg_iface()
                .unwrap_or_else(|| "default".to_string());
            if let Err(e) = probe::session::write_session_meta(&dir, &meta) {
                tracing::warn!(slot, error = %e, "probe: falha ao gravar session.toml");
            }

            let engine = ProbeEngine::new(
                FeedIdentity {
                    slot,
                    name: spec.name.clone(),
                    url: spec.url_text.clone(),
                    fec: spec.fec,
                },
                self.cfg.clone(),
                clock.clone(),
                Some(dir.clone()),
                Some(writer_handle.clone()),
            );

            let published: Arc<RwLock<Option<FeedSnapshot>>> = Arc::new(RwLock::new(None));
            slots.push(published.clone());

            handles.push(spawn_engine_thread(EngineThread {
                slot,
                engine,
                shared: pipeline.shared.clone(),
                snapshot_rx: pipeline.snapshot_rx.clone(),
                published,
                stop: stop.clone(),
                window: self.window.clone(),
                session_dir: dir,
                meta,
                agg_reset: pipeline.agg_reset_sender(),
            })?);

            feeds.push(pipeline);
        }

        // SPEC-PROBE-003b — um único decoder percorre os feeds em round-robin,
        // com os ticks escalonados; um decoder por feed derrubaria a garantia
        // de que dois decodes SW nunca coincidem (§5.4).
        handles.push(spawn_snapshot_thread(
            self.cfg.clone(),
            feeds
                .iter()
                .map(|f| SnapshotTarget {
                    slot: f.spec.slot,
                    shared: f.shared.clone(),
                    tap: f.tap.clone(),
                    rx: f.tap_rx.clone(),
                })
                .collect(),
            self.thumbnails.clone(),
            stop.clone(),
        )?);

        let run_meta = probe::RunMeta {
            run_id: run.run_id.clone(),
            started_utc: Some(started_utc),
            ended_utc: None,
            host: probe::session::host_name(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            profile_version: self.cfg.profile_version,
            feeds: feed_dirs,
        };
        if let Err(e) = run.write_meta(&run_meta) {
            tracing::warn!(error = %e, "probe: falha ao gravar run.toml");
        }

        tracing::info!(
            run_id = %run.run_id,
            feeds = feeds.len(),
            dir = %run.dir.display(),
            "probe: run aberto"
        );

        self.active = Some(ActiveRun {
            run,
            feeds,
            slots,
            stop,
            handles,
            writer_handle: Some(writer_handle),
            writer_thread: Some(writer_thread),
            _awake: KeepAwake::acquire(self.cfg.prevent_sleep),
            started: Instant::now(),
        });
        self.publish();
        Ok(())
    }

    fn receiver_cfg_iface(&self) -> Option<String> {
        None
    }

    /// Fecha o run, gravando o resumo de cada sessão.
    ///
    /// SPEC-PROBE-004 — "parar/fechar grava o resumo de cada uma".
    pub fn stop(&mut self) {
        let Some(mut active) = self.active.take() else {
            return;
        };

        active.stop.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(5);
        for handle in active.handles.drain(..) {
            crate::join_with_deadline(handle, deadline);
        }
        for feed in active.feeds.iter_mut() {
            feed.shutdown();
        }

        // Só agora o writer pode encerrar: as engines já enfileiraram o
        // fechamento dos eventos e a última linha de CSV.
        if let Some(w) = active.writer_handle.take() {
            w.send(probe::WriteJob::FlushNow);
            drop(w);
        }
        if let Some(t) = active.writer_thread.take() {
            crate::join_with_deadline(t, Instant::now() + Duration::from_secs(3));
        }

        // `run.toml` recebe o fim do run.
        let meta_path = active.run.dir.join(probe::session::RUN_FILE);
        if let Ok(text) = std::fs::read_to_string(&meta_path) {
            if let Ok(mut meta) = toml::from_str::<probe::RunMeta>(&text) {
                meta.ended_utc = Some(chrono::Utc::now());
                if let Err(e) = active.run.write_meta(&meta) {
                    tracing::warn!(error = %e, "probe: falha ao fechar run.toml");
                }
            }
        }

        tracing::info!(
            run_id = %active.run.run_id,
            duracao_s = active.started.elapsed().as_secs(),
            "probe: run encerrado"
        );
        self.publish_stopped();
    }

    /// Gera o relatório HTML do run em curso (ou do último publicado).
    ///
    /// SPEC-PROBE-014 · SPEC-PROBE-020
    pub fn export_report(&self) -> Result<PathBuf, String> {
        let snapshot = self
            .snapshot
            .read()
            .map(|g| g.clone())
            .map_err(|_| "estado do run indisponível".to_string())?;

        let dir = snapshot
            .run_dir
            .clone()
            .ok_or_else(|| "nenhum run aberto para exportar".to_string())?;
        let path = dir.join(probe::session::REPORT_FILE);
        std::fs::write(&path, render_run_report(&snapshot))
            .map_err(|e| format!("falha ao gravar {}: {e}", path.display()))?;
        tracing::info!(path = %path.display(), "probe: relatório exportado");
        Ok(path)
    }

    /// Recolhe os snapshots por slot e publica o estado do run.
    ///
    /// Chamado a 1 Hz pelo `cmd-handler`.
    pub fn publish(&self) {
        let Some(active) = &self.active else {
            return;
        };
        let feeds: Vec<FeedSnapshot> = active
            .slots
            .iter()
            .filter_map(|s| s.read().ok().and_then(|g| g.clone()))
            .collect();

        if let Ok(mut guard) = self.snapshot.write() {
            *guard = ProbeSnapshot {
                run_id: active.run.run_id.clone(),
                run_dir: Some(active.run.dir.clone()),
                started_utc: Some(active.run.started_utc),
                run_secs: active.started.elapsed().as_secs(),
                recording: true,
                feeds,
            };
        }
    }

    fn publish_stopped(&self) {
        if let Ok(mut guard) = self.snapshot.write() {
            guard.recording = false;
        }
    }
}

impl Drop for ProbeRunner {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Thread de engine (uma por feed)
// ---------------------------------------------------------------------------

struct EngineThread {
    slot: usize,
    engine: ProbeEngine,
    shared: Arc<crate::feed::FeedShared>,
    snapshot_rx: ts::aggregator::SnapshotReceiver,
    published: Arc<RwLock<Option<FeedSnapshot>>>,
    stop: Arc<AtomicBool>,
    window: Arc<AtomicUsize>,
    session_dir: PathBuf,
    meta: probe::SessionMeta,
    agg_reset: crossbeam_channel::Sender<ts::aggregator::AggregatorNetEvent>,
}

impl EngineThread {
    fn reset_metrics(&self) {
        let _ = self
            .agg_reset
            .try_send(ts::aggregator::AggregatorNetEvent::Reset);
    }
}

fn spawn_engine_thread(mut t: EngineThread) -> Result<std::thread::JoinHandle<()>, String> {
    let name = format!("probe-engine-{}", t.slot);
    std::thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            let mut was_connected = false;

            while !t.stop.load(Ordering::Relaxed) {
                // Dorme até o instante agendado; o desvio vira `sched_jitter_ms`
                // (SPEC-PROBE-013), por isso não se corrige o alvo aqui.
                let deadline = t.engine.next_deadline();
                let now = Instant::now();
                if deadline > now {
                    std::thread::sleep((deadline - now).min(Duration::from_millis(250)));
                    if Instant::now() < t.engine.next_deadline() {
                        continue;
                    }
                }

                let connected = t.shared.connected();
                // SPEC-PROBE-011 — ao voltar de uma queda, os contadores do
                // aggregator recomeçam; sem rebaseline a retomada apareceria
                // como rajada de erros que nunca existiu.
                if connected && !was_connected {
                    t.engine.rebaseline();
                    // Limpa também a tabela de PIDs do aggregator: PIDs que
                    // sumiram durante a queda não devem reaparecer como
                    // fantasmas no tile.
                    t.reset_metrics();
                }
                was_connected = connected;

                let events = t.engine.tick(probe::TickInput {
                    metrics: Some(t.snapshot_rx.borrow()),
                    connected,
                    local_drops_total: t.shared.local_drops(),
                    dropped_events_total: 0,
                    encapsulation: t.shared.encapsulation(),
                    video_pids: t.shared.video_pids(),
                    audio_pids: t.shared.audio_pids(),
                    // A altura vem por serviço, junto do thumbnail; o campo do
                    // feed só existiria para um multiplex sem PSI, onde não há
                    // vídeo decodificado para medir (SPEC-PROBE-024).
                    video_height: None,
                    scrambled: t.shared.scrambled(),
                    snapshot_state: t.shared.snapshot_state(),
                    writer_drops_total: 0,
                    reconnect_attempts: t.shared.reconnect_attempts(),
                    services: (*t.shared.services()).clone(),
                    visuals: t.shared.visuals(),
                });
                for ev in &events {
                    tracing::debug!(
                        slot = t.slot,
                        check = %ev.check_id,
                        phase = ?ev.phase,
                        count = ev.count,
                        "probe: evento"
                    );
                }

                let window = SeriesWindow::ALL
                    .get(t.window.load(Ordering::Relaxed))
                    .copied()
                    .unwrap_or(SeriesWindow::OneHour);
                let mut snap = t.engine.snapshot(window);
                // §8.1 — sem nome no TOML, o tile mostra o nome do serviço da
                // SDT; só se nem isso houver é que cai na URL.
                if snap.name.trim().is_empty() {
                    if let Some(name) = t.shared.service_name() {
                        snap.name = name;
                    }
                }
                // SPEC-PROBE-013a — o 1º estágio de degradação suspende o
                // thumbnail; quem lê isso é a thread `probe-snapshot`.
                t.shared
                    .set_snapshot_suspended(t.engine.degradation() >= probe::DegradationStage::Thumbnail);

                if let Ok(mut guard) = t.published.write() {
                    *guard = Some(snap);
                }
            }

            // Encerramento: fecha os eventos abertos e grava o resumo.
            let summary = t.engine.finish();
            t.meta.encapsulation = t.shared.encapsulation();
            t.meta.summary = summary;
            if let Err(e) = probe::session::write_session_meta(&t.session_dir, &t.meta) {
                tracing::warn!(slot = t.slot, error = %e, "probe: falha ao gravar resumo da sessão");
            }
            if let Ok(mut guard) = t.published.write() {
                *guard = Some(t.engine.snapshot(SeriesWindow::WholeSession));
            }
            tracing::info!(slot = t.slot, "probe-engine: encerrado");
        })
        .map_err(|e| format!("falha ao criar thread {name}: {e}"))
}

// ---------------------------------------------------------------------------
// Thread de snapshot de vídeo (global, round-robin)
// ---------------------------------------------------------------------------

struct SnapshotTarget {
    slot: usize,
    shared: Arc<crate::feed::FeedShared>,
    tap: Arc<crate::feed::SnapshotTap>,
    rx: crossbeam_channel::Receiver<ts::PesData>,
}

/// Uma captura planejada: um serviço de um feed.
///
/// SPEC-PROBE-024 — o round-robin deixou de ser por feed. Num MPTS, um
/// thumbnail por feed mostraria sempre o mesmo serviço e o mosaico de serviços
/// (§8.2) ficaria cego.
#[derive(Debug, Clone, Copy)]
struct Capture {
    target: usize,
    service_id: u16,
    pid: ts::Pid,
    codec: av::MediaCodec,
}

/// Monta a volta do round-robin: um item por serviço com vídeo, de todos os
/// feeds, na ordem dos slots.
///
/// O plano é refeito a cada captura de propósito: a PSI muda em runtime (troca
/// de grade, PMT nova), e um plano fixo capturado no start apontaria para PIDs
/// que não existem mais.
fn plan_captures(targets: &[SnapshotTarget]) -> Vec<Capture> {
    let mut plan = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        for service in target.shared.services().iter() {
            let (Some(pid), Some(stream_type)) = (
                service.primary_video_pid(),
                service.primary_video_stream_type(),
            ) else {
                continue;
            };
            let Some(codec) = av::MediaCodec::from_stream_type(stream_type) else {
                continue;
            };
            plan.push(Capture {
                target: index,
                service_id: service.service_id,
                pid,
                codec,
            });
        }
    }
    plan
}

fn spawn_snapshot_thread(
    cfg: ProbeConfig,
    targets: Vec<SnapshotTarget>,
    thumbnails: ui_slint::SharedThumbnails,
    stop: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("probe-snapshot".into())
        .spawn(move || {
            // §5.4 — abaixo do normal: o decode SW nunca pode disputar CPU com
            // a recepção UDP (SPEC-PROBE-013a).
            probe::power::lower_current_thread_priority();

            if targets.is_empty() {
                return;
            }
            // SPEC-PROBE-003b — offset = intervalo / n_feeds, de modo que dois
            // decodes SW nunca coincidam no mesmo instante.  A volta agora
            // percorre **serviços**, então num MPTS de N serviços cada um é
            // atualizado a cada `N × stagger`: o custo de CPU por captura fica
            // igual ao de antes (SPEC-PROBE-002), só a cadência por serviço é
            // que dilui.
            let stagger = cfg.snapshot_interval() / targets.len() as u32;
            let arm_window = Duration::from_secs_f64(cfg.snapshot_arm_secs.max(0.1));
            let mut generation: u64 = 0;
            let mut index = 0usize;

            let mut decoder = match av::FfmpegDecoder::new() {
                Ok(mut d) => {
                    // SPEC-PROBE-002 — snapshot é sempre SW: nenhum recurso de
                    // hwaccel é alocado no modo Probe.
                    let _ = d.enable_hwaccel(av::HwAccelMode::Off);
                    d
                }
                Err(e) => {
                    tracing::warn!(error = %e, "probe-snapshot: decoder indisponível — thumbnails desativados");
                    return;
                }
            };

            // Espera fatiada, para responder ao stop sem esperar o `stagger`
            // inteiro no shutdown.
            let wait = |stop: &AtomicBool| {
                let deadline = Instant::now() + stagger;
                while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                }
            };

            while !stop.load(Ordering::Relaxed) {
                let plan = plan_captures(&targets);
                if plan.is_empty() {
                    // Sem PSI ainda, ou multiplex sem vídeo: nada a decodificar.
                    for target in &targets {
                        target.shared.set_snapshot_state(if target.shared.connected() {
                            probe::SnapshotState::Pending
                        } else {
                            probe::SnapshotState::NoSignal
                        });
                    }
                    wait(&stop);
                    continue;
                }

                let capture = plan[index % plan.len()];
                index = index.wrapping_add(1);
                let target = &targets[capture.target];

                if target.shared.snapshot_suspended() {
                    // 1º estágio de degradação: o thumbnail é a primeira coisa
                    // a cair, nunca a recepção (SPEC-PROBE-013a).
                    let suspended = probe::SnapshotState::Suspended;
                    target.shared.set_snapshot_state(suspended);
                    target.shared.set_visual(
                        capture.service_id,
                        probe::ServiceVisual {
                            video_height: None,
                            state: suspended,
                        },
                    );
                    wait(&stop);
                    continue;
                }

                let result = capture_one(
                    target,
                    capture.pid,
                    capture.codec,
                    &mut decoder,
                    arm_window,
                    cfg.snapshot_max_width,
                    &stop,
                );
                match result {
                    Some((thumb, height)) => {
                        generation += 1;
                        target.shared.set_snapshot_state(probe::SnapshotState::Ok);
                        target.shared.set_visual(
                            capture.service_id,
                            probe::ServiceVisual {
                                video_height: Some(height),
                                state: probe::SnapshotState::Ok,
                            },
                        );
                        if let Ok(mut map) = thumbnails.write() {
                            // SPEC-PROBE-003 — só uma imagem viva por serviço:
                            // a anterior é liberada aqui, ao publicar a nova.
                            map.insert(
                                (target.slot, capture.service_id),
                                ui_slint::ProbeThumbnail {
                                    generation,
                                    ..thumb
                                },
                            );
                        }
                    }
                    None => {
                        // SPEC-PROBE-003a — sem IRAP na janela o estado vira
                        // "sem keyframe"; isso **não** gera alarme por si só.
                        let state = if target.shared.connected() {
                            probe::SnapshotState::NoKeyframe
                        } else {
                            probe::SnapshotState::NoSignal
                        };
                        target.shared.set_snapshot_state(state);
                        target.shared.set_visual(
                            capture.service_id,
                            probe::ServiceVisual {
                                video_height: None,
                                state,
                            },
                        );
                        tracing::trace!(
                            slot = target.slot,
                            service = capture.service_id,
                            "probe-snapshot: sem keyframe na janela"
                        );
                    }
                }

                wait(&stop);
            }
            tracing::info!("probe-snapshot: encerrado");
        })
        .map_err(|e| format!("falha ao criar thread probe-snapshot: {e}"))
}

/// Arma o decoder no PID de vídeo de um serviço, espera **um** frame e desarma.
///
/// Devolve o thumbnail e a altura **nativa** do frame (badge `HD`/`SD`, que
/// descreve o stream e não a miniatura).
///
/// SPEC-PROBE-003a — a janela de armação é limitada; sem IRAP nela, devolve
/// `None` e o estado do tile vira "sem keyframe", sem gerar alarme.
#[allow(clippy::too_many_arguments)]
fn capture_one(
    target: &SnapshotTarget,
    pid: ts::Pid,
    codec: av::MediaCodec,
    decoder: &mut av::FfmpegDecoder,
    arm_window: Duration,
    max_width: u32,
    stop: &AtomicBool,
) -> Option<(ui_slint::ProbeThumbnail, u32)> {
    // Descarta o que sobrou da janela anterior antes de armar.
    while target.rx.try_recv().is_ok() {}
    decoder.reset();

    // O `PesAssembler` publica num canal; aqui ele é local à janela de
    // armação, e não um estágio permanente do pipeline (SPEC-PROBE-002).
    let (pes_tx, pes_rx) = crossbeam_channel::bounded::<av::PesPacket>(64);
    let mut asm = av::PesAssembler::new(pes_tx);
    asm.register_pid(pid, codec);
    target.tap.arm(pid);

    let deadline = Instant::now() + arm_window;
    let mut out = None;

    'window: while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        let Ok(data) = target.rx.recv_timeout(Duration::from_millis(100)) else {
            continue;
        };
        asm.push(data.pid, data.pusi, data.data);

        while let Ok(packet) = pes_rx.try_recv() {
            let Ok(frames) = decoder.decode(&packet) else {
                continue;
            };
            for frame in frames {
                if let av::DecodedFrame::Video(vf) = frame {
                    // Badge HD/SD: a altura vem do frame decodificado, já que o
                    // modo Probe não roda o Media Info completo.
                    out = downscale(&vf, max_width).map(|t| (t, source_height(&vf)));
                    if out.is_some() {
                        break 'window;
                    }
                }
            }
        }
    }

    // SPEC-PROBE-003a — desarma após 1 frame (ou ao expirar a janela); fora
    // daqui nenhum PES de vídeo é sequer enfileirado.
    target.tap.disarm();
    while target.rx.try_recv().is_ok() {}
    out
}

/// Altura nativa do frame decodificado (antes da redução do thumbnail).
///
/// SPEC-PROBE-018 — o badge `HD`/`SD` descreve o stream, não o thumbnail.
fn source_height(frame: &av::VideoFrame) -> u32 {
    match frame {
        av::VideoFrame::Sw(yuv) => yuv.height,
        av::VideoFrame::Hw(hw) => match &hw.surface {
            av::HwSurface::Cpu(p) => p.height,
            av::HwSurface::Shared(_) => 0,
        },
    }
}

/// Converte e reduz o frame para no máximo `max_width` de largura.
///
/// SPEC-PROBE-003 — thumbnail ≤ 320×180 por default.
fn downscale(frame: &av::VideoFrame, max_width: u32) -> Option<ui_slint::ProbeThumbnail> {
    let rgba = ui_slint::video::convert(frame)?;
    let (sw, sh) = (rgba.width(), rgba.height());
    if sw == 0 || sh == 0 {
        return None;
    }
    let max_width = max_width.max(16);
    let (dw, dh) = if sw <= max_width {
        (sw, sh)
    } else {
        (max_width, (sh * max_width / sw).max(1))
    };

    let src = rgba.as_bytes();
    let mut dst = vec![0u8; (dw * dh * 4) as usize];
    // Amostragem por vizinho mais próximo: o thumbnail é indicador de
    // "tem imagem e qual", não material de análise perceptual (§2.2).
    for y in 0..dh {
        let sy = (y as u64 * sh as u64 / dh as u64) as u32;
        for x in 0..dw {
            let sx = (x as u64 * sw as u64 / dw as u64) as u32;
            let si = ((sy * sw + sx) * 4) as usize;
            let di = ((y * dw + x) * 4) as usize;
            if si + 4 <= src.len() && di + 4 <= dst.len() {
                dst[di..di + 4].copy_from_slice(&src[si..si + 4]);
            }
        }
    }

    Some(ui_slint::ProbeThumbnail {
        width: dw,
        height: dh,
        rgba: dst,
        generation: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC-PROBE-010 — o índice de janela vindo da UI é saturado, não
    /// indexa fora do array.
    #[test]
    fn spec_probe_010_window_index_is_clamped() {
        let runner = ProbeRunner::new(
            ProbeConfig::default(),
            net::ReceiverConfig::default(),
            Arc::new(RwLock::new(ProbeSnapshot::default())),
            Arc::new(RwLock::new(Default::default())),
        );
        runner.set_window(99);
        assert_eq!(
            runner.window.load(Ordering::Relaxed),
            SeriesWindow::ALL.len() - 1
        );
        runner.set_window(0);
        assert_eq!(runner.window.load(Ordering::Relaxed), 0);
    }

    /// SPEC-PROBE-004 — sem feeds configurados, abrir o run falha com uma
    /// mensagem acionável em vez de criar pastas vazias.
    #[test]
    fn spec_probe_004_start_without_feeds_is_an_actionable_error() {
        let mut runner = ProbeRunner::new(
            ProbeConfig::default(),
            net::ReceiverConfig::default(),
            Arc::new(RwLock::new(ProbeSnapshot::default())),
            Arc::new(RwLock::new(Default::default())),
        );
        let err = runner.start().expect_err("sem feeds deve falhar");
        assert!(err.contains("[[probe.feeds]]"), "{err}");
        assert!(!runner.is_running());
    }

    /// SPEC-PROBE-014 — exportar sem run aberto não estoura.
    #[test]
    fn spec_probe_014_export_without_run_is_an_error_not_a_panic() {
        let runner = ProbeRunner::new(
            ProbeConfig::default(),
            net::ReceiverConfig::default(),
            Arc::new(RwLock::new(ProbeSnapshot::default())),
            Arc::new(RwLock::new(Default::default())),
        );
        assert!(runner.export_report().is_err());
    }
}
