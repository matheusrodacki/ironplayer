//! Motor de checks com perfil versionado, debounce, histerese e deduplicação.
//!
//! SPEC-PROBE-007 · SPEC-PROBE-008

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::config::{CheckOverride, ProbeConfig};
use crate::event::{EventContext, EventIdGen, EventPhase, ProbeEvent};
use crate::severity::{HealthRow, Layer, LayerHealth, Severity};

/// Sentido da comparação com o limiar.
///
/// Não consta do `CheckDef` do §7 (que só descreve `threshold`), mas é
/// obrigatório: `cc_error > 0` e `video_bitrate < 1 kbps` são ambos checks
/// legítimos e não cabem no mesmo operador.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    /// Viola quando `medido > limiar`.
    Above,
    /// Viola quando `medido < limiar`.
    Below,
}

/// Como a medição é agregada dentro da janela de avaliação.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregation {
    /// Soma os deltas observados na janela (contadores: CC, CRC, perda).
    Sum,
    /// Usa o último valor observado (medidores: jitter, bitrate, estado).
    Gauge,
}

/// Definição de um check no perfil.
///
/// SPEC-PROBE-007
#[derive(Debug, Clone, PartialEq)]
pub struct CheckDef {
    pub id: &'static str,
    pub layer: Layer,
    pub threshold: f64,
    pub unit: &'static str,
    /// Janela de avaliação.
    pub window: Duration,
    /// Debounce de abertura: quanto tempo a violação precisa persistir.
    pub min_duration: Duration,
    /// Histerese de fechamento: quanto tempo sem violação antes de fechar.
    pub clear_duration: Duration,
    pub severity: Severity,
    pub enabled: bool,
    pub comparison: Comparison,
    pub aggregation: Aggregation,
}

impl CheckDef {
    /// Aplica um override do perfil (`[probe.checks.<id>]`).
    ///
    /// SPEC-PROBE-007 — alterar o TOML muda o resultado sem recompilar.
    fn apply(&mut self, o: &CheckOverride) {
        if let Some(v) = o.enabled {
            self.enabled = v;
        }
        if let Some(v) = o.threshold {
            self.threshold = v;
        }
        if let Some(v) = o.window_secs {
            self.window = secs_to_duration(v);
        }
        if let Some(v) = o.min_duration_secs {
            self.min_duration = secs_to_duration(v);
        }
        if let Some(v) = o.clear_duration_secs {
            self.clear_duration = secs_to_duration(v);
        }
        if let Some(v) = o.severity {
            self.severity = v;
        }
    }

    fn breaches(&self, measured: f64) -> bool {
        match self.comparison {
            Comparison::Above => measured > self.threshold,
            Comparison::Below => measured < self.threshold,
        }
    }
}

/// Converte segundos fracionários do TOML em `Duration`, tolerando negativo e
/// NaN (que viram zero) — RNF-PRB-003.
fn secs_to_duration(secs: f64) -> Duration {
    if secs.is_finite() && secs > 0.0 {
        Duration::from_secs_f64(secs)
    } else {
        Duration::ZERO
    }
}

// ── Identificadores dos checks embutidos ────────────────────────────────────

/// Feed sem datagramas (SPEC-PROBE-011).
pub const CHECK_FEED_UNAVAILABLE: &str = "feed_unavailable";
/// Perda de sincronismo TS.
pub const CHECK_TS_SYNC_LOSS: &str = "ts_sync_loss";
/// Continuity counter error por PID.
pub const CHECK_CC_ERROR: &str = "cc_error";
/// CRC inválido em seção PSI/SI.
pub const CHECK_CRC_ERROR: &str = "crc_error";
/// Jitter de PCR acima do perfil do analisador.
pub const CHECK_PCR_ERROR: &str = "pcr_error";
/// Descontinuidade de PCR sem `discontinuity_indicator`.
pub const CHECK_PCR_DISCONTINUITY: &str = "pcr_discontinuity";
/// Pacote RTP fora de ordem / faltante.
pub const CHECK_RTP_OUT_OF_ORDER: &str = "rtp_out_of_order";
/// Ausência de PID de vídeo com bitrate.
pub const CHECK_VIDEO_MISSING: &str = "video_missing";
/// Ausência de PID de áudio com bitrate (presença, **não** nível — §8.1).
pub const CHECK_AUDIO_MISSING: &str = "audio_missing";
/// Descarte local: canal cheio ou buffer de socket (SPEC-PROBE-013).
pub const CHECK_LOCAL_DROPS: &str = "probe_local_drops";
/// Jitter de agendamento do próprio tick (SPEC-PROBE-013).
pub const CHECK_SCHED_JITTER: &str = "probe_sched_jitter";
/// Degradação deliberada sob sobrecarga (SPEC-PROBE-013b).
pub const CHECK_DEGRADED: &str = "probe_degraded";

// ── Camada IP / UDP (spec-14 §5.1 e §5.4) ───────────────────────────────────

/// Mais de uma fonte no mesmo grupo/porta (SPEC-PROBE-IP-010).
pub const CHECK_MULTI_SOURCE: &str = "multi_source";
/// Encapsulamento declarado no TOML difere do observado (SPEC-PROBE-IP-045).
pub const CHECK_ENCAPSULATION_MISMATCH: &str = "encapsulation_mismatch";
/// Pico de inter-arrival acima do perfil (SPEC-PROBE-IP-025 · IP-006).
pub const CHECK_IAT_MAX: &str = "iat_max";

// ── Camada RTP (spec-14 §5.2 e §5.3) ────────────────────────────────────────

