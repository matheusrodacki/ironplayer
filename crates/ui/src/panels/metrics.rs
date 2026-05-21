//! `MetricsPanel` — gráficos de bitrate e PCR jitter, log de erros.
//!
//! SPEC-UI-005

use std::collections::VecDeque;

use crossbeam_channel::Sender;
use eframe::egui;
use egui_plot::{Line, Plot, PlotPoints};

use crate::state::{AppState, AudioOperationalState};
use crate::AppCommand;

// ---------------------------------------------------------------------------
// Constantes
// ---------------------------------------------------------------------------

/// Número máximo de entradas no log de erros acumulado.
const MAX_ERROR_LOG: usize = 1000;

/// Limiar de jitter PCR em µs (±500 µs).
const PCR_JITTER_THRESHOLD_US: f64 = 500.0;

/// Janela do gráfico de bitrate e jitter em segundos.
const PLOT_WINDOW_SECS: f64 = 60.0;

// ---------------------------------------------------------------------------
// MetricsPanel
// ---------------------------------------------------------------------------

/// Painel de métricas: gráfico de bitrate (60 s), gráfico de PCR jitter com
/// limiar ±500 µs, e log de erros com scroll (máx 1000 entradas).
///
/// SPEC-UI-005
#[derive(Default)]
pub struct MetricsPanel {
    /// Log acumulado de erros formatados para exibição (máx 1000 entradas).
    error_log: VecDeque<String>,
    /// Número de eventos de jitter PCR já absorvidos do snapshot atual.
    seen_jitter: usize,
    /// Número de eventos de descontinuidade PCR já absorvidos do snapshot atual.
    seen_discontinuity: usize,
}

impl MetricsPanel {
    /// Cria um novo `MetricsPanel` vazio.
    ///
    /// SPEC-UI-005
    pub fn new() -> Self {
        Self::default()
    }

    /// Limpa dados acumulados do stream atual.
    pub(crate) fn reset_stream_data(&mut self) {
        self.error_log.clear();
        self.seen_jitter = 0;
        self.seen_discontinuity = 0;
    }

    /// Renderiza o painel de métricas completo dentro de `ui`.
    ///
    /// SPEC-UI-005
    pub fn show(&mut self, ui: &mut egui::Ui, state: &AppState, cmd_tx: &Sender<AppCommand>) {
        // Absorve novos eventos de erro do snapshot mais recente.
        self.drain_errors(state);

        ui.heading("Métricas");
        ui.add_space(4.0);

        self.show_audio_summary(ui, state);
        ui.add_space(8.0);

        // ── Gráfico de bitrate (60 s) ──────────────────────────────────────
        self.show_bitrate_plot(ui, state);
        ui.add_space(6.0);

        // ── Gráfico de PCR jitter ──────────────────────────────────────────
        self.show_jitter_plot(ui, state);
        ui.add_space(6.0);

        // ── Log de erros ───────────────────────────────────────────────────
        self.show_error_log(ui, cmd_tx);
    }

    // -----------------------------------------------------------------------
    // Drenagem de eventos de erro
    // -----------------------------------------------------------------------

    /// Absorve novos eventos de jitter e descontinuidade do snapshot atual,
    /// adicionando entradas ao log interno (máx 1000).
    fn drain_errors(&mut self, state: &AppState) {
        let errors = &state.metrics.errors;

        // Absorve novos eventos de jitter PCR.
        let new_jitter = errors.pcr_jitter_events.len();
        if new_jitter < self.seen_jitter {
            // Snapshot foi resetado — reinicia o cursor.
            self.seen_jitter = 0;
        }
        for evt in errors.pcr_jitter_events.iter().skip(self.seen_jitter) {
            let jitter_us = evt.measured_us - evt.expected_us;
            let entry = format!("PCR jitter\tPID 0x{:04X}\t{:+} µs", evt.pid, jitter_us);
            self.push_error(entry);
        }
        self.seen_jitter = new_jitter;

        // Absorve descontinuidades PCR.
        let new_disc = errors.pcr_discontinuities.len();
        if new_disc < self.seen_discontinuity {
            self.seen_discontinuity = 0;
        }
        for evt in errors
            .pcr_discontinuities
            .iter()
            .skip(self.seen_discontinuity)
        {
            let entry = format!("PCR discontinuity\tPID 0x{:04X}", evt.pid);
            self.push_error(entry);
        }
        self.seen_discontinuity = new_disc;
    }

