//! `ProbeEngine` — o tick de 1 Hz de um feed.
//!
//! Amostra os contadores acumulados, avalia os checks, atualiza séries e
//! linha do tempo e **enfileira** a escrita.  O engine nunca faz I/O de disco
//! no próprio tick: escrita lenta (disco de notebook, antivírus) não pode
//! atrasar a amostragem (§5.4).
//!
//! SPEC-PROBE-005 · SPEC-PROBE-006 · SPEC-PROBE-007 · SPEC-PROBE-013

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use ts::metrics::MetricsSnapshot;

use crate::check::{
    CheckEngine, CheckProfile, Measurement, CHECK_AUDIO_MISSING, CHECK_CC_ERROR, CHECK_CRC_ERROR,
    CHECK_DEGRADED, CHECK_FEED_UNAVAILABLE, CHECK_LOCAL_DROPS, CHECK_PCR_DISCONTINUITY,
    CHECK_PCR_ERROR, CHECK_RTP_OUT_OF_ORDER, CHECK_SCHED_JITTER, CHECK_TS_SYNC_LOSS,
    CHECK_VIDEO_MISSING,
};
use crate::clock::ProbeClock;
use crate::config::{FecMode, ProbeConfig};
use crate::degrade::{DegradeController, DegradePolicy, OverloadSignals};
use crate::event::{EventContext, EventOrigin, EventPhase, ProbeEvent};
use crate::sample::{AvPresence, CounterBaseline, ProbeSample, RawCounters, CSV_HEADER};
use crate::series::{MetricId, SeriesStore, SeriesWindow};
use crate::session::{Encapsulation, SessionSummary, EVENTS_FILE, METRICS_FILE};
use crate::severity::{Layer, Severity};
use crate::snapshot::{
    DegradationStage, EventRow, FeedSnapshot, ProbeHealth, SnapshotState, UnavailableStats,
    EVENT_LOG_CAPACITY,
};
use crate::writer::{WriteJob, WriterHandle};

/// Janela usada na disponibilidade exibida no tile.
///
/// SPEC-PROBE-018 — "disponibilidade da janela corrente (default últimos 60
/// min)".
pub const AVAILABILITY_WINDOW_SECS: u64 = 3_600;

/// Identidade estática de um feed.
#[derive(Debug, Clone)]
pub struct FeedIdentity {
    pub slot: usize,
    pub name: String,
    pub url: String,
    pub fec: FecMode,
}

/// Entrada de um tick.
///
/// Tudo que o engine precisa saber sobre o segundo que passou.  `metrics`
/// ausente significa "sem snapshot novo" — o tick ainda acontece (a linha do
/// tempo precisa da célula), mas os deltas ficam zerados.
#[derive(Debug, Clone, Default)]
pub struct TickInput {
    pub metrics: Option<MetricsSnapshot>,
    pub connected: bool,
    /// Descartes locais **acumulados** (canal cheio, socket, writer).
    pub local_drops_total: u64,
    /// Eventos discretos descartados por fila cheia, acumulados (§5.3).
    pub dropped_events_total: u64,
    pub encapsulation: Encapsulation,
    /// PIDs de vídeo da PMT; vazio = PMT ainda não recebida (SPEC-PROBE-018).
    pub video_pids: Vec<ts::Pid>,
    /// PIDs de áudio da PMT; vazio = PMT ainda não recebida.
    pub audio_pids: Vec<ts::Pid>,
    pub video_height: Option<u32>,
    pub scrambled: bool,
    pub snapshot_state: SnapshotState,
    /// Linhas descartadas pelo writer, acumuladas (RNF-PRB-004).
    pub writer_drops_total: u64,
    /// Tentativas de reconexão desde a última queda (SPEC-PROBE-011).
    pub reconnect_attempts: u32,
}

/// Motor de um feed.
///
/// SPEC-PROBE-005 · SPEC-PROBE-007
pub struct ProbeEngine {
    identity: FeedIdentity,
    cfg: ProbeConfig,
    clock: Arc<dyn ProbeClock>,
    writer: Option<WriterHandle>,
    session_dir: Option<PathBuf>,
    metrics_path: Option<PathBuf>,
    events_path: Option<PathBuf>,

    checks: CheckEngine,
    series: SeriesStore,
    baseline: CounterBaseline,

    started_mono: Instant,
    next_deadline: Instant,
    /// Descartes locais acumulados no tick anterior.
    prev_local_drops: u64,

    events: VecDeque<EventRow>,
    events_opened: u64,

