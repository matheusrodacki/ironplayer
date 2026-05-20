//! Frame de vídeo decodificado e renderizador wgpu (D3D11) + CPU fallback.
//!
//! SPEC-AV-003 · SPEC-AV-003c

use std::sync::Arc;

use bytes::Bytes;
use egui::mutex::RwLock;
use egui::{ColorImage, TextureHandle, TextureId, TextureOptions};

use crate::error::AvError;

// ─── VideoFrame ───────────────────────────────────────────────────────────────

/// Frame de vídeo decodificado em formato RGB24.
///
/// Cada pixel ocupa 3 bytes (`R`, `G`, `B`). O tamanho esperado do `data` é
/// `width * height * 3` bytes, linha a linha, top-down.
///
/// SPEC-AV-003
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Largura do frame em pixels.
    pub width: u32,
    /// Altura do frame em pixels.
    pub height: u32,
    /// Presentation Timestamp em unidades de 90 kHz.
    pub pts: Option<u64>,
    /// Dados RGB24: `width * height * 3` bytes, linha a linha, top-down.
    pub data: Bytes,
}

impl VideoFrame {
    /// Cria um `VideoFrame` a partir de dados RGB24 brutos.
    ///
    /// SPEC-AV-003
    pub fn new(width: u32, height: u32, pts: Option<u64>, data: Bytes) -> Self {
        Self {
            width,
            height,
            pts,
            data,
        }
    }

    /// Verifica se o tamanho de `data` é consistente com `width × height × 3`.
    ///
    /// SPEC-AV-003
    pub fn is_valid_size(&self) -> bool {
        self.data.len() == (self.width as usize) * (self.height as usize) * 3
    }
}

// ─── Helper ───────────────────────────────────────────────────────────────────

/// Converte RGB24 → RGBA8 (alpha fixo = 255).
///
/// Necessário porque wgpu/egui usam RGBA8, enquanto `VideoFrame` usa RGB24.
fn rgb24_to_rgba8(rgb: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(rgb.len() / 3 * 4);
    for chunk in rgb.chunks_exact(3) {
        rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
    }
    rgba
}

// ─── GPU renderer ─────────────────────────────────────────────────────────────

struct GpuRenderer {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    egui_renderer: Arc<RwLock<egui_wgpu::Renderer>>,
    texture: Option<wgpu::Texture>,
    texture_id: Option<TextureId>,
    dims: Option<(u32, u32)>,
}

impl GpuRenderer {
    fn upload(&mut self, frame: &VideoFrame) -> Result<(), AvError> {
        let new_dims = (frame.width, frame.height);

        // Recria a textura wgpu quando as dimensões mudam.
        if self.dims != Some(new_dims) {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("video_frame"),
                size: wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                // egui_wgpu::Renderer requer Rgba8UnormSrgb para texturas nativas.
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

            let mut rend = self.egui_renderer.write();
            if let Some(id) = self.texture_id {
                rend.update_egui_texture_from_wgpu_texture(
                    &self.device,
                    &view,
                    wgpu::FilterMode::Linear,
                    id,
                );
            } else {
                let id =
                    rend.register_native_texture(&self.device, &view, wgpu::FilterMode::Linear);
                self.texture_id = Some(id);
            }
            // `view` pode ser dropped; o recurso GPU é mantido pelo bind group interno do egui.
            self.texture = Some(texture);
            self.dims = Some(new_dims);
        }

        // Faz upload dos pixels RGB24 → RGBA8 para a textura wgpu.
        if let Some(texture) = &self.texture {
            let rgba = rgb24_to_rgba8(frame.data.as_ref());
            self.queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &rgba,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * frame.width),
                    rows_per_image: Some(frame.height),
                },
                wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
            );
        }

        Ok(())
    }
}

impl Drop for GpuRenderer {
    fn drop(&mut self) {
        if let Some(id) = self.texture_id {
            self.egui_renderer.write().free_texture(&id);
        }
    }
}

// ─── CPU renderer ─────────────────────────────────────────────────────────────

struct CpuRenderer {
    ctx: egui::Context,
    handle: Option<TextureHandle>,
}

impl CpuRenderer {
    fn upload(&mut self, frame: &VideoFrame) -> Result<(), AvError> {
        let pixels: Vec<egui::Color32> = frame
            .data
            .chunks_exact(3)
            .map(|c| egui::Color32::from_rgb(c[0], c[1], c[2]))
            .collect();

        let color_image = ColorImage {
            size: [frame.width as usize, frame.height as usize],
            pixels,
        };

        if let Some(ref mut handle) = self.handle {
            handle.set(color_image, TextureOptions::LINEAR);
        } else {
            self.handle = Some(self.ctx.load_texture(
                "video_frame",
                color_image,
                TextureOptions::LINEAR,
            ));
        }

        Ok(())
    }
}

// ─── VideoRenderer ────────────────────────────────────────────────────────────

enum RendererInner {
    Gpu(GpuRenderer),
    Cpu(CpuRenderer),
}

