//! `TablesPanel` — abas PIDs / Tables / Serviços com árvore PSI/SI.
//!
//! SPEC-UI-004

use crossbeam_channel::Sender;
use eframe::egui;
use ts::tables::pmt::stream_type_label;
use ts::tables::sdt::RunningStatus;

use crate::state::AppState;
use crate::AppCommand;

use super::pid::PidPanel;

// ---------------------------------------------------------------------------
// Helpers de formatação
// ---------------------------------------------------------------------------

/// Retorna um rótulo legível para `RunningStatus` DVB.
fn running_status_label(rs: RunningStatus) -> &'static str {
    match rs {
        RunningStatus::Undefined => "Indefinido",
        RunningStatus::NotRunning => "Parado",
        RunningStatus::StartsInFewSeconds => "Iniciando",
        RunningStatus::Pausing => "Pausando",
        RunningStatus::Running => "Em ar",
        RunningStatus::ServiceOffAir => "Fora do ar",
        RunningStatus::Reserved => "Reservado",
    }
}

/// Retorna um rótulo legível para `service_type` DVB (EN 300 468, Tabela 87).
fn service_type_label(st: u8) -> &'static str {
    match st {
        0x01 => "TV Digital",
        0x02 => "Rádio Digital",
        0x03 => "Teletexto",
        0x04 => "NVOD Referência",
        0x05 => "NVOD Shifted",
        0x06 => "Mosaic",
        0x0A => "Rádio FM",
        0x0B => "NVOD Ref (DVB-S)",
        0x0C => "Data Broadcast",
        0x10 => "DVB MHP",
        0x11 => "MPEG-2 HD TV",
        0x16 => "H.264/AVC SD TV",
        0x19 => "H.264/AVC HD TV",
        0x1F => "HEVC TV",
        0x80..=0xFF => "Privado",
        _ => "Desconhecido",
    }
}