    // Estado publicado.
    connected: bool,
    encapsulation: Encapsulation,
    last_sample: Option<ProbeSample>,
    presence: AvPresence,
    health: ProbeHealth,
    unavailable: UnavailableStats,
    unavailable_since: Option<Instant>,
    snapshot_state: SnapshotState,
    worst_session_severity: Option<Severity>,
    /// `true` depois que a PMT identificou pelo menos um PID de A/V.
    presence_seen_pids: bool,
    /// SPEC-PROBE-013a — quem decide o estágio de degradação.
    degrade: DegradeController,
    prev_writer_drops: u64,
}

impl ProbeEngine {
    /// Cria o motor de um feed e prepara os arquivos da sessão.
    ///
    /// `session_dir`/`writer` ausentes ⇒ modo em memória (usado em fixtures e
    /// quando o motor roda em Broadcast sem persistência, §3.1).
    ///
    /// SPEC-PROBE-004 · SPEC-PROBE-006
    pub fn new(
        identity: FeedIdentity,
        cfg: ProbeConfig,
        clock: Arc<dyn ProbeClock>,
        session_dir: Option<PathBuf>,
        writer: Option<WriterHandle>,
    ) -> Self {
        let now = clock.now_mono();
        let interval = cfg.sample_interval();
        let series = SeriesStore::new(cfg.rollup_secs, cfg.timeline_bucket_secs);
        let checks = CheckEngine::new(CheckProfile::from_config(&cfg), cfg.summary_interval());

        let metrics_path = session_dir.as_ref().map(|d| d.join(METRICS_FILE));
        let events_path = session_dir.as_ref().map(|d| d.join(EVENTS_FILE));

        if let (Some(w), Some(m), Some(e)) = (&writer, &metrics_path, &events_path) {
            w.send(WriteJob::Create {
                path: m.clone(),
                header: CSV_HEADER.to_string(),
            });
            w.send(WriteJob::Create {
                path: e.clone(),
                header: String::new(),
            });
        }

        Self {
            identity,
            // O primeiro tick é imediato: o deadline inicial é o próprio
            // instante de criação, senão a primeira amostra nasceria com um
            // "jitter" de um período inteiro que nunca existiu.
            next_deadline: now,
            cfg,
            clock,
            writer,
            session_dir,
            metrics_path,
            events_path,
            checks,
            series,
            baseline: CounterBaseline::new(),
            started_mono: now,
            prev_local_drops: 0,
            events: VecDeque::with_capacity(64),
            events_opened: 0,
            connected: false,
            encapsulation: Encapsulation::Unknown,
            last_sample: None,
            presence: AvPresence::default(),
            health: ProbeHealth::default(),
            unavailable: UnavailableStats::default(),
            unavailable_since: None,
            snapshot_state: SnapshotState::Pending,
            worst_session_severity: None,
            presence_seen_pids: false,
            degrade: DegradeController::new(DegradePolicy::for_interval(interval)),
            prev_writer_drops: 0,
        }
    }

    /// Instante em que o próximo tick deveria acontecer.
    ///
    /// A thread `probe-engine-{slot}` dorme até aqui; o desvio entre o
    /// agendado e o real vira `sched_jitter_ms` (SPEC-PROBE-013).
    pub fn next_deadline(&self) -> Instant {
        self.next_deadline
    }