/// Renderizador de frames de vídeo: modo GPU (wgpu/D3D11) ou CPU (egui::ColorImage fallback).
///
/// Em modo GPU, faz upload do `VideoFrame` RGB24 diretamente para uma textura wgpu,
/// recriando-a quando as dimensões mudam. Em modo CPU (fallback quando D3D11 está
/// indisponível), converte o frame para `egui::ColorImage`.
///
/// O `texture_id()` retornado é válido para uso em `egui::Image`.
///
/// SPEC-AV-003 · SPEC-AV-003c
pub struct VideoRenderer {
    inner: RendererInner,
}

impl VideoRenderer {
    /// Cria um `VideoRenderer` em modo GPU (wgpu/D3D11).
    ///
    /// `device`, `queue` e `egui_renderer` devem ser obtidos de
    /// `eframe::egui_wgpu::RenderState` (ou `egui_wgpu::RenderState`) na
    /// inicialização do app.
    ///
    /// SPEC-AV-003
    pub fn new_gpu(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        egui_renderer: Arc<RwLock<egui_wgpu::Renderer>>,
    ) -> Self {
        Self {
            inner: RendererInner::Gpu(GpuRenderer {
                device,
                queue,
                egui_renderer,
                texture: None,
                texture_id: None,
                dims: None,
            }),
        }
    }

    /// Cria um `VideoRenderer` em modo CPU (fallback via `egui::ColorImage`).
    ///
    /// Usado quando D3D11/wgpu está indisponível.
    ///
    /// SPEC-AV-003c
    pub fn new_cpu(ctx: egui::Context) -> Self {
        Self {
            inner: RendererInner::Cpu(CpuRenderer { ctx, handle: None }),
        }
    }

    /// Envia um `VideoFrame` RGB24 para a textura (GPU ou CPU).
    ///
    /// - Modo GPU: converte RGB24 → RGBA8, executa `queue.write_texture()` e recria
    ///   a textura wgpu se as dimensões mudaram desde o último upload.
    /// - Modo CPU: atualiza `egui::TextureHandle` via `egui::ColorImage`.
    ///
    /// Retorna `Err` se o frame tiver tamanho inconsistente com `width × height × 3`.
    ///
    /// SPEC-AV-003
    pub fn upload(&mut self, frame: &VideoFrame) -> Result<(), AvError> {
        if !frame.is_valid_size() {
            return Err(AvError::Other(anyhow::anyhow!(
                "VideoFrame com tamanho inválido: esperado {}×{}×3={} bytes, obtido {}",
                frame.width,
                frame.height,
                (frame.width as usize) * (frame.height as usize) * 3,
                frame.data.len(),
            )));
        }
        match &mut self.inner {
            RendererInner::Gpu(gpu) => gpu.upload(frame),
            RendererInner::Cpu(cpu) => cpu.upload(frame),
        }
    }

    /// Retorna o `egui::TextureId` atual para uso em `egui::Image`.
    ///
    /// Retorna `None` até o primeiro `upload()` bem-sucedido.
    ///
    /// SPEC-AV-003
    pub fn texture_id(&self) -> Option<TextureId> {
        match &self.inner {
            RendererInner::Gpu(gpu) => gpu.texture_id,
            RendererInner::Cpu(cpu) => cpu.handle.as_ref().map(|h| h.id()),
        }
    }

    /// Retorna `true` se o renderer está em modo GPU (wgpu/D3D11).
    ///
    /// Retorna `false` quando operando em modo CPU (fallback `egui::ColorImage`).
    ///
    /// SPEC-AV-003c
    pub fn is_gpu_mode(&self) -> bool {
        matches!(&self.inner, RendererInner::Gpu(_))
    }
}

