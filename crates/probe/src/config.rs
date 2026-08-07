//! Seção `[probe]` do `ironstream.toml`.
//!
//! SPEC-PROBE-015 · SPEC-PROBE-007 · SPEC-PROBE-017

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::severity::Severity;

/// Número máximo de feeds simultâneos no mosaico.
///
/// SPEC-PROBE-017a — **constante única**: índices de slot, nomes de thread e de
/// canal derivam do slot, nunca de "0 ou 1" escrito à mão.  Elevar este valor
/// para 4 deve compilar e rodar sem outras alterações estruturais.
pub const MAX_FEEDS: usize = 2;

/// Detecção/uso de FEC (ST 2022-1) num feed.
///
/// SPEC-PROBE-018a
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FecMode {
    /// Escuta `base+2` e `base+4`; o badge vira `RTP+FEC` se houver tráfego.
    #[default]
    Auto,
    /// Não abre as portas de FEC; checks de FEC ficam `n/a`.
    Off,
}

/// Um feed do mosaico (`[[probe.feeds]]`).
///
/// SPEC-PROBE-017 · SPEC-PROBE-018
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedConfig {
    /// Nome exibido no tile; vazio ⇒ a UI usa `grupo:porta` (§8.1).
    pub name: String,
    /// URL multicast (`udp://@grupo:porta` ou `rtp://@grupo:porta`).
    pub url: String,
    /// Política de FEC deste feed.
    pub fec: FecMode,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            url: String::new(),
            fec: FecMode::Auto,
        }
    }
}

/// Override de um check no perfil (`[probe.checks.<id>]`).
///
/// Todos os campos são opcionais: o que não vier no TOML mantém o valor
/// embutido em [`crate::check::default_checks`].
///
/// SPEC-PROBE-007 — alterar um limiar aqui muda o resultado sem recompilar.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckOverride {
    pub enabled: Option<bool>,
    pub threshold: Option<f64>,
    pub window_secs: Option<f64>,
    pub min_duration_secs: Option<f64>,
    pub clear_duration_secs: Option<f64>,
    pub severity: Option<Severity>,
}

/// Configuração do modo Probe.
///
/// A ordem dos campos importa: o `AppConfig` é reserializado com
/// `toml::to_string_pretty` quando o arquivo não existe (SPEC-CFG-001), e no
/// TOML todo valor escalar precisa vir **antes** das tabelas (`checks`) e dos
/// arrays de tabelas (`feeds`).
///
/// SPEC-PROBE-015
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProbeConfig {
    /// Versão do perfil de checks, carimbada em cada evento (SPEC-PROBE-007).
    pub profile_version: u32,
    /// Limite de feeds do mosaico; sempre truncado a [`MAX_FEEDS`].
    pub max_feeds: usize,
    /// Intervalo entre snapshots de vídeo, em segundos (SPEC-PROBE-003).
    pub snapshot_interval_secs: u64,
    /// Janela em que o decoder fica armado antes do tick (SPEC-PROBE-003a).
    pub snapshot_arm_secs: f64,
    /// Largura máxima do thumbnail em pixels (SPEC-PROBE-003).
    pub snapshot_max_width: u32,
    /// Grava o thumbnail como evidência ao abrir evento crítico.
    ///
    /// Default `false` — decisão 13.2 #6: respeita "descarta o anterior".
    pub save_snapshot_on_error: bool,
    /// Período de amostragem da série temporal, em ms (SPEC-PROBE-005).
    pub sample_interval_ms: u64,
    /// Janela de rollup, em segundos (SPEC-PROBE-005).
    pub rollup_secs: u64,
    /// Largura do bucket da linha do tempo, em segundos (SPEC-PROBE-009).
    pub timeline_bucket_secs: u64,
    /// Intervalo de flush do writer, em segundos (SPEC-PROBE-006).
    pub flush_interval_secs: u64,
    /// Intervalo entre eventos `update` de um evento aberto (SPEC-PROBE-008).
    pub summary_interval_secs: u64,
    /// Backoff de reconexão, em ms (SPEC-PROBE-011).
    pub reconnect_backoff_ms: Vec<u64>,
    /// Impede suspensão do Windows durante a sessão (SPEC-PROBE-012).
    pub prevent_sleep: bool,
    /// Idade máxima de uma sessão em disco, em dias (SPEC-PROBE-016).
    pub retention_days: u64,
    /// Tamanho máximo total de `probe-sessions/`, em MiB (SPEC-PROBE-016).
    pub max_disk_mb: u64,
    /// Liga o motor de checks também em modo Broadcast.
    ///
    /// Default `false`: Broadcast não deve pagar o custo de escrita em disco (§3.1).
    pub enabled_in_broadcast: bool,
    /// Feeds do mosaico, na ordem dos slots.
    pub feeds: Vec<FeedConfig>,
    /// Overrides de check por id (`[probe.checks.<id>]`).
    pub checks: BTreeMap<String, CheckOverride>,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            profile_version: 1,
            max_feeds: MAX_FEEDS,
            snapshot_interval_secs: 5,
            snapshot_arm_secs: 2.5,
            snapshot_max_width: 320,
            save_snapshot_on_error: false,
            sample_interval_ms: 1000,
            rollup_secs: 60,
            timeline_bucket_secs: 300,
            flush_interval_secs: 5,
            summary_interval_secs: 60,
            reconnect_backoff_ms: vec![1000, 2000, 5000, 10000, 30000],
            prevent_sleep: true,
            retention_days: 14,
            max_disk_mb: 4096,
            enabled_in_broadcast: false,
            feeds: Vec::new(),
            checks: BTreeMap::new(),
        }
    }
}

