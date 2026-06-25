//! `MediaInfoPanel` — relatório estilo MediaInfo por serviço e PID.
//!
//! SPEC-MI-004 · SPEC-MI-005 · SPEC-MI-006

use eframe::egui;

use ts::tables::pmt::stream_type_label;
use ts::tables::Descriptor;
use ts::{
    build_elementary_stream_fields, build_media_info_report, enrich_tables_ctx_from_descriptors,
    MediaInfoBuildInput, MediaInfoTablesCtx, ReportField, StreamKind,
};

use crate::state::{AppState, ConnectionState};

/// Painel da aba Media Info.
///
/// SPEC-MI-004
pub struct MediaInfoPanel {
    last_report_text: String,
}

impl Default for MediaInfoPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaInfoPanel {
    /// SPEC-MI-004
    pub fn new() -> Self {
        Self {
            last_report_text: String::new(),
        }
    }

    /// Renderiza o relatório Media Info com colapsáveis por serviço e PID.
    ///
    /// SPEC-MI-004
    pub fn show(&mut self, ui: &mut egui::Ui, state: &AppState) {
        let tables_ctx = tables_ctx_from_snapshot(&state.tables, &state.media_info_tables_ctx);
        let input = media_info_input(state, &tables_ctx);
        let max_height = ui.available_height().max(0.0);
        let panel_width = ui.available_width();

        egui::ScrollArea::vertical()
            .id_salt("mediainfo_scroll")
            .max_height(max_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_min_width(panel_width);
                ui.horizontal(|ui| {
                    if ui.small_button("Copiar relatório").clicked() {
                        let report_text = build_media_info_report(&input).to_text();
                        ui.ctx().copy_text(report_text.clone());
                        self.last_report_text = report_text;
                    }
                    if state.tables.pmts.is_empty() {
                        ui.label("(aguardando PMT/SDT…)");
                    }
                });

                let general = build_media_info_report(&input).sections.first().cloned();
                if let Some(section) = general {
                    egui::CollapsingHeader::new("General")
                        .id_salt("mi_general")
                        .default_open(true)
                        .show(ui, |ui| {
                            render_fields_grid(ui, "mi_general_grid", &section.fields);
                        });
                }

                let mut program_ids: Vec<u16> = state.tables.pmts.keys().copied().collect();
                program_ids.sort_unstable();

                for program_number in program_ids {
                    let Some(pmt) = state.tables.pmts.get(&program_number) else {
                        continue;
                    };
                    let service_title = service_header_label(state, program_number);

                    egui::CollapsingHeader::new(service_title)
                        .id_salt(format!("mi_svc_{program_number}"))
                        .show(ui, |ui| {
                            for stream in &pmt.streams {
                                let pid = stream.elementary_pid;
                                let pid_title = pid_header_label(state, stream, pid);
                                let fields = build_elementary_stream_fields(
                                    &input,
                                    pid,
                                    program_number,
                                    stream,
                                );

                                egui::CollapsingHeader::new(pid_title)
                                    .id_salt(format!("mi_pid_{program_number}_{pid}"))
                                    .show(ui, |ui| {
                                        render_fields_grid(
                                            ui,
                                            format!("mi_grid_{program_number}_{pid}"),
                                            &fields,
                                        );
                                    });
                            }
                        });
                }
            });
    }

    /// Último texto gerado (para testes).
    pub fn last_report_text(&self) -> &str {
        &self.last_report_text
    }
}

fn render_fields_grid(ui: &mut egui::Ui, _id: impl std::hash::Hash, fields: &[ReportField]) {
    if fields.is_empty() {
        ui.label("(sem dados)");
        return;
    }
    let total_width = ui.available_width().max(0.0);
    let key_width = (total_width * 0.40).clamp(96.0, 180.0);

    ui.vertical(|ui| {
        ui.set_min_width(total_width);
        for (row, field) in fields.iter().enumerate() {
            ui.push_id(row, |ui| {
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(key_width, 0.0),
                        egui::Layout::left_to_right(egui::Align::TOP),
                        |ui| {
                            ui.label(egui::RichText::new(&field.key).strong());
                        },
                    );
                    ui.allocate_ui_with_layout(
                        egui::vec2((total_width - key_width).max(0.0), 0.0),
                        egui::Layout::left_to_right(egui::Align::TOP),
                        |ui| {
                            ui.add(egui::Label::new(&field.value).wrap().selectable(true));
                        },
                    );
                });
            });
        }
    });
}