/// Perda RTP confirmada após a janela de reconciliação (SPEC-PROBE-IP-021).
pub const CHECK_RTP_MISSING: &str = "rtp_missing";
/// Razão de perda RTP na janela longa (SPEC-PROBE-IP-021).
pub const CHECK_RTP_LOSS_RATIO: &str = "rtp_loss_ratio";
/// Reordenação reconciliada dentro da janela (SPEC-PROBE-IP-020).
pub const CHECK_RTP_REORDER: &str = "rtp_reorder";
/// Pacote RTP duplicado (SPEC-PROBE-IP-022).
pub const CHECK_RTP_DUPLICATE: &str = "rtp_duplicate";
/// Pacote RTP abaixo da janela de reordenação (SPEC-PROBE-IP-023).
pub const CHECK_RTP_TOO_OLD: &str = "rtp_too_old";
/// Reinício da fonte RTP (SPEC-PROBE-IP-024).
pub const CHECK_RTP_SOURCE_RESTART: &str = "rtp_source_restart";
/// Troca de SSRC (SPEC-PROBE-IP-019).
pub const CHECK_RTP_SSRC_CHANGED: &str = "rtp_ssrc_changed";
/// Payload type fora do perfil (SPEC-PROBE-IP-015).
pub const CHECK_RTP_INVALID_PT: &str = "rtp_invalid_pt";
/// Bit de padding presente — proibido pelo perfil ST 2022-2 (SPEC-PROBE-IP-016).
pub const CHECK_RTP_PADDING: &str = "rtp_padding";
/// Header de extensão presente — proibido pelo perfil (SPEC-PROBE-IP-016).
pub const CHECK_RTP_EXTENSION: &str = "rtp_extension";
/// Bit marker presente — proibido pelo perfil (SPEC-PROBE-IP-016).
pub const CHECK_RTP_MARKER: &str = "rtp_marker";
/// Payload não múltiplo de 188 ou sem sync byte (SPEC-PROBE-IP-017).
pub const CHECK_BAD_PAYLOAD_SIZE: &str = "bad_payload_size";
/// Pacotes TS por datagrama fora do perfil (SPEC-PROBE-IP-018).
pub const CHECK_TS_PER_DATAGRAM: &str = "ts_per_datagram";

// ── FEC ST 2022-1 (spec-14 §5.5) ────────────────────────────────────────────

/// L fora da faixa do perfil (SPEC-PROBE-IP-033).
pub const CHECK_FEC_L_RANGE: &str = "fec_l_range";
/// D fora da faixa do perfil (SPEC-PROBE-IP-033).
pub const CHECK_FEC_D_RANGE: &str = "fec_d_range";
/// L×D acima do teto do perfil (SPEC-PROBE-IP-034).
pub const CHECK_FEC_LXD: &str = "fec_lxd";
/// Um único fluxo de FEC onde o perfil espera dois (SPEC-PROBE-IP-035).
pub const CHECK_FEC_DUAL_STREAM: &str = "fec_dual_stream";
/// SSRC do fluxo de FEC divergente do principal (SPEC-PROBE-IP-036).
pub const CHECK_FEC_SSRC_MISMATCH: &str = "fec_ssrc_mismatch";
/// FEC ausente onde o perfil a declara obrigatória (SPEC-PROBE-IP-037).
pub const CHECK_FEC_MISSING: &str = "fec_missing";
/// FEC presente sem o perfil pedir (SPEC-PROBE-IP-037).
pub const CHECK_FEC_UNEXPECTED: &str = "fec_unexpected";