impl ProbeConfig {
    /// Intervalo de amostragem como [`Duration`], com piso de 100 ms.
    ///
    /// Um `sample_interval_ms` de 0 vindo de um TOML editado à mão colocaria o
    /// tick do engine em busy-loop; o piso é preferível a um panic.
    ///
    /// SPEC-PROBE-005 · RNF-PRB-003
    pub fn sample_interval(&self) -> Duration {
        Duration::from_millis(self.sample_interval_ms.max(100))
    }

    /// Janela de rollup como [`Duration`], com piso de 1 s.
    ///
    /// SPEC-PROBE-005
    pub fn rollup_window(&self) -> Duration {
        Duration::from_secs(self.rollup_secs.max(1))
    }

    /// Largura do bucket da linha do tempo, com piso de 1 s.
    ///
    /// SPEC-PROBE-009
    pub fn timeline_bucket(&self) -> Duration {
        Duration::from_secs(self.timeline_bucket_secs.max(1))
    }

    /// Intervalo entre eventos `update`, com piso de 1 s.
    ///
    /// SPEC-PROBE-008
    pub fn summary_interval(&self) -> Duration {
        Duration::from_secs(self.summary_interval_secs.max(1))
    }

    /// Intervalo de flush do writer, com piso de 1 s.
    ///
    /// SPEC-PROBE-006
    pub fn flush_interval(&self) -> Duration {
        Duration::from_secs(self.flush_interval_secs.max(1))
    }

    /// Intervalo entre snapshots de vídeo, com piso de 1 s.
    ///
    /// SPEC-PROBE-003
    pub fn snapshot_interval(&self) -> Duration {
        Duration::from_secs(self.snapshot_interval_secs.max(1))
    }

    /// Backoff da tentativa `attempt` (0-based); satura no último elemento.
    ///
    /// SPEC-PROBE-011
    pub fn reconnect_backoff(&self, attempt: usize) -> Duration {
        let ms = if self.reconnect_backoff_ms.is_empty() {
            1000
        } else {
            let idx = attempt.min(self.reconnect_backoff_ms.len() - 1);
            self.reconnect_backoff_ms[idx]
        };
        Duration::from_millis(ms.max(100))
    }

