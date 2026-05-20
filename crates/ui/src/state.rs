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
        assert!(state.selected_pid.is_none());
        assert!(state.selected_service.is_none());
        assert!(state.bitrate_history.is_empty());
        assert!(state.pcr_history.is_empty());
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
