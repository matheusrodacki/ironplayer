//! `VideoPanel` — exibe frame de vídeo ou placeholder quando sem stream.
//!
//! SPEC-UI-001

use eframe::egui;

use crate::state::ConnectionState;
use crate::AppState;

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
    /// Quando não há stream ativo (estado Idle ou Error) ou o serviço ainda não
    /// foi selecionado, exibe um placeholder centralizado com a mensagem
    /// `"[ sem stream ]"`. Quando conectado, reserva a área para o frame de
    /// vídeo futuro.
    ///
    /// SPEC-UI-001
    pub fn show(ui: &mut egui::Ui, state: &AppState) {
        let has_stream = matches!(state.connection, ConnectionState::Connected { .. })
            && state.selected_service.is_some();

        if has_stream {
            // Área reservada para o frame de vídeo (TextureId futuro).
            let available = ui.available_size();
            let (rect, _) = ui.allocate_exact_size(available, egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, egui::Rounding::ZERO, egui::Color32::BLACK);
            // TODO(SPEC-AV-*): renderizar TextureId aqui quando disponível.
        } else {
            // Placeholder centralizado.
            let available = ui.available_size();
            ui.allocate_ui_with_layout(
                available,
                egui::Layout::centered_and_justified(egui::Direction::TopDown),
                |ui| {
                    ui.label(
                        egui::RichText::new("[ sem stream ]")
                            .color(egui::Color32::GRAY)
                            .size(18.0),
                    );
                },
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Testes
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
}