/// Atalho para as durações do perfil embutido.
///
/// Só existe para a tabela de [`default_checks`] caber na largura da linha
/// sem quebrar cada campo em duas.
const fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Perfil embutido de checks da camada base.
///
/// Os limiares aqui são o **default documentado**; o TOML sobrescreve
/// qualquer campo (SPEC-PROBE-007).  Os checks de camada IP detalhada
/// (spec-14) e a promoção completa dos checks TS (spec-15) estendem esta
/// lista sem alterar o motor.
///
/// SPEC-PROBE-007
pub fn default_checks() -> Vec<CheckDef> {
    vec![
        CheckDef {
            id: CHECK_FEED_UNAVAILABLE,
            layer: Layer::Ip,
            threshold: 0.0,
            unit: "state",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(2),
            severity: Severity::Critical,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Gauge,
        },
        CheckDef {
            id: CHECK_TS_SYNC_LOSS,
            layer: Layer::Ts,
            threshold: 0.0,
            unit: "events",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(10),
            severity: Severity::Critical,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        CheckDef {
            id: CHECK_CC_ERROR,
            layer: Layer::Ts,
            threshold: 0.0,
            unit: "errors",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(10),
            severity: Severity::Error,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        CheckDef {
            id: CHECK_CRC_ERROR,
            layer: Layer::Ts,
            threshold: 0.0,
            unit: "errors",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(10),
            severity: Severity::Error,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        CheckDef {
            id: CHECK_PCR_ERROR,
            layer: Layer::Ts,
            threshold: 0.0,
            unit: "events",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(10),
            severity: Severity::Error,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        CheckDef {
            id: CHECK_PCR_DISCONTINUITY,
            layer: Layer::Ts,
            threshold: 0.0,
            unit: "events",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(10),
            severity: Severity::Error,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        CheckDef {
            id: CHECK_RTP_OUT_OF_ORDER,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "pkts",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(10),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        CheckDef {
            id: CHECK_VIDEO_MISSING,
            layer: Layer::Video,
            threshold: 1.0,
            unit: "kbps",
            window: secs(5),
            min_duration: secs(5),
            clear_duration: secs(5),
            severity: Severity::Critical,
            enabled: true,
            comparison: Comparison::Below,
            aggregation: Aggregation::Gauge,
        },
        CheckDef {
            id: CHECK_AUDIO_MISSING,
            layer: Layer::Audio,
            threshold: 1.0,
            unit: "kbps",
            window: secs(5),
            min_duration: secs(5),
            clear_duration: secs(5),
            severity: Severity::Error,
            enabled: true,
            comparison: Comparison::Below,
            aggregation: Aggregation::Gauge,
        },
        CheckDef {
            id: CHECK_LOCAL_DROPS,
            layer: Layer::Probe,
            threshold: 0.0,
            unit: "drops",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(10),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        CheckDef {
            id: CHECK_SCHED_JITTER,
            layer: Layer::Probe,
            threshold: 250.0,
            unit: "ms",
            window: secs(1),
            min_duration: secs(2),
            clear_duration: secs(10),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Gauge,
        },
        CheckDef {
            id: CHECK_DEGRADED,
            layer: Layer::Probe,
            threshold: 0.0,
            unit: "stage",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(30),
            severity: Severity::Info,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Gauge,
        },
        // ── Camada IP / UDP ─────────────────────────────────────────────
        CheckDef {
            id: CHECK_MULTI_SOURCE,
            layer: Layer::Ip,
            threshold: 1.0,
            unit: "fontes",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(30),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Gauge,
        },
        CheckDef {
            id: CHECK_ENCAPSULATION_MISMATCH,
            layer: Layer::Ip,
            threshold: 0.0,
            unit: "state",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(30),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Gauge,
        },
        // §8 — 50 ms é "Major" na referência; 5 ms já é rajada num CBR cujo
        // inter-arrival nominal é 700 µs.  O piso de ruído medido pela própria
        // probe levanta este limiar quando for o caso (SPEC-PROBE-IP-006).
        CheckDef {
            id: CHECK_IAT_MAX,
            layer: Layer::Ip,
            threshold: 5_000.0,
            unit: "us",
            window: secs(1),
            min_duration: secs(10),
            clear_duration: secs(30),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Gauge,
        },
        // ── Camada RTP ──────────────────────────────────────────────────
        // Perda é pontual: abre imediatamente, mas agrega por minuto no
        // `summary_window` do motor (§8).
        CheckDef {
            id: CHECK_RTP_MISSING,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "pkts",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(60),
            severity: Severity::Error,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        // 1e-4 é ~2× a razão observada na referência (5,6e-5 em 3 h 34 min);
        // acima disso é degradação, não o ruído normal de um multicast real.
        CheckDef {
            id: CHECK_RTP_LOSS_RATIO,
            layer: Layer::Rtp,
            threshold: 1e-4,
            unit: "ratio",
            window: secs(300),
            min_duration: secs(60),
            clear_duration: secs(60),
            severity: Severity::Error,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Gauge,
        },
        CheckDef {
            id: CHECK_RTP_REORDER,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "pkts",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(60),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        CheckDef {
            id: CHECK_RTP_DUPLICATE,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "pkts",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(60),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        CheckDef {
            id: CHECK_RTP_TOO_OLD,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "pkts",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(60),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        CheckDef {
            id: CHECK_RTP_SOURCE_RESTART,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "events",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(30),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        // Troca de SSRC é mudança de estado observável, não degradação: Info
        // (mesma classe de PAT/PMT/codec no §5.1 da spec-13).
        CheckDef {
            id: CHECK_RTP_SSRC_CHANGED,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "events",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(30),
            severity: Severity::Info,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        // Warning, e não Error, porque a questão 10.2 #6 — se o PT dinâmico da
        // FEC é 96 fixo na casa — ainda está aberta.
        CheckDef {
            id: CHECK_RTP_INVALID_PT,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "pkts",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(30),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        // Os três bits proibidos são checks independentes e desligáveis um a um
        // (SPEC-PROBE-IP-016).  Ficam ligados em `warning`: a questão 10.2 #3 —
        // se o perfil da casa é ST 2022-2 declarado — está aberta, e observar
        // primeiro é mais barato do que decidir no escuro.
        CheckDef {
            id: CHECK_RTP_PADDING,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "pkts",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(30),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        CheckDef {
            id: CHECK_RTP_EXTENSION,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "pkts",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(30),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        CheckDef {
            id: CHECK_RTP_MARKER,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "pkts",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(30),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        CheckDef {
            id: CHECK_BAD_PAYLOAD_SIZE,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "pkts",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(30),
            severity: Severity::Error,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        CheckDef {
            id: CHECK_TS_PER_DATAGRAM,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "pkts",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(30),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Sum,
        },
        // ── FEC ST 2022-1 ───────────────────────────────────────────────
        // Severidade máxima `warning` em todo o bloco: os offsets do header e a
        // regra dos dois fluxos vêm do RFC 2733 e ainda precisam ser conferidos
        // contra o ST 2022-1 antes de virarem alarme operacional (§5.5).
        CheckDef {
            id: CHECK_FEC_L_RANGE,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "state",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(30),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Gauge,
        },
        CheckDef {
            id: CHECK_FEC_D_RANGE,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "state",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(30),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Gauge,
        },
        CheckDef {
            id: CHECK_FEC_LXD,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "state",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(30),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Gauge,
        },
        CheckDef {
            id: CHECK_FEC_DUAL_STREAM,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "state",
            window: secs(1),
            min_duration: secs(10),
            clear_duration: secs(30),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Gauge,
        },
        CheckDef {
            id: CHECK_FEC_SSRC_MISMATCH,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "state",
            window: secs(1),
            min_duration: secs(0),
            clear_duration: secs(30),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Gauge,
        },
        // Debounce de 10 s nos dois: a FEC leva alguns segundos para aparecer
        // depois do join, e abrir alarme nesse intervalo seria ruído (§5.5).
        CheckDef {
            id: CHECK_FEC_MISSING,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "state",
            window: secs(1),
            min_duration: secs(10),
            clear_duration: secs(30),
            severity: Severity::Warning,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Gauge,
        },
        CheckDef {
            id: CHECK_FEC_UNEXPECTED,
            layer: Layer::Rtp,
            threshold: 0.0,
            unit: "state",
            window: secs(1),
            min_duration: secs(10),
            clear_duration: secs(30),
            severity: Severity::Info,
            enabled: true,
            comparison: Comparison::Above,
            aggregation: Aggregation::Gauge,
        },
    ]
}

/// Camada de um check pelo id, independente do perfil carregado.
///
/// `[probe.checks.<id>]` sobrescreve limiar, janela e severidade, mas **não** a
/// camada — ela é estrutural.  Por isso a UI pode resolvê-la sem carregar o
/// perfil, que é o que permite filtrar os alertas de uma linha da grade
/// (`TRANSPORTE`, `IP`) sem carregar a camada em cada `EventRow`.
///
/// SPEC-PROBE-023 · SPEC-PROBE-025
pub fn layer_of(check_id: &str) -> Option<Layer> {
    // O event log da UI resolve a camada de cada linha a cada repintura; com o
    // perfil da spec-14 são dezenas de checks e milhares de linhas, e
    // reconstruir a tabela em cada consulta seria desperdício puro.
    static INDEX: OnceLock<BTreeMap<&'static str, Layer>> = OnceLock::new();
    INDEX
        .get_or_init(|| default_checks().into_iter().map(|d| (d.id, d.layer)).collect())
        .get(check_id)
        .copied()
}

/// Linha da grade em que os eventos de um check aparecem.
///
/// SPEC-PROBE-IP-050 — atalho de `layer_of` + [`Layer::health_row`], usado pela
/// UI e pelo teste de cobertura.
pub fn health_row_of(check_id: &str) -> Option<HealthRow> {
    layer_of(check_id).map(Layer::health_row)
}

/// Perfil resolvido: defaults embutidos + overrides do TOML.
///
/// SPEC-PROBE-007
#[derive(Debug, Clone)]
pub struct CheckProfile {
    pub version: u32,
    defs: BTreeMap<&'static str, CheckDef>,
}

impl CheckProfile {
    /// Resolve o perfil a partir da configuração.
    ///
    /// Ids desconhecidos em `[probe.checks]` são logados e ignorados — um erro
    /// de digitação no TOML não pode derrubar a sessão (RNF-PRB-003).
    ///
    /// SPEC-PROBE-007
    pub fn from_config(cfg: &ProbeConfig) -> Self {
        let mut defs: BTreeMap<&'static str, CheckDef> =
            default_checks().into_iter().map(|d| (d.id, d)).collect();

        for (id, over) in &cfg.checks {
            match defs.get_mut(id.as_str()) {
                Some(def) => def.apply(over),
                None => tracing::warn!(
                    check_id = %id,
                    "probe: [probe.checks.{id}] não corresponde a nenhum check conhecido — ignorado"
                ),
            }
        }

        Self {
            version: cfg.profile_version,
            defs,
        }
    }

    /// Perfil padrão sem overrides (usado em fixtures).
    pub fn builtin() -> Self {
        Self::from_config(&ProbeConfig::default())
    }

    /// Definição de um check, se existir e estiver habilitado.
    pub fn get(&self, id: &str) -> Option<&CheckDef> {
        self.defs.get(id).filter(|d| d.enabled)
    }

    /// Todos os checks do perfil, habilitados ou não.
    pub fn all(&self) -> impl Iterator<Item = &CheckDef> {
        self.defs.values()
    }
}

// ── Medição de um tick ──────────────────────────────────────────────────────

/// Uma observação levada ao motor num tick.
///
/// SPEC-PROBE-007
#[derive(Debug, Clone)]
pub struct Measurement {
    pub check_id: &'static str,
    pub context: EventContext,
    /// Valor observado neste tick (delta para `Sum`, leitura para `Gauge`).
    pub value: f64,
    /// Ocorrências a somar na contagem agregada do evento (SPEC-PROBE-008).
    pub occurrences: u64,
}

impl Measurement {
    /// Medição sem contexto, com ocorrências iguais ao valor arredondado.
    pub fn counter(check_id: &'static str, value: f64) -> Self {
        Self {
            check_id,
            context: EventContext::network(),
            value,
            occurrences: value.max(0.0).round() as u64,
        }
    }

    /// Medição de medidor (não contribui para a contagem agregada).
    pub fn gauge(check_id: &'static str, value: f64) -> Self {
        Self {
            check_id,
            context: EventContext::network(),
            value,
            occurrences: 0,
        }
    }

    /// Substitui o contexto (PID, serviço, SSRC, origem).
    pub fn with_context(mut self, context: EventContext) -> Self {
        self.context = context;
        self
    }
}

/// Um evento aberto, resumido para atribuição por escopo.
///
/// SPEC-PROBE-021 · SPEC-PROBE-023
#[derive(Debug, Clone, PartialEq)]
pub struct OpenCheck {
    pub check_id: &'static str,
    pub layer: Layer,
    pub severity: Severity,
    pub context: EventContext,
}

// ── Estado interno por (check, contexto) ────────────────────────────────────

#[derive(Debug)]
struct CheckState {
    context: EventContext,
    /// Amostras dentro da janela de avaliação (`Sum`).
    window: VecDeque<(Instant, f64)>,
    /// Última leitura (`Gauge`).
    gauge: f64,
    /// Desde quando a violação persiste sem ter aberto evento (debounce).
    breaching_since: Option<Instant>,
    /// Desde quando não há violação com evento aberto (histerese).
    clear_since: Option<Instant>,
    open: Option<OpenEvent>,
    /// Marcado a cada tick em que a chave recebeu medição.
    touched: bool,
}

#[derive(Debug)]
struct OpenEvent {
    event_id: String,
    first_seen: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    opened_at: Instant,
    last_update: Instant,
    count: u64,
    peak: f64,
}

/// Motor de checks de um feed.
///
/// Mantém o estado de debounce/histerese e a deduplicação por
/// `(check_id, contexto)`, emitindo `open` / `update` / `close`.
///
/// SPEC-PROBE-007 · SPEC-PROBE-008
#[derive(Debug)]
pub struct CheckEngine {
    profile: CheckProfile,
    summary_interval: Duration,
    ids: EventIdGen,
    states: HashMap<(&'static str, String), CheckState>,
}

impl CheckEngine {
    /// Cria o motor a partir de um perfil resolvido.
    ///
    /// SPEC-PROBE-007
    pub fn new(profile: CheckProfile, summary_interval: Duration) -> Self {
        Self {
            profile,
            summary_interval,
            ids: EventIdGen::new(),
            states: HashMap::new(),
        }
    }

    /// Versão do perfil carimbada nos eventos.
    ///
    /// SPEC-PROBE-007
    pub fn profile_version(&self) -> u32 {
        self.profile.version
    }

    /// Perfil resolvido em uso.
    pub fn profile(&self) -> &CheckProfile {
        &self.profile
    }

    /// Avalia um tick.
    ///
    /// Chaves que **não** aparecem em `measurements` são tratadas como "sem
    /// violação" — é assim que um evento fecha quando o PID que o originou
    /// simplesmente some do multiplex.
    ///
    /// SPEC-PROBE-007 · SPEC-PROBE-008
    pub fn evaluate(
        &mut self,
        measurements: &[Measurement],
        now: Instant,
        now_utc: DateTime<Utc>,
    ) -> Vec<ProbeEvent> {
        let mut out = Vec::new();

        for state in self.states.values_mut() {
            state.touched = false;
        }

        for m in measurements {
            let Some(def) = self.profile.get(m.check_id).cloned() else {
                continue;
            };
            let key = (def.id, m.context.dedupe_key());
            let state = self.states.entry(key).or_insert_with(|| CheckState {
                context: m.context.clone(),
                window: VecDeque::new(),
                gauge: 0.0,
                breaching_since: None,
                clear_since: None,
                open: None,
                touched: false,
            });
            state.touched = true;
            // A origem pode ser reclassificada em rajada (SPEC-PROBE-013);
            // o contexto mais recente é o que descreve melhor o evento.
            state.context = m.context.clone();

            let measured = match def.aggregation {
                Aggregation::Sum => {
                    state.window.push_back((now, m.value));
                    let cutoff = now.checked_sub(def.window).unwrap_or(now);
                    while state.window.front().is_some_and(|(t, _)| *t < cutoff) {
                        state.window.pop_front();
                    }
                    state.window.iter().map(|(_, v)| *v).sum()
                }
                Aggregation::Gauge => {
                    state.gauge = m.value;
                    m.value
                }
            };

            Self::step(
                &def,
                state,
                measured,
                m.occurrences,
                now,
                now_utc,
                self.summary_interval,
                self.profile.version,
                &self.ids,
                &mut out,
            );
        }

        // Chaves sem medição neste tick: aplica o caminho "sem violação".
        let profile = &self.profile;
        let ids = &self.ids;
        let summary = self.summary_interval;
        let version = profile.version;
        self.states.retain(|(check_id, _), state| {
            if state.touched {
                return true;
            }
            let Some(def) = profile.get(check_id).cloned() else {
                return false;
            };
            state.window.clear();
            state.gauge = 0.0;
            let measured = match def.aggregation {
                Aggregation::Sum => 0.0,
                // Gauge sem leitura: assume o limiar como valor neutro, para
                // que um check `Below` (bitrate ausente) não dispare só porque
                // a fonte parou de reportar.
                Aggregation::Gauge => def.threshold,
            };
            Self::step(
                &def, state, measured, 0, now, now_utc, summary, version, ids, &mut out,
            );
            state.open.is_some() || state.breaching_since.is_some()
        });

        out
    }

    /// Fecha todos os eventos abertos — usado ao encerrar a sessão para que o
    /// `events.jsonl` nunca termine com um evento sem `close`.
    ///
    /// SPEC-PROBE-004 · SPEC-PROBE-008
    pub fn close_all(&mut self, now: Instant, now_utc: DateTime<Utc>) -> Vec<ProbeEvent> {
        let mut out = Vec::new();
        let version = self.profile.version;
        for ((check_id, _), state) in self.states.iter_mut() {
            let Some(def) = self.profile.get(check_id) else {
                continue;
            };
            if let Some(open) = state.open.take() {
                out.push(finish(
                    def,
                    state,
                    &open,
                    now,
                    now_utc,
                    EventPhase::Close,
                    version,
                ));
            }
        }
        self.states.clear();
        out
    }

    /// Estado de saúde agregado por camada, para os indicadores do tile.
    ///
    /// Toda camada de [`Layer::TILE_ORDER`] aparece no mapa: as que este feed
    /// não avalia ficam `NotApplicable` — cinza, nunca verde (SPEC-PROBE-018a).
    ///
    /// SPEC-PROBE-018
    pub fn layer_health(&self, applicable: &[Layer]) -> BTreeMap<Layer, LayerHealth> {
        self.layer_health_where(applicable, |_| true)
    }

    /// Igual a [`CheckEngine::layer_health`], mas só considera os eventos
    /// abertos cujo contexto passa em `accepts`.
    ///
    /// É o que dá indicadores próprios ao tile de **serviço**: um CC error no
    /// PID de outro serviço do mesmo multiplex não pode acender o `V` deste.
    ///
    /// SPEC-PROBE-022
    pub fn layer_health_where<F>(
        &self,
        applicable: &[Layer],
        accepts: F,
    ) -> BTreeMap<Layer, LayerHealth>
    where
        F: Fn(&EventContext) -> bool,
    {
        let mut map: BTreeMap<Layer, LayerHealth> = Layer::TILE_ORDER
            .iter()
            .map(|l| (*l, LayerHealth::NotApplicable))
            .collect();
        for layer in applicable {
            map.insert(*layer, LayerHealth::Ok);
        }
        for open in self.open_checks() {
            if !accepts(&open.context) {
                continue;
            }
            if let Some(entry) = map.get_mut(&open.layer) {
                entry.worsen(open.severity);
            }
        }
        map
    }

    /// Todos os eventos abertos agora, com camada, severidade e contexto.
    ///
    /// A grade de saúde precisa atribuir cada evento aberto ao seu escopo
    /// (camada do feed, serviço, PID) no mesmo tick — e só o motor sabe quais
    /// estão abertos depois do debounce.
    ///
    /// SPEC-PROBE-021 · SPEC-PROBE-023
    pub fn open_checks(&self) -> Vec<OpenCheck> {
        self.states
            .iter()
            .filter(|(_, s)| s.open.is_some())
            .filter_map(|((check_id, _), state)| {
                let def = self.profile.get(check_id)?;
                Some(OpenCheck {
                    check_id: def.id,
                    layer: def.layer,
                    severity: def.severity,
                    context: state.context.clone(),
                })
            })
            .collect()
    }

    /// Pior severidade entre os eventos abertos, se houver.
    ///
    /// SPEC-PROBE-009 — alimenta a cor do bucket da linha do tempo.
    pub fn worst_open_severity(&self) -> Option<Severity> {
        self.states
            .iter()
            .filter(|(_, s)| s.open.is_some())
            .filter_map(|((id, _), _)| self.profile.get(id))
            .map(|d| d.severity)
            .max()
    }

    /// Número de eventos abertos no momento.
    pub fn open_count(&self) -> usize {
        self.states.values().filter(|s| s.open.is_some()).count()
    }

    #[allow(clippy::too_many_arguments)]
    fn step(
        def: &CheckDef,
        state: &mut CheckState,
        measured: f64,
        occurrences: u64,
        now: Instant,
        now_utc: DateTime<Utc>,
        summary_interval: Duration,
        profile_version: u32,
        ids: &EventIdGen,
        out: &mut Vec<ProbeEvent>,
    ) {
        if def.breaches(measured) {
            state.clear_since = None;

            if state.open.is_none() {
                let since = *state.breaching_since.get_or_insert(now);
                if now.duration_since(since) >= def.min_duration {
                    let open = OpenEvent {
                        event_id: ids.next(now_utc),
                        first_seen: now_utc,
                        last_seen: now_utc,
                        opened_at: now,
                        last_update: now,
                        count: occurrences.max(1),
                        peak: measured,
                    };
                    out.push(ProbeEvent {
                        event_id: open.event_id.clone(),
                        check_id: def.id.to_string(),
                        phase: EventPhase::Open,
                        severity: def.severity,
                        ts_utc: now_utc,
                        first_seen: open.first_seen,
                        last_seen: open.last_seen,
                        count: open.count,
                        duration_ms: 0,
                        measured,
                        threshold: def.threshold,
                        unit: def.unit.to_string(),
                        context: state.context.clone(),
                        profile_version,
                    });
                    state.open = Some(open);
                    state.breaching_since = None;
                }
                return;
            }

            let Some(open) = state.open.as_mut() else {
                return;
            };
            open.count = open.count.saturating_add(occurrences);
            open.last_seen = now_utc;
            open.peak = open.peak.max(measured.abs());

            if now.duration_since(open.last_update) >= summary_interval {
                open.last_update = now;
                let snapshot = OpenEvent {
                    event_id: open.event_id.clone(),
                    first_seen: open.first_seen,
                    last_seen: open.last_seen,
                    opened_at: open.opened_at,
                    last_update: open.last_update,
                    count: open.count,
                    peak: open.peak,
                };
                out.push(finish(
                    def,
                    state,
                    &snapshot,
                    now,
                    now_utc,
                    EventPhase::Update,
                    profile_version,
                ));
            }
            return;
        }

        // Sem violação neste tick.
        state.breaching_since = None;
        if state.open.is_none() {
            return;
        }
        let since = *state.clear_since.get_or_insert(now);
        if now.duration_since(since) >= def.clear_duration {
            if let Some(open) = state.open.take() {
                out.push(finish(
                    def,
                    state,
                    &open,
                    now,
                    now_utc,
                    EventPhase::Close,
                    profile_version,
                ));
            }
            state.clear_since = None;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finish(
    def: &CheckDef,
    state: &CheckState,
    open: &OpenEvent,
    now: Instant,
    now_utc: DateTime<Utc>,
    phase: EventPhase,
    profile_version: u32,
) -> ProbeEvent {
    ProbeEvent {
        event_id: open.event_id.clone(),
        check_id: def.id.to_string(),
        phase,
        severity: def.severity,
        ts_utc: now_utc,
        first_seen: open.first_seen,
        last_seen: open.last_seen,
        count: open.count,
        duration_ms: now.duration_since(open.opened_at).as_millis() as u64,
        measured: open.peak,
        threshold: def.threshold,
        unit: def.unit.to_string(),
        context: state.context.clone(),
        profile_version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{ProbeClock, TestClock};
    use crate::event::EventOrigin;

    fn engine() -> (CheckEngine, TestClock) {
        (
            CheckEngine::new(CheckProfile::builtin(), Duration::from_secs(60)),
            TestClock::new(),
        )
    }

    fn tick(eng: &mut CheckEngine, clock: &TestClock, ms: &[Measurement]) -> Vec<ProbeEvent> {
        let out = eng.evaluate(ms, clock.now_mono(), clock.now_utc());
        clock.advance(Duration::from_secs(1));
        out
    }

    /// SPEC-PROBE-008 — 1000 CC errors contínuos no mesmo PID geram **um**
    /// evento com `count = 1000`, não 1000 eventos.
    #[test]
    fn spec_probe_008_burst_produces_single_event_with_aggregate_count() {
        let (mut eng, clock) = engine();
        let mut opens = 0usize;
        let mut updates = 0usize;

        for _ in 0..100 {
            let evs = tick(
                &mut eng,
                &clock,
                &[Measurement {
                    check_id: CHECK_CC_ERROR,
                    context: EventContext::pid(6100),
                    value: 10.0,
                    occurrences: 10,
                }],
            );
            for e in evs {
                match e.phase {
                    EventPhase::Open => opens += 1,
                    EventPhase::Update => updates += 1,
                    EventPhase::Close => panic!("não deveria fechar durante a rajada"),
                }
            }
        }

        assert_eq!(opens, 1, "a rajada deve abrir exatamente um evento");
        assert!(updates >= 1, "deve haver resumo periódico");

        // Fecha e confere a contagem agregada: 100 ticks × 10 ocorrências.
        // A histerese conta a partir do primeiro tick limpo, não do último
        // tick sujo — por isso é preciso ticar até `clear_duration` vencer.
        let mut close = None;
        for _ in 0..20 {
            for e in tick(&mut eng, &clock, &[]) {
                if e.phase == EventPhase::Close {
                    close = Some(e);
                }
            }
        }
        let close = close.expect("evento deve fechar após a histerese");
        assert_eq!(close.count, 1000);
        assert_eq!(close.check_id, CHECK_CC_ERROR);
        assert_eq!(close.context.pid, Some(6100));
    }

    /// SPEC-PROBE-008 — PIDs distintos são eventos distintos.
    #[test]
    fn spec_probe_008_distinct_pids_open_distinct_events() {
        let (mut eng, clock) = engine();
        let evs = tick(
            &mut eng,
            &clock,
            &[
                Measurement {
                    check_id: CHECK_CC_ERROR,
                    context: EventContext::pid(6100),
                    value: 1.0,
                    occurrences: 1,
                },
                Measurement {
                    check_id: CHECK_CC_ERROR,
                    context: EventContext::pid(6102),
                    value: 1.0,
                    occurrences: 1,
                },
            ],
        );
        assert_eq!(
            evs.iter().filter(|e| e.phase == EventPhase::Open).count(),
            2
        );
        assert_eq!(eng.open_count(), 2);
    }

    /// SPEC-PROBE-007 — `min_duration` faz debounce: violação curta não abre.
    #[test]
    fn spec_probe_007_min_duration_debounces_open() {
        let (mut eng, clock) = engine();
        // sched_jitter tem min_duration = 2 s no perfil embutido.
        let evs = tick(
            &mut eng,
            &clock,
            &[Measurement::gauge(CHECK_SCHED_JITTER, 900.0)],
        );
        assert!(evs.is_empty(), "não abre no primeiro tick violando");

        let evs = tick(
            &mut eng,
            &clock,
            &[Measurement::gauge(CHECK_SCHED_JITTER, 900.0)],
        );
        assert!(evs.is_empty(), "ainda dentro do debounce de 2 s");

        let evs = tick(
            &mut eng,
            &clock,
            &[Measurement::gauge(CHECK_SCHED_JITTER, 900.0)],
        );
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].phase, EventPhase::Open);
        assert_eq!(evs[0].check_id, CHECK_SCHED_JITTER);
    }

    /// SPEC-PROBE-007 — `clear_duration` faz histerese: o evento não fecha no
    /// primeiro tick limpo (evita flap em stream intermitente).
    #[test]
    fn spec_probe_007_clear_duration_applies_hysteresis() {
        let (mut eng, clock) = engine();
        tick(
            &mut eng,
            &clock,
            &[Measurement::counter(CHECK_CC_ERROR, 5.0)],
        );
        assert_eq!(eng.open_count(), 1);

        // cc_error tem clear_duration = 10 s, contada a partir do primeiro
        // tick limpo (t = 1 s).
        for _ in 0..10 {
            let evs = tick(&mut eng, &clock, &[]);
            assert!(evs.is_empty(), "não pode fechar antes da histerese");
            assert_eq!(eng.open_count(), 1);
        }
        let evs = tick(&mut eng, &clock, &[]);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].phase, EventPhase::Close);
        assert_eq!(eng.open_count(), 0);
    }

    /// SPEC-PROBE-007 — o override do TOML muda o resultado sem recompilar.
    #[test]
    fn spec_probe_007_toml_override_changes_outcome() {
        let text = r#"
profile_version = 42

[checks.cc_error]
threshold = 20.0
"#;
        let cfg: ProbeConfig = toml::from_str(text).expect("config");
        let mut eng = CheckEngine::new(CheckProfile::from_config(&cfg), Duration::from_secs(60));
        let clock = TestClock::new();

        // 10 erros/s ficam abaixo do novo limiar de 20 — nada abre.
        let evs = tick(
            &mut eng,
            &clock,
            &[Measurement::counter(CHECK_CC_ERROR, 10.0)],
        );
        assert!(evs.is_empty());

        let evs = tick(
            &mut eng,
            &clock,
            &[Measurement::counter(CHECK_CC_ERROR, 25.0)],
        );
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].profile_version, 42, "evento carrega profile_version");
    }

    /// SPEC-PROBE-007 — `enabled = false` desliga o check.
    #[test]
    fn spec_probe_007_disabled_check_never_fires() {
        let text = r#"
[checks.cc_error]
enabled = false
"#;
        let cfg: ProbeConfig = toml::from_str(text).expect("config");
        let mut eng = CheckEngine::new(CheckProfile::from_config(&cfg), Duration::from_secs(60));
        let clock = TestClock::new();
        let evs = tick(
            &mut eng,
            &clock,
            &[Measurement::counter(CHECK_CC_ERROR, 999.0)],
        );
        assert!(evs.is_empty());
        assert_eq!(eng.open_count(), 0);
    }

    /// SPEC-PROBE-013 — a origem `local` é preservada no evento emitido.
    #[test]
    fn spec_probe_013_local_origin_is_carried_into_event() {
        let (mut eng, clock) = engine();
        let evs = tick(
            &mut eng,
            &clock,
            &[Measurement::counter(CHECK_LOCAL_DROPS, 4.0)
                .with_context(EventContext::network().with_origin(EventOrigin::Local))],
        );
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].context.origin, EventOrigin::Local);
    }

    /// SPEC-PROBE-018a — camadas sem check aplicável ficam `n/a`, não verdes.
    #[test]
    fn spec_probe_018a_layer_health_marks_inapplicable_layers() {
        let (mut eng, clock) = engine();
        tick(
            &mut eng,
            &clock,
            &[Measurement::counter(CHECK_CC_ERROR, 3.0)],
        );

        // Feed UDP puro: RTP não é aplicável.
        let health = eng.layer_health(&[Layer::Ip, Layer::Ts, Layer::Video, Layer::Audio]);
        assert_eq!(
            health.get(&Layer::Ts),
            Some(&LayerHealth::Degraded(Severity::Error))
        );
        assert_eq!(health.get(&Layer::Ip), Some(&LayerHealth::Ok));
        assert_eq!(
            health.get(&Layer::Rtp),
            Some(&LayerHealth::NotApplicable),
            "RTP não avaliado num feed UDP puro — cinza, não verde"
        );
    }

    /// SPEC-PROBE-009 — a pior severidade aberta alimenta a cor do bucket.
    #[test]
    fn spec_probe_009_worst_open_severity_wins() {
        let (mut eng, clock) = engine();
        tick(
            &mut eng,
            &clock,
            &[
                Measurement::counter(CHECK_RTP_OUT_OF_ORDER, 2.0), // Warning
                Measurement::counter(CHECK_CC_ERROR, 2.0),         // Error
            ],
        );
        assert_eq!(eng.worst_open_severity(), Some(Severity::Error));

        tick(
            &mut eng,
            &clock,
            &[Measurement::counter(CHECK_TS_SYNC_LOSS, 1.0)],
        );
        assert_eq!(eng.worst_open_severity(), Some(Severity::Critical));
    }

    /// SPEC-PROBE-IP-050 — todo check declarado resolve para uma camada, e
    /// toda camada com check cai numa superfície visível.
    ///
    /// É a falha silenciosa que a spec manda cobrir: um check novo que não
    /// entre em `default_checks()` produz evento no `events.jsonl` e **nunca**
    /// aparece na grade — invisível justamente no artefato que se olha depois
    /// de 12 h de sessão.
    #[test]
    fn spec_probe_ip_050_every_check_resolves_to_a_visible_row() {
        let defs = default_checks();
        assert!(!defs.is_empty());

        let mut ids = std::collections::HashSet::new();
        for def in &defs {
            assert!(ids.insert(def.id), "id duplicado em default_checks: {}", def.id);
            assert_eq!(
                layer_of(def.id),
                Some(def.layer),
                "{} não resolve para a própria camada",
                def.id
            );
            let row = health_row_of(def.id).expect("todo check tem uma linha");
            assert!(
                row.layers().contains(&def.layer),
                "{} cai na linha {:?}, que não representa {}",
                def.id,
                row,
                def.layer.label()
            );
        }

        // Id desconhecido continua devolvendo `None` em vez de inventar linha.
        assert_eq!(layer_of("check_que_nao_existe"), None);
        assert_eq!(health_row_of("check_que_nao_existe"), None);
    }

    /// SPEC-PROBE-IP-043 — os checks de RTP e de FEC vivem todos na camada
    /// `Rtp`: é isso que permite um feed UDP puro deixá-los `n/a` de uma vez,
    /// sem lista paralela de exceções.
    #[test]
    fn spec_probe_ip_043_rtp_and_fec_checks_share_one_layer() {
        for id in [
            CHECK_RTP_MISSING,
            CHECK_RTP_LOSS_RATIO,
            CHECK_RTP_REORDER,
            CHECK_RTP_DUPLICATE,
            CHECK_RTP_TOO_OLD,
            CHECK_RTP_SOURCE_RESTART,
            CHECK_RTP_SSRC_CHANGED,
            CHECK_RTP_INVALID_PT,
            CHECK_RTP_PADDING,
            CHECK_RTP_EXTENSION,
            CHECK_RTP_MARKER,
            CHECK_BAD_PAYLOAD_SIZE,
            CHECK_TS_PER_DATAGRAM,
            CHECK_FEC_L_RANGE,
            CHECK_FEC_D_RANGE,
            CHECK_FEC_LXD,
            CHECK_FEC_DUAL_STREAM,
            CHECK_FEC_SSRC_MISMATCH,
            CHECK_FEC_MISSING,
            CHECK_FEC_UNEXPECTED,
        ] {
            assert_eq!(layer_of(id), Some(Layer::Rtp), "{id}");
        }
        for id in [CHECK_MULTI_SOURCE, CHECK_ENCAPSULATION_MISMATCH, CHECK_IAT_MAX] {
            assert_eq!(layer_of(id), Some(Layer::Ip), "{id}");
        }
    }

    /// §5.5 — enquanto os offsets do header FEC não forem conferidos contra o
    /// ST 2022-1, nenhum check de FEC pode passar de `warning`.
    #[test]
    fn spec_probe_ip_035_fec_checks_are_capped_at_warning() {
        for def in default_checks().iter().filter(|d| d.id.starts_with("fec_")) {
            assert!(
                def.severity <= Severity::Warning,
                "{} está em {:?}, acima do teto do §5.5",
                def.id,
                def.severity
            );
        }
    }

    /// SPEC-PROBE-004 — encerrar a sessão fecha todos os eventos abertos.
    #[test]
    fn spec_probe_004_close_all_emits_close_for_every_open_event() {
        let (mut eng, clock) = engine();
        tick(
            &mut eng,
            &clock,
            &[
                Measurement::counter(CHECK_CC_ERROR, 1.0).with_context(EventContext::pid(1)),
                Measurement::counter(CHECK_CC_ERROR, 1.0).with_context(EventContext::pid(2)),
            ],
        );
        let closes = eng.close_all(clock.now_mono(), clock.now_utc());
        assert_eq!(closes.len(), 2);
        assert!(closes.iter().all(|e| e.phase == EventPhase::Close));
        assert_eq!(eng.open_count(), 0);
    }
}