/// Formata o intervalo de tempo de um evento EIT como `HH:MM–HH:MM`.
///
/// Retorna uma string vazia se `start_time` for `None`.
fn format_eit_time_range(ev: &ts::tables::EitEvent) -> String {
    let start = match ev.start_time {
        Some(t) => t,
        None => return String::new(),
    };
    let start_str = start.format("%H:%M").to_string();
    let end_str = ev.duration_seconds.map(|dur| {
        use chrono::Duration;
        let end = start + Duration::seconds(dur as i64);
        end.format("%H:%M").to_string()
    });
    match end_str {
        Some(e) => format!("{start_str}\u{2013}{e}"),
        None => start_str,
    }
}

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
                let pat_label = if let Some(pat) = &tables.pat {
                    format!(
                        "PAT  (v{}, TS-ID: 0x{:04X})",
                        pat.version, pat.transport_stream_id
                    )
                } else {
                    "PAT — Program Association Table".to_string()
                };
                egui::CollapsingHeader::new(pat_label)
                    .id_salt("tables_pat")
                    .show(ui, |ui| {
                        if let Some(pat) = &tables.pat {
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
                                    "Programa {:4}  v{}  (PCR PID 0x{:04X})",
                                    prog_id, pmt.version, pmt.pcr_pid
                                ))
                                .id_salt(format!("pmt_{prog_id}"))
                                .show(ui, |ui| {
                                    for stream in &pmt.streams {
                                        ui.label(format!(
                                            "  0x{:04X}  {}",
                                            stream.elementary_pid,
                                            stream_type_label(stream.stream_type),
                                        ));
                                    }
                                });
                            }
                        }
                    });

                // ── NIT ───────────────────────────────────────────────────────
                let nit_label = if let Some(nit) = &tables.nit {
                    let name = nit.network_name.as_deref().unwrap_or("—");
                    format!("NIT  (v{}, network: \"{}\")", nit.version, name)
                } else {
                    "NIT — Network Information Table".to_string()
                };
                egui::CollapsingHeader::new(nit_label)
                    .id_salt("tables_nit")
                    .show(ui, |ui| {
                        if let Some(nit) = &tables.nit {
                            ui.label(format!(
                                "Network ID: 0x{:04X}  {}",
                                nit.network_id,
                                if nit.actual { "actual" } else { "other" }
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
                let sdt_label = if let Some(sdt) = &tables.sdt {
                    format!(
                        "SDT  (v{}, TS: 0x{:04X})",
                        sdt.version, sdt.transport_stream_id
                    )
                } else {
                    "SDT — Service Description Table".to_string()
                };
                egui::CollapsingHeader::new(sdt_label)
                    .id_salt("tables_sdt")
                    .show(ui, |ui| {
                        if let Some(sdt) = &tables.sdt {
                            ui.label(format!(
                                "ONID: 0x{:04X}  {}",
                                sdt.original_network_id,
                                if sdt.actual { "actual" } else { "other" }
                            ));
                            for svc in &sdt.services {
                                let name = svc.service_name.as_deref().unwrap_or("—");
                                let status = running_status_label(svc.running_status);
                                ui.label(format!(
                                    "  Svc 0x{:04X}: \"{}\"  [{}]",
                                    svc.service_id, name, status
                                ));
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
                                egui::CollapsingHeader::new(format!("Svc 0x{sid:04X}"))
                                    .id_salt(format!("eit_{sid}"))
                                    .show(ui, |ui| {
                                        if let Some(ev) = current {
                                            let evname = ev.event_name.as_deref().unwrap_or("—");
                                            let time_range = format_eit_time_range(ev);
                                            ui.label(format!(
                                                "  Atual: \"{evname}\"  {time_range}"
                                            ));
                                        } else {
                                            ui.label("  Atual: (nenhum)");
                                        }
                                        if let Some(ev) = next {
                                            let evname = ev.event_name.as_deref().unwrap_or("—");
                                            let time_range = format_eit_time_range(ev);
                                            ui.label(format!(
                                                "  Próximo: \"{evname}\"  {time_range}"
                                            ));
                                        } else {
                                            ui.label("  Próximo: (nenhum)");
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
                            let offset = tdt.offset_from_system();
                            ui.label(format!(
                                "{}  UTC  (sistema: {}s)",
                                tdt.utc_time.format("%Y-%m-%d %H:%M:%S"),
                                offset
                            ));
                        } else {
                            ui.label("(aguardando…)");
                        }
                    });

                // ── BAT ───────────────────────────────────────────────────────
                let bat_label = if let Some(bat) = &tables.bat {
                    let name = bat.bouquet_name.as_deref().unwrap_or("—");
                    format!("BAT  (bouquet: 0x{:04X} \"{}\")", bat.bouquet_id, name)
                } else {
                    "BAT — Bouquet Association Table".to_string()
                };
                egui::CollapsingHeader::new(bat_label)
                    .id_salt("tables_bat")
                    .show(ui, |ui| {
                        if let Some(bat) = &tables.bat {
                            ui.label(format!("versão: {}", bat.version));
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
                    .num_columns(5)
                    .striped(true)
                    .min_col_width(50.0)
                    .show(ui, |ui| {
                        // Cabeçalho
                        ui.strong("ID");
                        ui.strong("Nome");
                        ui.strong("Tipo");
                        ui.strong("EIT p/f");
                        ui.strong("Status");
                        ui.end_row();

                        for svc in &sdt.services {
                            let name = svc.service_name.as_deref().unwrap_or("—");
                            let tipo = svc.service_type.map(service_type_label).unwrap_or("—");
                            let eit_pf = match (svc.eit_present_following, svc.eit_schedule_flag) {
                                (true, true) => "p/f + sched",
                                (true, false) => "p/f",
                                (false, true) => "sched",
                                (false, false) => "—",
                            };
                            let status = running_status_label(svc.running_status);
                            let is_selected = state.selected_service == Some(svc.service_id);

                            let sid_str = format!("0x{:04X}", svc.service_id);

                            // Helper: produz RichText com bold se selecionado
                            let mk = |s: &str| {
                                if is_selected {
                                    egui::RichText::new(s).strong()
                                } else {
                                    egui::RichText::new(s)
                                }
                            };

                            let r1 =
                                ui.add(egui::Label::new(mk(&sid_str)).sense(egui::Sense::click()));
                            let r2 = ui.add(egui::Label::new(mk(name)).sense(egui::Sense::click()));
                            let r3 = ui.add(egui::Label::new(mk(tipo)).sense(egui::Sense::click()));
                            let r4 =
                                ui.add(egui::Label::new(mk(eit_pf)).sense(egui::Sense::click()));
                            let r5 =
                                ui.add(egui::Label::new(mk(status)).sense(egui::Sense::click()));

                            // Clique duplo em qualquer célula envia SelectService.
                            if r1.double_clicked()
                                || r2.double_clicked()
                                || r3.double_clicked()
                                || r4.double_clicked()
                                || r5.double_clicked()
                            {
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

    #[test]
    fn spec_ui_004_running_status_labels() {
        assert_eq!(running_status_label(RunningStatus::Running), "Em ar");
        assert_eq!(running_status_label(RunningStatus::NotRunning), "Parado");
        assert_eq!(
            running_status_label(RunningStatus::ServiceOffAir),
            "Fora do ar"
        );
        assert_eq!(
            running_status_label(RunningStatus::StartsInFewSeconds),
            "Iniciando"
        );
    }

    #[test]
    fn spec_ui_004_service_type_labels() {
        assert_eq!(service_type_label(0x01), "TV Digital");
        assert_eq!(service_type_label(0x02), "Rádio Digital");
        assert_eq!(service_type_label(0x19), "H.264/AVC HD TV");
        assert_eq!(service_type_label(0xFF), "Privado");
    }

    #[test]
    fn spec_ui_004_eit_time_range_with_duration() {
        use chrono::NaiveDateTime;
        use ts::tables::eit::EitEvent;
        use ts::tables::sdt::RunningStatus;

        let ev = EitEvent {
            event_id: 1,
            start_time: NaiveDateTime::parse_from_str("2026-05-20 20:00:00", "%Y-%m-%d %H:%M:%S")
                .ok(),
            duration_seconds: Some(3600),
            running_status: RunningStatus::Running,
            free_ca_mode: false,
            event_name: Some("Test Event".to_string()),
            short_description: None,
            descriptors: vec![],
        };

        let range = format_eit_time_range(&ev);
        assert_eq!(range, "20:00\u{2013}21:00");
    }

    #[test]
    fn spec_ui_004_eit_time_range_no_start() {
        use ts::tables::eit::EitEvent;
        use ts::tables::sdt::RunningStatus;

        let ev = EitEvent {
            event_id: 2,
            start_time: None,
            duration_seconds: None,
            running_status: RunningStatus::Undefined,
            free_ca_mode: false,
            event_name: None,
            short_description: None,
            descriptors: vec![],
        };

        let range = format_eit_time_range(&ev);
        assert!(range.is_empty());
    }
}
