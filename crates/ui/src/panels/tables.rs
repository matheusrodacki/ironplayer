//! `TablesPanel` — abas PIDs / Tables / Serviços com árvore PSI/SI.
//!
//! SPEC-UI-004

use crossbeam_channel::Sender;
use eframe::egui;
use ts::tables::pmt::stream_type_label;

use crate::state::AppState;
use crate::AppCommand;

use super::pid::PidPanel;

// ---------------------------------------------------------------------------
// ActiveTab
// ---------------------------------------------------------------------------

/// Aba ativa do `TablesPanel`.
///
/// SPEC-UI-004
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActiveTab {
    /// Tabela de PIDs com ordenação.
    #[default]
    Pids,
    /// Árvore PSI/SI: PAT/PMT/NIT/SDT/EIT/TDT/BAT.
    Tables,
    /// Lista de serviços DVB com clique duplo.
    Services,
}

// ---------------------------------------------------------------------------
// TablesPanel
// ---------------------------------------------------------------------------

/// Painel central com 3 abas: PIDs, Tables e Serviços.
///
/// SPEC-UI-004
pub struct TablesPanel {
    active_tab: ActiveTab,
    pid_panel: PidPanel,
}

impl Default for TablesPanel {
    fn default() -> Self {
        Self {
            active_tab: ActiveTab::Pids,
            pid_panel: PidPanel::new(),
        }
    }
}

impl TablesPanel {
    /// Cria um novo `TablesPanel`.
    ///
    /// SPEC-UI-004
    pub fn new() -> Self {
        Self::default()
    }

