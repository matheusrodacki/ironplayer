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
/// Serializado como texto para caber na sintaxe que a spec fixa:
/// `fec = "auto" | "off" | "ports:50002,50004"`.
///
/// SPEC-PROBE-018a · SPEC-PROBE-IP-030
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum FecMode {
    /// Escuta `base+2` e `base+4`; o badge vira `RTP+FEC` se houver tráfego.
    #[default]
    Auto,
    /// Não abre as portas de FEC; checks de FEC ficam `n/a`.
    Off,
    /// Portas explícitas: coluna e linha, nessa ordem.
    Ports { column: u16, row: u16 },
}

impl FecMode {
    /// `true` quando a probe deve entrar nos grupos de FEC.
    ///
    /// SPEC-PROBE-IP-030 — em `off`, os checks de FEC ficam `n/a` e nenhum join
    /// extra é feito.
    pub fn listens(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Portas de FEC deste feed, dadas a porta base e os offsets do perfil.
    ///
    /// SPEC-PROBE-IP-030 — `auto` deriva de `base+2` (coluna) e `base+4`
    /// (linha), que é a convenção do ST 2022-1 e o que a operação usa.
    pub fn ports(self, base: u16, offsets: [u16; 2]) -> Option<(u16, u16)> {
        match self {
            Self::Off => None,
            Self::Auto => net::fec_ports(base, offsets),
            Self::Ports { column, row } => Some((column, row)),
        }
    }
}

impl std::fmt::Display for FecMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::Off => write!(f, "off"),
            Self::Ports { column, row } => write!(f, "ports:{column},{row}"),
        }
    }
}

impl From<FecMode> for String {
    fn from(value: FecMode) -> Self {
        value.to_string()
    }
}

impl TryFrom<String> for FecMode {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let text = value.trim();
        if text.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        if text.eq_ignore_ascii_case("off") {
            return Ok(Self::Off);
        }
        let ports = text
            .strip_prefix("ports:")
            .or_else(|| text.strip_prefix("PORTS:"))
            .ok_or_else(|| format!("fec inválido: {value:?} — use auto, off ou ports:a,b"))?;
        let (a, b) = ports
            .split_once(',')
            .ok_or_else(|| format!("fec inválido: {value:?} — ports:a,b exige duas portas"))?;
        let parse = |s: &str| {
            s.trim()
                .parse::<u16>()
                .map_err(|_| format!("porta de FEC inválida: {s:?}"))
        };
        Ok(Self::Ports {
            column: parse(a)?,
            row: parse(b)?,
        })
    }
}

/// Parâmetros da validação de FEC (`[probe.fec]`).
///
/// A spec desenha estes campos dentro de `[probe.checks.fec]`; aqui eles vivem
/// numa seção própria porque `[probe.checks.*]` é um mapa homogêneo de
/// [`CheckOverride`] — limiar, janela, severidade — e enfiar faixa de L, offsets
/// de porta e teto de L×D nele obrigaria todo check do perfil a carregar campos
/// que só a FEC entende.  A semântica e os defaults são exatamente os do §8.
///
/// SPEC-PROBE-IP-030 · SPEC-PROBE-IP-033 · SPEC-PROBE-IP-034
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FecProfile {
    /// Offsets de porta usados por `fec = auto`.
    pub port_offsets: [u16; 2],
    /// Faixa aceita de L (coluna).
    pub l_range: [u32; 2],
    /// Faixa aceita de D (linha).
    pub d_range: [u32; 2],
    /// Teto de L×D.
    pub max_lxd: u32,
    /// A partir de qual L o perfil espera **dois** fluxos de FEC.
    ///
    /// SPEC-PROBE-IP-035 — regra marcada como "a validar" contra o ST 2022-1;
    /// até lá a severidade máxima é `warning`.
    pub dual_stream_min_l: u32,
    /// O perfil exige FEC neste feed (SPEC-PROBE-IP-037).
    pub required: bool,
}