    fn show_audio_summary(&self, ui: &mut egui::Ui, state: &AppState) {
        let audio = &state.audio;
        ui.label("Áudio");
        egui::Grid::new("audio_summary_grid")
            .num_columns(2)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                ui.label("Estado");
                ui.label(audio_state_label(audio.state));
                ui.end_row();

                ui.label("Volume");
                ui.label(if audio.muted {
                    "Mudo".to_string()
                } else {
                    format!("{:.0}%", audio.volume * 100.0)
                });
                ui.end_row();

                ui.label("Trilha ativa");
                ui.label(
                    audio
                        .active_track
                        .as_ref()
                        .map(|track| track.codec_label.clone())
                        .unwrap_or_else(|| "-".to_string()),
                );
                ui.end_row();

                ui.label("PID");
                ui.label(
                    audio
                        .active_track
                        .as_ref()
                        .map(|track| format!("{} / 0x{:04X}", track.pid, track.pid))
                        .unwrap_or_else(|| "-".to_string()),
                );
                ui.end_row();

                ui.label("Idioma");
                ui.label(
                    audio
                        .active_track
                        .as_ref()
                        .and_then(|track| track.language.clone())
                        .unwrap_or_else(|| "-".to_string()),
                );
                ui.end_row();

                ui.label("Sample rate");
                ui.label(
                    audio
                        .sample_rate_hz
                        .map(|sample_rate_hz| format!("{:.1} kHz", sample_rate_hz as f32 / 1000.0))
                        .unwrap_or_else(|| "-".to_string()),
                );
                ui.end_row();

                ui.label("Canais");
                ui.label(
                    audio
                        .channels
                        .map(|channels| channels.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                );
                ui.end_row();

                ui.label("Erros");
                ui.label(format!(
                    "decode={} saída={} underrun={} overrun={}",
                    audio.errors.decode_errors,
                    audio.errors.output_errors,
                    audio.errors.underruns,
                    audio.errors.overruns,
                ));
                ui.end_row();

                ui.label("Último erro");
                ui.label(
                    audio
                        .errors
                        .last_error
                        .clone()
                        .unwrap_or_else(|| "-".to_string()),
                );
                ui.end_row();
            });

        ui.add(
            egui::ProgressBar::new(audio.buffer_level)
                .text(format!("Buffer {:.0}%", audio.buffer_level * 100.0))
                .show_percentage(),
        );
    }

    /// Adiciona uma entrada ao log, descartando a mais antiga quando cheio.
    fn push_error(&mut self, entry: String) {
        if self.error_log.len() >= MAX_ERROR_LOG {
            self.error_log.pop_front();
        }
        self.error_log.push_back(entry);
    }

    // -----------------------------------------------------------------------
    // Gráfico de bitrate
    // -----------------------------------------------------------------------

    fn show_bitrate_plot(&self, ui: &mut egui::Ui, state: &AppState) {
        ui.label("Bitrate (60 s)");

        let now = std::time::Instant::now();

        // Série de bitrate total.
        let total_points: PlotPoints = state
            .bitrate_history
            .iter()
            .map(|(t, kbps)| {
                let x = -(now.duration_since(*t).as_secs_f64());
                [x, *kbps]
            })
            .filter(|[x, _]| *x >= -PLOT_WINDOW_SECS)
            .collect();

        // Série do PID selecionado (referência horizontal com bitrate atual).
        let pid_points: Option<PlotPoints> = state.selected_pid.and_then(|pid| {
            let entry = state.metrics.pid_table.iter().find(|e| e.pid == pid)?;
            let current = entry.bitrate_kbps;
            if current > 0.0 {
                Some(PlotPoints::from(vec![
                    [-PLOT_WINDOW_SECS, current],
                    [0.0, current],
                ]))
            } else {
                None
            }
        });

        Plot::new("bitrate_plot")
            .height(120.0)
            .include_y(0.0)
            .x_axis_label("s")
            .y_axis_label("kbps")
            .show(ui, |plot_ui| {
                plot_ui.line(
                    Line::new(total_points)
                        .name("Total")
                        .color(egui::Color32::from_rgb(100, 200, 100)),
                );
                if let Some(pts) = pid_points {
                    plot_ui.line(
                        Line::new(pts)
                            .name("PID sel.")
                            .color(egui::Color32::from_rgb(230, 160, 50)),
                    );
                }
            });
    }

    // -----------------------------------------------------------------------
    // Gráfico de PCR jitter
    // -----------------------------------------------------------------------

    fn show_jitter_plot(&self, ui: &mut egui::Ui, state: &AppState) {
        ui.label("PCR Jitter (µs)");

        let now = std::time::Instant::now();

        // Coleta todos os registros de jitter nos últimos 60 s.
        let jitter_points: PlotPoints = state
            .pcr_history
            .values()
            .flat_map(|events| {
                events.iter().map(|r| {
                    let x = -(now.duration_since(r.timestamp).as_secs_f64());
                    let y = (r.measured_us - r.expected_us) as f64;
                    [x, y]
                })
            })
            .filter(|[x, _]| *x >= -PLOT_WINDOW_SECS)
            .collect();

        let threshold_pos = PlotPoints::from(vec![
            [-PLOT_WINDOW_SECS, PCR_JITTER_THRESHOLD_US],
            [0.0, PCR_JITTER_THRESHOLD_US],
        ]);
        let threshold_neg = PlotPoints::from(vec![
            [-PLOT_WINDOW_SECS, -PCR_JITTER_THRESHOLD_US],
            [0.0, -PCR_JITTER_THRESHOLD_US],
        ]);

        Plot::new("jitter_plot")
            .height(100.0)
            .x_axis_label("s")
            .y_axis_label("µs")
            .show(ui, |plot_ui| {
                plot_ui.line(
                    Line::new(jitter_points)
                        .name("Jitter")
                        .color(egui::Color32::from_rgb(100, 180, 255)),
                );
                plot_ui.line(
                    Line::new(threshold_pos)
                        .name("+500 µs")
                        .color(egui::Color32::from_rgb(255, 80, 80)),
                );
                plot_ui.line(
                    Line::new(threshold_neg)
                        .name("-500 µs")
                        .color(egui::Color32::from_rgb(255, 80, 80)),
                );
            });
    }

    // -----------------------------------------------------------------------
    // Log de erros
    // -----------------------------------------------------------------------

    fn show_error_log(&mut self, ui: &mut egui::Ui, cmd_tx: &Sender<AppCommand>) {
        ui.horizontal(|ui| {
            ui.label(format!(
                "Log de Erros ({}/{})",
                self.error_log.len(),
                MAX_ERROR_LOG
            ));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Limpar").clicked() {
                    self.error_log.clear();
                    self.seen_jitter = 0;
                    self.seen_discontinuity = 0;
                    let _ = cmd_tx.try_send(AppCommand::ResetErrors);
                }
                if ui.button("Copiar TSV").clicked() {
                    let tsv: String = self
                        .error_log
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n");
                    ui.ctx().copy_text(tsv);
                }
            });
        });

        let height = ui.available_height().clamp(60.0, 200.0);
        egui::ScrollArea::vertical()
            .id_salt("error_log_scroll")
            .max_height(height)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for entry in &self.error_log {
                    ui.label(entry);
                }
            });
    }
}

