//! Modelo de estado da aplicação: `AppState`, `AppCommand`, `ConnectionState`,
//! `TablesSnapshot`.
//!
//! SPEC-UI-002

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use ts::metrics::{MetricsSnapshot, PcrJitterRecord};
use ts::tables::{Bat, EitEvent, Nit, Pat, Pmt, Sdt, Tdt};
use ts::Pid;

// ---------------------------------------------------------------------------
// AudioStatusSnapshot
// ---------------------------------------------------------------------------

/// Estado operacional atual do pipeline de áudio.
///
/// SPEC-UI-002
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AudioOperationalState {
    /// Sem stream de áudio selecionado ou pipeline parado.
    #[default]
    Idle,
    /// A UI já conhece a trilha, mas ainda aguarda frames suficientes.
    Buffering,
    /// Reprodução em andamento.
    Playing,
    /// Saída de áudio em recuperação após falha do dispositivo.
    Recovering,
    /// Pipeline com falha operacional recente.
    Error,
}

/// Metadados da trilha de áudio atualmente ativa.
///
/// SPEC-UI-002
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioTrackInfo {
    /// Serviço DVB ao qual a trilha pertence.
    pub service_id: u16,
    /// PID elementar do áudio.
    pub pid: Pid,
    /// Nome legível do codec atual.
    pub codec_label: String,
    /// Idioma ISO-639 quando disponível.
    pub language: Option<String>,
}

/// Snapshot dos contadores de erro observados pelo pipeline de áudio.
///
/// SPEC-UI-002
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioErrorSnapshot {
    /// Total de falhas de decode acumuladas.
    pub decode_errors: u64,
    /// Total de falhas de saída/recriação do dispositivo.
    pub output_errors: u64,
    /// Total de underruns reportados pelo callback WASAPI.
    pub underruns: u64,
    /// Total de overruns no jitter buffer.
    pub overruns: u64,
    /// Última mensagem de erro relevante observada.
    pub last_error: Option<String>,
}

/// Snapshot imutável das métricas e estado operacional do áudio.
///
/// SPEC-UI-002
#[derive(Debug, Clone, PartialEq)]
pub struct AudioStatusSnapshot {
    /// Volume atual normalizado em `[0.0, 1.0]`.
    pub volume: f32,
    /// `true` quando o áudio está mutado.
    pub muted: bool,
    /// Trilha de áudio atualmente ativa.
    pub active_track: Option<AudioTrackInfo>,
    /// Taxa de amostragem efetiva da saída em Hz.
    pub sample_rate_hz: Option<u32>,
    /// Número de canais efetivos da saída.
    pub channels: Option<u16>,
    /// Nível atual do jitter buffer em `[0.0, 1.0]`.
    pub buffer_level: f32,
    /// Estado operacional do pipeline.
    pub state: AudioOperationalState,
    /// Contadores de erro acumulados.
    pub errors: AudioErrorSnapshot,
}

impl Default for AudioStatusSnapshot {
    fn default() -> Self {
        Self {
            volume: 1.0,
            muted: false,
            active_track: None,
            sample_rate_hz: None,
            channels: None,
            buffer_level: 0.0,
            state: AudioOperationalState::Idle,
            errors: AudioErrorSnapshot::default(),
        }
    }
}

impl AudioStatusSnapshot {
    /// Atualiza o volume normalizado e recalcula o flag de mute.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.muted = self.volume <= f32::EPSILON;
    }

    /// Limpa os dados transitórios do stream mantendo preferências do usuário.
    pub fn reset_stream_runtime(&mut self, state: AudioOperationalState) {
        self.active_track = None;
        self.sample_rate_hz = None;
        self.channels = None;
        self.buffer_level = 0.0;
        self.state = state;
        self.errors = AudioErrorSnapshot::default();
    }
}

// ---------------------------------------------------------------------------
// TableEvent
// ---------------------------------------------------------------------------

/// Evento incremental de tabela PSI/SI recebido do pipeline.
///
/// SPEC-UI-002
#[derive(Debug, Clone)]
pub enum TableEvent {
    /// Snapshot mais recente da PAT.
    Pat(Pat),
    /// Snapshot mais recente de uma PMT.
    Pmt(Pmt),
    /// Snapshot mais recente da NIT.
    Nit(Nit),
    /// Snapshot mais recente da SDT.
    Sdt(Sdt),
    /// Present/following extraído de EIT p/f.
    EitPf {
        service_id: u16,
        current: Option<EitEvent>,
        next: Option<EitEvent>,
    },
    /// Snapshot mais recente da TDT.
    Tdt(Tdt),
    /// Snapshot mais recente da BAT.
    Bat(Bat),
}

// ---------------------------------------------------------------------------
// ConnectionState
// ---------------------------------------------------------------------------

/// Estado atual da conexão com a fonte de stream.
///
/// SPEC-UI-002
#[derive(Debug, Clone, Default)]
pub enum ConnectionState {
    /// Nenhuma conexão ativa ou pendente.
    #[default]
    Idle,
    /// Conectando à URL informada.
    Connecting { url: String },
    /// Conexão estabelecida.
    Connected { url: String, since: Instant },
    /// Erro durante a conexão ou recepção.
    Error { url: String, reason: String },
}

// ---------------------------------------------------------------------------
// TablesSnapshot
// ---------------------------------------------------------------------------