    /// Executa um tick.
    ///
    /// Devolve os eventos emitidos, já persistidos na fila do writer.
    ///
    /// SPEC-PROBE-005 · SPEC-PROBE-007 · SPEC-PROBE-013
    pub fn tick(&mut self, input: TickInput) -> Vec<ProbeEvent> {
        let now = self.clock.now_mono();
        let now_utc = self.clock.now_utc();

        // ── Jitter de agendamento (SPEC-PROBE-013) ──────────────────────
        let sched_jitter_ms = now
            .saturating_duration_since(self.next_deadline)
            .as_secs_f64()
            * 1000.0;
        self.health.sched_jitter_ms = sched_jitter_ms;
        self.health.sched_jitter_peak_ms = self.health.sched_jitter_peak_ms.max(sched_jitter_ms);
        // Reagenda a partir do agora quando o atraso passou de um período
        // inteiro: recuperar o atraso "correndo atrás" geraria uma rajada de
        // ticks e falsearia os deltas por segundo.
        let interval = self.cfg.sample_interval();
        self.next_deadline = if now >= self.next_deadline + interval {
            now + interval
        } else {
            self.next_deadline + interval
        };

        // ── Deltas de contadores (§5.3) ─────────────────────────────────
        let deltas = match &input.metrics {
            Some(m) => self.baseline.delta(RawCounters::from_metrics(m)),
            None => Default::default(),
        };
        let local_drops_delta = input
            .local_drops_total
            .saturating_sub(self.prev_local_drops);
        self.prev_local_drops = input.local_drops_total;
        self.health.local_drops = input.local_drops_total;
        self.health.local_drops_last = local_drops_delta;
        self.health.dropped_events = input.dropped_events_total;
        if let Some(w) = &self.writer {
            self.health.writer_drops = w.dropped();
        }
        let writer_drops_delta = input
            .writer_drops_total
            .max(self.health.writer_drops)
            .saturating_sub(self.prev_writer_drops);
        self.prev_writer_drops = input.writer_drops_total.max(self.health.writer_drops);

        // SPEC-PROBE-013a/013b — o estágio é decidido aqui, dentro do engine:
        // é ele quem enxerga jitter de tick, descarte local e perda de escrita
        // no mesmo lugar, e é ele quem emite o evento `probe_degraded`.
        let degradation = self.degrade.observe(OverloadSignals {
            sched_jitter_ms,
            local_drops_delta,
            writer_drops_delta,
        });
        self.health.degradation = degradation;

        // ── Estado do feed ──────────────────────────────────────────────
        self.track_availability(input.connected, now);
        self.connected = input.connected;
        if input.encapsulation != Encapsulation::Unknown {
            self.encapsulation = input.encapsulation;
        }
        self.snapshot_state = input.snapshot_state;
        self.unavailable.reconnect_attempts = input.reconnect_attempts;

        let (bitrate_kbps, null_ratio) = input
            .metrics
            .as_ref()
            .map_or((0.0, 0.0), |m| (m.total_bitrate_kbps, m.null_ratio));
        // SPEC-PROBE-018 — quando a PMT já chegou, a presença sai dos PIDs
        // dela; sem PMT, cai na classificação do aggregator (que só existe no
        // pipeline do player).
        let av_known = !input.video_pids.is_empty() || !input.audio_pids.is_empty();
        if let Some(m) = &input.metrics {
            let mut presence = if av_known {
                AvPresence::from_metrics_with_pids(m, &input.video_pids, &input.audio_pids)
            } else {
                AvPresence::from_metrics(m)
            };
            presence.scrambled = input.scrambled;
            presence.video_height = input.video_height.or(self.presence.video_height);
            self.presence = presence;
        }

        // ── Medições → motor de checks ──────────────────────────────────
        // SPEC-PROBE-013: perda no mesmo segundo de descarte local é atribuída
        // à probe, não à rede.
        let origin = if local_drops_delta > 0 {
            EventOrigin::Local
        } else {
            EventOrigin::Network
        };
        let net_ctx = EventContext::network().with_origin(origin);

        let mut measurements: Vec<Measurement> = Vec::with_capacity(16);
        measurements.push(Measurement::gauge(
            CHECK_FEED_UNAVAILABLE,
            if input.connected { 0.0 } else { 1.0 },
        ));
        measurements.push(Measurement::gauge(CHECK_SCHED_JITTER, sched_jitter_ms));
        measurements.push(
            Measurement::counter(CHECK_LOCAL_DROPS, local_drops_delta as f64)
                .with_context(EventContext::network().with_origin(EventOrigin::Local)),
        );
        measurements.push(Measurement::gauge(
            CHECK_DEGRADED,
            degradation.stage_value(),
        ));

        if input.connected {
            for (pid, count) in &deltas.cc_by_pid {
                measurements.push(Measurement {
                    check_id: CHECK_CC_ERROR,
                    context: EventContext::pid(*pid).with_origin(origin),
                    value: *count as f64,
                    occurrences: *count,
                });
            }
            measurements.push(
                Measurement::counter(CHECK_CRC_ERROR, deltas.crc as f64)
                    .with_context(net_ctx.clone()),
            );
            measurements.push(
                Measurement::counter(CHECK_TS_SYNC_LOSS, deltas.sync_loss as f64)
                    .with_context(net_ctx.clone()),
            );
            measurements.push(
                Measurement::counter(CHECK_PCR_ERROR, deltas.pcr_jitter as f64)
                    .with_context(net_ctx.clone()),
            );
            measurements.push(
                Measurement::counter(CHECK_PCR_DISCONTINUITY, deltas.pcr_disc as f64)
                    .with_context(net_ctx.clone()),
            );
            if self.encapsulation.has_rtp() {
                measurements.push(
                    Measurement::counter(CHECK_RTP_OUT_OF_ORDER, deltas.rtp_out_of_order as f64)
                        .with_context(net_ctx.clone()),
                );
            }
            // Só afirma "falta vídeo/áudio" depois de saber quais PIDs
            // procurar: antes da PMT, a ausência de medição não é ausência de
            // conteúdo, e um alarme crítico aqui seria um falso positivo em
            // todo feed nos primeiros segundos.
            self.presence_seen_pids |= av_known;
            if self.presence_seen_pids {
                measurements.push(Measurement::gauge(
                    CHECK_VIDEO_MISSING,
                    self.presence.video_kbps,
                ));
                measurements.push(Measurement::gauge(
                    CHECK_AUDIO_MISSING,
                    self.presence.audio_kbps,
                ));
            }
        }

        let events = self.checks.evaluate(&measurements, now, now_utc);

        // ── Séries e linha do tempo ─────────────────────────────────────
        let worst = self.checks.worst_open_severity();
        self.worst_session_severity = match (self.worst_session_severity, worst) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };

