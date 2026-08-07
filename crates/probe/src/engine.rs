//! `ProbeEngine` — o tick de 1 Hz de um feed.
//!
//! Amostra os contadores acumulados, avalia os checks, atualiza séries e
//! linha do tempo e **enfileira** a escrita.  O engine nunca faz I/O de disco
//! no próprio tick: escrita lenta (disco de notebook, antivírus) não pode
//! atrasar a amostragem (§5.4).
//!
//! SPEC-PROBE-005 · SPEC-PROBE-006 · SPEC-PROBE-007 · SPEC-PROBE-013

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use ts::metrics::MetricsSnapshot;
use ts::Pid;

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
use crate::series::{HealthScope, HealthTimeline, MetricId, SeriesStore, SeriesWindow};
use crate::service::{ServiceInfo, ServiceVisual, StreamKind};
use crate::session::{Encapsulation, SessionSummary, EVENTS_FILE, METRICS_FILE};
use crate::severity::{Layer, LayerHealth, Severity};
use crate::snapshot::{
    DegradationStage, EventRow, FeedSnapshot, ProbeHealth, ServiceSnapshot, SnapshotState,
    StreamSnapshot, UnavailableStats, EVENT_LOG_CAPACITY,
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
    /// Inventário de serviços vindo de PAT/PMT/SDT; vazio = PSI ainda não
    /// chegou.  Não confundir com "multiplex sem serviços" — enquanto está
    /// vazio, o motor mede só o feed inteiro (SPEC-PROBE-021).
    pub services: Vec<ServiceInfo>,
    /// Último resultado do thumbnail por serviço (SPEC-PROBE-024).
    pub visuals: BTreeMap<u16, ServiceVisual>,
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
    /// Uma linha do tempo por escopo da grade (§8.3): camada, serviço, PID.
    ///
    /// SPEC-PROBE-023
    scopes: BTreeMap<HealthScope, HealthTimeline>,
    /// Agregados por serviço do último tick, sem as linhas do tempo.
    services: Vec<ServiceAggregate>,
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

/// Agregado de um serviço no último tick, **sem** as linhas do tempo.
///
/// As linhas vivem em `ProbeEngine::scopes` e são anexadas em
/// [`ProbeEngine::snapshot`]: guardá-las aqui significaria duplicar a mesma
/// história em dois lugares e mantê-las em sincronia à mão.
///
/// SPEC-PROBE-021
#[derive(Debug, Clone, Default)]
struct ServiceAggregate {
    info: ServiceInfo,
    bitrate_kbps: f64,
    video_kbps: f64,
    audio_kbps: f64,
    visual: ServiceVisual,
    layer_health: BTreeMap<Layer, LayerHealth>,
    worst: Option<Severity>,
    open_events: usize,
    streams: Vec<StreamAggregate>,
}

/// Agregado de um PID elementar no último tick.
#[derive(Debug, Clone, Default)]
struct StreamAggregate {
    pid: Pid,
    kind: StreamKind,
    codec: String,
    language: Option<String>,
    bitrate_kbps: f64,
    cc_errors: u64,
    worst: Option<Severity>,
    open_events: usize,
}

/// Mantém a pior severidade de uma chave.
fn worsen<K: Ord>(map: &mut BTreeMap<K, Severity>, key: K, sev: Severity) {
    map.entry(key)
        .and_modify(|cur| *cur = (*cur).max(sev))
        .or_insert(sev);
}

/// Serviço que representa o feed no mosaico: o primeiro com vídeo, ou o
/// primeiro da PAT.
///
/// SPEC-PROBE-024
fn primary_service(services: &[ServiceInfo]) -> Option<&ServiceInfo> {
    services
        .iter()
        .find(|s| s.primary_video_pid().is_some())
        .or_else(|| services.first())
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
            scopes: BTreeMap::new(),
            services: Vec::new(),
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
        // SPEC-PROBE-024 — o tile do feed mostra um quadro só, e num MPTS ele é
        // o do serviço primário; o estado do thumbnail acompanha o mesmo
        // serviço, senão o rótulo diria "sem keyframe" sobre a imagem de outro.
        let primary_visual = primary_service(&input.services)
            .and_then(|info| input.visuals.get(&info.service_id).copied());
        self.snapshot_state = primary_visual.map_or(input.snapshot_state, |v| v.state);
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
            presence.video_height = primary_visual
                .and_then(|v| v.video_height)
                .or(input.video_height)
                .or(self.presence.video_height);
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
        // SPEC-PROBE-021 — toda ocorrência com PID conhecido também carrega o
        // serviço dono dele.  Sem isso a grade de saúde e o mosaico de serviços
        // não conseguiriam distinguir "o multiplex está ruim" de "**este**
        // serviço está ruim", que é a pergunta que o operador faz.
        let pid_ctx = |pid: Pid| {
            let ctx = EventContext::pid(pid).with_origin(origin);
            match crate::service::owner_of(&input.services, pid) {
                Some(sid) => ctx.with_service(sid),
                None => ctx,
            }
        };

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
                    context: pid_ctx(*pid),
                    value: *count as f64,
                    occurrences: *count,
                });
            }
            for (pid, count) in &deltas.crc_by_pid {
                measurements.push(Measurement {
                    check_id: CHECK_CRC_ERROR,
                    context: pid_ctx(*pid),
                    value: *count as f64,
                    occurrences: *count,
                });
            }
            for (pid, count) in &deltas.pcr_jitter_by_pid {
                measurements.push(Measurement {
                    check_id: CHECK_PCR_ERROR,
                    context: pid_ctx(*pid),
                    value: *count as f64,
                    occurrences: *count,
                });
            }
            for (pid, count) in &deltas.pcr_disc_by_pid {
                measurements.push(Measurement {
                    check_id: CHECK_PCR_DISCONTINUITY,
                    context: pid_ctx(*pid),
                    value: *count as f64,
                    occurrences: *count,
                });
            }
            measurements.push(
                Measurement::counter(CHECK_TS_SYNC_LOSS, deltas.sync_loss as f64)
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

        // ── Saúde por escopo e agregados por serviço (§8.3) ──────────────
        self.refresh_services(&input, now_utc);

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

    /// Recalcula os agregados por serviço e alimenta a linha do tempo de cada
    /// escopo da grade de saúde.
    ///
    /// Roda **depois** de `evaluate`: a severidade de uma célula é a dos
    /// eventos que estão abertos no fim do tick, já passados por debounce.
    /// Contar a violação crua faria a grade acender em rajadas de um segundo
    /// que o motor de checks deliberadamente ignora.
    ///
    /// SPEC-PROBE-021 · SPEC-PROBE-022 · SPEC-PROBE-023
    fn refresh_services(&mut self, input: &TickInput, now_utc: DateTime<Utc>) {
        let open = self.checks.open_checks();
        let connected = input.connected;

        // Bitrate e CC por PID saem da tabela do aggregator; sem métrica no
        // tick, os valores anteriores são preservados em vez de zerar o tile.
        let mut pid_bitrate: HashMap<Pid, f64> = HashMap::new();
        let mut pid_cc: HashMap<Pid, u64> = HashMap::new();
        if let Some(m) = &input.metrics {
            for entry in &m.pid_table {
                pid_bitrate.insert(entry.pid, entry.bitrate_kbps);
                pid_cc.insert(entry.pid, entry.cc_errors);
            }
        }

        // Severidade por camada e por PID, uma varredura só.
        let mut layer_worst: BTreeMap<Layer, Severity> = BTreeMap::new();
        let mut pid_worst: BTreeMap<Pid, Severity> = BTreeMap::new();
        let mut pid_open: BTreeMap<Pid, usize> = BTreeMap::new();
        // Evento crítico sem PID nem serviço (feed fora do ar, sync loss):
        // atinge **todo** serviço do multiplex, e deixar a linha do serviço
        // verde enquanto a do transporte está vermelha seria mentir.
        let mut feed_wide: Option<Severity> = None;
        for o in &open {
            worsen(&mut layer_worst, o.layer, o.severity);
            if let Some(pid) = o.context.pid {
                worsen(&mut pid_worst, pid, o.severity);
                *pid_open.entry(pid).or_insert(0) += 1;
            }
            if o.context.pid.is_none()
                && o.context.service_id.is_none()
                && o.layer != Layer::Probe
                && o.severity == Severity::Critical
            {
                feed_wide = Some(feed_wide.map_or(o.severity, |c: Severity| c.max(o.severity)));
            }
        }

        // A camada RTP entra na linha `IP` da grade: são a mesma pergunta
        // ("a rede entregou?") e uma linha só evita uma faixa quase sempre
        // cinza nos feeds UDP puro.
        let ip_worst = layer_worst
            .get(&Layer::Ip)
            .copied()
            .into_iter()
            .chain(layer_worst.get(&Layer::Rtp).copied())
            .max();
        self.push_scope(
            HealthScope::Layer(Layer::Ip),
            now_utc,
            ip_worst,
            connected,
        );
        self.push_scope(
            HealthScope::Layer(Layer::Ts),
            now_utc,
            layer_worst.get(&Layer::Ts).copied(),
            connected,
        );

        let mut services = Vec::with_capacity(input.services.len());
        for info in &input.services {
            let owned: Vec<&crate::check::OpenCheck> =
                open.iter().filter(|o| info.owns(&o.context)).collect();
            let worst = owned
                .iter()
                .map(|o| o.severity)
                .chain(feed_wide)
                .max();

            let applicable = Self::service_layers(info);
            let layer_health = self
                .checks
                .layer_health_where(&applicable, |ctx| info.owns(ctx));

            let mut streams = Vec::with_capacity(info.streams.len());
            let (mut video_kbps, mut audio_kbps, mut bitrate_kbps) = (0.0, 0.0, 0.0);
            for s in &info.streams {
                let kbps = pid_bitrate.get(&s.pid).copied().unwrap_or(0.0);
                bitrate_kbps += kbps;
                match s.kind {
                    StreamKind::Video => video_kbps += kbps,
                    StreamKind::Audio => audio_kbps += kbps,
                    _ => {}
                }
                let worst_pid = pid_worst.get(&s.pid).copied();
                self.push_scope(HealthScope::Pid(s.pid), now_utc, worst_pid, connected);
                streams.push(StreamAggregate {
                    pid: s.pid,
                    kind: s.kind,
                    codec: s.codec.clone(),
                    language: s.language.clone(),
                    bitrate_kbps: kbps,
                    cc_errors: pid_cc.get(&s.pid).copied().unwrap_or(0),
                    worst: worst_pid,
                    open_events: pid_open.get(&s.pid).copied().unwrap_or(0),
                });
            }

            self.push_scope(
                HealthScope::Service(info.service_id),
                now_utc,
                worst,
                connected,
            );

            services.push(ServiceAggregate {
                info: info.clone(),
                bitrate_kbps,
                video_kbps,
                audio_kbps,
                visual: input
                    .visuals
                    .get(&info.service_id)
                    .copied()
                    .unwrap_or_default(),
                layer_health,
                worst,
                open_events: owned.len(),
                streams,
            });
        }

        // Serviço ou PID que saiu da PSI perde a linha: manter história de algo
        // que não existe mais faria a grade crescer para sempre num multiplex
        // que muda de grade de programação.
        self.scopes.retain(|scope, _| match scope {
            HealthScope::Layer(_) => true,
            HealthScope::Service(id) => input.services.iter().any(|s| s.service_id == *id),
            HealthScope::Pid(pid) => input.services.iter().any(|s| s.contains_pid(*pid)),
        });
        self.services = services;
    }

    /// Camadas avaliadas para um serviço.
    ///
    /// `V` e `A` só ficam verdes quando o serviço **tem** aquele tipo de
    /// stream; um rádio sem vídeo mostra `V` cinza (`n/a`), nunca verde
    /// (SPEC-PROBE-018a).
    fn service_layers(info: &ServiceInfo) -> Vec<Layer> {
        let mut layers = vec![Layer::Ts];
        if info.streams.iter().any(|s| s.kind == StreamKind::Video) {
            layers.push(Layer::Video);
        }
        if info.streams.iter().any(|s| s.kind == StreamKind::Audio) {
            layers.push(Layer::Audio);
        }
        layers
    }

    /// Ingere uma amostra de saúde numa linha do tempo de escopo.
    fn push_scope(
        &mut self,
        scope: HealthScope,
        ts: DateTime<Utc>,
        worst: Option<Severity>,
        connected: bool,
    ) {
        let bucket = self.series.timeline_secs();
        self.scopes
            .entry(scope)
            .or_insert_with(|| HealthTimeline::new(bucket))
            .push(ts, worst, connected);
    }

    /// Células de um escopo, ou vazio se ele nunca recebeu amostra.
    fn scope_cells(&self, scope: HealthScope) -> Vec<crate::series::TimelineBucket> {
        self.scopes
            .get(&scope)
            .map(|t| t.buckets().to_vec())
            .unwrap_or_default()
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

        let availability_window = self.series.availability_window(AVAILABILITY_WINDOW_SECS);
        let services: Vec<ServiceSnapshot> = self
            .services
            .iter()
            .map(|a| ServiceSnapshot {
                service_id: a.info.service_id,
                name: a.info.display_name(),
                provider: a.info.provider.clone(),
                pmt_pid: a.info.pmt_pid,
                pcr_pid: a.info.pcr_pid,
                scrambled: a.info.scrambled || self.presence.scrambled,
                bitrate_kbps: a.bitrate_kbps,
                video_kbps: a.video_kbps,
                audio_kbps: a.audio_kbps,
                video_height: a.visual.video_height,
                layer_health: a.layer_health.clone(),
                worst_severity: a.worst,
                open_events: a.open_events,
                availability_window,
                timeline: self.scope_cells(HealthScope::Service(a.info.service_id)),
                streams: a
                    .streams
                    .iter()
                    .map(|s| StreamSnapshot {
                        pid: s.pid,
                        kind: s.kind,
                        codec: s.codec.clone(),
                        language: s.language.clone(),
                        bitrate_kbps: s.bitrate_kbps,
                        cc_errors: s.cc_errors,
                        worst_severity: s.worst,
                        open_events: s.open_events,
                        timeline: self.scope_cells(HealthScope::Pid(s.pid)),
                    })
                    .collect(),
                snapshot_state: a.visual.state,
            })
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
            availability_window,
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
            timeline_bucket_secs: self.series.timeline_secs(),
            ip_timeline: self.scope_cells(HealthScope::Layer(Layer::Ip)),
            ts_timeline: self.scope_cells(HealthScope::Layer(Layer::Ts)),
            services,
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

    /// Dois serviços num MPTS: 100 leva vídeo 6100 + áudio 6101; 200 leva
    /// vídeo 6200.
    fn inventory() -> Vec<ServiceInfo> {
        use crate::service::ServiceStream;
        let stream = |pid: Pid, kind: StreamKind, codec: &str| ServiceStream {
            pid,
            stream_type: 0,
            kind,
            codec: codec.into(),
            language: None,
        };
        vec![
            ServiceInfo {
                service_id: 100,
                name: Some("CANAL_A".into()),
                pmt_pid: 0x1000,
                pcr_pid: 6100,
                streams: vec![
                    stream(6100, StreamKind::Video, "H.264 Video"),
                    stream(6101, StreamKind::Audio, "AC-3 Audio"),
                ],
                ..Default::default()
            },
            ServiceInfo {
                service_id: 200,
                name: Some("CANAL_B".into()),
                pmt_pid: 0x1001,
                pcr_pid: 6200,
                streams: vec![stream(6200, StreamKind::Video, "H.264 Video")],
                ..Default::default()
            },
        ]
    }

    /// SPEC-PROBE-021 — um CC error no PID de um serviço acende **aquele**
    /// serviço, não o multiplex inteiro: é a diferença entre "tem erro em algum
    /// lugar" e um diagnóstico.
    #[test]
    fn spec_probe_021_errors_land_on_the_owning_service_only() {
        let clock = Arc::new(TestClock::new());
        let mut eng = engine(clock.clone());

        let tick = |total: u64| TickInput {
            services: inventory(),
            ..connected_input(metrics(15_000.0, &[(6100, total)], 0))
        };

        eng.tick(tick(0));
        clock.advance(Duration::from_secs(1));
        let events = eng.tick(tick(40));

        let cc = events
            .iter()
            .find(|e| e.check_id == CHECK_CC_ERROR)
            .expect("CC error deve abrir");
        assert_eq!(cc.context.pid, Some(6100));
        assert_eq!(
            cc.context.service_id,
            Some(100),
            "o evento precisa saber de quem é o PID"
        );

        let snap = eng.snapshot(SeriesWindow::WholeSession);
        assert_eq!(snap.services.len(), 2);

        let a = snap.service(100).expect("serviço 100");
        assert_eq!(a.worst_severity, Some(Severity::Error));
        assert_eq!(a.open_events, 1);
        assert_eq!(
            a.layer_health.get(&Layer::Ts),
            Some(&crate::severity::LayerHealth::Degraded(Severity::Error))
        );

        let b = snap.service(200).expect("serviço 200");
        assert_eq!(
            b.worst_severity, None,
            "erro no PID do vizinho não pode sujar este serviço"
        );
        assert_eq!(b.open_events, 0);
        assert_eq!(
            b.layer_health.get(&Layer::Ts),
            Some(&crate::severity::LayerHealth::Ok)
        );
    }

    /// SPEC-PROBE-023 — a grade tem uma linha por camada, por serviço e por
    /// PID, todas no mesmo eixo de tempo.
    #[test]
    fn spec_probe_023_health_grid_has_a_row_per_scope() {
        let clock = Arc::new(TestClock::new());
        let mut eng = engine(clock.clone());

        let mut total = 0u64;
        for _ in 0..10 {
            total += 5;
            eng.tick(TickInput {
                services: inventory(),
                ..connected_input(metrics(15_000.0, &[(6101, total)], 0))
            });
            clock.advance(Duration::from_secs(1));
        }

        let snap = eng.snapshot(SeriesWindow::WholeSession);
        assert_eq!(snap.ts_timeline.len(), 1, "10 s cabem num bucket de 5 min");
        assert_eq!(snap.ip_timeline.len(), 1);
        assert_eq!(
            snap.ts_timeline[0].worst,
            Some(Severity::Error),
            "a linha do transporte acende com o CC error"
        );
        assert_eq!(
            snap.ip_timeline[0].worst, None,
            "a rede entregou; a linha IP fica verde"
        );

        let a = snap.service(100).expect("serviço 100");
        assert_eq!(a.timeline.len(), 1);
        assert_eq!(a.timeline[0].worst, Some(Severity::Error));
        assert_eq!(a.streams.len(), 2);

        let audio = a
            .streams
            .iter()
            .find(|s| s.pid == 6101)
            .expect("PID de áudio");
        assert_eq!(audio.timeline[0].worst, Some(Severity::Error));
        assert_eq!(audio.describe(), "AC-3 Audio (6101)");

        let video = a
            .streams
            .iter()
            .find(|s| s.pid == 6100)
            .expect("PID de vídeo");
        assert_eq!(
            video.timeline[0].worst, None,
            "o PID sem erro fica verde mesmo com o vizinho quebrado"
        );

        let b = snap.service(200).expect("serviço 200");
        assert_eq!(b.timeline[0].worst, None);
    }

    /// SPEC-PROBE-021 — feed fora do ar é crítico para **todo** serviço: linha
    /// de serviço verde com transporte vermelho seria mentira.
    #[test]
    fn spec_probe_021_feed_outage_reaches_every_service() {
        let clock = Arc::new(TestClock::new());
        let mut eng = engine(clock.clone());

        eng.tick(TickInput {
            services: inventory(),
            ..connected_input(metrics(15_000.0, &[], 0))
        });
        clock.advance(Duration::from_secs(1));
        eng.tick(TickInput {
            connected: false,
            encapsulation: Encapsulation::Rtp,
            snapshot_state: SnapshotState::NoSignal,
            services: inventory(),
            ..Default::default()
        });

        let snap = eng.snapshot(SeriesWindow::WholeSession);
        for svc in &snap.services {
            assert_eq!(
                svc.worst_severity,
                Some(Severity::Critical),
                "serviço {} deveria refletir a queda do feed",
                svc.service_id
            );
        }
    }

    /// SPEC-PROBE-022 — um serviço só de áudio mostra `V` como `n/a`, nunca
    /// verde: afirmar que o vídeo está bom quando não existe vídeo é pior do
    /// que não medir (SPEC-PROBE-018a).
    #[test]
    fn spec_probe_022_radio_service_marks_video_not_applicable() {
        use crate::service::ServiceStream;
        let clock = Arc::new(TestClock::new());
        let mut eng = engine(clock.clone());

        eng.tick(TickInput {
            services: vec![ServiceInfo {
                service_id: 7,
                pmt_pid: 0x1002,
                streams: vec![ServiceStream {
                    pid: 700,
                    kind: StreamKind::Audio,
                    codec: "MPEG-1 Audio".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..connected_input(metrics(15_000.0, &[], 0))
        });

        let snap = eng.snapshot(SeriesWindow::WholeSession);
        let radio = snap.service(7).expect("serviço 7");
        assert_eq!(
            radio.layer_health.get(&Layer::Video),
            Some(&crate::severity::LayerHealth::NotApplicable)
        );
        assert_eq!(
            radio.layer_health.get(&Layer::Audio),
            Some(&crate::severity::LayerHealth::Ok)
        );
        assert_eq!(radio.display_name(), "Serviço 7");
    }

    /// SPEC-PROBE-021 — serviço removido da PSI perde a linha da grade em vez
    /// de acumular história de algo que não existe mais.
    #[test]
    fn spec_probe_021_service_removed_from_psi_drops_its_scope() {
        let clock = Arc::new(TestClock::new());
        let mut eng = engine(clock.clone());

        eng.tick(TickInput {
            services: inventory(),
            ..connected_input(metrics(15_000.0, &[], 0))
        });
        clock.advance(Duration::from_secs(1));
        assert_eq!(eng.snapshot(SeriesWindow::WholeSession).services.len(), 2);

        // Nova PAT sem o serviço 200.
        eng.tick(TickInput {
            services: vec![inventory().remove(0)],
            ..connected_input(metrics(15_000.0, &[], 0))
        });

        let snap = eng.snapshot(SeriesWindow::WholeSession);
        assert_eq!(snap.services.len(), 1);
        assert!(snap.service(200).is_none());
        assert!(
            !eng.scopes.contains_key(&HealthScope::Service(200)),
            "o escopo do serviço removido precisa sair do mapa"
        );
        assert!(!eng.scopes.contains_key(&HealthScope::Pid(6200)));
        assert!(eng.scopes.contains_key(&HealthScope::Pid(6100)));
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