impl Default for FecProfile {
    fn default() -> Self {
        Self {
            port_offsets: [2, 4],
            l_range: [1, 20],
            d_range: [4, 20],
            max_lxd: 100,
            dual_stream_min_l: 4,
            required: false,
        }
    }
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

/// Override de análise de vídeo para um serviço específico.
///
/// Campos ausentes herdam o bloco `[probe.video]`; um serviço desligado não
/// deve instanciar decoder ou analisador secundário.
///
/// SPEC-PROBE-VID-001
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoServiceProfile {
    pub enabled: Option<bool>,
    pub sample_fps: Option<u16>,
}

/// Perfil auditável dos observadores e detectores de vídeo.
///
/// Os checks perceptuais começam desabilitados: seus limiares são política
/// operacional, não valores intrínsecos do decoder.
///
/// SPEC-PROBE-VID-001 · SPEC-PROBE-VID-006 · SPEC-PROBE-VID-008
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoProfileConfig {
    pub enabled: bool,
    pub sample_fps: u16,
    pub luma_max_width: u16,
    pub luma_max_height: u16,
    pub observation_capacity: usize,
    pub freeze_enabled: bool,
    pub freeze_similarity: u8,
    pub freeze_duration_secs: f64,
    pub black_enabled: bool,
    pub black_luma_threshold: u8,
    pub black_coverage_pct: u8,
    pub black_duration_secs: f64,
    pub blockiness_enabled: bool,
    pub blockiness_threshold: f64,
    /// Chave TOML: `[probe.video.services.<service_id>]`.
    pub services: BTreeMap<u16, VideoServiceProfile>,
}

impl Default for VideoProfileConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_fps: 1,
            luma_max_width: 160,
            luma_max_height: 90,
            observation_capacity: 8,
            freeze_enabled: false,
            freeze_similarity: 2,
            freeze_duration_secs: 3.0,
            black_enabled: false,
            black_luma_threshold: 16,
            black_coverage_pct: 95,
            black_duration_secs: 1.0,
            blockiness_enabled: false,
            blockiness_threshold: 20.0,
            services: BTreeMap::new(),
        }
    }
}

impl VideoProfileConfig {
    /// Resolve os valores do perfil para um serviço sem criar estado de análise.
    ///
    /// SPEC-PROBE-VID-001
    pub fn effective_for_service(&self, service_id: u16) -> VideoProfileConfig {
        let mut effective = self.clone();
        if let Some(service) = self.services.get(&service_id) {
            if let Some(enabled) = service.enabled {
                effective.enabled = enabled;
            }
            if let Some(sample_fps) = service.sample_fps {
                effective.sample_fps = sample_fps.max(1);
            }
        }
        effective
    }
}

/// Perfil operacional dos checks MPEG-TS.
///
/// O parser TS continua funcionando para o player quando este bloco está
/// desligado; apenas a promoção de fatos para alarmes da Probe é suprimida.
///
/// SPEC-PROBE-TS-001 · SPEC-PROBE-TS-009 · SPEC-PROBE-TS-015
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransportProfile {
    /// Habilita a avaliação dos checks da camada MPEG-TS.
    pub enabled: bool,
    /// Máximo entre PATs válidas consecutivas.
    pub pat_max_interval_secs: f64,
    /// Máximo entre PMTs válidas consecutivas.
    pub pmt_max_interval_secs: f64,
    /// Período inicial em que ausência de PSI é estado desconhecido.
    pub psi_grace_secs: f64,
    /// CAT passa a ser obrigatória apenas em perfis que exigem CA.
    pub ca_required: bool,
    /// PIDs que devem aparecer no inventário conhecido do multiplex.
    pub required_pids: Vec<u16>,
    /// PIDs cuja presença é proibida neste perfil.
    pub forbidden_pids: Vec<u16>,
    /// Buffer analysis/T-STD dependem de modelo e parâmetros ainda não
    /// definidos; por padrão permanecem inativos e não viram falso `ok`.
    pub tstd_enabled: bool,
    /// MGF/MGB também são opt-in até existir definição operacional fechada.
    pub mgf_mgb_enabled: bool,
}

impl Default for TransportProfile {
    fn default() -> Self {
        Self {
            enabled: true,
            pat_max_interval_secs: 0.5,
            pmt_max_interval_secs: 0.5,
            psi_grace_secs: 2.0,
            ca_required: false,
            required_pids: Vec::new(),
            forbidden_pids: Vec::new(),
            tstd_enabled: false,
            mgf_mgb_enabled: false,
        }
    }
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

    /// Perfil dos observadores compactos e detectores de vídeo (spec-16).
    pub video: VideoProfileConfig,

    /// Perfil da camada de transporte MPEG-TS (spec-15).
    pub transport: TransportProfile,

