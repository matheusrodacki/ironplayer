//! Estado publicado a 1 Hz para a UI do modo Probe.
//!
//! §5.3 — `ProbeEngine --ProbeSnapshot 1 Hz--> ui-slint`.  Tudo aqui é dado
//! **derivado**: a UI nunca alcança as estruturas internas do engine, e a
//! repintura dos painéis fica em ≤ 1 Hz (§8.2).
//!
//! SPEC-PROBE-009 · SPEC-PROBE-010 · SPEC-PROBE-013 · SPEC-PROBE-018

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::check::{
    CHECK_AUDIO_MISSING, CHECK_CC_ERROR, CHECK_CRC_ERROR, CHECK_DEGRADED, CHECK_FEED_UNAVAILABLE,
    CHECK_LOCAL_DROPS, CHECK_PCR_DISCONTINUITY, CHECK_PCR_ERROR, CHECK_RTP_OUT_OF_ORDER,
    CHECK_SCHED_JITTER, CHECK_TS_SYNC_LOSS, CHECK_VIDEO_MISSING,
};
use crate::event::{EventContext, EventOrigin, EventPhase, ProbeEvent};
use crate::series::{MetricId, SeriesPoints, TimelineBucket};
use crate::service::StreamKind;
use crate::session::Encapsulation;
use crate::severity::{Layer, LayerHealth, Severity};

/// Quantos eventos ficam no log em memória por feed.
///
/// 12 h de sessão num stream ruim produzem muito mais do que isso; o
/// `events.jsonl` continua completo em disco, e o log da UI é só a janela
/// recente (§8.2).
pub const EVENT_LOG_CAPACITY: usize = 2_000;

/// Estágio de degradação acionado sob sobrecarga.
///
/// SPEC-PROBE-013a — a ordem de degradação é 1) thumbnail, 2) rollups de UI,
/// 3) séries secundárias.  A recepção UDP e a contagem de perda/CC **nunca**
/// são as primeiras a degradar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum DegradationStage {
    #[default]
    None,
    /// 1º estágio: para de decodificar thumbnails.
    Thumbnail,
    /// 2º estágio: para de recalcular rollups para a UI.
    Rollups,
    /// 3º estágio: para de alimentar as séries secundárias.
    SecondarySeries,
}

impl DegradationStage {
    /// Rótulo usado no evento `probe_degraded` e no painel de saúde.
    ///
    /// SPEC-PROBE-013b
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "normal",
            Self::Thumbnail => "thumbnail suspenso",
            Self::Rollups => "rollups de UI suspensos",
            Self::SecondarySeries => "séries secundárias suspensas",
        }
    }

    /// Valor numérico levado ao check `probe_degraded`.
    ///
    /// SPEC-PROBE-013b
    pub fn stage_value(self) -> f64 {
        match self {
            Self::None => 0.0,
            Self::Thumbnail => 1.0,
            Self::Rollups => 2.0,
            Self::SecondarySeries => 3.0,
        }
    }
}

/// Resultado do último tick de snapshot de vídeo.
///
/// SPEC-PROBE-003a — "Sem IRAP na janela → `snapshot_state = "sem keyframe"`;
/// não gera alarme por si só".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnapshotState {
    /// Ainda não houve tick.
    #[default]
    Pending,
    /// Um frame foi decodificado e publicado.
    Ok,
    /// Nenhum IRAP na janela de armação.
    NoKeyframe,
    /// Snapshot suspenso por degradação (SPEC-PROBE-013a).
    Suspended,
    /// Feed sem sinal — não há o que decodificar.
    NoSignal,
}

impl SnapshotState {
    /// Rótulo exibido sob o thumbnail.
    ///
    /// SPEC-PROBE-003a
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "aguardando",
            Self::Ok => "ok",
            Self::NoKeyframe => "sem keyframe",
            Self::Suspended => "suspenso",
            Self::NoSignal => "sem sinal",
        }
    }
}