        // SPEC-PROBE-013a: no 3º estágio de degradação as séries secundárias
        // param; bitrate e a linha do tempo nunca param.
        let secondary_ok = degradation < DegradationStage::SecondarySeries;
        self.series
            .push_metric(MetricId::BitrateKbps, now_utc, bitrate_kbps);
        self.series.push_health(now_utc, worst, input.connected);
        if secondary_ok {
            self.series
                .push_metric(MetricId::CcErrorsPerS, now_utc, deltas.cc_total as f64);
            self.series
                .push_metric(MetricId::CrcErrorsPerS, now_utc, deltas.crc as f64);
            self.series.push_metric(
                MetricId::RtpLossPerS,
                now_utc,
                deltas.rtp_out_of_order as f64,
            );
            self.series
                .push_metric(MetricId::LocalDropsPerS, now_utc, local_drops_delta as f64);
            self.series
                .push_metric(MetricId::SchedJitterMs, now_utc, sched_jitter_ms);
        }

        // ── Amostra persistida ──────────────────────────────────────────
        let sample = ProbeSample {
            ts_utc: now_utc,
            uptime_s: now.duration_since(self.started_mono).as_secs(),
            connected: input.connected,
            bitrate_kbps,
            null_ratio,
            cc_errors_delta: deltas.cc_total,
            crc_errors_delta: deltas.crc,
            sync_loss_delta: deltas.sync_loss,
            pcr_jitter_delta: deltas.pcr_jitter,
            pcr_disc_delta: deltas.pcr_disc,
            local_drops_delta,
            sched_jitter_ms,
            worst_severity: worst,
        };

        if let (Some(w), Some(path)) = (&self.writer, &self.metrics_path) {
            if !w.append(path.clone(), sample.to_csv()) {
                self.health.local_drops = self.health.local_drops.saturating_add(1);
            }
        }
        self.last_sample = Some(sample);

