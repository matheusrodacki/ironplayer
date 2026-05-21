//! `PidPanel` — tabela de PIDs com ordenação e highlight de erros CC.
//!
//! SPEC-UI-003

use crossbeam_channel::Sender;
use eframe::egui::{self, Color32, RichText};
use ts::metrics::{PidEntry, PidType};
use ts::Pid;

use crate::AppCommand;

// ---------------------------------------------------------------------------
// Sort state
// ---------------------------------------------------------------------------

/// Coluna pela qual a tabela de PIDs está ordenada.
///
/// SPEC-UI-003
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortColumn {
    #[default]
    Pid,
    Type,
    Label,
    Bitrate,
    CcErrors,
    PacketCount,
}

/// Direção de ordenação.
///
/// SPEC-UI-003
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDir {
    #[default]
    Asc,
    Desc,
}

// ---------------------------------------------------------------------------
// PidPanel
// ---------------------------------------------------------------------------

/// Painel de tabela de PIDs com 6 colunas, ordenação clicável e highlighting
/// de erros CC.
///
/// SPEC-UI-003
pub struct PidPanel {
    sort_col: SortColumn,
    sort_dir: SortDir,
}

impl Default for PidPanel {
    fn default() -> Self {
        Self {
            sort_col: SortColumn::Pid,
            sort_dir: SortDir::Asc,
        }
    }
}

impl PidPanel {
    /// Cria um novo `PidPanel` com ordenação padrão por PID ascendente.
    ///
    /// SPEC-UI-003
    pub fn new() -> Self {
        Self::default()
    }

