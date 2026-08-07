//! Run e sessão de monitoração: layout em disco e metadados.
//!
//! SPEC-PROBE-004 · SPEC-PROBE-006 · §6

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{FecMode, ProbeConfig};
use crate::sample::CSV_SCHEMA_VERSION;

/// Nome da pasta raiz das sessões, relativa à pasta do executável.
///
/// §6 — mesma pasta do `ironstream.toml`, o que mantém a portabilidade
/// "copiar a pasta para o notebook".
pub const SESSIONS_DIR: &str = "probe-sessions";

/// Nome do arquivo de metadados do run.
pub const RUN_FILE: &str = "run.toml";
/// Nome do arquivo de metadados da sessão de um feed.
pub const SESSION_FILE: &str = "session.toml";
/// Nome do CSV de amostras 1 Hz.
pub const METRICS_FILE: &str = "metrics.csv";
/// Nome do JSONL de eventos.
pub const EVENTS_FILE: &str = "events.jsonl";
/// Nome do relatório HTML do run.
pub const REPORT_FILE: &str = "report.html";

/// Encapsulamento detectado em runtime para um feed.
///
/// SPEC-PROBE-018a — os três convivem no mesmo mosaico.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encapsulation {
    /// Ainda não há datagrama suficiente para decidir.
    #[default]
    Unknown,
    /// UDP puro: o datagrama começa direto no sync byte 0x47.
    Udp,
    /// RTP (PT=33) sem tráfego nas portas de FEC.
    Rtp,
    /// RTP com tráfego em `base+2` e/ou `base+4` (ST 2022-1).
    RtpFec,
}

impl Encapsulation {
    /// Badge exibido no tile.
    ///
    /// SPEC-PROBE-018a
    pub fn badge(self) -> &'static str {
        match self {
            Self::Unknown => "—",
            Self::Udp => "UDP",
            Self::Rtp => "RTP",
            Self::RtpFec => "RTP+FEC",
        }
    }

    /// `true` quando os checks da camada RTP se aplicam a este feed.
    ///
    /// SPEC-PROBE-018a — num feed UDP puro eles ficam `n/a`, não verdes.
    pub fn has_rtp(self) -> bool {
        matches!(self, Self::Rtp | Self::RtpFec)
    }
}

/// Resumo final gravado em `session.toml` ao fechar a sessão.
///
/// SPEC-PROBE-004
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSummary {
    pub ended_utc: Option<DateTime<Utc>>,
    pub duration_secs: u64,
    pub samples: u64,
    pub availability_pct: f64,
    pub events_opened: u64,
    pub worst_severity: Option<String>,
    pub unavailable_periods: u64,
    pub unavailable_secs: u64,
}

/// Metadados de uma sessão (um feed dentro de um run).
///
/// SPEC-PROBE-004 — `session_id`, `run_id`, `feed_slot`, início/fim, feed,
/// host, interface e versões.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionMeta {
    pub session_id: String,
    pub run_id: String,
    pub feed_slot: usize,
    pub feed_name: String,
    pub feed_url: String,
    pub fec_mode: FecMode,
    pub encapsulation: Encapsulation,
    pub started_utc: Option<DateTime<Utc>>,
    pub host: String,
    pub interface: String,
    /// `SO_RCVBUF` efetivo do socket, em bytes.
    pub so_rcvbuf_bytes: usize,
    /// Piso de ruído do relógio de agendamento, em µs (SPEC-PROBE-013).
    pub noise_floor_us: f64,
    pub app_version: String,
    pub profile_version: u32,
    pub csv_schema_version: u32,
    pub summary: SessionSummary,
}

impl Default for SessionMeta {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            run_id: String::new(),
            feed_slot: 0,
            feed_name: String::new(),
            feed_url: String::new(),
            fec_mode: FecMode::Auto,
            encapsulation: Encapsulation::Unknown,
            started_utc: None,
            host: String::new(),
            interface: String::new(),
            so_rcvbuf_bytes: 0,
            noise_floor_us: 0.0,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            profile_version: 1,
            csv_schema_version: CSV_SCHEMA_VERSION,
            summary: SessionSummary::default(),
        }
    }
}

