//! Dados do menu de contexto do painel de vídeo (PopupWindow Slint).

use slint::{ModelRc, SharedString, VecModel};

use crate::state::{AppState, AspectRatioMode, ConnectionState};
use crate::MenuEntry;
use ts::tables::PmtStream;
use ts::Pid;

/// Monta os modelos do menu de contexto a partir do estado atual.
pub fn build_menu_models(
    state: &AppState,
    aspect_ratio: AspectRatioMode,
) -> (
    ModelRc<MenuEntry>,
    ModelRc<MenuEntry>,
    ModelRc<MenuEntry>,
    ModelRc<MenuEntry>,
    ModelRc<MenuEntry>,
) {
    let connected = matches!(state.connection, ConnectionState::Connected { .. });
    (
        ModelRc::new(VecModel::from(build_services(state, connected))),
        ModelRc::new(VecModel::from(build_video(state, connected))),
        ModelRc::new(VecModel::from(build_audio(state, connected))),
        ModelRc::new(VecModel::from(build_subtitles(state))),
        ModelRc::new(VecModel::from(build_aspect(aspect_ratio))),
    )
}

fn build_services(state: &AppState, connected: bool) -> Vec<MenuEntry> {
    let services: Vec<u16> = state
        .tables
        .pat
        .as_ref()
        .map(|pat| {
            pat.programs
                .iter()
                .filter(|p| p.program_number != 0)
                .map(|p| p.program_number)
                .collect()
        })
        .unwrap_or_default();

    if services.is_empty() {
        return vec![disabled_entry("(nenhum serviço)")];
    }

    services
        .into_iter()
        .map(|svc_id| {
            let name = service_name(state, svc_id);
            let active = state.selected_service == Some(svc_id);
            MenuEntry {
                label: SharedString::from(name),
                id: svc_id as i32,
                active,
                enabled: connected,
            }
        })
        .collect()
}

fn build_video(state: &AppState, connected: bool) -> Vec<MenuEntry> {
    let active_pid = active_video_pid(state);
    let streams: Vec<(Pid, String)> = state
        .selected_service
        .and_then(|svc_id| state.tables.pmts.get(&svc_id))
        .map(|pmt| {
            pmt.streams
                .iter()
                .filter(|s| is_video_stream(s))
                .map(|stream| {
                    let label = format!(
                        "{}  {}",
                        stream.elementary_pid,
                        stream.label(),
                    );
                    (stream.elementary_pid, label)
                })
                .collect()
        })
        .unwrap_or_default();

    if streams.is_empty() {
        return vec![disabled_entry("(nenhum)")];
    }

    streams
        .into_iter()
        .map(|(pid, label)| MenuEntry {
            label: SharedString::from(label),
            id: pid as i32,
            active: active_pid == Some(pid),
            enabled: connected,
        })
        .collect()
}

fn build_audio(state: &AppState, connected: bool) -> Vec<MenuEntry> {
    let streams: Vec<(Pid, String)> = state
        .selected_service
        .and_then(|svc_id| state.tables.pmts.get(&svc_id))
        .map(|pmt| {
            pmt.streams
                .iter()
                .filter(|s| is_audio_stream(s))
                .map(|stream| {
                    let language = audio_language(stream)
                        .map(|language| format!(" [{language}]"))
                        .unwrap_or_default();
                    let label = format!(
                        "{}  {}{}",
                        stream.elementary_pid,
                        stream.label(),
                        language,
                    );
                    (stream.elementary_pid, label)
                })
                .collect()
        })
        .unwrap_or_default();

    if streams.is_empty() {
        return vec![disabled_entry("(nenhum)")];
    }

    streams
        .into_iter()
        .map(|(pid, label)| MenuEntry {
            label: SharedString::from(label),
            id: pid as i32,
            active: state
                .audio
                .active_track
                .as_ref()
                .is_some_and(|t| t.pid == pid),
            enabled: connected,
        })
        .collect()
}

fn build_subtitles(state: &AppState) -> Vec<MenuEntry> {
    let pids: Vec<Pid> = state
        .selected_service
        .and_then(|svc_id| state.tables.pmts.get(&svc_id))
        .map(|pmt| {
            pmt.streams
                .iter()
                .filter(|s| is_subtitle_stream(s))
                .map(|s| s.elementary_pid)
                .collect()
        })
        .unwrap_or_default();

    if pids.is_empty() {
        return vec![disabled_entry("(nenhum)")];
    }

    // Legendas ainda não têm handler no backend — itens desabilitados (como na UI egui).
    pids.into_iter()
        .map(|pid| MenuEntry {
            label: SharedString::from(format!("0x{pid:04X}  Legenda")),
            id: pid as i32,
            active: false,
            enabled: false,
        })
        .collect()
}

fn build_aspect(mode: AspectRatioMode) -> Vec<MenuEntry> {
    [
        (AspectRatioMode::Dar, "DAR (padrão)", 0),
        (AspectRatioMode::Force16x9, "16:9 (forçado)", 1),
        (AspectRatioMode::Force4x3, "4:3 (forçado)", 2),
    ]
    .into_iter()
    .map(|(m, label, id)| {
        let active = mode == m;
        MenuEntry {
            label: SharedString::from(label),
            id,
            active,
            enabled: true,
        }
    })
    .collect()
}

