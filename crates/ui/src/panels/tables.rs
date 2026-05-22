//! `TablesPanel` — abas PIDs / Tables / Serviços com árvore PSI/SI.
//!
//! SPEC-UI-004

use std::collections::{HashMap, HashSet};

use crossbeam_channel::Sender;
use eframe::egui;
use ts::metrics::{
    AudioCodec as MetricsAudioCodec, PidEntry, PidType, VideoCodec as MetricsVideoCodec,
};
use ts::tables::pmt::stream_type_label;
use ts::tables::sdt::RunningStatus;
use ts::tables::PmtStream;
use ts::Pid;

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

fn pmt_stream_language(stream: &PmtStream) -> Option<String> {
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

fn stream_pid_type(stream: &PmtStream) -> PidType {
    match stream.stream_type {
        0x02 => PidType::Video {
            codec: MetricsVideoCodec::Mpeg2,
        },
        0x1B => PidType::Video {
            codec: MetricsVideoCodec::H264,
        },
        0x24 => PidType::Video {
            codec: MetricsVideoCodec::H265,
        },
        0x03 | 0x04 => PidType::Audio {
            codec: MetricsAudioCodec::MpegAudio,
        },
        0x0F | 0x11 => PidType::Audio {
            codec: MetricsAudioCodec::Aac,
        },
        0x81 => PidType::Audio {
            codec: MetricsAudioCodec::Ac3,
        },
        0x87 => PidType::Audio {
            codec: MetricsAudioCodec::Eac3,
        },
        0x06 if stream.is_audio() => PidType::Audio {
            codec: match stream.label() {
                "E-AC-3 Audio (DVB)" => MetricsAudioCodec::Eac3,
                "AC-3 Audio (DVB)" => MetricsAudioCodec::Ac3,
                _ => MetricsAudioCodec::Aac,
            },
        },
        _ => PidType::Unknown,
    }
}

fn fixed_pid_info(pid: Pid) -> Option<(PidType, String)> {
    match pid {
        0x0000 => Some((PidType::Pat, "PAT".to_string())),
        0x0010 => Some((PidType::Nit, "NIT".to_string())),
        0x0011 => Some((PidType::Sdt, "SDT/BAT".to_string())),
        0x0012 => Some((PidType::Eit, "EIT".to_string())),
        0x0014 => Some((PidType::Tdt, "TDT/TOT".to_string())),
        0x1FFF => Some((PidType::NullPacket, "Null packets".to_string())),
        _ => None,
    }
}

fn known_pid_info(state: &AppState) -> HashMap<Pid, (PidType, String)> {
    let mut info = HashMap::new();

    if let Some(pat) = &state.tables.pat {
        info.insert(0x0000, (PidType::Pat, "PAT".to_string()));
        for program in &pat.programs {
            if program.program_number == 0 {
                info.insert(program.pid, (PidType::Nit, "NIT".to_string()));
            } else {
                info.insert(
                    program.pid,
                    (
                        PidType::Pmt,
                        format!("PMT - Serviço {}", program.program_number),
                    ),
                );
            }
        }
    }

    if state.tables.nit.is_some() {
        info.insert(0x0010, (PidType::Nit, "NIT".to_string()));
    }
    if state.tables.sdt.is_some() || state.tables.bat.is_some() {
        info.insert(0x0011, (PidType::Sdt, "SDT/BAT".to_string()));
    }
    if !state.tables.eit_pf.is_empty() {
        info.insert(0x0012, (PidType::Eit, "EIT".to_string()));
    }
    if state.tables.tdt.is_some() {
        info.insert(0x0014, (PidType::Tdt, "TDT/TOT".to_string()));
    }

    for pmt in state.tables.pmts.values() {
        info.entry(pmt.pcr_pid).or_insert_with(|| {
            (
                PidType::Pcr,
                format!("PCR - Serviço {}", pmt.program_number),
            )
        });

        for stream in &pmt.streams {
            let pid_type = stream_pid_type(stream);
            let language = pmt_stream_language(stream)
                .map(|language| format!(" [{language}]"))
                .unwrap_or_default();
            let pcr_suffix = if stream.elementary_pid == pmt.pcr_pid {
                " + PCR"
            } else {
                ""
            };
            info.insert(
                stream.elementary_pid,
                (
                    pid_type,
                    format!(
                        "{}{}{} - Serviço {}",
                        stream.label(),
                        language,
                        pcr_suffix,
                        pmt.program_number
                    ),
                ),
            );
        }
    }

    info
}

fn enriched_pid_entries(state: &AppState) -> Vec<PidEntry> {
    let mut known = known_pid_info(state);
    let mut rows = state.metrics.pid_table.clone();
    let mut seen = HashSet::new();

    for row in &mut rows {
        seen.insert(row.pid);
        if let Some((pid_type, label)) = known.remove(&row.pid).or_else(|| fixed_pid_info(row.pid))
        {
            row.pid_type = pid_type;
            row.label = label;
        }
    }

    for (pid, (pid_type, label)) in known {
        if seen.insert(pid) {
            rows.push(PidEntry {
                pid,
                pid_type,
                label,
                bitrate_kbps: 0.0,
                cc_errors: state
                    .metrics
                    .errors
                    .cc_errors
                    .get(&pid)
                    .copied()
                    .unwrap_or(0),
                packet_count: 0,
            });
        }
    }

    rows
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
        let entries = enriched_pid_entries(state);
        self.pid_panel
            .show(ui, &entries, state.selected_pid, cmd_tx);
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
                                    ui.label(format!(
                                        "  NIT PID: {} (0x{:04X})",
                                        prog.pid, prog.pid
                                    ));
                                } else {
                                    ui.label(format!(
                                        "  Prog {:4}  ->  PMT PID {} (0x{:04X})",
                                        prog.program_number, prog.pid, prog.pid
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
                                    "Programa {:4}  v{}  (PCR PID {} (0x{:04X}))",
                                    prog_id, pmt.version, pmt.pcr_pid, pmt.pcr_pid
                                ))
                                .id_salt(format!("pmt_{prog_id}"))
                                .show(ui, |ui| {
                                    for stream in &pmt.streams {
                                        ui.label(format!(
                                            "  {} (0x{:04X})  {}",
                                            stream.elementary_pid,
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
    use ts::metrics::{MetricsSnapshot, PidEntry, PidType};
    use ts::tables::{Pat, PatProgram, Pmt, PmtStream};

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
    fn spec_ui_004_enriched_pid_entries_empty_without_metrics_or_tables() {
        let state = AppState::default();
        assert!(enriched_pid_entries(&state).is_empty());
    }

    #[test]
    fn spec_ui_004_enriched_pid_entries_labels_pmt_streams() {
        let mut state = AppState::default();
        state.metrics = MetricsSnapshot {
            pid_table: vec![PidEntry {
                pid: 0x0100,
                pid_type: PidType::Unknown,
                label: String::new(),
                bitrate_kbps: 18_000.0,
                cc_errors: 0,
                packet_count: 120,
            }],
            ..MetricsSnapshot::default()
        };
        state.tables.pat = Some(Pat {
            transport_stream_id: 1,
            version: 0,
            current_next: true,
            programs: vec![PatProgram {
                program_number: 16,
                pid: 0x1000,
            }],
        });
        state.tables.pmts.insert(
            16,
            Pmt {
                program_number: 16,
                version: 0,
                current_next: true,
                pcr_pid: 0x0100,
                program_descriptors: vec![],
                streams: vec![
                    PmtStream {
                        stream_type: 0x1B,
                        elementary_pid: 0x0100,
                        descriptors: vec![],
                    },
                    PmtStream {
                        stream_type: 0x11,
                        elementary_pid: 0x0101,
                        descriptors: vec![],
                    },
                ],
            },
        );

        let entries = enriched_pid_entries(&state);
        let video = entries
            .iter()
            .find(|entry| entry.pid == 0x0100)
            .expect("video PID deve existir");
        let audio = entries
            .iter()
            .find(|entry| entry.pid == 0x0101)
            .expect("audio PID deve ser adicionado a partir da PMT");
        let pmt = entries
            .iter()
            .find(|entry| entry.pid == 0x1000)
            .expect("PMT PID deve ser adicionado a partir da PAT");

        assert!(matches!(video.pid_type, PidType::Video { .. }));
        assert_eq!(video.label, "H.264 / AVC Video + PCR - Serviço 16");
        assert!(matches!(audio.pid_type, PidType::Audio { .. }));
        assert_eq!(audio.label, "AAC Audio (LATM) - Serviço 16");
        assert_eq!(audio.packet_count, 0);
        assert_eq!(pmt.pid_type, PidType::Pmt);
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
