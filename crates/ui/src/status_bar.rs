//! `StatusBar` — barra de status com ícone de conexão, bitrate e CC errors.
//!
//! SPEC-UI-006

use egui::Ui;

use crate::state::{AppState, AudioOperationalState, AudioStatusSnapshot, ConnectionState};

/// Retorna `(ícone, texto)` para o estado de conexão.
///
/// O ícone é sempre um caractere Unicode visível — nunca apenas cor.
///
/// SPEC-UI-006
pub fn status_icon_text(state: &ConnectionState) -> (&'static str, String) {
    match state {
        ConnectionState::Idle => ("○", "Desconectado".to_string()),
        ConnectionState::Connecting { url } => ("◌", format!("Conectando\u{2026} {url}")),
        ConnectionState::Connected { url, .. } => ("●", format!("Conectado  {url}")),
        ConnectionState::Error { reason, .. } => ("⚠", format!("Erro: {reason}")),
    }
}

fn audio_status_text(audio: &AudioStatusSnapshot) -> String {
    let mut parts = vec![match audio.state {
        AudioOperationalState::Idle => "Áudio ocioso".to_string(),
        AudioOperationalState::Buffering => "Áudio bufferizando".to_string(),
        AudioOperationalState::Playing => "Áudio reproduzindo".to_string(),
        AudioOperationalState::Recovering => "Áudio recuperando".to_string(),
        AudioOperationalState::Error => "Áudio com erro".to_string(),
    }];

    if audio.muted {
        parts.push("mudo".to_string());
    } else {
        parts.push(format!("vol {:.0}%", audio.volume * 100.0));
    }

    if let Some(track) = &audio.active_track {
        parts.push(format!(
            "{} PID {}/0x{:04X}",
            track.codec_label, track.pid, track.pid
        ));
        if let Some(language) = &track.language {
            parts.push(language.to_uppercase());
        }
    }

    if let Some(sample_rate_hz) = audio.sample_rate_hz {
        parts.push(format!("{:.1} kHz", sample_rate_hz as f32 / 1000.0));
    }
    if let Some(channels) = audio.channels {
        parts.push(format!("{channels} ch"));
    }

    parts.push(format!("buf {:.0}%", audio.buffer_level * 100.0));
    parts.join("  ")
}

/// Renderiza a barra de status na parte inferior da janela.
///
/// SPEC-UI-006
pub struct StatusBar;

impl StatusBar {
    /// Exibe o conteúdo da barra de status em `ui`.
    ///
    /// SPEC-UI-006
    pub fn show(ui: &mut Ui, state: &AppState) {
        ui.horizontal(|ui| {
            let (icon, text) = status_icon_text(&state.connection);
            ui.label(format!("{icon} {text}"));

            ui.separator();

            let kbps = state.metrics.total_bitrate_kbps;
            if kbps >= 1000.0 {
                ui.label(format!("{:.1} Mbps", kbps / 1000.0));
            } else {
                ui.label(format!("{kbps:.0} kbps"));
            }

            ui.separator();

            let total_cc: u64 = state.metrics.errors.cc_errors.values().sum();
            ui.label(format!("CC: {total_cc}"));

            ui.separator();
            ui.label(audio_status_text(&state.audio));

            if let Some(offset) = state.metrics.tdt_offset_secs {
                ui.separator();
                ui.label(format!("TDT: {offset:+}s"));
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
    use std::time::Instant;

    use crate::state::{AudioStatusSnapshot, AudioTrackInfo};

    #[test]
    fn spec_ui_006_status_bar_text_idle() {
        let (icon, text) = status_icon_text(&ConnectionState::Idle);
        assert_eq!(icon, "○");
        assert!(
            text.contains("Desconectado"),
            "expected 'Desconectado' in '{text}'"
        );
    }

    #[test]
    fn spec_ui_006_status_bar_text_connecting() {
        let state = ConnectionState::Connecting {
            url: "udp://@239.1.1.1:1234".to_string(),
        };
        let (icon, text) = status_icon_text(&state);
        assert_eq!(icon, "◌");
        assert!(text.contains("239.1.1.1"), "expected URL in '{text}'");
    }

    #[test]
    fn spec_ui_006_status_bar_text_connected() {
        let state = ConnectionState::Connected {
            url: "udp://@239.1.1.1:1234".to_string(),
            since: Instant::now(),
        };
        let (icon, text) = status_icon_text(&state);
        assert_eq!(icon, "●");
        assert!(
            text.contains("Conectado"),
            "expected 'Conectado' in '{text}'"
        );
        assert!(text.contains("239.1.1.1"), "expected URL in '{text}'");
    }

    #[test]
    fn spec_ui_006_status_bar_text_error() {
        let state = ConnectionState::Error {
            url: "udp://@239.1.1.1:1234".to_string(),
            reason: "timeout".to_string(),
        };
        let (icon, text) = status_icon_text(&state);
        assert_eq!(icon, "⚠");
        assert!(text.contains("timeout"), "expected reason in '{text}'");
    }

    #[test]
    fn spec_ui_006_audio_status_text_includes_track_and_buffer() {
        let mut audio = AudioStatusSnapshot::default();
        audio.state = AudioOperationalState::Playing;
        audio.set_volume(0.8);
        audio.active_track = Some(AudioTrackInfo {
            service_id: 1,
            pid: 0x0120,
            codec_label: "AAC (ADTS)".to_string(),
            language: Some("por".to_string()),
        });
        audio.sample_rate_hz = Some(48_000);
        audio.channels = Some(2);
        audio.buffer_level = 0.42;

        let text = audio_status_text(&audio);
        assert!(text.contains("Áudio reproduzindo"));
        assert!(text.contains("AAC (ADTS) PID 288/0x0120"));
        assert!(text.contains("POR"));
        assert!(text.contains("48.0 kHz"));
        assert!(text.contains("2 ch"));
        assert!(text.contains("buf 42%"));
    }

    #[test]
    fn spec_ui_006_audio_status_text_marks_mute() {
        let mut audio = AudioStatusSnapshot::default();
        audio.state = AudioOperationalState::Buffering;
        audio.set_volume(0.0);

        let text = audio_status_text(&audio);
        assert!(text.contains("Áudio bufferizando"));
        assert!(text.contains("mudo"));
    }
}
