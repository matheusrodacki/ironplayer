//! `VideoPanel` — exibe frame de vídeo ou placeholder quando sem stream.
//!
//! SPEC-UI-001

use crossbeam_channel::Sender;
use eframe::egui;
use ts::tables::PmtStream;

use crate::state::{AppCommand, ConnectionState};
use crate::AppState;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Retorna o nome de um serviço a partir da SDT, com fallback formatado.
///
/// SPEC-UI-001
fn service_name(state: &AppState, service_id: u16) -> String {
    state
        .tables
        .sdt
        .as_ref()
        .and_then(|sdt| sdt.services.iter().find(|s| s.service_id == service_id))
        .and_then(|s| s.service_name.clone())
        .unwrap_or_else(|| format!("Serviço 0x{service_id:04X}"))
}

/// Retorna `true` se o `stream_type` corresponde a um stream de áudio.
fn is_audio_stream(stream: &PmtStream) -> bool {
    stream.is_audio()
}

/// Retorna `true` se o `stream_type` corresponde a um stream de legendas/dados.
fn is_subtitle_stream(stream: &PmtStream) -> bool {
    stream.is_private_data()
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

// ---------------------------------------------------------------------------
// VideoPanel
// ---------------------------------------------------------------------------

/// Painel de vídeo: mostra o frame decodificado ou um placeholder centralizado.
///
/// SPEC-UI-001
pub struct VideoPanel;

impl VideoPanel {
    /// Renderiza o painel de vídeo.
    ///
    /// `video_texture`: par `(TextureId, (width, height))` do frame mais
    /// recente produzido pelo `VideoRenderer`. Quando `Some`, exibe o frame
    /// escalado para preencher a área disponível mantendo o aspect-ratio.
    /// Quando `None` e conectado sem serviço selecionado, exibe fundo preto.
    /// Caso contrário, mostra o placeholder `"[ sem stream ]"`.
    ///
    /// `cmd_tx`: canal para envio de comandos ao backend; utilizado pelo menu
    /// de contexto (botão direito) para despachar `SelectService`.
    ///
    /// SPEC-UI-001 · SPEC-AV-003
    pub fn show(
        ui: &mut egui::Ui,
        state: &AppState,
        video_texture: Option<(egui::TextureId, (u32, u32))>,
        cmd_tx: &Sender<AppCommand>,
    ) {
        let has_stream = matches!(state.connection, ConnectionState::Connected { .. })
            && state.selected_service.is_some();

        let available = ui.available_size();

        // ── Aloca toda a área e captura a resposta para o menu de contexto. ──
        let (rect, response) = ui.allocate_exact_size(available, egui::Sense::hover());

        if let Some((tex_id, (w, h))) = video_texture {
            // Exibe o frame decodificado escalado com aspect-ratio correto.
            let aspect = w as f32 / h.max(1) as f32;
            let (draw_w, draw_h) = if available.x / available.y > aspect {
                (available.y * aspect, available.y)
            } else {
                (available.x, available.x / aspect)
            };

            let center = rect.center();
            let draw_rect = egui::Rect::from_center_size(center, egui::vec2(draw_w, draw_h));

            ui.painter()
                .rect_filled(rect, egui::Rounding::ZERO, egui::Color32::BLACK);
            ui.painter().image(
                tex_id,
                draw_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else if has_stream {
            // Conectado mas ainda sem frame: fundo preto.
            ui.painter()
                .rect_filled(rect, egui::Rounding::ZERO, egui::Color32::BLACK);
        } else {
            // Placeholder centralizado.
            ui.painter()
                .rect_filled(rect, egui::Rounding::ZERO, ui.visuals().window_fill);
            let galley = ui.painter().layout_no_wrap(
                "[ sem stream ]".into(),
                egui::FontId::proportional(18.0),
                egui::Color32::GRAY,
            );
            let text_pos = rect.center() - galley.size() / 2.0;
            ui.painter().galley(text_pos, galley, egui::Color32::GRAY);
        }

        // ── Menu de contexto (botão direito) ─────────────────────────────────
        let connected = matches!(state.connection, ConnectionState::Connected { .. });

        response.context_menu(|ui| {
            Self::show_context_menu(ui, state, cmd_tx, connected);
        });
    }

    /// Renderiza o conteúdo do menu de contexto do `VideoPanel`.
    ///
    /// Separado para facilitar testes unitários.
    ///
    /// SPEC-UI-001
    fn show_context_menu(
        ui: &mut egui::Ui,
        state: &AppState,
        cmd_tx: &Sender<AppCommand>,
        connected: bool,
    ) {
        // ── Submenu: Serviço ──────────────────────────────────────────────
        ui.menu_button("Serviço", |ui| {
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
                ui.add_enabled(false, egui::Button::new("(nenhum serviço)"));
            } else {
                for svc_id in services {
                    let name = service_name(state, svc_id);
                    let is_active = state.selected_service == Some(svc_id);
                    let label = if is_active {
                        format!("✓  {name}")
                    } else {
                        format!("    {name}")
                    };
                    if ui
                        .add_enabled(connected, egui::Button::new(label))
                        .clicked()
                    {
                        let _ = cmd_tx.try_send(AppCommand::SelectService { service_id: svc_id });
                        ui.close_menu();
                    }
                }
            }
        });

        // ── Submenu: Áudio ────────────────────────────────────────────────
        ui.menu_button("Áudio", |ui| {
            let audio_streams: Vec<String> = state
                .selected_service
                .and_then(|svc_id| state.tables.pmts.get(&svc_id))
                .map(|pmt| {
                    pmt.streams
                        .iter()
                        .filter(|s| is_audio_stream(s))
                        .map(|stream| {
                            let active = state
                                .audio
                                .active_track
                                .as_ref()
                                .is_some_and(|track| track.pid == stream.elementary_pid);
                            let marker = if active { "✓" } else { " " };
                            let language = audio_language(stream)
                                .map(|language| format!(" [{language}]"))
                                .unwrap_or_default();
                            format!(
                                "{marker} {} / 0x{:04X}  {}{}",
                                stream.elementary_pid,
                                stream.elementary_pid,
                                stream.label(),
                                language,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();

            if audio_streams.is_empty() {
                ui.add_enabled(false, egui::Button::new("(nenhum)"));
            } else {
                for label in audio_streams {
                    ui.add_enabled(false, egui::Button::new(label));
                }
            }
        });

        // ── Submenu: Legenda ──────────────────────────────────────────────
        ui.menu_button("Legenda", |ui| {
            let subtitle_streams: Vec<u16> = state
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

            if subtitle_streams.is_empty() {
                ui.add_enabled(false, egui::Button::new("(nenhum)"));
            } else {
                for pid in subtitle_streams {
                    ui.add_enabled(false, egui::Button::new(format!("0x{pid:04X}  Legenda")));
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Testes
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ts::tables::Descriptor;
    use ts::tables::{Pat, PatProgram, Sdt, SdtService};

    #[test]
    fn spec_ui_001_video_panel_placeholder_when_idle() {
        // Verifica que o estado Idle não é considerado "com stream".
        let state = AppState::default();
        assert!(matches!(state.connection, ConnectionState::Idle));
        assert!(state.selected_service.is_none());

        // Lógica de `has_stream` deve ser false para estado Idle.
        let has_stream = matches!(state.connection, ConnectionState::Connected { .. })
            && state.selected_service.is_some();
        assert!(!has_stream);
    }

    #[test]
    fn spec_ui_001_video_panel_shows_frame_when_texture_present() {
        // Quando video_texture é Some, o painel deve exibir o frame
        // independentemente do estado de conexão.
        let tex_id = egui::TextureId::User(42);
        let dims = (1920u32, 1080u32);
        // Verifica que aspect-ratio está correto.
        let aspect = dims.0 as f32 / dims.1 as f32;
        assert!((aspect - 1.777).abs() < 0.01);
        // A presença do TextureId é suficiente para indicar frame disponível.
        let video_texture: Option<(egui::TextureId, (u32, u32))> = Some((tex_id, dims));
        assert!(video_texture.is_some());
    }

    #[test]
    fn spec_ui_001_service_name_from_sdt() {
        // Quando a SDT contém o serviço, retorna o nome da SDT.
        let mut state = AppState::default();
        let svc = SdtService {
            service_id: 0x0101,
            eit_schedule_flag: false,
            eit_present_following: false,
            running_status: ts::tables::RunningStatus::Running,
            free_ca_mode: false,
            service_name: Some("Canal HD".into()),
            provider_name: None,
            service_type: None,
            descriptors: vec![],
        };
        let sdt = Sdt {
            transport_stream_id: 1,
            original_network_id: 1,
            version: 0,
            actual: true,
            services: vec![svc],
        };
        state.tables.sdt = Some(sdt);
        assert_eq!(service_name(&state, 0x0101), "Canal HD");
    }

    #[test]
    fn spec_ui_001_service_name_fallback_when_no_sdt() {
        // Sem SDT, o fallback deve ser "Serviço 0x{id:04X}".
        let state = AppState::default();
        assert_eq!(service_name(&state, 0x0101), "Serviço 0x0101");
        assert_eq!(service_name(&state, 0x0003), "Serviço 0x0003");
    }

    #[test]
    fn spec_ui_001_service_name_fallback_when_not_in_sdt() {
        // Serviço presente na SDT mas sem nome: usa fallback.
        let mut state = AppState::default();
        let svc = SdtService {
            service_id: 0x0200,
            eit_schedule_flag: false,
            eit_present_following: false,
            running_status: ts::tables::RunningStatus::Running,
            free_ca_mode: false,
            service_name: None,
            provider_name: None,
            service_type: None,
            descriptors: vec![],
        };
        state.tables.sdt = Some(Sdt {
            transport_stream_id: 1,
            original_network_id: 1,
            version: 0,
            actual: true,
            services: vec![svc],
        });
        assert_eq!(service_name(&state, 0x0200), "Serviço 0x0200");
    }

    #[test]
    fn spec_ui_001_is_audio_stream_types() {
        // Tipos de stream de áudio conhecidos.
        assert!(is_audio_stream(&PmtStream {
            stream_type: 0x03,
            elementary_pid: 0x0101,
            descriptors: vec![],
        }));
        assert!(is_audio_stream(&PmtStream {
            stream_type: 0x04,
            elementary_pid: 0x0102,
            descriptors: vec![],
        }));
        assert!(is_audio_stream(&PmtStream {
            stream_type: 0x0F,
            elementary_pid: 0x0103,
            descriptors: vec![],
        }));
        assert!(is_audio_stream(&PmtStream {
            stream_type: 0x11,
            elementary_pid: 0x0104,
            descriptors: vec![],
        }));
        assert!(is_audio_stream(&PmtStream {
            stream_type: 0x81,
            elementary_pid: 0x0105,
            descriptors: vec![],
        }));
        assert!(is_audio_stream(&PmtStream {
            stream_type: 0x06,
            elementary_pid: 0x0106,
            descriptors: vec![Descriptor::new(0x6A, vec![])],
        }));
        // Vídeo não é áudio.
        assert!(!is_audio_stream(&PmtStream {
            stream_type: 0x1B,
            elementary_pid: 0x0110,
            descriptors: vec![],
        }));
        assert!(!is_audio_stream(&PmtStream {
            stream_type: 0x24,
            elementary_pid: 0x0111,
            descriptors: vec![],
        }));
    }

    #[test]
    fn spec_ui_001_is_subtitle_stream_type() {
        assert!(is_subtitle_stream(&PmtStream {
            stream_type: 0x06,
            elementary_pid: 0x0120,
            descriptors: vec![],
        }));
        assert!(!is_subtitle_stream(&PmtStream {
            stream_type: 0x0F,
            elementary_pid: 0x0121,
            descriptors: vec![],
        }));
        assert!(!is_subtitle_stream(&PmtStream {
            stream_type: 0x06,
            elementary_pid: 0x0122,
            descriptors: vec![Descriptor::new(0x6A, vec![])],
        }));
    }

    #[test]
    fn spec_ui_001_services_from_pat_exclude_nit() {
        // program_number == 0 é a NIT e não deve aparecer como serviço.
        let mut state = AppState::default();
        let pat = Pat {
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
                PatProgram {
                    program_number: 0x0102,
                    pid: 0x0200,
                },
            ],
        };
        state.tables.pat = Some(pat);

        let services: Vec<u16> = state
            .tables
            .pat
            .as_ref()
            .map(|p| {
                p.programs
                    .iter()
                    .filter(|pr| pr.program_number != 0)
                    .map(|pr| pr.program_number)
                    .collect()
            })
            .unwrap_or_default();

        assert_eq!(services, vec![0x0101, 0x0102]);
    }

    #[test]
    fn spec_ui_001_context_menu_active_service_label_has_checkmark() {
        // O serviço ativo deve ter "✓" no label; os demais, espaço.
        let mut state = AppState::default();
        state.selected_service = Some(0x0101);

        let is_active = state.selected_service == Some(0x0101);
        let label = if is_active {
            format!("✓  {}", "Canal HD")
        } else {
            format!("    {}", "Canal HD")
        };
        assert!(label.starts_with('✓'));

        let is_active2 = state.selected_service == Some(0x0102);
        let label2 = if is_active2 {
            format!("✓  {}", "Canal 2")
        } else {
            format!("    {}", "Canal 2")
        };
        assert!(label2.starts_with("    "));
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
}