/// Metadados do run (`run.toml`).
///
/// SPEC-PROBE-004 · SPEC-PROBE-020
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunMeta {
    pub run_id: String,
    pub started_utc: Option<DateTime<Utc>>,
    pub ended_utc: Option<DateTime<Utc>>,
    pub host: String,
    pub app_version: String,
    pub profile_version: u32,
    /// Nome das subpastas de feed, na ordem dos slots.
    pub feeds: Vec<String>,
}

/// Um run aberto em disco.
///
/// SPEC-PROBE-004 — "Iniciar Probe cria uma pasta de sessão por feed sob o
/// mesmo `run_id`".
#[derive(Debug, Clone)]
pub struct ProbeRun {
    pub run_id: String,
    pub dir: PathBuf,
    pub started_utc: DateTime<Utc>,
}

/// Erros de I/O da camada de sessão.
///
/// RNF-PRB-003 — nenhum caminho de erro faz panic.
#[derive(Debug)]
pub enum SessionError {
    Io(std::io::Error),
    Serialize(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "erro de disco: {e}"),
            Self::Serialize(e) => write!(f, "erro ao serializar metadados: {e}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<std::io::Error> for SessionError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Raiz das sessões: `<pasta do executável>/probe-sessions/`.
///
/// §6
pub fn default_sessions_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(SESSIONS_DIR)
}

/// Nome de host da máquina, para os metadados.
///
/// Lê `COMPUTERNAME` (Windows) ou `HOSTNAME`; ausente ⇒ `"desconhecido"`.
/// Não vale acrescentar dependência só para isto.
pub fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "desconhecido".to_string())
}

impl ProbeRun {
    /// Cria a pasta do run.
    ///
    /// Nome: `<ISO-8601 com ':' trocado por '-'>_run`, p.ex.
    /// `2026-08-07T09-15-32_run` (§6).  Dois-pontos é ilegal em NTFS.
    ///
    /// SPEC-PROBE-004
    pub fn create(root: &Path, started_utc: DateTime<Utc>) -> Result<Self, SessionError> {
        let run_id = started_utc.format("%Y-%m-%dT%H-%M-%S").to_string();
        let dir = root.join(format!("{run_id}_run"));
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            run_id,
            dir,
            started_utc,
        })
    }

    /// Cria a subpasta de um feed e devolve seu caminho.
    ///
    /// Nome: `feed-<slot>_<grupo>-<porta>` — legível e único dentro do run,
    /// porque SPEC-PROBE-017 proíbe dois feeds no mesmo grupo/porta (§6).
    ///
    /// SPEC-PROBE-004
    pub fn create_feed_dir(&self, slot: usize, url: &str) -> Result<PathBuf, SessionError> {
        let dir = self.dir.join(feed_dir_name(slot, url));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Grava (ou regrava) o `run.toml`.
    ///
    /// SPEC-PROBE-004
    pub fn write_meta(&self, meta: &RunMeta) -> Result<(), SessionError> {
        let text =
            toml::to_string_pretty(meta).map_err(|e| SessionError::Serialize(e.to_string()))?;
        std::fs::write(self.dir.join(RUN_FILE), text)?;
        Ok(())
    }
}

/// Nome da subpasta de um feed dentro do run.
///
/// §6
pub fn feed_dir_name(slot: usize, url: &str) -> String {
    let host_port = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .trim_start_matches('@')
        .split('?')
        .next()
        .unwrap_or("")
        .replace(':', "-");
    let sanitized: String = host_port
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("feed-{slot}_{sanitized}")
}

/// Grava (ou regrava) o `session.toml` de um feed.
///
/// SPEC-PROBE-004
pub fn write_session_meta(dir: &Path, meta: &SessionMeta) -> Result<(), SessionError> {
    let text = toml::to_string_pretty(meta).map_err(|e| SessionError::Serialize(e.to_string()))?;
    std::fs::write(dir.join(SESSION_FILE), text)?;
    Ok(())
}