/// Snapshot imutável das tabelas PSI/SI mais recentes recebidas.
///
/// SPEC-UI-002
#[derive(Debug, Clone, Default)]
pub struct TablesSnapshot {
    pub pat: Option<Pat>,
    /// `program_number` → `Pmt`
    pub pmts: HashMap<u16, Pmt>,
    pub nit: Option<Nit>,
    pub sdt: Option<Sdt>,
    /// `service_id` → `(atual, próximo)`
    pub eit_pf: HashMap<u16, (Option<EitEvent>, Option<EitEvent>)>,
    pub tdt: Option<Tdt>,
    pub bat: Option<Bat>,
}

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

/// Estado completo da interface, atualizado a cada frame a partir dos
/// snapshots do pipeline.
///
/// SPEC-UI-002
#[derive(Default)]
pub struct AppState {
    pub connection: ConnectionState,
    pub metrics: MetricsSnapshot,
    pub audio: AudioStatusSnapshot,
    pub tables: TablesSnapshot,
    pub selected_pid: Option<Pid>,
    pub selected_service: Option<u16>,
    /// Histórico de bitrate total dos últimos 60 s.
    pub bitrate_history: VecDeque<(Instant, f64)>,
    /// Histórico de jitter de PCR por PID.
    pub pcr_history: HashMap<Pid, VecDeque<PcrJitterRecord>>,
}

impl AppState {
    /// Aplica um evento incremental de tabela ao snapshot imutável da UI.
    ///
    /// SPEC-UI-002
    pub(crate) fn apply_table_event(&mut self, event: TableEvent) {
        match event {
            TableEvent::Pat(pat) => self.tables.pat = Some(pat),
            TableEvent::Pmt(pmt) => {
                self.tables.pmts.insert(pmt.program_number, pmt);
            }
            TableEvent::Nit(nit) => self.tables.nit = Some(nit),
            TableEvent::Sdt(sdt) => self.tables.sdt = Some(sdt),
            TableEvent::EitPf {
                service_id,
                current,
                next,
            } => {
                self.tables.eit_pf.insert(service_id, (current, next));
            }
            TableEvent::Tdt(tdt) => self.tables.tdt = Some(tdt),
            TableEvent::Bat(bat) => self.tables.bat = Some(bat),
        }
    }
}

// ---------------------------------------------------------------------------
// AppCommand
// ---------------------------------------------------------------------------

/// Comandos enviados pela UI ao backend via canal MPSC bounded.
///
/// SPEC-UI-002
#[derive(Debug, Clone)]
pub enum AppCommand {
    /// Inicia conexão com a URL informada, opcionalmente ligada a uma
    /// interface de rede específica.
    Connect { url: String, iface: Option<String> },
    /// Encerra a conexão ativa.
    Disconnect,
    /// Seleciona um serviço DVB para exibição no `VideoPanel`.
    SelectService { service_id: u16 },
    /// Seleciona um PID para destaque nas métricas.
    SelectPid { pid: Pid },
    /// Ajusta o volume de áudio (0.0 – 1.0).
    SetVolume { volume: f32 },
    /// Limpa os contadores de erros acumulados.
    ResetErrors,
    /// Alterna entre tema escuro e claro.
    ChangeTheme { dark: bool },
}

// ---------------------------------------------------------------------------
// Testes
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_ui_002_app_state_default_is_idle() {
        let state = AppState::default();
        assert!(matches!(state.connection, ConnectionState::Idle));
        assert_eq!(state.audio.volume, 1.0);
        assert!(!state.audio.muted);
        assert_eq!(state.audio.state, AudioOperationalState::Idle);
        assert!(state.selected_pid.is_none());
        assert!(state.selected_service.is_none());
        assert!(state.bitrate_history.is_empty());
        assert!(state.pcr_history.is_empty());
    }

    #[test]
    fn spec_ui_002_audio_status_snapshot_default_is_idle() {
        let audio = AudioStatusSnapshot::default();
        assert_eq!(audio.volume, 1.0);
        assert!(!audio.muted);
        assert!(audio.active_track.is_none());
        assert_eq!(audio.sample_rate_hz, None);
        assert_eq!(audio.channels, None);
        assert_eq!(audio.buffer_level, 0.0);
        assert_eq!(audio.state, AudioOperationalState::Idle);
        assert_eq!(audio.errors, AudioErrorSnapshot::default());
    }

    #[test]
    fn spec_ui_002_audio_status_snapshot_set_volume_updates_mute() {
        let mut audio = AudioStatusSnapshot::default();
        audio.set_volume(0.0);
        assert_eq!(audio.volume, 0.0);
        assert!(audio.muted);

        audio.set_volume(0.75);
        assert_eq!(audio.volume, 0.75);
        assert!(!audio.muted);
    }

    #[test]
    fn spec_ui_002_tables_snapshot_default_all_none() {
        let snap = TablesSnapshot::default();
        assert!(snap.pat.is_none());
        assert!(snap.pmts.is_empty());
        assert!(snap.nit.is_none());
        assert!(snap.sdt.is_none());
        assert!(snap.eit_pf.is_empty());
        assert!(snap.tdt.is_none());
        assert!(snap.bat.is_none());
    }

    #[test]
    fn spec_ui_002_connection_state_default_is_idle() {
        let cs = ConnectionState::default();
        assert!(matches!(cs, ConnectionState::Idle));
    }

    #[test]
    fn spec_ui_002_apply_table_event_updates_pat_snapshot() {
        let mut state = AppState::default();
        let pat = Pat {
            transport_stream_id: 1,
            version: 3,
            current_next: true,
            programs: Vec::new(),
        };

        state.apply_table_event(TableEvent::Pat(pat.clone()));

        assert_eq!(state.tables.pat, Some(pat));
    }
}