/// Autodiagnóstico da probe.
///
/// SPEC-PROBE-013 — painel "Saúde da probe".
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProbeHealth {
    /// Descartes locais acumulados na sessão (canal cheio, socket, writer).
    pub local_drops: u64,
    /// Descartes locais no último segundo.
    pub local_drops_last: u64,
    /// Atraso do último tick em relação ao agendado, em ms.
    pub sched_jitter_ms: f64,
    /// Pico de atraso de tick observado na sessão, em ms.
    pub sched_jitter_peak_ms: f64,
    /// Eventos discretos descartados por fila cheia (§5.3).
    pub dropped_events: u64,
    /// Linhas descartadas pelo writer (RNF-PRB-004).
    pub writer_drops: u64,
    /// Estágio de degradação corrente.
    pub degradation: DegradationStage,
}

/// Uma linha do event log da UI.
///
/// §8.2 · SPEC-PROBE-008
#[derive(Debug, Clone, PartialEq)]
pub struct EventRow {
    pub event_id: String,
    pub ts_utc: DateTime<Utc>,
    pub severity: Severity,
    pub check_id: String,
    pub phase: EventPhase,
    pub count: u64,
    pub measured: f64,
    pub unit: String,
    /// Contexto legível ("pid 6100 · origin=local").
    pub context: String,
    /// PID a que a ocorrência foi atribuída, quando há um.
    pub pid: Option<u16>,
    /// Serviço a que a ocorrência foi atribuída, quando há um.
    pub service_id: Option<u16>,
    /// `true` quando a ocorrência foi atribuída à própria probe.
    pub local: bool,
}

impl EventRow {
    /// Converte um evento persistido em linha de UI.
    pub fn from_event(ev: &ProbeEvent) -> Self {
        Self {
            event_id: ev.event_id.clone(),
            ts_utc: ev.ts_utc,
            severity: ev.severity,
            check_id: ev.check_id.clone(),
            phase: ev.phase,
            count: ev.count,
            measured: ev.measured,
            unit: ev.unit.clone(),
            context: ev.context.describe(),
            pid: ev.context.pid,
            service_id: ev.context.service_id,
            local: ev.context.origin == EventOrigin::Local,
        }
    }

    /// Descrição do problema em uma frase, para a lista consolidada de alertas.
    ///
    /// O event log compacto mostra `check_id` cru, que só é legível para quem
    /// conhece o perfil.  A lista de alertas de uma janela é o artefato que o
    /// operador manda para o fornecedor do sinal — ali o texto precisa se
    /// explicar sozinho, com a referência normativa quando ela existe.
    ///
    /// SPEC-PROBE-025
    pub fn describe(&self) -> String {
        let n = self.count;
        let pid = self
            .pid
            .map(|p| format!(" no PID {p}"))
            .unwrap_or_default();
        let mut text = match self.check_id.as_str() {
            CHECK_CC_ERROR => format!(
                "TR 101 290 P1.4 Continuity Counter Error: {n} descontinuidade(s) de \
                 continuity_counter{pid}."
            ),
            CHECK_CRC_ERROR => format!(
                "TR 101 290 P2.2 CRC Error: {n} seção(ões) PSI/SI com CRC-32 inválido{pid}."
            ),
            CHECK_PCR_ERROR => format!(
                "TR 101 290 P2.3 PCR Accuracy: {n} evento(s) de jitter de PCR acima do \
                 limiar do perfil{pid}."
            ),
            CHECK_PCR_DISCONTINUITY => format!(
                "TR 101 290 P1.5 PCR Discontinuity: {n} salto(s) de PCR sem \
                 discontinuity_indicator{pid}."
            ),
            CHECK_TS_SYNC_LOSS => format!(
                "TR 101 290 P1.1 TS sync loss: {n} perda(s) de sincronismo do transport stream."
            ),
            CHECK_FEED_UNAVAILABLE => {
                "Feed indisponível: nenhum datagrama recebido na janela de detecção.".to_string()
            }
            CHECK_RTP_OUT_OF_ORDER => format!(
                "RTP: {n} pacote(s) fora de ordem ou faltando na sequência."
            ),
            CHECK_VIDEO_MISSING => format!(
                "Vídeo ausente: bitrate do PID de vídeo abaixo do limiar ({:.1} kbps){pid}.",
                self.measured
            ),
            CHECK_AUDIO_MISSING => format!(
                "Áudio ausente: bitrate do PID de áudio abaixo do limiar ({:.1} kbps){pid}.",
                self.measured
            ),
            CHECK_LOCAL_DROPS => format!(
                "Descarte local da probe: {n} amostra(s) perdida(s) por canal cheio ou \
                 buffer de socket — não é perda de rede."
            ),
            CHECK_SCHED_JITTER => format!(
                "Jitter de agendamento da probe: tick atrasado {:.1} ms — a medição do \
                 segundo pode estar comprimida.",
                self.measured
            ),
            CHECK_DEGRADED => {
                "Probe degradada sob sobrecarga: recursos secundários suspensos para \
                 preservar recepção e contagem de erros."
                    .to_string()
            }
            other => format!(
                "{other}: {n} ocorrência(s), medido {:.3} {}{pid}.",
                self.measured, self.unit
            ),
        };
        if self.local && self.check_id != CHECK_LOCAL_DROPS {
            text.push_str(" Ocorreu no mesmo segundo de um descarte local — atribuído à probe.");
        }
        text
    }
}