    /// Renderiza a tabela de PIDs.
    ///
    /// - Ordena `entries` de acordo com a coluna e direção correntes.
    /// - Linhas com `cc_errors > 0` recebem fundo vermelho.
    /// - Clique em uma linha envia `AppCommand::SelectPid`.
    /// - Clique no cabeçalho de coluna alterna ordenação.
    ///
    /// SPEC-UI-003
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        entries: &[PidEntry],
        selected_pid: Option<Pid>,
        cmd_tx: &Sender<AppCommand>,
    ) {
        // Clone e ordena.
        let mut rows: Vec<&PidEntry> = entries.iter().collect();
        sort_entries(&mut rows, self.sort_col, self.sort_dir);

        egui::ScrollArea::vertical()
            .id_salt("pid_panel_scroll")
            .show(ui, |ui| {
                egui::Grid::new("pid_table")
                    .num_columns(6)
                    .striped(false)
                    .min_col_width(50.0)
                    .show(ui, |ui| {
                        // --- Cabeçalho ---
                        self.header_cell(ui, "PID", SortColumn::Pid);
                        self.header_cell(ui, "Tipo", SortColumn::Type);
                        self.header_cell(ui, "Label", SortColumn::Label);
                        self.header_cell(ui, "Bitrate (kbps)", SortColumn::Bitrate);
                        self.header_cell(ui, "CC Errors", SortColumn::CcErrors);
                        self.header_cell(ui, "Packets", SortColumn::PacketCount);
                        ui.end_row();

                        // --- Linhas de dados ---
                        for entry in &rows {
                            let has_errors = entry.cc_errors > 0;
                            let is_selected = selected_pid == Some(entry.pid);

                            // Fundo colorido para linha com erro CC.
                            let row_color: Option<Color32> = if has_errors {
                                Some(Color32::from_rgba_premultiplied(180, 30, 30, 60))
                            } else {
                                None
                            };

                            // Aplica cor de fundo pintando retângulo se necessário.
                            let pid_text = format_pid_hex(entry.pid);
                            let type_text = format_pid_type(&entry.pid_type);
                            let bitrate_text = format!("{:.1}", entry.bitrate_kbps);
                            let cc_text = entry.cc_errors.to_string();
                            let pkt_text = entry.packet_count.to_string();

                            macro_rules! cell_text {
                                ($text:expr) => {{
                                    let mut rt = RichText::new($text);
                                    if is_selected {
                                        rt = rt.strong();
                                    }
                                    if let Some(c) = row_color {
                                        rt = rt.background_color(c);
                                    }
                                    rt
                                }};
                            }

                            let clicked = ui
                                .add(
                                    egui::Label::new(cell_text!(pid_text))
                                        .sense(egui::Sense::click()),
                                )
                                .clicked();
                            ui.add(
                                egui::Label::new(cell_text!(type_text)).sense(egui::Sense::click()),
                            );
                            ui.add(
                                egui::Label::new(cell_text!(entry.label.as_str()))
                                    .sense(egui::Sense::click()),
                            );
                            ui.add(
                                egui::Label::new(cell_text!(bitrate_text))
                                    .sense(egui::Sense::click()),
                            );
                            ui.add(
                                egui::Label::new(cell_text!(cc_text)).sense(egui::Sense::click()),
                            );
                            ui.add(
                                egui::Label::new(cell_text!(pkt_text)).sense(egui::Sense::click()),
                            );
                            ui.end_row();

                            if clicked {
                                let _ = cmd_tx.try_send(AppCommand::SelectPid { pid: entry.pid });
                            }
                        }
                    });
            });
    }

    /// Renderiza uma célula de cabeçalho clicável que alterna ordenação.
    fn header_cell(&mut self, ui: &mut egui::Ui, label: &str, col: SortColumn) {
        let indicator = if self.sort_col == col {
            match self.sort_dir {
                SortDir::Asc => " ▲",
                SortDir::Desc => " ▼",
            }
        } else {
            ""
        };
        let text = format!("{label}{indicator}");
        if ui
            .add(egui::Label::new(RichText::new(text).strong()).sense(egui::Sense::click()))
            .clicked()
        {
            if self.sort_col == col {
                self.sort_dir = match self.sort_dir {
                    SortDir::Asc => SortDir::Desc,
                    SortDir::Desc => SortDir::Asc,
                };
            } else {
                self.sort_col = col;
                self.sort_dir = SortDir::Asc;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Formata um PID como string hexadecimal com 4 dígitos (ex.: "0x0100").
///
/// SPEC-UI-003
pub fn format_pid_hex(pid: Pid) -> String {
    format!("0x{:04X}", pid)
}

/// Retorna uma descrição curta para o tipo de PID.
///
/// SPEC-UI-003
pub fn format_pid_type(pid_type: &PidType) -> &'static str {
    match pid_type {
        PidType::Pat => "PAT",
        PidType::Pmt => "PMT",
        PidType::Nit => "NIT",
        PidType::Sdt => "SDT",
        PidType::Eit => "EIT",
        PidType::Tdt => "TDT",
        PidType::Bat => "BAT",
        PidType::Video { .. } => "Video",
        PidType::Audio { .. } => "Audio",
        PidType::Pcr => "PCR",
        PidType::NullPacket => "Null",
        PidType::Unknown => "Desconhecido",
    }
}

/// Ordena as entradas de PID conforme coluna e direção.
fn sort_entries(rows: &mut Vec<&PidEntry>, col: SortColumn, dir: SortDir) {
    rows.sort_by(|a, b| {
        let ord = match col {
            SortColumn::Pid => a.pid.cmp(&b.pid),
            SortColumn::Type => format_pid_type(&a.pid_type).cmp(format_pid_type(&b.pid_type)),
            SortColumn::Label => a.label.cmp(&b.label),
            SortColumn::Bitrate => a
                .bitrate_kbps
                .partial_cmp(&b.bitrate_kbps)
                .unwrap_or(std::cmp::Ordering::Equal),
            SortColumn::CcErrors => a.cc_errors.cmp(&b.cc_errors),
            SortColumn::PacketCount => a.packet_count.cmp(&b.packet_count),
        };
        if dir == SortDir::Desc {
            ord.reverse()
        } else {
            ord
        }
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_ui_003_pid_format_hex() {
        assert_eq!(format_pid_hex(0x0000_u16), "0x0000");
        assert_eq!(format_pid_hex(0x0100_u16), "0x0100");
        assert_eq!(format_pid_hex(0x1FFF_u16), "0x1FFF");
        assert_eq!(format_pid_hex(0x0010_u16), "0x0010");
    }
}