// ─── Testes ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Cria um `VideoFrame` RGB24 sólido (todos os pixels com a mesma cor).
    fn solid_frame(width: u32, height: u32, r: u8, g: u8, b: u8) -> VideoFrame {
        let count = (width * height) as usize;
        let mut data = Vec::with_capacity(count * 3);
        for _ in 0..count {
            data.extend_from_slice(&[r, g, b]);
        }
        VideoFrame::new(width, height, None, Bytes::from(data))
    }

    // ── SPEC-AV-003c: fallback CPU ──────────────────────────────────────────

    /// SPEC-AV-003c: modo CPU deve retornar `is_gpu_mode() == false`.
    #[test]
    fn spec_av_003c_cpu_mode_is_not_gpu() {
        let ctx = egui::Context::default();
        let renderer = VideoRenderer::new_cpu(ctx);
        assert!(
            !renderer.is_gpu_mode(),
            "Modo CPU deve retornar is_gpu_mode() == false (SPEC-AV-003c)"
        );
    }

    /// `texture_id()` deve ser `None` antes do primeiro upload.
    #[test]
    fn spec_av_003_cpu_texture_id_none_before_upload() {
        let ctx = egui::Context::default();
        let renderer = VideoRenderer::new_cpu(ctx);
        assert!(
            renderer.texture_id().is_none(),
            "texture_id() deve ser None antes do primeiro upload"
        );
    }

    /// Após o primeiro upload, `texture_id()` deve retornar `Some`.
    #[test]
    fn spec_av_003_cpu_upload_creates_texture_id() {
        let ctx = egui::Context::default();
        let mut renderer = VideoRenderer::new_cpu(ctx);
        let frame = solid_frame(4, 4, 255, 0, 0);
        renderer.upload(&frame).expect("upload não deve falhar");
        assert!(
            renderer.texture_id().is_some(),
            "texture_id() deve ser Some após upload bem-sucedido"
        );
    }

    /// Segundo upload com mesma dimensão deve reutilizar o mesmo `TextureId`.
    #[test]
    fn spec_av_003_cpu_upload_same_id_on_second_upload() {
        let ctx = egui::Context::default();
        let mut renderer = VideoRenderer::new_cpu(ctx);

        let frame1 = solid_frame(8, 8, 255, 0, 0);
        renderer.upload(&frame1).expect("primeiro upload");
        let id1 = renderer.texture_id();

        let frame2 = solid_frame(8, 8, 0, 255, 0);
        renderer.upload(&frame2).expect("segundo upload");
        let id2 = renderer.texture_id();

        assert_eq!(
            id1, id2,
            "TextureId deve ser estável entre uploads de mesma dimensão"
        );
    }

    // ── Conversão RGB24 → RGBA8 ─────────────────────────────────────────────

    /// Conversão de RGB24 para RGBA8 preserva R, G, B e injeta alpha=255.
    #[test]
    fn spec_av_003_rgb24_to_rgba8_preserves_channels() {
        let rgb = [255u8, 128, 0, 0, 64, 255];
        let rgba = rgb24_to_rgba8(&rgb);
        assert_eq!(rgba, [255, 128, 0, 255, 0, 64, 255, 255]);
    }

    /// Todos os alphas gerados por `rgb24_to_rgba8` devem ser 255.
    #[test]
    fn spec_av_003_rgb24_to_rgba8_alpha_is_255() {
        let rgb = [10u8, 20, 30, 40, 50, 60, 70, 80, 90];
        let rgba = rgb24_to_rgba8(&rgb);
        for (i, chunk) in rgba.chunks_exact(4).enumerate() {
            assert_eq!(chunk[3], 255, "Alpha do pixel {i} deve ser 255");
        }
    }

    // ── VideoFrame ──────────────────────────────────────────────────────────

    /// `is_valid_size()` deve retornar `true` para frame com tamanho correto.
    #[test]
    fn spec_av_003_video_frame_valid_size_check() {
        let frame = solid_frame(16, 9, 0, 0, 0);
        assert!(
            frame.is_valid_size(),
            "Frame 16×9 RGB24 deve ter tamanho válido"
        );
    }

    /// `is_valid_size()` deve retornar `false` para frame com tamanho incorreto.
    #[test]
    fn spec_av_003_video_frame_invalid_size_detected() {
        let frame = VideoFrame::new(16, 9, None, Bytes::from(vec![0u8; 10]));
        assert!(
            !frame.is_valid_size(),
            "Frame com 10 bytes deve ser inválido para 16×9"
        );
    }

    /// `upload()` deve retornar `Err` para frame com tamanho inválido.
    #[test]
    fn spec_av_003_upload_rejects_invalid_frame() {
        let ctx = egui::Context::default();
        let mut renderer = VideoRenderer::new_cpu(ctx);
        let bad_frame = VideoFrame::new(4, 4, None, Bytes::from(vec![0u8; 5]));
        assert!(
            renderer.upload(&bad_frame).is_err(),
            "upload() deve retornar Err para frame com tamanho inválido"
        );
    }

    /// Frame 0×0 tem tamanho zero, que é consistente com `0*0*3 = 0` bytes.
    #[test]
    fn spec_av_003_zero_dimension_frame_is_valid_size() {
        let frame = VideoFrame::new(0, 0, None, Bytes::new());
        assert!(frame.is_valid_size());
    }

    /// Upload de frame 0×0 não deve falhar no modo CPU.
    #[test]
    fn spec_av_003_cpu_zero_dim_upload() {
        let ctx = egui::Context::default();
        let mut renderer = VideoRenderer::new_cpu(ctx);
        let frame = VideoFrame::new(0, 0, None, Bytes::new());
        renderer
            .upload(&frame)
            .expect("upload de frame 0×0 não deve falhar");
    }

    /// Mudança de dimensão no modo CPU mantém o mesmo `TextureId` (handle reutilizado).
    #[test]
    fn spec_av_003_cpu_dimension_change_reuses_handle() {
        let ctx = egui::Context::default();
        let mut renderer = VideoRenderer::new_cpu(ctx);

        let frame_small = solid_frame(4, 4, 128, 128, 128);
        renderer.upload(&frame_small).expect("upload 4×4");
        let id_before = renderer.texture_id();

        // CpuRenderer chama `handle.set()` ao mudar dimensão, mantendo o mesmo TextureId.
        let frame_large = solid_frame(8, 8, 64, 64, 64);
        renderer.upload(&frame_large).expect("upload 8×8");
        let id_after = renderer.texture_id();

        assert_eq!(
            id_before, id_after,
            "CpuRenderer reutiliza o mesmo TextureHandle (mesmo ID) ao mudar dimensão"
        );
    }
}