/// Estatísticas de indisponibilidade do feed.
///
/// SPEC-PROBE-011
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UnavailableStats {
    /// Quantas vezes o feed ficou indisponível na sessão.
    pub periods: u64,
    /// Tempo total indisponível, em segundos.
    pub total_secs: u64,
    /// Tentativas de reconexão feitas desde a última queda.
    pub reconnect_attempts: u32,
}

/// Um elementary stream de um serviço, publicado para a UI.
///
/// Vira uma linha da grade de saúde do §8.3 e uma linha da tabela de PIDs do
/// detalhe do serviço.
///
/// SPEC-PROBE-023
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamSnapshot {
    pub pid: u16,
    pub kind: StreamKind,
    /// Rótulo do codec vindo da PMT.
    pub codec: String,
    pub language: Option<String>,
    pub bitrate_kbps: f64,
    /// CC errors acumulados neste PID na sessão.
    pub cc_errors: u64,
    pub worst_severity: Option<Severity>,
    pub open_events: usize,
    /// Linha do tempo de saúde **deste PID**.
    pub timeline: Vec<TimelineBucket>,
}

impl StreamSnapshot {
    /// Rótulo da linha na grade: `H.264 Video · por (6100)`.
    ///
    /// SPEC-PROBE-023
    pub fn describe(&self) -> String {
        let mut label = if self.codec.trim().is_empty() {
            self.kind.label().to_string()
        } else {
            self.codec.clone()
        };
        if let Some(lang) = &self.language {
            label.push_str(" · ");
            label.push_str(lang);
        }
        format!("{label} ({})", self.pid)
    }
}

/// Estado de um serviço do multiplex, publicado para a UI.
///
/// SPEC-PROBE-021 · SPEC-PROBE-022
#[derive(Debug, Clone, Default)]
pub struct ServiceSnapshot {
    pub service_id: u16,
    pub name: String,
    pub provider: Option<String>,
    pub pmt_pid: u16,
    pub pcr_pid: u16,
    /// `free_CA_mode` da SDT — badge `SCR` do tile de serviço.
    pub scrambled: bool,
    /// Soma dos bitrates dos PIDs do serviço, em kbps.
    pub bitrate_kbps: f64,
    pub video_kbps: f64,
    /// Presença de áudio, em kbps — **não** é nível de áudio (§8.1).
    pub audio_kbps: f64,
    pub video_height: Option<u32>,
    pub layer_health: BTreeMap<Layer, LayerHealth>,
    pub worst_severity: Option<Severity>,
    pub open_events: usize,
    /// Disponibilidade da janela corrente do feed (a conectividade é do feed,
    /// não do serviço: um serviço não "cai" sozinho enquanto o datagrama chega).
    pub availability_window: Option<f64>,
    /// Linha do tempo de saúde do serviço inteiro.
    pub timeline: Vec<TimelineBucket>,
    pub streams: Vec<StreamSnapshot>,
    pub snapshot_state: SnapshotState,
}