fn service_header_label(state: &AppState, program_number: u16) -> String {
    let name = state
        .tables
        .sdt
        .as_ref()
        .and_then(|sdt| sdt.services.iter().find(|s| s.service_id == program_number))
        .and_then(|s| s.service_name.clone())
        .unwrap_or_else(|| format!("Serviço 0x{program_number:04X}"));

    format!("{name}  (prog {program_number})")
}

fn pid_header_label(state: &AppState, stream: &ts::tables::PmtStream, pid: u16) -> String {
    let kind = state
        .media_info
        .get(pid)
        .and_then(|c| c.kind)
        .unwrap_or_else(|| {
            if stream.is_audio() {
                StreamKind::Audio
            } else if matches!(stream.stream_type, 0x01 | 0x02 | 0x1B | 0x24) {
                StreamKind::Video
            } else {
                StreamKind::Data
            }
        });

    let kind_label = match kind {
        StreamKind::Video => "Vídeo",
        StreamKind::Audio => "Áudio",
        StreamKind::Data => "Dados",
        StreamKind::Menu => "Menu",
    };

    let format = state
        .media_info
        .get(pid)
        .and_then(|c| c.format.clone())
        .unwrap_or_else(|| stream_type_label(stream.stream_type).to_string());

    format!("PID 0x{pid:04X} — {kind_label} — {format}")
}

fn media_info_input<'a>(
    state: &'a AppState,
    tables_ctx: &'a MediaInfoTablesCtx,
) -> MediaInfoBuildInput<'a> {
    let source_name = match &state.connection {
        ConnectionState::Connected { url, .. }
        | ConnectionState::Connecting { url }
        | ConnectionState::Error { url, .. } => Some(url.as_str()),
        ConnectionState::Idle => None,
    };
    MediaInfoBuildInput {
        source_name,
        metrics: &state.metrics,
        tables: tables_ctx,
        codec: &state.media_info,
    }
}

/// Monta `MediaInfoReport` a partir do estado da UI (clipboard / testes).
///
/// SPEC-MI-005
pub fn build_report_from_state(state: &AppState) -> ts::MediaInfoReport {
    let tables_ctx = tables_ctx_from_snapshot(&state.tables, &state.media_info_tables_ctx);
    build_media_info_report(&media_info_input(state, &tables_ctx))
}

fn tables_ctx_from_snapshot(
    tables: &crate::state::TablesSnapshot,
    extra: &MediaInfoTablesCtx,
) -> MediaInfoTablesCtx {
    let mut ctx = MediaInfoTablesCtx {
        pat: tables.pat.clone(),
        pmts: tables.pmts.clone(),
        sdt: tables.sdt.clone(),
        nit_network_name: extra.nit_network_name.clone(),
        nit_original_network: extra.nit_original_network.clone(),
        nit_frequency_hz: extra.nit_frequency_hz,
        nit_orbital_position: extra.nit_orbital_position.clone(),
        tot_country: extra.tot_country.clone(),
        tot_timezone: extra.tot_timezone.clone(),
    };
    if let Some(nit) = &tables.nit {
        enrich_tables_ctx_from_descriptors(&mut ctx, &nit.network_descriptors, &[]);
        for ts_entry in &nit.transport_streams {
            enrich_tables_ctx_from_descriptors(&mut ctx, &ts_entry.descriptors, &[]);
        }
        if ctx.nit_network_name.is_none() {
            ctx.nit_network_name = nit.network_name.clone();
        }
    }
    if let Some(tot) = &tables.tot {
        enrich_tables_ctx_from_descriptors(&mut ctx, &[], &tot.descriptors);
    }
    if let Some(sdt) = &tables.sdt {
        if ctx.nit_original_network.is_none() {
            ctx.nit_original_network = Some(format!("ONID {}", sdt.original_network_id));
        }
    }
    ctx
}

/// Atualiza contexto General a partir de eventos de tabela.
///
/// SPEC-MI-005
pub fn update_media_info_tables_ctx(
    ctx: &mut MediaInfoTablesCtx,
    nit_descriptors: &[Descriptor],
    tot_descriptors: &[Descriptor],
) {
    enrich_tables_ctx_from_descriptors(ctx, nit_descriptors, tot_descriptors);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    #[test]
    fn spec_mi_004_empty_state_report() {
        let state = AppState::default();
        let report = build_report_from_state(&state);
        assert!(!report.sections.is_empty());
        assert_eq!(report.sections[0].title, "General");
        assert!(!report.sections.iter().any(|s| s.title.starts_with("Menu")));
    }
}