        self.record_events(&events);
        events
    }

    /// Registra eventos no log de UI e na fila do writer.
    ///
    /// SPEC-PROBE-006 · SPEC-PROBE-008
    fn record_events(&mut self, events: &[ProbeEvent]) {
        for ev in events {
            if ev.phase == EventPhase::Open {
                self.events_opened += 1;
            }
            if let (Some(w), Some(path)) = (&self.writer, &self.events_path) {
                if let Some(line) = ev.to_jsonl() {
                    w.append(path.clone(), line);
                }
            }
            self.events.push_front(EventRow::from_event(ev));
        }
        while self.events.len() > EVENT_LOG_CAPACITY {
            self.events.pop_back();
        }
    }

    /// Contabiliza transições de disponibilidade.
    ///
    /// SPEC-PROBE-011 — "cabo removido por 30 s ⇒ 1 evento
    /// `feed_unavailable` com duração ≈ 30 s".
    fn track_availability(&mut self, connected: bool, now: Instant) {
        match (self.unavailable_since, connected) {
            (None, false) => {
                self.unavailable_since = Some(now);
                self.unavailable.periods += 1;
            }
            (Some(since), true) => {
                self.unavailable.total_secs += now.duration_since(since).as_secs();
                self.unavailable_since = None;
            }
            _ => {}
        }
    }

    /// Notifica o engine de que o pipeline foi reiniciado (reconexão).
    ///
    /// Os contadores do agregador voltam do zero; sem rebaseline, o primeiro
    /// tick depois da reconexão viraria uma rajada de erros inexistentes.
    ///
    /// SPEC-PROBE-011
    pub fn rebaseline(&mut self) {
        self.baseline.reset();
    }

    /// Estado corrente para a UI.
    ///
    /// SPEC-PROBE-018 · SPEC-PROBE-019
    pub fn snapshot(&self, window: SeriesWindow) -> FeedSnapshot {
        let mut applicable = vec![
            Layer::Ip,
            Layer::Ts,
            Layer::Video,
            Layer::Audio,
            Layer::Probe,
        ];
        if self.encapsulation.has_rtp() {
            applicable.push(Layer::Rtp);
        }

        let series = MetricId::ALL
            .iter()
            .map(|m| (*m, self.series.points(*m, window)))
            .collect();

        FeedSnapshot {
            slot: self.identity.slot,
            name: self.identity.name.clone(),
            url: self.identity.url.clone(),
            encapsulation: self.encapsulation,
            connected: self.connected,
            uptime_secs: self
                .clock
                .now_mono()
                .duration_since(self.started_mono)
                .as_secs(),
            availability_window: self.series.availability_window(AVAILABILITY_WINDOW_SECS),
            availability_session: self.series.availability(),
            bitrate_kbps: self.last_sample.as_ref().map_or(0.0, |s| s.bitrate_kbps),
            null_ratio: self.last_sample.as_ref().map_or(0.0, |s| s.null_ratio),
            video_kbps: self.presence.video_kbps,
            audio_kbps: self.presence.audio_kbps,
            video_height: self.presence.video_height,
            scrambled: self.presence.scrambled,
            layer_health: self.checks.layer_health(&applicable),
            worst_severity: self.checks.worst_open_severity(),
            open_events: self.checks.open_count(),
            timeline: self.series.timeline().to_vec(),
            series,
            events: self.events.iter().cloned().collect(),
            health: self.health,
            unavailable: self.unavailable,
            snapshot_state: self.snapshot_state,
            session_dir: self.session_dir.clone(),
        }
    }

    /// Séries do feed (usado pelo relatório).
    pub fn series(&self) -> &SeriesStore {
        &self.series
    }

    /// Identidade do feed.
    pub fn identity(&self) -> &FeedIdentity {
        &self.identity
    }

    /// Estágio de degradação corrente.
    ///
    /// SPEC-PROBE-013a — a thread de snapshot consulta isto para saber se deve
    /// pular o thumbnail (1º estágio).
    pub fn degradation(&self) -> DegradationStage {
        self.degrade.stage()
    }

    /// Encapsulamento detectado.
    ///
    /// SPEC-PROBE-018a
    pub fn encapsulation(&self) -> Encapsulation {
        self.encapsulation
    }

    /// Encerra a sessão: fecha os eventos abertos, faz flush e devolve o
    /// resumo gravado em `session.toml`.
    ///
    /// SPEC-PROBE-004 — "parar/fechar grava o resumo de cada uma".
    pub fn finish(&mut self) -> SessionSummary {
        let now = self.clock.now_mono();
        let now_utc = self.clock.now_utc();

        if let Some(since) = self.unavailable_since.take() {
            self.unavailable.total_secs += now.duration_since(since).as_secs();
        }

        let closing = self.checks.close_all(now, now_utc);
        self.record_events(&closing);

        if let Some(w) = &self.writer {
            for path in [self.metrics_path.clone(), self.events_path.clone()]
                .into_iter()
                .flatten()
            {
                w.send(WriteJob::Close { path });
            }
        }

        SessionSummary {
            ended_utc: Some(now_utc),
            duration_secs: now.duration_since(self.started_mono).as_secs(),
            samples: self.series.total_samples(),
            availability_pct: self.series.availability() * 100.0,
            events_opened: self.events_opened,
            worst_severity: self.worst_session_severity.map(|s| s.label().to_string()),
            unavailable_periods: self.unavailable.periods,
            unavailable_secs: self.unavailable.total_secs,
        }
    }

    /// Pasta desta sessão.
    pub fn session_dir(&self) -> Option<&Path> {
        self.session_dir.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use std::collections::HashMap;
    use std::time::Duration;
    use ts::metrics::{ErrorSnapshot, PidEntry, PidType, PipelineMetrics};

    fn metrics(bitrate: f64, cc: &[(u16, u64)], sync: u64) -> MetricsSnapshot {
        MetricsSnapshot {
            pid_table: vec![
                PidEntry {
                    pid: 100,
                    pid_type: PidType::Video {
                        codec: ts::metrics::VideoCodec::H264,
                    },
                    label: "video".into(),
                    bitrate_kbps: bitrate * 0.9,
                    cc_errors: 0,
                    packet_count: 0,
                },
                PidEntry {
                    pid: 101,
                    pid_type: PidType::Audio {
                        codec: ts::metrics::AudioCodec::Ac3,
                    },
                    label: "audio".into(),
                    bitrate_kbps: 192.0,
                    cc_errors: 0,
                    packet_count: 0,
                },
            ],
            total_bitrate_kbps: bitrate,
            null_ratio: 0.05,
            errors: ErrorSnapshot {
                cc_errors: cc.iter().copied().collect::<HashMap<_, _>>(),
                sync_losses: sync,
                ..Default::default()
            },
            tdt_offset_secs: None,
            timestamp: Instant::now(),
            av_sync_offset_ms: 0,
            late_frames_dropped: 0,
            early_frames_held: 0,
            pts_discontinuities: 0,
            video_queue_depth: 0,
            pipeline: PipelineMetrics::default(),
        }
    }

    fn engine(clock: Arc<TestClock>) -> ProbeEngine {
        ProbeEngine::new(
            FeedIdentity {
                slot: 0,
                name: "0084_CANAL_A".into(),
                url: "rtp://@239.15.0.183:50000".into(),
                fec: FecMode::Auto,
            },
            ProbeConfig::default(),
            clock,
            None,
            None,
        )
    }

    fn connected_input(m: MetricsSnapshot) -> TickInput {
        TickInput {
            metrics: Some(m),
            connected: true,
            encapsulation: Encapsulation::Rtp,
            snapshot_state: SnapshotState::Ok,
            ..Default::default()
        }
    }

    /// SPEC-PROBE-005 — o tick alimenta a série e a linha do tempo a 1 Hz.
    #[test]
    fn spec_probe_005_tick_feeds_series_and_timeline() {
        let clock = Arc::new(TestClock::new());
        let mut eng = engine(clock.clone());

        for _ in 0..120 {
            eng.tick(connected_input(metrics(15_000.0, &[], 0)));
            clock.advance(Duration::from_secs(1));
        }

        let snap = eng.snapshot(SeriesWindow::OneHour);
        assert_eq!(snap.timeline.len(), 1, "120 s cabem num bucket de 5 min");
        assert_eq!(snap.timeline[0].samples, 120);
        let bitrate = snap.series.get(&MetricId::BitrateKbps).expect("série");
        assert_eq!(bitrate.values.len(), 2, "120 s = 2 buckets de 60 s");
        assert!((bitrate.last - 15_000.0).abs() < 1.0);
    }

    /// SPEC-PROBE-008 — CC errors contínuos no mesmo PID viram um evento só.
    #[test]
    fn spec_probe_008_continuous_cc_errors_open_one_event() {
        let clock = Arc::new(TestClock::new());
        let mut eng = engine(clock.clone());

        let mut total = 0u64;
        let mut opens = 0usize;
        for i in 0..100 {
            total += 10;
            for ev in eng.tick(connected_input(metrics(15_000.0, &[(6100, total)], 0))) {
                if ev.phase == EventPhase::Open && ev.check_id == CHECK_CC_ERROR {
                    opens += 1;
                    assert_eq!(ev.context.pid, Some(6100));
                }
            }
            let _ = i;
            clock.advance(Duration::from_secs(1));
        }
        assert_eq!(opens, 1, "a rajada abre exatamente um evento");
        assert!(eng.snapshot(SeriesWindow::OneHour).open_events >= 1);
    }

    /// SPEC-PROBE-011 — 30 s sem feed geram um período de indisponibilidade
    /// com a duração correta e um evento `feed_unavailable`.
    #[test]
    fn spec_probe_011_outage_is_one_event_with_correct_duration() {
        let clock = Arc::new(TestClock::new());
        let mut eng = engine(clock.clone());

        for _ in 0..5 {
            eng.tick(connected_input(metrics(15_000.0, &[], 0)));
            clock.advance(Duration::from_secs(1));
        }

        let mut opens = 0usize;
        for _ in 0..30 {
            for ev in eng.tick(TickInput {
                connected: false,
                encapsulation: Encapsulation::Rtp,
                snapshot_state: SnapshotState::NoSignal,
                ..Default::default()
            }) {
                if ev.check_id == CHECK_FEED_UNAVAILABLE && ev.phase == EventPhase::Open {
                    opens += 1;
                }
            }
            clock.advance(Duration::from_secs(1));
        }
        assert_eq!(opens, 1, "a queda abre um único evento");

        // Retomada.
        eng.rebaseline();
        for _ in 0..5 {
            eng.tick(connected_input(metrics(15_000.0, &[], 0)));
            clock.advance(Duration::from_secs(1));
        }

        let snap = eng.snapshot(SeriesWindow::OneHour);
        assert_eq!(snap.unavailable.periods, 1);
        assert_eq!(snap.unavailable.total_secs, 30);
        assert!(snap.connected, "sessão retomada automaticamente");
        // 40 amostras, 30 sem conexão.
        assert!((snap.availability_session - 0.25).abs() < 0.01);
    }

    /// SPEC-PROBE-011 — a reconexão não vira rajada de erros só porque os
    /// contadores do pipeline voltaram do zero.
    #[test]
    fn spec_probe_011_reconnect_rebaseline_avoids_phantom_burst() {
        let clock = Arc::new(TestClock::new());
        let mut eng = engine(clock.clone());

        for _ in 0..3 {
            eng.tick(connected_input(metrics(15_000.0, &[(6100, 5_000)], 0)));
            clock.advance(Duration::from_secs(1));
        }
        eng.rebaseline();

        let evs = eng.tick(connected_input(metrics(15_000.0, &[(6100, 0)], 0)));
        assert!(
            !evs.iter().any(|e| e.check_id == CHECK_CC_ERROR),
            "rebaseline não pode inventar CC errors"
        );
    }

    /// SPEC-PROBE-013 — perda no mesmo segundo de descarte local é marcada
    /// `origin = local`.
    #[test]
    fn spec_probe_013_local_drop_reclassifies_origin() {
        let clock = Arc::new(TestClock::new());
        let mut eng = engine(clock.clone());

        eng.tick(connected_input(metrics(15_000.0, &[], 0)));
        clock.advance(Duration::from_secs(1));

        let evs = eng.tick(TickInput {
            local_drops_total: 12,
            ..connected_input(metrics(15_000.0, &[(6100, 7)], 0))
        });

        let cc = evs
            .iter()
            .find(|e| e.check_id == CHECK_CC_ERROR)
            .expect("CC error deve abrir");
        assert_eq!(cc.context.origin, EventOrigin::Local);

        let snap = eng.snapshot(SeriesWindow::OneHour);
        assert_eq!(snap.health.local_drops, 12);
        assert_eq!(snap.health.local_drops_last, 12);
    }

    /// SPEC-PROBE-013 — o jitter de agendamento do próprio tick é medido.
    #[test]
    fn spec_probe_013_scheduling_jitter_is_measured() {
        let clock = Arc::new(TestClock::new());
        let mut eng = engine(clock.clone());

        eng.tick(connected_input(metrics(15_000.0, &[], 0)));
        // Tick atrasado em 400 ms além do período de 1 s.
        clock.advance(Duration::from_millis(1_400));
        eng.tick(connected_input(metrics(15_000.0, &[], 0)));

        let snap = eng.snapshot(SeriesWindow::OneHour);
        assert!(
            (snap.health.sched_jitter_ms - 400.0).abs() < 1.0,
            "jitter medido: {}",
            snap.health.sched_jitter_ms
        );
        assert!(snap.health.sched_jitter_peak_ms >= 400.0);
    }

    /// SPEC-PROBE-018a — num feed UDP puro a camada RTP fica `n/a`.
    #[test]
    fn spec_probe_018a_udp_feed_marks_rtp_not_applicable() {
        let clock = Arc::new(TestClock::new());
        let mut eng = engine(clock.clone());

        eng.tick(TickInput {
            encapsulation: Encapsulation::Udp,
            ..connected_input(metrics(15_000.0, &[], 0))
        });

        let snap = eng.snapshot(SeriesWindow::OneHour);
        assert_eq!(snap.encapsulation, Encapsulation::Udp);
        assert_eq!(
            snap.layer_health.get(&Layer::Rtp),
            Some(&crate::severity::LayerHealth::NotApplicable)
        );
    }

    /// SPEC-PROBE-013a — sob sobrecarga sustentada a probe degrada até o 3º
    /// estágio, e aí as séries secundárias param — mas o bitrate e a linha do
    /// tempo, que respondem "o stream está bom?", continuam.
    #[test]
    fn spec_probe_013a_overload_degrades_secondary_series_only() {
        let clock = Arc::new(TestClock::new());
        let mut eng = engine(clock.clone());

        // Cada tick chega 1 s depois do agendado: jitter de 1000 ms, acima do
        // limite de 500 ms derivado do período de amostragem.
        let mut degraded_tick = None;
        for i in 0..12 {
            eng.tick(connected_input(metrics(15_000.0, &[], 0)));
            if degraded_tick.is_none() && eng.degradation() == DegradationStage::SecondarySeries {
                degraded_tick = Some(i);
            }
            clock.advance(Duration::from_secs(2));
        }

        assert_eq!(eng.degradation(), DegradationStage::SecondarySeries);
        assert!(
            degraded_tick.is_some_and(|i| i >= 9),
            "a degradação não pode ser instantânea: {degraded_tick:?}"
        );

        // Congelado: mais ticks não alimentam a série secundária…
        let cc_before: u32 = eng
            .series()
            .rollups(MetricId::CcErrorsPerS)
            .iter()
            .map(|b| b.count)
            .sum();
        let bitrate_before: u32 = eng
            .series()
            .rollups(MetricId::BitrateKbps)
            .iter()
            .map(|b| b.count)
            .sum();

        for _ in 0..5 {
            eng.tick(connected_input(metrics(15_000.0, &[], 0)));
            clock.advance(Duration::from_secs(2));
        }

        let cc_after: u32 = eng
            .series()
            .rollups(MetricId::CcErrorsPerS)
            .iter()
            .map(|b| b.count)
            .sum();
        let bitrate_after: u32 = eng
            .series()
            .rollups(MetricId::BitrateKbps)
            .iter()
            .map(|b| b.count)
            .sum();

        assert_eq!(cc_before, cc_after, "série secundária deve estar suspensa");
        assert!(
            bitrate_after > bitrate_before,
            "bitrate nunca degrada (SPEC-PROBE-013a)"
        );

        let snap = eng.snapshot(SeriesWindow::WholeSession);
        assert_eq!(snap.health.degradation, DegradationStage::SecondarySeries);
        assert!(
            snap.timeline.iter().map(|b| b.samples).sum::<u32>() >= 17,
            "a linha do tempo recebe amostra em todo tick"
        );
    }

    /// SPEC-PROBE-013b — a degradação abre um evento informativo, não é
    /// silenciosa.
    #[test]
    fn spec_probe_013b_degradation_opens_an_informational_event() {
        let clock = Arc::new(TestClock::new());
        let mut eng = engine(clock.clone());

        let mut degraded_event = None;
        for _ in 0..12 {
            for ev in eng.tick(connected_input(metrics(15_000.0, &[], 0))) {
                if ev.check_id == CHECK_DEGRADED && ev.phase == EventPhase::Open {
                    degraded_event = Some(ev);
                }
            }
            clock.advance(Duration::from_secs(2));
        }

        let ev = degraded_event.expect("degradação deve gerar evento");
        assert_eq!(ev.severity, Severity::Info, "informativo, não alarme");
        assert!(ev.measured >= DegradationStage::Thumbnail.stage_value());
    }

    /// SPEC-PROBE-004 — `finish` fecha os eventos abertos e resume a sessão.
    #[test]
    fn spec_probe_004_finish_closes_events_and_summarises() {
        let clock = Arc::new(TestClock::new());
        let mut eng = engine(clock.clone());

        for i in 1..=10u64 {
            eng.tick(connected_input(metrics(15_000.0, &[(6100, i * 3)], 0)));
            clock.advance(Duration::from_secs(1));
        }

        let summary = eng.finish();
        assert_eq!(summary.samples, 10);
        assert!(summary.events_opened >= 1);
        assert_eq!(summary.duration_secs, 10);
        assert_eq!(summary.worst_severity.as_deref(), Some("error"));
        assert!((summary.availability_pct - 100.0).abs() < 0.01);

        let snap = eng.snapshot(SeriesWindow::OneHour);
        assert_eq!(snap.open_events, 0, "nenhum evento fica aberto após finish");
        assert!(snap.events.iter().any(|e| e.phase == EventPhase::Close));
    }

    /// SPEC-PROBE-006 — com sessão em disco, o CSV nasce com cabeçalho e uma
    /// linha por tick.
    #[test]
    fn spec_probe_006_writes_one_csv_row_per_tick() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (handle, writer) = crate::writer::writer_channel(Duration::from_millis(50));
        let t = std::thread::spawn(move || writer.run());

        let clock = Arc::new(TestClock::new());
        let mut eng = ProbeEngine::new(
            FeedIdentity {
                slot: 0,
                name: "f".into(),
                url: "udp://@239.0.0.1:1234".into(),
                fec: FecMode::Off,
            },
            ProbeConfig::default(),
            clock.clone(),
            Some(dir.path().to_path_buf()),
            Some(handle.clone()),
        );

        for _ in 0..5 {
            eng.tick(connected_input(metrics(15_000.0, &[], 0)));
            clock.advance(Duration::from_secs(1));
        }
        eng.finish();
        // O engine guarda um clone do `WriterHandle`; sem dropá-lo, o canal
        // nunca desconecta e o `join` abaixo trava para sempre.
        drop(eng);
        drop(handle);
        t.join().expect("writer encerra");

        let csv = std::fs::read_to_string(dir.path().join(METRICS_FILE)).expect("lê CSV");
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], CSV_HEADER);
        assert_eq!(lines.len(), 6, "cabeçalho + 5 amostras");
        assert!(lines[1].split(',').count() == CSV_HEADER.split(',').count());

        assert!(dir.path().join(EVENTS_FILE).exists());
    }
}