impl ServiceSnapshot {
    /// Nome exibido, nunca vazio.
    ///
    /// SPEC-PROBE-022
    pub fn display_name(&self) -> String {
        if self.name.trim().is_empty() {
            format!("Serviço {}", self.service_id)
        } else {
            self.name.clone()
        }
    }

    /// Badge `HD` / `SD`, quando a altura é conhecida.
    ///
    /// SPEC-PROBE-022
    pub fn resolution_badge(&self) -> Option<&'static str> {
        self.video_height
            .map(|h| if h >= 720 { "HD" } else { "SD" })
    }

    /// `true` se a ocorrência pertence a este serviço.
    ///
    /// Aceita tanto o evento carimbado com `service_id` quanto o carimbado só
    /// com um PID que é deste serviço — nem todo check consegue resolver o
    /// serviço no momento em que mede.
    ///
    /// SPEC-PROBE-021
    pub fn owns(&self, ctx: &EventContext) -> bool {
        crate::service::service_owns(
            self.service_id,
            self.pmt_pid,
            self.streams.iter().map(|s| s.pid),
            ctx,
        )
    }

    /// `true` se a linha do event log pertence a este serviço.
    ///
    /// SPEC-PROBE-021
    pub fn owns_event(&self, row: &EventRow) -> bool {
        self.owns(&EventContext {
            pid: row.pid,
            service_id: row.service_id,
            ssrc: None,
            origin: EventOrigin::Network,
        })
    }
}

/// Estado de um feed publicado para a UI.
///
/// SPEC-PROBE-018 · SPEC-PROBE-019
#[derive(Debug, Clone, Default)]
pub struct FeedSnapshot {
    pub slot: usize,
    pub name: String,
    pub url: String,
    pub encapsulation: Encapsulation,
    pub connected: bool,
    pub uptime_secs: u64,
    /// Disponibilidade da janela corrente (default 60 min) — §8.1.
    pub availability_window: Option<f64>,
    /// Disponibilidade da sessão inteira.
    pub availability_session: f64,
    pub bitrate_kbps: f64,
    pub null_ratio: f64,
    /// Presença de vídeo, em kbps.
    pub video_kbps: f64,
    /// Presença de áudio, em kbps — **não** é nível de áudio (§8.1).
    pub audio_kbps: f64,
    /// Altura do vídeo, para o badge `HD`/`SD`.
    pub video_height: Option<u32>,
    /// `scrambling_control ≠ 0` em algum PID — badge `SCR`.
    pub scrambled: bool,
    pub layer_health: BTreeMap<Layer, LayerHealth>,
    pub worst_severity: Option<Severity>,
    pub open_events: usize,
    pub timeline: Vec<TimelineBucket>,
    /// Largura das células de **todas** as linhas do tempo deste feed, em
    /// segundos (default 300 = clusters de 5 min).
    ///
    /// A grade monta um eixo de tempo único e projeta cada escopo nele; sem a
    /// largura, colunas de escopos que nasceram em instantes diferentes não
    /// teriam como se alinhar.
    ///
    /// SPEC-PROBE-023
    pub timeline_bucket_secs: u64,
    /// Linha do tempo da camada IP/RTP — linha `IP` da grade (§8.3).
    pub ip_timeline: Vec<TimelineBucket>,
    /// Linha do tempo da camada TS — linha `TRANSPORTE` da grade (§8.3).
    pub ts_timeline: Vec<TimelineBucket>,
    /// Serviços do multiplex, na ordem da PAT.
    ///
    /// SPEC-PROBE-021
    pub services: Vec<ServiceSnapshot>,
    pub series: BTreeMap<MetricId, SeriesPoints>,
    pub events: Vec<EventRow>,
    pub health: ProbeHealth,
    pub unavailable: UnavailableStats,
    pub snapshot_state: SnapshotState,
    /// Pasta desta sessão em disco, quando o run está aberto.
    pub session_dir: Option<PathBuf>,
}

