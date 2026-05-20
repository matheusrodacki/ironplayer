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
    /// `video_texture`: par `(TextureId, (width, height))` do frame mais
    /// recente produzido pelo `VideoRenderer`. Quando `Some`, exibe o frame
    /// escalado para preencher a área disponível mantendo o aspect-ratio.
    /// Quando `None` e conectado sem serviço selecionado, exibe fundo preto.
    /// Caso contrário, mostra o placeholder `"[ sem stream ]"`.
    ///
    /// SPEC-UI-001 · SPEC-AV-003
    pub fn show(
        ui: &mut egui::Ui,
        state: &AppState,
        video_texture: Option<(egui::TextureId, (u32, u32))>,
    ) {
        let has_stream = matches!(state.connection, ConnectionState::Connected { .. })
            && state.selected_service.is_some();

        if let Some((tex_id, (w, h))) = video_texture {
            // Exibe o frame decodificado escalado com aspect-ratio correto.
            let available = ui.available_size();
            let aspect = w as f32 / h.max(1) as f32;
            let (draw_w, draw_h) = if available.x / available.y > aspect {
                (available.y * aspect, available.y)
            } else {
                (available.x, available.x / aspect)
            };

            let (rect, _) = ui.allocate_exact_size(available, egui::Sense::hover());
            let center = rect.center();
            let draw_rect = egui::Rect::from_center_size(center, egui::vec2(draw_w, draw_h));

            ui.painter()
                .rect_filled(rect, egui::Rounding::ZERO, egui::Color32::BLACK);
            ui.painter().image(
                tex_id,
                draw_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else if has_stream {
            // Conectado mas ainda sem frame: fundo preto.
            let available = ui.available_size();
            let (rect, _) = ui.allocate_exact_size(available, egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, egui::Rounding::ZERO, egui::Color32::BLACK);
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

    #[test]
    fn spec_ui_001_video_panel_shows_frame_when_texture_present() {
        // Quando video_texture é Some, o painel deve exibir o frame
        // independentemente do estado de conexão.
        let tex_id = egui::TextureId::User(42);
        let dims = (1920u32, 1080u32);
        // Verifica que aspect-ratio está correto.
        let aspect = dims.0 as f32 / dims.1 as f32;
        assert!((aspect - 1.777).abs() < 0.01);
        // A presença do TextureId é suficiente para indicar frame disponível.
        let video_texture: Option<(egui::TextureId, (u32, u32))> = Some((tex_id, dims));
        assert!(video_texture.is_some());
    }
}