/// Monta os metadados iniciais de uma sessão.
///
/// SPEC-PROBE-004
pub fn new_session_meta(
    run: &ProbeRun,
    slot: usize,
    name: &str,
    url: &str,
    fec: FecMode,
    cfg: &ProbeConfig,
) -> SessionMeta {
    SessionMeta {
        session_id: format!("{}_feed-{slot}", run.run_id),
        run_id: run.run_id.clone(),
        feed_slot: slot,
        feed_name: if name.trim().is_empty() {
            url.to_string()
        } else {
            name.to_string()
        },
        feed_url: url.to_string(),
        fec_mode: fec,
        started_utc: Some(run.started_utc),
        host: host_name(),
        profile_version: cfg.profile_version,
        ..SessionMeta::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("timestamp")
    }

    /// §6 — o nome da pasta do run é válido em NTFS (sem ':').
    #[test]
    fn spec_probe_004_run_dir_name_is_ntfs_safe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = ProbeRun::create(dir.path(), utc(1_754_558_132)).expect("cria run");
        let name = run
            .dir
            .file_name()
            .and_then(|s| s.to_str())
            .expect("nome da pasta");
        assert!(!name.contains(':'), "':' é ilegal em NTFS: {name}");
        assert!(name.ends_with("_run"), "{name}");
        assert!(run.dir.is_dir());
    }

    /// §6 — a subpasta do feed usa `grupo-porta` e é única por slot.
    #[test]
    fn spec_probe_004_feed_dir_name_uses_group_and_port() {
        assert_eq!(
            feed_dir_name(0, "rtp://@239.15.0.183:50000"),
            "feed-0_239.15.0.183-50000"
        );
        assert_eq!(
            feed_dir_name(1, "udp://@239.15.0.190:50000"),
            "feed-1_239.15.0.190-50000"
        );
        // Query string e caracteres exóticos não vazam para o nome.
        assert_eq!(
            feed_dir_name(0, "udp://@239.1.2.3:1234?iface=10.0.0.1"),
            "feed-0_239.1.2.3-1234"
        );
    }

    /// SPEC-PROBE-004 — `run.toml` e `session.toml` fazem roundtrip.
    #[test]
    fn spec_probe_004_metadata_roundtrips_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = ProbeRun::create(dir.path(), utc(1_754_558_132)).expect("cria run");
        let cfg = ProbeConfig::default();

        let feed_dir = run
            .create_feed_dir(0, "rtp://@239.15.0.183:50000")
            .expect("cria feed dir");
        let meta = new_session_meta(
            &run,
            0,
            "0084_CANAL_A",
            "rtp://@239.15.0.183:50000",
            FecMode::Auto,
            &cfg,
        );
        write_session_meta(&feed_dir, &meta).expect("grava session.toml");

        let run_meta = RunMeta {
            run_id: run.run_id.clone(),
            started_utc: Some(run.started_utc),
            ended_utc: None,
            host: host_name(),
            app_version: "0.1.0".into(),
            profile_version: 1,
            feeds: vec![feed_dir_name(0, "rtp://@239.15.0.183:50000")],
        };
        run.write_meta(&run_meta).expect("grava run.toml");

        let back: SessionMeta = toml::from_str(
            &std::fs::read_to_string(feed_dir.join(SESSION_FILE)).expect("lê session.toml"),
        )
        .expect("desserializa session.toml");
        assert_eq!(back, meta);
        assert_eq!(back.feed_slot, 0);
        assert_eq!(back.csv_schema_version, CSV_SCHEMA_VERSION);

        let back_run: RunMeta =
            toml::from_str(&std::fs::read_to_string(run.dir.join(RUN_FILE)).expect("lê run.toml"))
                .expect("desserializa run.toml");
        assert_eq!(back_run, run_meta);
    }

    /// SPEC-PROBE-004 — feed sem nome cai na URL, nunca em string vazia.
    #[test]
    fn spec_probe_004_unnamed_feed_falls_back_to_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = ProbeRun::create(dir.path(), utc(0)).expect("run");
        let meta = new_session_meta(
            &run,
            1,
            "   ",
            "udp://@239.0.0.1:1234",
            FecMode::Off,
            &ProbeConfig::default(),
        );
        assert_eq!(meta.feed_name, "udp://@239.0.0.1:1234");
        assert_eq!(meta.session_id, format!("{}_feed-1", run.run_id));
    }

    /// SPEC-PROBE-018a — o badge distingue os três encapsulamentos e o
    /// "aplicável a RTP" reflete isso.
    #[test]
    fn spec_probe_018a_encapsulation_badges_and_applicability() {
        assert_eq!(Encapsulation::Udp.badge(), "UDP");
        assert_eq!(Encapsulation::Rtp.badge(), "RTP");
        assert_eq!(Encapsulation::RtpFec.badge(), "RTP+FEC");
        assert!(!Encapsulation::Udp.has_rtp());
        assert!(Encapsulation::Rtp.has_rtp());
        assert!(Encapsulation::RtpFec.has_rtp());
    }
}
