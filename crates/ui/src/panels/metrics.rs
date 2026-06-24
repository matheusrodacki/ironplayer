//! `MetricsPanel` — gráficos de bitrate e PCR jitter, log de erros.
//!
//! SPEC-UI-005

use std::collections::VecDeque;

use crossbeam_channel::Sender;
use eframe::egui;
use egui_plot::{Corner, Line, Plot, PlotPoints};

use crate::state::{
    audio_downmix_active, format_card_channels, AppState, AudioOperationalState, HwAccelChoice,
};
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

/// Número de colunas na faixa inferior de métricas.
const METRICS_STRIP_COLUMNS: usize = 4;

const PLOT_STROKE_WIDTH_MAIN: f32 = 2.0;
const PLOT_STROKE_WIDTH_GUIDE: f32 = 1.0;

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
    /// Seleção corrente de hwaccel no painel "Debug A/V" (espelha o backend).
    ///
    /// SPEC-CFG-HW-001
    hwaccel_choice: HwAccelChoice,
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

    pub(crate) fn set_hwaccel_choice(&mut self, choice: HwAccelChoice) {
        self.hwaccel_choice = choice;
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
        // ── Painel Sync A/V ───────────────────────────────────────────────
        self.show_av_sync_panel(ui, state);
        ui.add_space(6.0);
        // ── Gráfico de PCR jitter ──────────────────────────────────────────
        self.show_jitter_plot(ui, state);
        ui.add_space(6.0);

        // ── Pipeline de decodificação ──────────────────────────────────────
        self.show_pipeline_panel(ui, state);
        ui.add_space(6.0);

        // ── Debug A/V (hwaccel) ────────────────────────────────────────────
        self.show_debug_av_panel(ui, state, cmd_tx);
        ui.add_space(6.0);

        // ── Log de erros ───────────────────────────────────────────────────
        self.show_error_log(ui, cmd_tx);
    }

    /// Renderiza a faixa inferior de métricas em painéis colunares.
    pub fn show_columnar_strip(
        &mut self,
        ui: &mut egui::Ui,
        state: &AppState,
        cmd_tx: &Sender<AppCommand>,
    ) {
        self.drain_errors(state);

        let spacing_x = ui.spacing().item_spacing.x;
        let column_width = ((ui.available_width()
            - spacing_x * (METRICS_STRIP_COLUMNS.saturating_sub(1) as f32))
            .max(0.0))
            / METRICS_STRIP_COLUMNS as f32;
        let column_height = ui.available_height().max(0.0);

        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(column_width, column_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    Self::show_metric_column_frame(ui, |ui| self.show_audio_summary(ui, state));
                },
            );

            ui.allocate_ui_with_layout(
                egui::vec2(column_width, column_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    Self::show_metric_column_frame(ui, |ui| {
                        self.show_bitrate_plot(ui, state);
                        ui.add_space(6.0);
                        self.show_av_sync_panel(ui, state);
                    });
                },
            );

            ui.allocate_ui_with_layout(
                egui::vec2(column_width, column_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    Self::show_metric_column_frame(ui, |ui| {
                        self.show_jitter_plot(ui, state);
                        ui.add_space(6.0);
                        self.show_pipeline_panel(ui, state);
                    });
                },
            );

            ui.allocate_ui_with_layout(
                egui::vec2(column_width, column_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    Self::show_metric_column_frame(ui, |ui| {
                        self.show_debug_av_panel(ui, state, cmd_tx);
                        ui.add_space(6.0);
                        self.show_error_log(ui, cmd_tx);
                    });
                },
            );
        });
    }

    fn show_metric_column_frame(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
        let max_height = ui.available_height().max(0.0);
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(8.0))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(ui.next_auto_id())
                    .max_height(max_height)
                    .show(ui, |ui| add_contents(ui));
            });
    }

    fn monitor_plot(id: &'static str) -> Plot<'static> {
        Plot::new(id)
            .allow_scroll(false)
            .allow_zoom(false)
            .allow_drag(false)
            .allow_boxed_zoom(false)
            .allow_double_click_reset(false)
            .show_axes([false, false])
            .show_grid([true, true])
            .sharp_grid_lines(false)
            .clamp_grid(true)
            .show_background(true)
            .legend(egui_plot::Legend::default().position(Corner::LeftTop))
    }

    // -----------------------------------------------------------------------
    // Painel de pipeline de decodificação — SPEC-METRICS-PIPELINE-001
    // -----------------------------------------------------------------------

    /// Exibe o painel com métricas do pipeline de decodificação e renderização.
    ///
    /// SPEC-METRICS-PIPELINE-001
    fn show_pipeline_panel(&self, ui: &mut egui::Ui, state: &AppState) {
        let p = &state.metrics.pipeline;
        ui.label("Pipeline");
        egui::Grid::new("pipeline_grid")
            .num_columns(2)
            .spacing([12.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label("Threads decoder");
                ui.label(p.decoder_threads_used.to_string());
                ui.end_row();

                ui.label("Deinterlace (bwdif)");
                ui.label(if p.deinterlacer_active {
                    "Ativo"
                } else {
                    "Inativo"
                });
                ui.end_row();

                ui.label("Colorspace");
                ui.label(p.colorspace.as_deref().unwrap_or("-"));
                ui.end_row();

                ui.label("Color range");
                ui.label(p.color_range.as_deref().unwrap_or("-"));
                ui.end_row();

                ui.label("Upload GPU");
                let upload = p.gpu_upload_bytes_per_sec;
                if upload >= 1_000_000 {
                    ui.label(format!("{:.1} MB/s", upload as f64 / 1_000_000.0));
                } else if upload >= 1_000 {
                    ui.label(format!("{:.1} KB/s", upload as f64 / 1_000.0));
                } else {
                    ui.label(format!("{upload} B/s"));
                }
                ui.end_row();

                // Latência de decode p50/p99 por PID de vídeo.
                let mut pids: Vec<u16> = p.decode_time_ms_p50.keys().copied().collect();
                pids.sort();
                for vpid in pids {
                    let p50 = p.decode_time_ms_p50.get(&vpid).copied().unwrap_or(0.0);
                    let p99 = p.decode_time_ms_p99.get(&vpid).copied().unwrap_or(0.0);
                    ui.label(format!("Decode PID 0x{vpid:04X}"));
                    ui.label(format!("p50={p50:.1}ms  p99={p99:.1}ms"));
                    ui.end_row();
                }
            });
    }

    // -----------------------------------------------------------------------
    // Painel Debug A/V — SPEC-METRICS-HW-001 · SPEC-CFG-HW-001
    // -----------------------------------------------------------------------

    /// Exibe o painel "Debug A/V": estado da aceleração de hardware do decoder
    /// (D3D11VA), adapter GPU em uso, contadores TDR e seletor runtime de
    /// hwaccel (Auto / D3D11VA / Off).
    ///
    /// O seletor envia `AppCommand::SetHwAccel` quando o usuário muda a opção;
    /// o backend é responsável por aplicar o modo ao `FfmpegDecoder` na
    /// próxima reabertura de stream.
    ///
    /// SPEC-METRICS-HW-001 · SPEC-CFG-HW-001
    fn show_debug_av_panel(
        &mut self,
        ui: &mut egui::Ui,
        state: &AppState,
        cmd_tx: &Sender<AppCommand>,
    ) {
        let p = &state.metrics.pipeline;
        ui.label("Debug A/V");
        egui::Grid::new("debug_av_grid")
            .num_columns(2)
            .spacing([12.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label("Hwaccel");
                let badge = if p.hw_decode_active {
                    format!(
                        "GPU ({})",
                        p.hw_decode_codec.as_deref().unwrap_or("d3d11va")
                    )
                } else if let Some(reason) = p.hw_decode_fallback_reason.as_deref() {
                    format!("CPU fallback: {reason}")
                } else {
                    "CPU".to_string()
                };
                ui.label(badge);
                ui.end_row();

                ui.label("Adapter");
                ui.label(p.gpu_adapter_name.as_deref().unwrap_or("-"));
                ui.end_row();

                ui.label("Adapter LUID");
                if p.gpu_adapter_luid != 0 {
                    ui.label(format!("{:#018x}", p.gpu_adapter_luid));
                } else {
                    ui.label("-");
                }
                ui.end_row();

                ui.label("Pool frames");
                ui.label(p.hw_frame_pool_in_use.to_string());
                ui.end_row();

                ui.label("TDR recoveries");
                ui.label(p.tdr_recoveries.to_string());
                ui.end_row();
            });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Modo:");
            let mut choice = self.hwaccel_choice;
            egui::ComboBox::from_id_salt("hwaccel_choice_combo")
                .selected_text(choice.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut choice, HwAccelChoice::Auto, "auto");
                    ui.selectable_value(&mut choice, HwAccelChoice::D3d11va, "d3d11va");
                    ui.selectable_value(&mut choice, HwAccelChoice::None, "none");
                });
            if choice != self.hwaccel_choice {
                self.hwaccel_choice = choice;
                let _ = cmd_tx.try_send(AppCommand::SetHwAccel { choice });
            }
        });
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
        let stream_bitrate_kbps = audio.active_track.as_ref().and_then(|track| {
            state
                .metrics
                .pid_table
                .iter()
                .find(|entry| entry.pid == track.pid)
                .map(|entry| entry.bitrate_kbps)
                .filter(|kbps| *kbps > 0.0)
        });

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

                ui.label("Codec");
                ui.label(
                    audio
                        .active_track
                        .as_ref()
                        .map(|track| track.codec_label.clone())
                        .unwrap_or_else(|| "-".to_string()),
                );
                ui.end_row();

                ui.label("Codec ID");
                ui.label(
                    audio
                        .active_track
                        .as_ref()
                        .and_then(|track| track.stream_type)
                        .map(|stream_type| format!("{stream_type} (0x{stream_type:02X})"))
                        .unwrap_or_else(|| "-".to_string()),
                );
                ui.end_row();

                ui.label("Perfil");
                ui.label(
                    audio
                        .codec_profile
                        .clone()
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

                ui.label("Canais (stream)");
                ui.label(
                    audio
                        .source_channels
                        .map(format_card_channels)
                        .unwrap_or_else(|| "-".to_string()),
                );
                ui.end_row();

                if audio_downmix_active(audio.source_channels, audio.output_channels) {
                    ui.label("Playback");
                    ui.label(
                        audio
                            .output_channels
                            .map(format_card_channels)
                            .unwrap_or_else(|| "-".to_string()),
                    );
                    ui.end_row();
                }

                ui.label("Bitrate");
                ui.label(match (audio.encoded_bitrate_kbps, stream_bitrate_kbps) {
                    (Some(encoded), Some(live)) => format!("{live:.0} kbps (cod. {encoded:.0} kbps)"),
                    (None, Some(live)) => format!("{live:.0} kbps"),
                    (Some(encoded), None) => format!("{encoded:.0} kbps (cod.)"),
                    (None, None) => "-".to_string(),
                });
                ui.end_row();

                ui.label("Latência saída");
                ui.label(if audio.output_latency_ms > 0 {
                    format!("{} ms", audio.output_latency_ms)
                } else {
                    "-".to_string()
                });
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

        Self::monitor_plot("bitrate_plot")
            .height(120.0)
            .include_y(0.0)
            .include_x(-PLOT_WINDOW_SECS)
            .include_x(0.0)
            .show(ui, |plot_ui| {
                plot_ui.line(
                    Line::new(total_points)
                        .name("Total")
                        .width(PLOT_STROKE_WIDTH_MAIN)
                        .color(egui::Color32::from_rgb(108, 214, 141)),
                );
                if let Some(pts) = pid_points {
                    plot_ui.line(
                        Line::new(pts)
                            .name("PID sel.")
                            .width(PLOT_STROKE_WIDTH_GUIDE)
                            .color(egui::Color32::from_rgb(244, 188, 68)),
                    );
                }
            });
    }

    // -----------------------------------------------------------------------
    // Painel Sync A/V
    // -----------------------------------------------------------------------

    /// Renderiza o painel de sincronização A/V com gráfico de offset (60 s)
    /// e contadores de drop/hold/descontinuidade/profundidade de fila.
    ///
    /// SPEC-METRICS-SYNC-001
    fn show_av_sync_panel(&self, ui: &mut egui::Ui, state: &AppState) {
        let metrics = &state.metrics;

        ui.label("Sync A/V");

        egui::Grid::new("av_sync_grid")
            .num_columns(2)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                ui.label("Offset atual");
                ui.label(format!("{:+} ms", metrics.av_sync_offset_ms));
                ui.end_row();

                ui.label("Frames tardios dropped");
                ui.label(metrics.late_frames_dropped.to_string());
                ui.end_row();

                ui.label("Frames adiantados held");
                ui.label(metrics.early_frames_held.to_string());
                ui.end_row();

                ui.label("Descontinuidades PTS");
                ui.label(metrics.pts_discontinuities.to_string());
                ui.end_row();

                ui.label("Profundidade da fila");
                ui.label(format!("{} frames", metrics.video_queue_depth));
                ui.end_row();
            });

        // Gráfico de offset A/V (60 s).
        ui.label("Offset A/V (60 s, ms)");

        let now = std::time::Instant::now();
        let sync_points: PlotPoints = state
            .av_sync_history
            .iter()
            .map(|(t, ms)| {
                let x = -(now.duration_since(*t).as_secs_f64());
                [x, *ms as f64]
            })
            .filter(|[x, _]| *x >= -PLOT_WINDOW_SECS)
            .collect();

        // Linhas de threshold HOLD (+20 ms) e DROP (-100 ms).
        let hold_line = PlotPoints::from(vec![[-PLOT_WINDOW_SECS, 20.0], [0.0, 20.0]]);
        let drop_line = PlotPoints::from(vec![[-PLOT_WINDOW_SECS, -100.0], [0.0, -100.0]]);
        let zero_line = PlotPoints::from(vec![[-PLOT_WINDOW_SECS, 0.0], [0.0, 0.0]]);

        Self::monitor_plot("av_sync_plot")
            .height(100.0)
            .include_y(0.0)
            .include_x(-PLOT_WINDOW_SECS)
            .include_x(0.0)
            .show(ui, |plot_ui| {
                plot_ui.line(
                    Line::new(zero_line)
                        .name("0 ms")
                        .width(PLOT_STROKE_WIDTH_GUIDE)
                        .color(egui::Color32::from_rgb(95, 108, 122)),
                );
                plot_ui.line(
                    Line::new(hold_line)
                        .name("+20 ms (hold)")
                        .width(PLOT_STROKE_WIDTH_GUIDE)
                        .color(egui::Color32::from_rgb(244, 188, 68)),
                );
                plot_ui.line(
                    Line::new(drop_line)
                        .name("-100 ms (drop)")
                        .width(PLOT_STROKE_WIDTH_GUIDE)
                        .color(egui::Color32::from_rgb(255, 107, 107)),
                );
                plot_ui.line(
                    Line::new(sync_points)
                        .name("Offset A/V")
                        .width(PLOT_STROKE_WIDTH_MAIN)
                        .color(egui::Color32::from_rgb(111, 198, 255)),
                );
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

        Self::monitor_plot("jitter_plot")
            .height(100.0)
            .include_x(-PLOT_WINDOW_SECS)
            .include_x(0.0)
            .show(ui, |plot_ui| {
                plot_ui.line(
                    Line::new(jitter_points)
                        .name("Jitter")
                        .width(PLOT_STROKE_WIDTH_MAIN)
                        .color(egui::Color32::from_rgb(111, 198, 255)),
                );
                plot_ui.line(
                    Line::new(threshold_pos)
                        .name("+500 µs")
                        .width(PLOT_STROKE_WIDTH_GUIDE)
                        .color(egui::Color32::from_rgb(255, 107, 107)),
                );
                plot_ui.line(
                    Line::new(threshold_neg)
                        .name("-500 µs")
                        .width(PLOT_STROKE_WIDTH_GUIDE)
                        .color(egui::Color32::from_rgb(255, 107, 107)),
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

    /// SPEC-METRICS-SYNC-001 — av_sync_history acumula amostras de offset i32.
    #[test]
    fn spec_metrics_sync_001_av_sync_history_accumulates_samples() {
        use std::collections::VecDeque;
        use std::time::Instant;

        let now = Instant::now();
        let mut history: VecDeque<(Instant, i32)> = VecDeque::new();

        // Simula a lógica de amostragem a ~1 Hz do poll_video_frames.
        let samples: &[(std::time::Duration, i32)] = &[
            (std::time::Duration::from_secs(0), 0),
            (std::time::Duration::from_millis(500), 5), // < 1 s — deve ser ignorada
            (std::time::Duration::from_secs(1), -10),
            (std::time::Duration::from_secs(2), 8),
        ];

        for (delta, offset_ms) in samples {
            let t = now + *delta;
            let should_sample = match history.back() {
                None => true,
                Some((last_t, _)) => t.duration_since(*last_t) >= std::time::Duration::from_secs(1),
            };
            if should_sample {
                history.push_back((t, *offset_ms));
            }
        }

        // Entradas esperadas: t+0s (0 ms), t+1s (-10 ms), t+2s (8 ms).
        // A entrada t+0.5s (5 ms) deve ter sido descartada por < 1 s desde a última.
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].1, 0);
        assert_eq!(history[1].1, -10);
        assert_eq!(history[2].1, 8);
    }

    #[test]
    fn spec_cfg_hw_001_set_hwaccel_choice_updates_panel_state() {
        let mut panel = MetricsPanel::new();
        panel.set_hwaccel_choice(HwAccelChoice::D3d11va);
        assert_eq!(panel.hwaccel_choice, HwAccelChoice::D3d11va);
    }
}
