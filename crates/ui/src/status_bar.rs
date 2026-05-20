//! `StatusBar` — barra de status com ícone de conexão, bitrate e CC errors.
//!
//! SPEC-UI-006

use egui::Ui;

use crate::state::{AppState, ConnectionState};

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
}
