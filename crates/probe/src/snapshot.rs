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

use crate::event::{EventPhase, ProbeEvent};
use crate::series::{MetricId, SeriesPoints, TimelineBucket};
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
        }
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
}