impl FeedSnapshot {
    /// Nome exibido no tile: o configurado, ou `grupo:porta` (§8.1).
    ///
    /// SPEC-PROBE-018
    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.url
        } else {
            &self.name
        }
    }

    /// Badge `HD` / `SD`, quando a altura é conhecida.
    ///
    /// SPEC-PROBE-018
    pub fn resolution_badge(&self) -> Option<&'static str> {
        self.video_height
            .map(|h| if h >= 720 { "HD" } else { "SD" })
    }

    /// Eventos do log dentro de um intervalo — é o que a célula da linha do
    /// tempo filtra ao ser clicada.
    ///
    /// SPEC-PROBE-009
    pub fn events_in_range(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<&EventRow> {
        self.events
            .iter()
            .filter(|e| e.ts_utc >= from && e.ts_utc < to)
            .collect()
    }

    /// Serviço de um `service_id`, se existir.
    ///
    /// SPEC-PROBE-021
    pub fn service(&self, service_id: u16) -> Option<&ServiceSnapshot> {
        self.services
            .iter()
            .find(|s| s.service_id == service_id)
    }

    /// Serviço que o thumbnail do tile do feed representa.
    ///
    /// O primeiro com vídeo; sem nenhum, o primeiro da PAT.  O tile do feed
    /// mostra **um** quadro, e num MPTS ele precisa dizer qual — daí o mosaico
    /// de serviços do nível 1 (SPEC-PROBE-022).
    ///
    /// SPEC-PROBE-024
    pub fn primary_service(&self) -> Option<&ServiceSnapshot> {
        self.services
            .iter()
            .find(|s| s.video_kbps > 0.0)
            .or_else(|| self.services.first())
    }
}

/// Estado do run inteiro publicado para a UI.
///
/// SPEC-PROBE-019 · SPEC-PROBE-020
#[derive(Debug, Clone, Default)]
pub struct ProbeSnapshot {
    pub run_id: String,
    pub run_dir: Option<PathBuf>,
    pub started_utc: Option<DateTime<Utc>>,
    /// Duração do run, em segundos (o cronômetro do cabeçalho, §8.1).
    pub run_secs: u64,
    /// `true` enquanto o writer está gravando.
    pub recording: bool,
    pub feeds: Vec<FeedSnapshot>,
}

impl ProbeSnapshot {
    /// Duração do run formatada como `HH:MM:SS` (§8.1).
    pub fn run_clock(&self) -> String {
        let s = self.run_secs;
        format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    }