    // ── Camada IP (spec-14 §8) ──────────────────────────────────────────
    /// Janela de reconciliação de lacunas RTP, em ms.
    ///
    /// SPEC-PROBE-IP-020 — estado **sub-tick** dentro do pipeline do feed, onde
    /// cada pacote tem `Instant` próprio.  Não confundir com
    /// [`ProbeConfig::correlation_window_ms`], que opera no motor de checks.
    pub reorder_window_ms: u64,
    /// Janela de correlação IP→TS, em ms.
    ///
    /// SPEC-PROBE-IP-039 · SPEC-PROBE-IP-039a — o motor amostra contadores
    /// cumulativos a 1 Hz, então valores abaixo de 1000 ms não são
    /// implementáveis e são rejeitados na carga do perfil.
    pub correlation_window_ms: u64,
    /// Duração da calibração do piso de ruído, em s (SPEC-PROBE-IP-005).
    pub calib_secs: u64,
    /// Janela de detecção de encapsulamento, em s (SPEC-PROBE-IP-042).
    pub detect_secs: u64,
    /// Variação de bitrate acima da qual a burstiness vira `n/a`, em %.
    ///
    /// SPEC-PROBE-IP-027 — num VBR o inter-arrival esperado não é constante, e
    /// afirmar rajada em cima disso seria inventar defeito.
    pub vbr_tolerance_pct: f64,
    /// Multiplicador do piso de ruído nos alarmes de temporização.
    ///
    /// SPEC-PROBE-IP-006 — `max(threshold, k × noise_floor_us)`.
    pub noise_k: f64,
    /// Pacotes TS por datagrama esperados pelo perfil (SPEC-PROBE-IP-018).
    pub ts_per_datagram: u32,
    /// Payload types RTP aceitos (SPEC-PROBE-IP-015).
    ///
    /// Default: 33 (MPEG-TS) e 96 (PT dinâmico da FEC na casa — questão 10.2 #6
    /// ainda em aberto, por isso o check é `warning` e não `error`).
    pub rtp_payload_types: Vec<u8>,

    /// Parâmetros de FEC (`[probe.fec]`).
    pub fec: FecProfile,
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
            video: VideoProfileConfig::default(),
            transport: TransportProfile::default(),
            reorder_window_ms: 200,
            correlation_window_ms: 1000,
            calib_secs: 30,
            detect_secs: 3,
            vbr_tolerance_pct: 5.0,
            noise_k: 3.0,
            ts_per_datagram: 7,
            rtp_payload_types: vec![33, 96],
            fec: FecProfile::default(),
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

    /// Janela de reconciliação de lacunas RTP, com piso de 10 ms.
    ///
    /// SPEC-PROBE-IP-020
    pub fn reorder_window(&self) -> Duration {
        Duration::from_millis(self.reorder_window_ms.max(10))
    }

    /// Janela de correlação IP→TS **efetiva**, com piso de um tick.
    ///
    /// SPEC-PROBE-IP-039a — o motor de checks só vê deltas por segundo; uma
    /// janela de 200 ms exigiria timestamp por ocorrência e é fase 2.  Em vez
    /// de fingir que 200 ms funciona, o valor é elevado a 1 s e a sessão
    /// continua, com aviso.
    pub fn correlation_window(&self) -> Duration {
        let tick = self.sample_interval();
        let asked = Duration::from_millis(self.correlation_window_ms);
        if asked < tick {
            tracing::warn!(
                requested_ms = self.correlation_window_ms,
                effective_ms = tick.as_millis() as u64,
                "probe: correlation_window_ms abaixo de um tick não é implementável \
                 com contadores amostrados a 1 Hz (SPEC-PROBE-IP-039a) — usando o tick"
            );
            tick
        } else {
            asked
        }
    }

    /// Duração da calibração do piso de ruído, com piso de 1 s.
    ///
    /// SPEC-PROBE-IP-005
    pub fn calibration_window(&self) -> Duration {
        Duration::from_secs(self.calib_secs.max(1))
    }

    /// Janela de detecção de encapsulamento, com piso de 1 s.
    ///
    /// SPEC-PROBE-IP-042
    pub fn detect_window(&self) -> Duration {
        Duration::from_secs(self.detect_secs.max(1))
    }