    /// Feeds efetivamente instanciáveis: descarta entradas sem URL e trunca em
    /// `min(max_feeds, MAX_FEEDS)`.
    ///
    /// SPEC-PROBE-017 · SPEC-PROBE-017a
    pub fn effective_feeds(&self) -> Vec<FeedConfig> {
        let limit = self.max_feeds.clamp(1, MAX_FEEDS);
        self.feeds
            .iter()
            .filter(|f| !f.url.trim().is_empty())
            .take(limit)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC-PROBE-015 — os defaults são exatamente os documentados no §9.
    #[test]
    fn spec_probe_015_defaults_match_spec() {
        let c = ProbeConfig::default();
        assert_eq!(c.profile_version, 1);
        assert_eq!(c.max_feeds, 2);
        assert_eq!(c.snapshot_interval_secs, 5);
        assert!((c.snapshot_arm_secs - 2.5).abs() < f64::EPSILON);
        assert_eq!(c.snapshot_max_width, 320);
        assert!(!c.save_snapshot_on_error);
        assert_eq!(c.sample_interval_ms, 1000);
        assert_eq!(c.rollup_secs, 60);
        assert_eq!(c.timeline_bucket_secs, 300);
        assert_eq!(c.flush_interval_secs, 5);
        assert_eq!(c.summary_interval_secs, 60);
        assert_eq!(c.reconnect_backoff_ms, vec![1000, 2000, 5000, 10000, 30000]);
        assert!(c.prevent_sleep);
        assert_eq!(c.retention_days, 14);
        assert_eq!(c.max_disk_mb, 4096);
        assert!(!c.enabled_in_broadcast);
    }

    /// SPEC-PROBE-015 — a seção reserializa para TOML válido (escalares antes
    /// das tabelas), o que é o que permite gerar o arquivo na ausência dele.
    #[test]
    fn spec_probe_015_roundtrips_through_toml() {
        let mut c = ProbeConfig::default();
        c.feeds.push(FeedConfig {
            name: "0084_CANAL_A".into(),
            url: "rtp://@239.15.0.183:50000".into(),
            fec: FecMode::Auto,
        });
        c.checks.insert(
            "cc_error".into(),
            CheckOverride {
                threshold: Some(3.0),
                ..Default::default()
            },
        );

        let text = toml::to_string_pretty(&c).expect("serializa");
        let back: ProbeConfig = toml::from_str(&text).expect("desserializa");
        assert_eq!(back, c);
    }

    /// SPEC-PROBE-007 — limiar vindo do TOML sobrescreve o embutido.
    #[test]
    fn spec_probe_007_threshold_override_parses() {
        let text = r#"
profile_version = 7

[checks.cc_error]
threshold = 12.0
severity = "warning"
"#;
        let c: ProbeConfig = toml::from_str(text).expect("desserializa");
        assert_eq!(c.profile_version, 7);
        let o = c.checks.get("cc_error").expect("override presente");
        assert_eq!(o.threshold, Some(12.0));
        assert_eq!(o.severity, Some(Severity::Warning));
        // Campos ausentes continuam None — o default embutido prevalece.
        assert_eq!(o.enabled, None);
    }

    /// SPEC-PROBE-017 — feeds sem URL são descartados e a lista é truncada.
    #[test]
    fn spec_probe_017_effective_feeds_is_capped_and_filtered() {
        let mut c = ProbeConfig::default();
        for i in 0..(MAX_FEEDS + 3) {
            c.feeds.push(FeedConfig {
                name: format!("f{i}"),
                url: format!("udp://@239.0.0.{}:5000", i + 1),
                fec: FecMode::Off,
            });
        }
        c.feeds.insert(0, FeedConfig::default()); // sem URL
        assert_eq!(c.effective_feeds().len(), MAX_FEEDS);
        assert!(c.effective_feeds().iter().all(|f| !f.url.is_empty()));
    }

    /// SPEC-PROBE-011 — o backoff satura no último degrau em vez de estourar.
    #[test]
    fn spec_probe_011_backoff_saturates_on_last_step() {
        let c = ProbeConfig::default();
        assert_eq!(c.reconnect_backoff(0), Duration::from_millis(1000));
        assert_eq!(c.reconnect_backoff(4), Duration::from_millis(30000));
        assert_eq!(c.reconnect_backoff(99), Duration::from_millis(30000));
    }

    /// RNF-PRB-003 — valores absurdos no TOML não viram busy-loop nem panic.
    #[test]
    fn rnf_prb_003_zero_intervals_are_floored() {
        let c = ProbeConfig {
            sample_interval_ms: 0,
            rollup_secs: 0,
            timeline_bucket_secs: 0,
            summary_interval_secs: 0,
            flush_interval_secs: 0,
            snapshot_interval_secs: 0,
            reconnect_backoff_ms: vec![],
            ..Default::default()
        };
        assert_eq!(c.sample_interval(), Duration::from_millis(100));
        assert_eq!(c.rollup_window(), Duration::from_secs(1));
        assert_eq!(c.timeline_bucket(), Duration::from_secs(1));
        assert_eq!(c.summary_interval(), Duration::from_secs(1));
        assert_eq!(c.flush_interval(), Duration::from_secs(1));
        assert_eq!(c.snapshot_interval(), Duration::from_secs(1));
        assert_eq!(c.reconnect_backoff(0), Duration::from_millis(1000));
    }
}