    /// Renderiza o painel com as abas PIDs / Tables / Serviços.
    ///
    /// SPEC-UI-004
    pub fn show(&mut self, ui: &mut egui::Ui, state: &AppState, cmd_tx: &Sender<AppCommand>) {
        // ── Barra de abas ─────────────────────────────────────────────────────
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.active_tab, ActiveTab::Pids, "PIDs");
            ui.selectable_value(&mut self.active_tab, ActiveTab::Tables, "Tables");
            ui.selectable_value(&mut self.active_tab, ActiveTab::Services, "Serviços");
        });
        ui.separator();

        match self.active_tab {
            ActiveTab::Pids => self.show_pids(ui, state, cmd_tx),
            ActiveTab::Tables => self.show_tables(ui, state),
            ActiveTab::Services => self.show_services(ui, state, cmd_tx),
        }
    }

    // ── Aba PIDs ──────────────────────────────────────────────────────────────

    fn show_pids(&mut self, ui: &mut egui::Ui, state: &AppState, cmd_tx: &Sender<AppCommand>) {
        self.pid_panel
            .show(ui, &state.metrics.pid_table, state.selected_pid, cmd_tx);
    }

    // ── Aba Tables ────────────────────────────────────────────────────────────

    fn show_tables(&mut self, ui: &mut egui::Ui, state: &AppState) {
        let tables = &state.tables;

        egui::ScrollArea::vertical()
            .id_salt("tables_scroll")
            .show(ui, |ui| {
                // ── PAT ───────────────────────────────────────────────────────
                egui::CollapsingHeader::new("PAT — Program Association Table")
                    .id_salt("tables_pat")
                    .show(ui, |ui| {
                        if let Some(pat) = &tables.pat {
                            ui.label(format!(
                                "TS ID: 0x{:04X}  versão: {}",
                                pat.transport_stream_id, pat.version
                            ));
                            for prog in &pat.programs {
                                if prog.program_number == 0 {
                                    ui.label(format!("  NIT PID: 0x{:04X}", prog.pid));
                                } else {
                                    ui.label(format!(
                                        "  Prog {:4}  \u{2192}  PMT PID 0x{:04X}",
                                        prog.program_number, prog.pid
                                    ));
                                }
                            }
                        } else {
                            ui.label("(aguardando…)");
                        }
                    });

                // ── PMTs ──────────────────────────────────────────────────────
                egui::CollapsingHeader::new("PMTs — Program Map Tables")
                    .id_salt("tables_pmts")
                    .show(ui, |ui| {
                        if tables.pmts.is_empty() {
                            ui.label("(aguardando…)");
                        } else {
                            let mut prog_ids: Vec<u16> = tables.pmts.keys().copied().collect();
                            prog_ids.sort_unstable();
                            for prog_id in prog_ids {
                                let pmt = &tables.pmts[&prog_id];
                                egui::CollapsingHeader::new(format!(
                                    "Programa {:4}  (PCR PID 0x{:04X})",
                                    prog_id, pmt.pcr_pid
                                ))
                                .id_salt(format!("pmt_{prog_id}"))
                                .show(ui, |ui| {
                                    for stream in &pmt.streams {
                                        ui.label(format!(
                                            "  PID 0x{:04X}  type 0x{:02X}  {}",
                                            stream.elementary_pid,
                                            stream.stream_type,
                                            stream_type_label(stream.stream_type),
                                        ));
                                    }
                                });
                            }
                        }
                    });

                // ── NIT ───────────────────────────────────────────────────────
                egui::CollapsingHeader::new("NIT — Network Information Table")
                    .id_salt("tables_nit")
                    .show(ui, |ui| {
                        if let Some(nit) = &tables.nit {
                            let name = nit.network_name.as_deref().unwrap_or("—");
                            ui.label(format!(
                                "Network ID: 0x{:04X}  nome: {}  versão: {}",
                                nit.network_id, name, nit.version
                            ));
                            for ts in &nit.transport_streams {
                                ui.label(format!(
                                    "  TS 0x{:04X}  ONID 0x{:04X}",
                                    ts.transport_stream_id, ts.original_network_id
                                ));
                            }
                        } else {
                            ui.label("(aguardando…)");
                        }
                    });

                // ── SDT ───────────────────────────────────────────────────────
                egui::CollapsingHeader::new("SDT — Service Description Table")
                    .id_salt("tables_sdt")
                    .show(ui, |ui| {
                        if let Some(sdt) = &tables.sdt {
                            ui.label(format!(
                                "TS ID: 0x{:04X}  ONID: 0x{:04X}  versão: {}",
                                sdt.transport_stream_id, sdt.original_network_id, sdt.version
                            ));
                            for svc in &sdt.services {
                                let name = svc.service_name.as_deref().unwrap_or("—");
                                ui.label(format!("  SID 0x{:04X}  {}", svc.service_id, name));
                            }
                        } else {
                            ui.label("(aguardando…)");
                        }
                    });

                // ── EIT P/F ───────────────────────────────────────────────────
                egui::CollapsingHeader::new("EIT P/F — Event Information")
                    .id_salt("tables_eit")
                    .show(ui, |ui| {
                        if tables.eit_pf.is_empty() {
                            ui.label("(aguardando…)");
                        } else {
                            let mut sids: Vec<u16> = tables.eit_pf.keys().copied().collect();
                            sids.sort_unstable();
                            for sid in sids {
                                let (current, next) = &tables.eit_pf[&sid];
                                egui::CollapsingHeader::new(format!("SID 0x{sid:04X}"))
                                    .id_salt(format!("eit_{sid}"))
                                    .show(ui, |ui| {
                                        if let Some(ev) = current {
                                            let evname = ev.event_name.as_deref().unwrap_or("—");
                                            ui.label(format!("  Atual:   {evname}"));
                                        }
                                        if let Some(ev) = next {
                                            let evname = ev.event_name.as_deref().unwrap_or("—");
                                            ui.label(format!("  Próximo: {evname}"));
                                        }
                                    });
                            }
                        }
                    });

                // ── TDT ───────────────────────────────────────────────────────
                egui::CollapsingHeader::new("TDT — Time and Date Table")
                    .id_salt("tables_tdt")
                    .show(ui, |ui| {
                        if let Some(tdt) = &tables.tdt {
                            ui.label(format!("UTC: {}", tdt.utc_time.format("%Y-%m-%d %H:%M:%S")));
                        } else {
                            ui.label("(aguardando…)");
                        }
                    });

                // ── BAT ───────────────────────────────────────────────────────
                egui::CollapsingHeader::new("BAT — Bouquet Association Table")
                    .id_salt("tables_bat")
                    .show(ui, |ui| {
                        if let Some(bat) = &tables.bat {
                            let name = bat.bouquet_name.as_deref().unwrap_or("—");
                            ui.label(format!(
                                "Bouquet ID: 0x{:04X}  nome: {}  versão: {}",
                                bat.bouquet_id, name, bat.version
                            ));
                            for ts in &bat.transport_streams {
                                ui.label(format!(
                                    "  TS 0x{:04X}  ONID 0x{:04X}",
                                    ts.transport_stream_id, ts.original_network_id
                                ));
                            }
                        } else {
                            ui.label("(aguardando…)");
                        }
                    });
            });
    }

    // ── Aba Serviços ──────────────────────────────────────────────────────────

    fn show_services(&mut self, ui: &mut egui::Ui, state: &AppState, cmd_tx: &Sender<AppCommand>) {
        let sdt = match &state.tables.sdt {
            Some(s) => s,
            None => {
                ui.label("(aguardando SDT…)");
                return;
            }
        };

        egui::ScrollArea::vertical()
            .id_salt("services_scroll")
            .show(ui, |ui| {
                egui::Grid::new("services_table")
                    .num_columns(3)
                    .striped(true)
                    .min_col_width(60.0)
                    .show(ui, |ui| {
                        // Cabeçalho
                        ui.strong("SID");
                        ui.strong("Serviço");
                        ui.strong("Provedor");
                        ui.end_row();

                        for svc in &sdt.services {
                            let name = svc.service_name.as_deref().unwrap_or("—");
                            let provider = svc.provider_name.as_deref().unwrap_or("—");
                            let is_selected = state.selected_service == Some(svc.service_id);

                            let sid_text = format!("0x{:04X}", svc.service_id);

                            let style = if is_selected {
                                egui::RichText::new(&sid_text).strong()
                            } else {
                                egui::RichText::new(&sid_text)
                            };

                            // Clique duplo envia SelectService.
                            let resp = ui.add(egui::Label::new(style).sense(egui::Sense::click()));
                            if resp.double_clicked() {
                                let _ = cmd_tx.try_send(AppCommand::SelectService {
                                    service_id: svc.service_id,
                                });
                            }

                            let name_style = if is_selected {
                                egui::RichText::new(name).strong()
                            } else {
                                egui::RichText::new(name)
                            };
                            let name_resp =
                                ui.add(egui::Label::new(name_style).sense(egui::Sense::click()));
                            if name_resp.double_clicked() {
                                let _ = cmd_tx.try_send(AppCommand::SelectService {
                                    service_id: svc.service_id,
                                });
                            }

                            let prov_style = if is_selected {
                                egui::RichText::new(provider).strong()
                            } else {
                                egui::RichText::new(provider)
                            };
                            let prov_resp =
                                ui.add(egui::Label::new(prov_style).sense(egui::Sense::click()));
                            if prov_resp.double_clicked() {
                                let _ = cmd_tx.try_send(AppCommand::SelectService {
                                    service_id: svc.service_id,
                                });
                            }

                            ui.end_row();
                        }
                    });
            });
    }
}

// ---------------------------------------------------------------------------
// Testes
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_ui_004_tables_panel_default_tab_is_pids() {
        let panel = TablesPanel::new();
        assert_eq!(panel.active_tab, ActiveTab::Pids);
    }
}