fn disabled_entry(label: &str) -> MenuEntry {
    MenuEntry {
        label: SharedString::from(label),
        id: -1,
        active: false,
        enabled: false,
    }
}

fn service_name(state: &AppState, service_id: u16) -> String {
    state
        .tables
        .sdt
        .as_ref()
        .and_then(|sdt| sdt.services.iter().find(|s| s.service_id == service_id))
        .and_then(|s| s.service_name.clone())
        .unwrap_or_else(|| format!("Serviço 0x{service_id:04X}"))
}

fn is_audio_stream(stream: &PmtStream) -> bool {
    stream.is_audio()
}

fn is_video_stream(stream: &PmtStream) -> bool {
    matches!(stream.stream_type, 0x01 | 0x02 | 0x1B | 0x24)
}

fn is_subtitle_stream(stream: &PmtStream) -> bool {
    stream.is_private_data()
}

fn active_video_pid(state: &AppState) -> Option<Pid> {
    state
        .selected_video_pid
        .filter(|pid| video_track_exists(state, *pid))
        .or_else(|| first_video_pid(state))
}

fn video_track_exists(state: &AppState, pid: Pid) -> bool {
    state
        .selected_service
        .and_then(|svc_id| state.tables.pmts.get(&svc_id))
        .is_some_and(|pmt| {
            pmt.streams
                .iter()
                .any(|s| s.elementary_pid == pid && is_video_stream(s))
        })
}

fn first_video_pid(state: &AppState) -> Option<Pid> {
    state
        .selected_service
        .and_then(|svc_id| state.tables.pmts.get(&svc_id))
        .and_then(|pmt| {
            pmt.streams
                .iter()
                .find(|s| is_video_stream(s))
                .map(|s| s.elementary_pid)
        })
}

fn audio_language(stream: &PmtStream) -> Option<String> {
    stream
        .descriptors
        .iter()
        .find(|descriptor| descriptor.tag == 0x0A && descriptor.data.len() >= 3)
        .map(|descriptor| {
            String::from_utf8_lossy(&descriptor.data[..3])
                .trim()
                .to_lowercase()
        })
        .filter(|language| !language.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ts::tables::{Descriptor, Pat, PatProgram, PmtStream, Sdt, SdtService};

    #[test]
    fn spec_ui_001_services_from_pat_exclude_nit() {
        let mut state = AppState::default();
        state.tables.pat = Some(Pat {
            transport_stream_id: 1,
            version: 0,
            current_next: true,
            programs: vec![
                PatProgram {
                    program_number: 0,
                    pid: 0x0010,
                },
                PatProgram {
                    program_number: 0x0101,
                    pid: 0x0100,
                },
            ],
        });

        let entries = build_services(&state, true);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, 0x0101);
    }

    #[test]
    fn spec_ui_001_service_name_from_sdt() {
        let mut state = AppState::default();
        state.tables.sdt = Some(Sdt {
            transport_stream_id: 1,
            original_network_id: 1,
            version: 0,
            actual: true,
            services: vec![SdtService {
                service_id: 0x0101,
                eit_schedule_flag: false,
                eit_present_following: false,
                running_status: ts::tables::RunningStatus::Running,
                free_ca_mode: false,
                service_name: Some("Canal HD".into()),
                provider_name: None,
                service_type: None,
                descriptors: vec![],
            }],
        });
        assert_eq!(service_name(&state, 0x0101), "Canal HD");
    }

    #[test]
    fn spec_ui_001_audio_language_reads_iso639_descriptor() {
        let stream = PmtStream {
            stream_type: 0x0F,
            elementary_pid: 0x0120,
            descriptors: vec![Descriptor::new(0x0A, b"por\x00".to_vec())],
        };
        assert_eq!(audio_language(&stream), Some("por".to_string()));
    }

    #[test]
    fn spec_ui_001_video_menu_lists_multiple_streams() {
        use ts::tables::Pmt;

        let mut state = AppState::default();
        state.connection = ConnectionState::Connected {
            url: "udp://@239.0.0.1:1234".into(),
            since: std::time::Instant::now(),
        };
        state.selected_service = Some(1);
        state.tables.pmts.insert(
            1,
            Pmt {
                program_number: 1,
                pcr_pid: 0x0100,
                version: 0,
                current_next: true,
                program_descriptors: vec![],
                streams: vec![
                    PmtStream {
                        stream_type: 0x1B,
                        elementary_pid: 0x0100,
                        descriptors: vec![],
                    },
                    PmtStream {
                        stream_type: 0x1B,
                        elementary_pid: 0x0110,
                        descriptors: vec![],
                    },
                ],
            },
        );

        let entries = build_video(&state, true);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].active);
        assert!(!entries[1].active);

        state.selected_video_pid = Some(0x0110);
        let entries = build_video(&state, true);
        assert!(!entries[0].active);
        assert!(entries[1].active);
    }
}
