//! Modelo de estado da aplicação: `AppState`, `AppCommand`, `ConnectionState`,
//! `TablesSnapshot`.
//!
//! SPEC-UI-002

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use ts::metrics::{MetricsSnapshot, PcrJitterRecord};
use ts::tables::{Bat, Cat, EitEvent, Nit, Pat, Pmt, Sdt, Tdt, Tot};
use ts::Pid;

// ---------------------------------------------------------------------------
// AspectRatioMode
// ---------------------------------------------------------------------------

/// Modo de exibição do aspect-ratio do vídeo.
///
/// Controla como o `VideoPanel` calcula o retângulo de exibição.
/// Preferência puramente visual; não afeta o pipeline de decodificação.
///
/// SPEC-UI-001
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AspectRatioMode {
    /// Usa o DAR derivado do SAR sinalizado no stream (comportamento padrão).
    #[default]
    Dar,
    /// Força proporção 16:9 independente do que o stream reporta.
    Force16x9,
    /// Força proporção 4:3 independente do que o stream reporta.
    Force4x3,
}

impl AspectRatioMode {
    /// Retorna o aspect-ratio efetivo para exibição.
    ///
    /// `stream_aspect` é o aspect-ratio calculado a partir das dimensões de
    /// exibição SAR-corrigidas (`display_w / display_h`).
    pub fn effective_aspect(self, stream_aspect: f32) -> f32 {
        match self {
            Self::Dar => stream_aspect,
            Self::Force16x9 => 16.0 / 9.0,
            Self::Force4x3 => 4.0 / 3.0,
        }
    }
}

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
/// SPEC-UI-002 · SPEC-UI-009
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioTrackInfo {
    /// Serviço DVB ao qual a trilha pertence.
    pub service_id: u16,
    /// PID elementar do áudio.
    pub pid: Pid,
    /// `stream_type` da PMT (decimal), quando conhecido.
    pub stream_type: Option<u8>,
    /// Nome legível do codec atual.
    pub codec_label: String,
    /// Idioma ISO-639 quando disponível.
    pub language: Option<String>,
    /// Hint de canais do descriptor DVB (ex. AC-3 0x6A), antes do decode.
    pub descriptor_channel_hint: Option<u16>,
    /// Hint de perfil do descriptor DVB (ex. HE-AAC via 0x7C), antes do decode.
    pub descriptor_profile_hint: Option<String>,
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
/// SPEC-UI-002 · SPEC-UI-009
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
    /// Canais do elementary stream (antes de downmix).
    pub source_channels: Option<u16>,
    /// Canais efetivos da saída WASAPI.
    pub output_channels: Option<u16>,
    /// Número de canais efetivos da saída (alias de `output_channels`).
    pub channels: Option<u16>,
    /// Perfil do codec detectado pelo decoder (ex. `HE-AAC`).
    pub codec_profile: Option<String>,
    /// Bitrate codificado reportado pelo decoder, em kbps.
    pub encoded_bitrate_kbps: Option<f64>,
    /// Bitrate ao vivo do PID ativo (aggregator), em kbps.
    pub stream_bitrate_kbps: Option<f64>,
    /// Nível atual do jitter buffer em `[0.0, 1.0]`.
    pub buffer_level: f32,
    /// Latência estimada entre callback de áudio e playback audível.
    pub output_latency_ms: u64,
    /// Estado operacional do pipeline.
    pub state: AudioOperationalState,
    /// Contadores de erro acumulados.
    pub errors: AudioErrorSnapshot,
}

/// SPEC-UI-006b — texto compacto de canais para a status bar (`6ch > 2ch`).
pub fn format_status_bar_channels(source: u16, output: u16) -> String {
    if source != output {
        format!("{source}ch > {output}ch")
    } else {
        format!("{source}ch")
    }
}