    /// `true` se `pt` é um payload type aceito pelo perfil.
    ///
    /// Lista vazia significa "aceita qualquer um": desligar o check pelo TOML é
    /// `[probe.checks.rtp_invalid_pt] enabled = false`, não esvaziar a lista.
    ///
    /// SPEC-PROBE-IP-015
    pub fn accepts_payload_type(&self, pt: u8) -> bool {
        self.rtp_payload_types.is_empty() || self.rtp_payload_types.contains(&pt)
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

    /// Valida combinações que não admitem defaults implícitos.
    ///
    /// SPEC-PROBE-TS-015 — T-STD e MGF/MGB não podem ser "ativados" sem um
    /// modelo operacional e limites que ainda não existem no perfil.
    pub fn validate_transport(&self) -> Result<(), String> {
        for (name, value) in [
            (
                "pat_max_interval_secs",
                self.transport.pat_max_interval_secs,
            ),
            (
                "pmt_max_interval_secs",
                self.transport.pmt_max_interval_secs,
            ),
            ("psi_grace_secs", self.transport.psi_grace_secs),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(format!(
                    "probe.transport.{name} deve ser finito e não negativo"
                ));
            }
        }
        if self.transport.tstd_enabled || self.transport.mgf_mgb_enabled {
            return Err(
                "probe.transport: T-STD/MGF/MGB exigem modelo e limites operacionais; mantenha-os desativados"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Janela inicial em que PAT/PMT/CAT ausentes ainda são desconhecidas.
    ///
    /// SPEC-PROBE-TS-007 · SPEC-PROBE-TS-008 · SPEC-PROBE-TS-013
    pub fn psi_grace(&self) -> Duration {
        duration_from_profile_secs(self.transport.psi_grace_secs)
    }

    /// Intervalo máximo de PAT válido antes de abrir `pat_error`.
    ///
    /// SPEC-PROBE-TS-007
    pub fn pat_max_interval(&self) -> Duration {
        duration_from_profile_secs(self.transport.pat_max_interval_secs)
    }

    /// Intervalo máximo de PMT válida antes de abrir `pmt_error`.
    ///
    /// SPEC-PROBE-TS-008
    pub fn pmt_max_interval(&self) -> Duration {
        duration_from_profile_secs(self.transport.pmt_max_interval_secs)
    }
}

/// Converte segundos de configuração sem permitir que `NaN`/infinito vindos
/// de um perfil montado programaticamente cheguem a `Duration::from_secs_f64`.
fn duration_from_profile_secs(secs: f64) -> Duration {
    if secs.is_finite() && secs >= 0.0 {
        Duration::from_secs_f64(secs)
    } else {
        Duration::ZERO
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
        assert!(c.video.enabled);
        assert!(!c.video.freeze_enabled);
        assert!(!c.video.black_enabled);
        assert!(!c.video.blockiness_enabled);
        assert!(c.transport.enabled);
        assert!(!c.transport.tstd_enabled);
        assert!(!c.transport.mgf_mgb_enabled);
    }

    /// §8 da spec-14 — os limiares default da camada IP são os documentados,
    /// derivados da medição de referência e não de zero-tolerância.
    #[test]
    fn spec_probe_ip_030_ip_layer_defaults_match_spec() {
        let c = ProbeConfig::default();
        assert_eq!(c.reorder_window_ms, 200);
        assert_eq!(c.correlation_window_ms, 1000);
        assert_eq!(c.calib_secs, 30);
        assert_eq!(c.detect_secs, 3);
        assert!((c.vbr_tolerance_pct - 5.0).abs() < f64::EPSILON);
        assert!((c.noise_k - 3.0).abs() < f64::EPSILON);
        assert_eq!(c.ts_per_datagram, 7);
        assert_eq!(c.rtp_payload_types, vec![33, 96]);
        assert_eq!(c.fec.port_offsets, [2, 4]);
        assert_eq!(c.fec.l_range, [1, 20]);
        assert_eq!(c.fec.d_range, [4, 20]);
        assert_eq!(c.fec.max_lxd, 100);
        assert!(!c.fec.required);
    }

    /// SPEC-PROBE-IP-030 — `auto` deriva 50002/50004 de um feed em 50000;
    /// `off` não abre porta nenhuma; `ports:a,b` manda.
    #[test]
    fn spec_probe_ip_030_fec_ports_follow_the_mode() {
        let offsets = ProbeConfig::default().fec.port_offsets;
        assert_eq!(FecMode::Auto.ports(50_000, offsets), Some((50_002, 50_004)));
        assert_eq!(FecMode::Off.ports(50_000, offsets), None);
        assert!(!FecMode::Off.listens());
        assert!(FecMode::Auto.listens());
        assert_eq!(
            FecMode::Ports {
                column: 6000,
                row: 6002
            }
            .ports(50_000, offsets),
            Some((6000, 6002))
        );
    }

    /// SPEC-PROBE-IP-030 — a sintaxe `auto | off | ports:a,b` da spec atravessa
    /// serde nos dois sentidos, e texto inválido vira erro, não um default
    /// silencioso.
    #[test]
    fn spec_probe_ip_030_fec_mode_parses_the_spec_syntax() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Wrap {
            fec: FecMode,
        }
        let parse = |text: &str| toml::from_str::<Wrap>(text).map(|w| w.fec);

        assert_eq!(parse(r#"fec = "auto""#), Ok(FecMode::Auto));
        assert_eq!(parse(r#"fec = "off""#), Ok(FecMode::Off));
        assert_eq!(
            parse(r#"fec = "ports:50002,50004""#),
            Ok(FecMode::Ports {
                column: 50_002,
                row: 50_004
            })
        );
        assert!(parse(r#"fec = "ports:50002""#).is_err());
        assert!(parse(r#"fec = "talvez""#).is_err());

        let text = toml::to_string(&Wrap {
            fec: FecMode::Ports {
                column: 50_002,
                row: 50_004,
            },
        })
        .expect("serializa");
        assert!(text.contains("ports:50002,50004"), "{text}");
    }

    /// SPEC-PROBE-IP-039a — uma janela de correlação abaixo de um tick não é
    /// implementável com contadores a 1 Hz: o valor efetivo vira o tick e a
    /// sessão continua, em vez de prometer uma resolução que não existe.
    #[test]
    fn spec_probe_ip_039a_sub_tick_correlation_window_is_rejected() {
        let c = ProbeConfig {
            correlation_window_ms: 200,
            ..Default::default()
        };
        assert_eq!(c.correlation_window(), Duration::from_secs(1));

        // Uma janela maior que o tick é respeitada.
        let wide = ProbeConfig {
            correlation_window_ms: 5_000,
            ..Default::default()
        };
        assert_eq!(wide.correlation_window(), Duration::from_secs(5));
    }

    /// SPEC-PROBE-IP-015 — o perfil aceita 33 e o PT dinâmico da FEC; 97 não.
    #[test]
    fn spec_probe_ip_015_profile_payload_types() {
        let c = ProbeConfig::default();
        assert!(c.accepts_payload_type(33));
        assert!(c.accepts_payload_type(96));
        assert!(!c.accepts_payload_type(97));

        // Lista vazia = sem restrição (desligar é pelo `enabled` do check).
        let any = ProbeConfig {
            rtp_payload_types: Vec::new(),
            ..Default::default()
        };
        assert!(any.accepts_payload_type(97));
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

    /// SPEC-PROBE-TS-015 — modelos sem parâmetros operacionais não podem ser
    /// habilitados por acidente no perfil.
    #[test]
    fn spec_probe_ts_015_advanced_transport_models_require_definition() {
        let mut c = ProbeConfig::default();
        c.transport.tstd_enabled = true;
        assert!(c.validate_transport().is_err());
        c.transport.tstd_enabled = false;
        c.transport.mgf_mgb_enabled = true;
        assert!(c.validate_transport().is_err());
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

    /// SPEC-PROBE-VID-001 — o serviço pode ser desligado sem afetar os demais.
    #[test]
    fn spec_probe_vid_001_service_profile_overrides_without_global_mutation() {
        let mut video = VideoProfileConfig::default();
        video.services.insert(
            42,
            VideoServiceProfile {
                enabled: Some(false),
                sample_fps: Some(4),
            },
        );
        let service = video.effective_for_service(42);
        assert!(!service.enabled);
        assert_eq!(service.sample_fps, 4);
        assert!(video.effective_for_service(7).enabled);
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
            reorder_window_ms: 0,
            calib_secs: 0,
            detect_secs: 0,
            ..Default::default()
        };
        assert_eq!(c.reorder_window(), Duration::from_millis(10));
        assert_eq!(c.calibration_window(), Duration::from_secs(1));
        assert_eq!(c.detect_window(), Duration::from_secs(1));
        assert_eq!(c.sample_interval(), Duration::from_millis(100));
        assert_eq!(c.rollup_window(), Duration::from_secs(1));
        assert_eq!(c.timeline_bucket(), Duration::from_secs(1));
        assert_eq!(c.summary_interval(), Duration::from_secs(1));
        assert_eq!(c.flush_interval(), Duration::from_secs(1));
        assert_eq!(c.snapshot_interval(), Duration::from_secs(1));
        assert_eq!(c.reconnect_backoff(0), Duration::from_millis(1000));
    }
}