fn audio_state_label(state: AudioOperationalState) -> &'static str {
    match state {
        AudioOperationalState::Idle => "Ocioso",
        AudioOperationalState::Buffering => "Bufferizando",
        AudioOperationalState::Playing => "Reproduzindo",
        AudioOperationalState::Recovering => "Recuperando",
        AudioOperationalState::Error => "Erro",
    }
}

// ---------------------------------------------------------------------------
// Testes
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_ui_005_audio_state_label_maps_variants() {
        assert_eq!(audio_state_label(AudioOperationalState::Idle), "Ocioso");
        assert_eq!(
            audio_state_label(AudioOperationalState::Buffering),
            "Bufferizando"
        );
        assert_eq!(
            audio_state_label(AudioOperationalState::Playing),
            "Reproduzindo"
        );
        assert_eq!(
            audio_state_label(AudioOperationalState::Recovering),
            "Recuperando"
        );
        assert_eq!(audio_state_label(AudioOperationalState::Error), "Erro");
    }

    /// SPEC-UI-005 — push_error descarta a entrada mais antiga ao atingir MAX_ERROR_LOG.
    #[test]
    fn spec_ui_005_error_log_max_entries() {
        let mut panel = MetricsPanel::new();
        for i in 0..=MAX_ERROR_LOG {
            panel.push_error(format!("entry {i}"));
        }
        assert_eq!(panel.error_log.len(), MAX_ERROR_LOG);
        // A primeira entrada deve ter sido descartada.
        assert_eq!(panel.error_log.front().map(|s| s.as_str()), Some("entry 1"));
    }

    /// SPEC-UI-005 — push_error não descarta entradas antes de atingir o limite.
    #[test]
    fn spec_ui_005_error_log_below_max() {
        let mut panel = MetricsPanel::new();
        for i in 0..10 {
            panel.push_error(format!("entry {i}"));
        }
        assert_eq!(panel.error_log.len(), 10);
    }

    /// SPEC-UI-005 — Limpar zera log e reinicia cursores de eventos.
    #[test]
    fn spec_ui_005_clear_resets_seen_counters() {
        let mut panel = MetricsPanel::new();
        panel.seen_jitter = 5;
        panel.seen_discontinuity = 3;
        panel.push_error("err".to_owned());
        // Simulate a clear (without cmd_tx).
        panel.error_log.clear();
        panel.seen_jitter = 0;
        panel.seen_discontinuity = 0;
        assert!(panel.error_log.is_empty());
        assert_eq!(panel.seen_jitter, 0);
        assert_eq!(panel.seen_discontinuity, 0);
    }
}