/// SPEC-UI-009 — formata contagem de canais para o card (`6 ch`).
pub fn format_card_channels(channels: u16) -> String {
    format!("{channels} ch")
}

/// `true` quando o playback usa menos canais que o stream de origem.
pub fn audio_downmix_active(source: Option<u16>, output: Option<u16>) -> bool {
    match (source, output) {
        (Some(src), Some(out)) => src != out,
        _ => false,
    }
}

impl Default for AudioStatusSnapshot {
    fn default() -> Self {
        Self {
            volume: 1.0,
            muted: false,
            active_track: None,
            sample_rate_hz: None,
            source_channels: None,
            output_channels: None,
            channels: None,
            codec_profile: None,
            encoded_bitrate_kbps: None,
            stream_bitrate_kbps: None,
            buffer_level: 0.0,
            output_latency_ms: 0,
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

    /// Sincroniza canais de origem/saída e o alias legado `channels`.
    pub fn set_channel_counts(&mut self, source: u16, output: u16) {
        self.source_channels = Some(source);
        self.output_channels = Some(output);
        self.channels = Some(output);
    }

    /// Limpa os dados transitórios do stream mantendo preferências do usuário.
    pub fn reset_stream_runtime(&mut self, state: AudioOperationalState) {
        self.active_track = None;
        self.sample_rate_hz = None;
        self.source_channels = None;
        self.output_channels = None;
        self.channels = None;
        self.codec_profile = None;
        self.encoded_bitrate_kbps = None;
        self.stream_bitrate_kbps = None;
        self.buffer_level = 0.0;
        self.output_latency_ms = 0;
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
    /// Limpa todos os dados PSI/SI do stream atual.
    Reset,
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
    /// Snapshot mais recente da TOT.
    Tot(Tot),
    /// Snapshot mais recente da BAT.
    Bat(Bat),
    /// Snapshot mais recente da CAT.
    Cat(Cat),
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
    pub tot: Option<Tot>,
    pub bat: Option<Bat>,
    pub cat: Option<Cat>,
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
    /// Histórico de offset de sincronismo A/V dos últimos 60 s (em ms).
    ///
    /// Amostrado a ~1 Hz junto com o bitrate. Positivo = vídeo adiantado.
    ///
    /// SPEC-METRICS-SYNC-001
    pub av_sync_history: VecDeque<(Instant, i32)>,
}

impl AppState {
    /// Limpa dados derivados do stream atual, preservando preferências externas.
    pub(crate) fn reset_stream_data(&mut self) {
        self.metrics = MetricsSnapshot::default();
        self.tables = TablesSnapshot::default();
        self.selected_pid = None;
        self.selected_service = None;
        self.bitrate_history.clear();
        self.pcr_history.clear();
        self.av_sync_history.clear();
        self.audio.reset_stream_runtime(AudioOperationalState::Idle);
    }

    /// Aplica um evento incremental de tabela ao snapshot imutável da UI.
    ///
    /// SPEC-UI-002
    pub(crate) fn apply_table_event(&mut self, event: TableEvent) {
        match event {
            TableEvent::Reset => self.reset_stream_data(),
            TableEvent::Pat(pat) => self.tables.pat = Some(pat),
            TableEvent::Pmt(pmt) => {
                self.tables.pmts.insert(pmt.program_number, pmt);
            }
            TableEvent::Nit(nit) => self.tables.nit = Some(nit),
            // Aceita apenas SDT actual (table_id 0x42); SDT other (0x46) descreve
            // serviços de outros transport streams e não deve sobrescrever os dados locais.
            // O SDT pode ter múltiplas seções (last_section_number > 0); seções da mesma
            // versão são mescladas para acumular todos os serviços do multiplex.
            TableEvent::Sdt(sdt) if sdt.actual => match &mut self.tables.sdt {
                Some(existing) if existing.version == sdt.version => {
                    for svc in sdt.services {
                        if !existing
                            .services
                            .iter()
                            .any(|s| s.service_id == svc.service_id)
                        {
                            existing.services.push(svc);
                        }
                    }
                }
                _ => self.tables.sdt = Some(sdt),
            },
            TableEvent::Sdt(_) => {}
            TableEvent::EitPf {
                service_id,
                current,
                next,
            } => {
                self.tables.eit_pf.insert(service_id, (current, next));
            }
            TableEvent::Tdt(tdt) => self.tables.tdt = Some(tdt),
            TableEvent::Tot(tot) => self.tables.tot = Some(tot),
            TableEvent::Bat(bat) => self.tables.bat = Some(bat),
            TableEvent::Cat(cat) => self.tables.cat = Some(cat),
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
    /// Seleciona uma trilha de áudio dentro do serviço DVB atual.
    SelectAudio { service_id: u16, pid: Pid },
    /// Seleciona um PID para destaque nas métricas.
    SelectPid { pid: Pid },
    /// Ajusta o volume de áudio (0.0 – 1.0).
    SetVolume { volume: f32 },
    /// Limpa os contadores de erros acumulados.
    ResetErrors,
    /// Alterna entre tema escuro e claro.
    ChangeTheme { dark: bool },
    /// Solicita ao backend que troque o modo de aceleração de hardware do
    /// decoder de vídeo em runtime.
    ///
    /// SPEC-CFG-HW-001
    SetHwAccel { choice: HwAccelChoice },
    /// Notifica o backend que o renderer encontrou `DXGI_ERROR_DEVICE_REMOVED`.
    GpuDeviceRemoved,
}

// ---------------------------------------------------------------------------
// HwAccelChoice — seleção de hwaccel acessível pela UI
// ---------------------------------------------------------------------------

/// Seleção de hardware acceleration exposta para a UI (sem dependência de
/// `serde`/TOML).  Espelha as três opções aceitas pelo CLI e pelo
/// `ironstream.toml`; o binário converte para a sua própria
/// `config::HwAccelChoice` antes de aplicar no decoder.
///
/// SPEC-CFG-HW-001
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HwAccelChoice {
    /// Tenta D3D11VA quando o sistema suporta; cai em CPU caso contrário.
    #[default]
    Auto,
    /// Força D3D11VA; se não disponível, fica em CPU mas registra fallback.
    D3d11va,
    /// Desativa qualquer hwaccel; decode 100 % CPU.
    None,
}

impl HwAccelChoice {
    /// Identificador estável para logs e telemetria.
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::D3d11va => "d3d11va",
            Self::None => "none",
        }
    }
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

    /// `HwAccelChoice` expõe labels estáveis e default = Auto.
    ///
    /// SPEC-CFG-HW-001
    #[test]
    fn spec_cfg_hw_001_hwaccel_choice_labels() {
        assert_eq!(HwAccelChoice::default(), HwAccelChoice::Auto);
        assert_eq!(HwAccelChoice::Auto.label(), "auto");
        assert_eq!(HwAccelChoice::D3d11va.label(), "d3d11va");
        assert_eq!(HwAccelChoice::None.label(), "none");
    }

    /// `AppCommand::SetHwAccel` carrega o choice transportando-o intacto.
    ///
    /// SPEC-CFG-HW-001
    #[test]
    fn spec_cfg_hw_001_set_hwaccel_command_payload() {
        let cmd = AppCommand::SetHwAccel {
            choice: HwAccelChoice::D3d11va,
        };
        match cmd {
            AppCommand::SetHwAccel { choice } => {
                assert_eq!(choice, HwAccelChoice::D3d11va);
            }
            _ => panic!("variante incorreta"),
        }
    }

    #[test]
    fn spec_ui_002_format_status_bar_channels_downmix() {
        assert_eq!(format_status_bar_channels(6, 2), "6ch > 2ch");
        assert_eq!(format_status_bar_channels(2, 2), "2ch");
        assert_eq!(format_card_channels(6), "6 ch");
    }

    #[test]
    fn spec_ui_005_audio_card_channels_downmix_active() {
        let mut audio = AudioStatusSnapshot::default();
        audio.set_channel_counts(6, 2);
        assert!(audio_downmix_active(audio.source_channels, audio.output_channels));
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
        assert_eq!(audio.output_latency_ms, 0);
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

    /// SPEC-UI-002: seções SDT da mesma versão são mescladas; versão nova substitui.
    #[test]
    fn spec_ui_002_sdt_multi_section_merge() {
        use ts::tables::{RunningStatus, SdtService};

        let make_svc = |id: u16, name: &str| SdtService {
            service_id: id,
            eit_schedule_flag: false,
            eit_present_following: false,
            running_status: RunningStatus::Running,
            free_ca_mode: false,
            service_name: Some(name.to_owned()),
            provider_name: None,
            service_type: None,
            descriptors: vec![],
        };
        let make_sdt = |version: u8, services: Vec<SdtService>| Sdt {
            transport_stream_id: 1,
            original_network_id: 1,
            version,
            actual: true,
            services,
        };

        let mut state = AppState::default();

        // Seção 0: service 0x0001 "Service01"
        state.apply_table_event(TableEvent::Sdt(make_sdt(
            3,
            vec![make_svc(0x0001, "Service01")],
        )));
        assert_eq!(state.tables.sdt.as_ref().unwrap().services.len(), 1);

        // Seção 1 (mesma versão): service 0x0010 "Globo" — deve mesclar
        state.apply_table_event(TableEvent::Sdt(make_sdt(
            3,
            vec![make_svc(0x0010, "Globo")],
        )));
        let sdt = state.tables.sdt.as_ref().unwrap();
        assert_eq!(sdt.services.len(), 2);
        assert!(sdt.services.iter().any(|s| s.service_id == 0x0001));
        assert!(sdt.services.iter().any(|s| s.service_id == 0x0010));

        // Mesma seção repetida não duplica
        state.apply_table_event(TableEvent::Sdt(make_sdt(
            3,
            vec![make_svc(0x0001, "Service01")],
        )));
        assert_eq!(state.tables.sdt.as_ref().unwrap().services.len(), 2);

        // Nova versão substitui completamente
        state.apply_table_event(TableEvent::Sdt(make_sdt(
            4,
            vec![make_svc(0x0010, "Globo v2")],
        )));
        let sdt = state.tables.sdt.as_ref().unwrap();
        assert_eq!(sdt.services.len(), 1);
        assert_eq!(sdt.services[0].service_id, 0x0010);
    }

    #[test]
    fn spec_ui_002_table_reset_clears_stream_state() {
        let mut state = AppState::default();
        state.selected_pid = Some(0x0100);
        state.selected_service = Some(16);
        state.bitrate_history.push_back((Instant::now(), 1_000.0));
        state.metrics.total_bitrate_kbps = 1_000.0;
        state.audio.active_track = Some(AudioTrackInfo {
            service_id: 16,
            pid: 0x0112,
            codec_label: "AAC".to_owned(),
            language: Some("por".to_owned()),
            stream_type: Some(0x11),
            ..Default::default()
        });
        state.tables.pat = Some(Pat {
            transport_stream_id: 1,
            version: 3,
            current_next: true,
            programs: Vec::new(),
        });

        state.apply_table_event(TableEvent::Reset);

        assert!(state.tables.pat.is_none());
        assert!(state.selected_pid.is_none());
        assert!(state.selected_service.is_none());
        assert!(state.bitrate_history.is_empty());
        assert!(state.pcr_history.is_empty());
        assert_eq!(state.metrics.total_bitrate_kbps, 0.0);
        assert!(state.audio.active_track.is_none());
        assert_eq!(state.audio.state, AudioOperationalState::Idle);
    }
}