    /// Feed de um slot, se existir.
    pub fn feed(&self, slot: usize) -> Option<&FeedSnapshot> {
        self.feeds.iter().find(|f| f.slot == slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §8.1 — o cronômetro do run é `HH:MM:SS`, sem estourar em 12 h.
    #[test]
    fn spec_probe_004_run_clock_formats_as_hhmmss() {
        let s = ProbeSnapshot {
            run_secs: 3 * 3600 + 34 * 60 + 21,
            ..Default::default()
        };
        assert_eq!(s.run_clock(), "03:34:21");

        let long = ProbeSnapshot {
            run_secs: 12 * 3600,
            ..Default::default()
        };
        assert_eq!(long.run_clock(), "12:00:00");
    }

    /// §8.1 — sem nome configurado, o tile mostra a URL.
    #[test]
    fn spec_probe_018_display_name_falls_back_to_url() {
        let f = FeedSnapshot {
            name: "  ".into(),
            url: "udp://@239.15.0.190:50000".into(),
            ..Default::default()
        };
        assert_eq!(f.display_name(), "udp://@239.15.0.190:50000");

        let named = FeedSnapshot {
            name: "0084_CANAL_A".into(),
            ..f
        };
        assert_eq!(named.display_name(), "0084_CANAL_A");
    }

    /// SPEC-PROBE-018 — o badge de resolução só aparece com altura conhecida.
    #[test]
    fn spec_probe_018_resolution_badge() {
        let mut f = FeedSnapshot::default();
        assert_eq!(f.resolution_badge(), None);
        f.video_height = Some(1080);
        assert_eq!(f.resolution_badge(), Some("HD"));
        f.video_height = Some(576);
        assert_eq!(f.resolution_badge(), Some("SD"));
    }

    /// SPEC-PROBE-009 — clicar numa célula filtra o log daquele intervalo.
    #[test]
    fn spec_probe_009_events_in_range_filters_by_cell_window() {
        let row = |secs: i64| EventRow {
            event_id: format!("e{secs}"),
            ts_utc: DateTime::from_timestamp(secs, 0).expect("ts"),
            severity: Severity::Error,
            check_id: "cc_error".into(),
            phase: EventPhase::Open,
            count: 1,
            measured: 1.0,
            unit: "errors".into(),
            context: String::new(),
            pid: None,
            service_id: None,
            local: false,
        };
        let f = FeedSnapshot {
            events: vec![row(0), row(299), row(300), row(900)],
            ..Default::default()
        };

        let from = DateTime::from_timestamp(0, 0).expect("ts");
        let to = DateTime::from_timestamp(300, 0).expect("ts");
        let hits = f.events_in_range(from, to);
        assert_eq!(hits.len(), 2, "o limite superior é exclusivo");
        assert_eq!(hits[0].event_id, "e0");
        assert_eq!(hits[1].event_id, "e299");
    }

    /// SPEC-PROBE-013a — a ordem de degradação é a da spec.
    #[test]
    fn spec_probe_013a_degradation_stages_are_ordered() {
        assert!(DegradationStage::Thumbnail < DegradationStage::Rollups);
        assert!(DegradationStage::Rollups < DegradationStage::SecondarySeries);
        assert_eq!(DegradationStage::None.stage_value(), 0.0);
        assert_eq!(DegradationStage::Thumbnail.stage_value(), 1.0);
    }

    /// SPEC-PROBE-003a — o estado "sem keyframe" tem rótulo próprio.
    #[test]
    fn spec_probe_003a_snapshot_state_labels() {
        assert_eq!(SnapshotState::NoKeyframe.label(), "sem keyframe");
        assert_eq!(SnapshotState::Ok.label(), "ok");
    }

    fn event(check_id: &str, pid: Option<u16>) -> EventRow {
        EventRow {
            event_id: "e".into(),
            ts_utc: DateTime::from_timestamp(0, 0).expect("ts"),
            severity: Severity::Error,
            check_id: check_id.into(),
            phase: EventPhase::Open,
            count: 130,
            measured: 130.0,
            unit: "errors".into(),
            context: String::new(),
            pid,
            service_id: None,
            local: false,
        }
    }

    /// SPEC-PROBE-025 — a lista de alertas explica o problema por extenso, com
    /// a contagem agregada e o PID; é o texto que sai da probe para quem opera
    /// o sinal, não o `check_id` cru do perfil.
    #[test]
    fn spec_probe_025_alert_description_is_self_explanatory() {
        let cc = event(CHECK_CC_ERROR, Some(6100));
        let text = cc.describe();
        assert!(text.contains("Continuity Counter"), "{text}");
        assert!(text.contains("130"), "{text}");
        assert!(text.contains("PID 6100"), "{text}");

        // Sem PID atribuído, a frase não inventa um.
        let sync = event(CHECK_TS_SYNC_LOSS, None);
        assert!(!sync.describe().contains("PID"), "{}", sync.describe());

        // Check desconhecido (camadas futuras) ainda produz linha utilizável.
        let unknown = event("rtp_fec_uncorrected", Some(1));
        assert!(unknown.describe().starts_with("rtp_fec_uncorrected:"));
    }

    /// SPEC-PROBE-013 — a lista de alertas diz quando a culpa é da própria
    /// probe: acusar a rede por um gargalo local é o pior erro possível aqui.
    #[test]
    fn spec_probe_025_local_origin_is_stated_in_the_description() {
        let mut cc = event(CHECK_CC_ERROR, Some(6100));
        cc.local = true;
        assert!(cc.describe().contains("atribuído à probe"));

        // O próprio check de descarte local não repete a ressalva.
        let mut drops = event(CHECK_LOCAL_DROPS, None);
        drops.local = true;
        let text = drops.describe();
        assert!(text.contains("não é perda de rede"));
        assert!(!text.contains("atribuído à probe"), "{text}");
    }

    /// SPEC-PROBE-021 — um evento pertence ao serviço quando carrega o
    /// `service_id` **ou** um PID que é dele; PID de outro serviço do mesmo
    /// multiplex não conta.
    #[test]
    fn spec_probe_021_service_owns_events_by_id_or_pid() {
        let svc = ServiceSnapshot {
            service_id: 55,
            pmt_pid: 0x1388,
            streams: vec![
                StreamSnapshot {
                    pid: 6100,
                    ..Default::default()
                },
                StreamSnapshot {
                    pid: 6102,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        assert!(svc.owns(&EventContext::pid(6100)));
        assert!(svc.owns(&EventContext::pid(0x1388)), "a PMT é do serviço");
        assert!(svc.owns(&EventContext::network().with_service(55)));
        assert!(!svc.owns(&EventContext::pid(401)));
        assert!(
            !svc.owns(&EventContext::network()),
            "evento do multiplex não é de nenhum serviço em particular"
        );
    }

    /// SPEC-PROBE-024 — o tile do feed mostra o serviço com vídeo; sem nenhum,
    /// o primeiro da PAT. Nunca fica sem serviço quando há algum.
    #[test]
    fn spec_probe_024_primary_service_prefers_the_one_with_video() {
        let svc = |id: u16, video: f64| ServiceSnapshot {
            service_id: id,
            video_kbps: video,
            ..Default::default()
        };

        let feed = FeedSnapshot {
            services: vec![svc(1, 0.0), svc(2, 14_000.0)],
            ..Default::default()
        };
        assert_eq!(feed.primary_service().map(|s| s.service_id), Some(2));
        assert_eq!(feed.service(1).map(|s| s.service_id), Some(1));
        assert!(feed.service(9).is_none());

        // Rádio (só áudio): cai no primeiro da PAT em vez de sumir.
        let radio = FeedSnapshot {
            services: vec![svc(7, 0.0)],
            ..Default::default()
        };
        assert_eq!(radio.primary_service().map(|s| s.service_id), Some(7));
        assert!(FeedSnapshot::default().primary_service().is_none());
    }

    /// SPEC-PROBE-023 — a linha da grade identifica o PID pelo codec e idioma.
    #[test]
    fn spec_probe_023_stream_row_label() {
        let s = StreamSnapshot {
            pid: 6106,
            kind: StreamKind::Audio,
            codec: "MPEG-2 Audio".into(),
            language: Some("por".into()),
            ..Default::default()
        };
        assert_eq!(s.describe(), "MPEG-2 Audio · por (6106)");

        let bare = StreamSnapshot {
            pid: 8191,
            kind: StreamKind::Data,
            ..Default::default()
        };
        assert_eq!(bare.describe(), "dados (8191)");
    }
}
