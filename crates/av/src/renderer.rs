//! Pipeline de renderização GPU: upload YUV planar → shader WGSL YUV→RGB.
//!
//! Arquitetura:
//! - Modo GPU: 3 texturas R8Unorm/R16Unorm (Y, U, V) + render pipeline WGSL +
//!   `egui::PaintCallback` via `egui_wgpu::CallbackTrait`.
//! - Modo CPU: fallback `egui::ColorImage` sem wgpu.
//!
//! SPEC-AV-003 · SPEC-AV-003c

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};

use egui::epaint::PaintCallbackInfo;
use egui::{ColorImage, TextureHandle, TextureId, TextureOptions};
use egui_wgpu::CallbackTrait;

use crate::error::AvError;
use crate::hw::{ColorSpace, D3d11Device, HwPixelFormat, TransferFunction};
use crate::video_queue::{HwVideoFrame, YuvColorRange, YuvColorspace, YuvFrame};

// ─── Constante: shader WGSL embutido ─────────────────────────────────────────

const YUV_SHADER_SRC: &str = include_str!("yuv_to_rgb.wgsl");
const NV12_SHADER_SRC: &str = include_str!("nv12_to_rgb.wgsl");

// ─── Uniform GPU struct ───────────────────────────────────────────────────────

/// Layout do uniform buffer `YuvParams` no shader WGSL.
///
/// Mapeamento WGSL std140:
/// - `mat3x3f` → 3 colunas × `vec4f` (16 bytes/coluna) = 48 bytes
/// - `vec3f offset` → 12 bytes, + `range_scale f32` na quarta slot = 16 bytes
/// - Total: 64 bytes (múltiplo de 16 ✓)
///
/// SPEC-AV-003
#[derive(Clone, Copy)]
#[repr(C)]
struct YuvParamsGpu {
    /// Coluna 0 da mat3x3f (coeficientes Y → R,G,B) + transfer mode.
    col0: [f32; 4],
    /// Coluna 1 da mat3x3f (coeficientes U/Cb → R,G,B) + hdr_clip flag.
    col1: [f32; 4],
    /// Coluna 2 da mat3x3f (coeficientes V/Cr → R,G,B) + padding.
    col2: [f32; 4],
    /// x=luma_offset, y=centro UV, z reservado, w=range_scale.
    offset_and_range: [f32; 4],
}

impl YuvParamsGpu {
    /// Serializa o struct para 64 bytes little-endian para upload no UBO.
    fn to_bytes(self) -> [u8; 64] {
        let mut out = [0u8; 64];
        let write_f32 = |buf: &mut [u8], off: usize, v: f32| {
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        };
        for (i, &v) in self.col0.iter().enumerate() {
            write_f32(&mut out, i * 4, v);
        }
        for (i, &v) in self.col1.iter().enumerate() {
            write_f32(&mut out, 16 + i * 4, v);
        }
        for (i, &v) in self.col2.iter().enumerate() {
            write_f32(&mut out, 32 + i * 4, v);
        }
        for (i, &v) in self.offset_and_range.iter().enumerate() {
            write_f32(&mut out, 48 + i * 4, v);
        }
        out
    }

    /// Computa os parâmetros YUV → RGB a partir dos metadados do frame.
    ///
    /// O shader usa:
    ///   `yuv = vec3((y - luma_offset) * range_scale, u - uv_center, v - uv_center)`
    ///   `rgb = matrix * yuv`
    ///
    /// SPEC-AV-003
    fn for_frame(
        colorspace: YuvColorspace,
        color_range: YuvColorRange,
        transfer: TransferFunction,
        ten_bit: bool,
    ) -> Self {
        // Coeficientes BT.xxx para Y normalizado (0..1), U/V centrados (-0.5..0.5).
        // Formato colunas: col0=Y, col1=U/Cb, col2=V/Cr (cada coluna produz [R,G,B]).
        let (cy, cu, cv): ([f32; 3], [f32; 3], [f32; 3]) = match colorspace {
            YuvColorspace::Bt601 => (
                [1.0, 1.0, 1.0],
                [0.0, -0.344_136, 1.772],
                [1.402, -0.714_136, 0.0],
            ),
            // Unspecified → tratar como BT.709 (padrão HD).
            YuvColorspace::Bt709 | YuvColorspace::Unspecified => (
                [1.0, 1.0, 1.0],
                [0.0, -0.187_324, 1.855_6],
                [1.574_8, -0.468_124, 0.0],
            ),
            YuvColorspace::Bt2020 => (
                [1.0, 1.0, 1.0],
                [0.0, -0.164_553, 1.881_4],
                [1.474_6, -0.571_353, 0.0],
            ),
        };

        let transfer_mode = match transfer {
            TransferFunction::Bt1886 => 0.0,
            TransferFunction::Pq => 1.0,
            TransferFunction::Hlg => 2.0,
            TransferFunction::Srgb => 3.0,
        };
        let hdr_clip =
            matches!(transfer, TransferFunction::Pq | TransferFunction::Hlg) as u8 as f32;

        match color_range {
            // ─ TV-range (limited): Y ∈ [16,235], U/V ∈ [16,240] (8-bit).
            // Em 10-bit: Y ∈ [64,940], U/V ∈ [64,960].
            YuvColorRange::Limited => {
                let (luma_min, luma_max, chroma_span) = if ten_bit {
                    (
                        64.0_f32 / 1023.0_f32,
                        940.0_f32 / 1023.0_f32,
                        896.0_f32 / 1023.0_f32,
                    )
                } else {
                    (
                        16.0_f32 / 255.0_f32,
                        235.0_f32 / 255.0_f32,
                        224.0_f32 / 255.0_f32,
                    )
                };
                let uv_scale = 1.0_f32 / chroma_span;
                let range_scale = 1.0_f32 / (luma_max - luma_min);
                let col0 = [cy[0], cy[1], cy[2], transfer_mode];
                let col1 = [
                    cu[0] * uv_scale,
                    cu[1] * uv_scale,
                    cu[2] * uv_scale,
                    hdr_clip,
                ];
                let col2 = [cv[0] * uv_scale, cv[1] * uv_scale, cv[2] * uv_scale, 0.0];
                Self {
                    col0,
                    col1,
                    col2,
                    offset_and_range: [luma_min, 0.5, 0.0, range_scale],
                }
            }

            // ─ Full-range: Y, U, V ∈ [0,255]; sem escala adicional.
            YuvColorRange::Full => Self {
                col0: [cy[0], cy[1], cy[2], transfer_mode],
                col1: [cu[0], cu[1], cu[2], hdr_clip],
                col2: [cv[0], cv[1], cv[2], 0.0],
                offset_and_range: [0.0, 0.5, 0.0, 1.0],
            },
        }
    }

    fn for_sw_frame(colorspace: YuvColorspace, color_range: YuvColorRange, ten_bit: bool) -> Self {
        Self::for_frame(colorspace, color_range, TransferFunction::Bt1886, ten_bit)
    }
}

fn yuv_colorspace_from_hw(color_space: ColorSpace) -> YuvColorspace {
    match color_space {
        ColorSpace::Bt601 => YuvColorspace::Bt601,
        ColorSpace::Bt709 => YuvColorspace::Bt709,
        ColorSpace::Bt2020 => YuvColorspace::Bt2020,
    }
}

// ─── Estado interno do pipeline GPU ──────────────────────────────────────────

/// Estado mutável do pipeline GPU YUV.
///
/// Protegido por `Mutex` para satisfazer `CallbackTrait: Send + Sync`
/// (o `prepare()` toma `&self`).
///
/// SPEC-AV-003
struct YuvPipelineInner {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buf: wgpu::Buffer,
    tex_y: Option<wgpu::Texture>,
    tex_u: Option<wgpu::Texture>,
    tex_v: Option<wgpu::Texture>,
    bind_group: Option<wgpu::BindGroup>,
    dims: Option<(u32, u32)>,
    ten_bit: bool,
    pending: Option<YuvFrame>,
}

impl YuvPipelineInner {
    /// Cria o pipeline wgpu, bind group layout, sampler e UBO.
    ///
    /// SPEC-AV-003
    fn create(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Result<Self, AvError> {
        let mut constants: HashMap<String, f64> = HashMap::new();
        if target_format.is_srgb() {
            constants.insert("DECODE_SRGB".to_string(), 1.0_f64);
        }

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv_to_rgb"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(YUV_SHADER_SRC)),
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: NonZeroU64::new(64),
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("yuv_pipeline_layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });

        let compilation_options = wgpu::PipelineCompilationOptions {
            constants: &constants,
            ..Default::default()
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("yuv_to_rgb_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[],
                compilation_options: compilation_options.clone(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options,
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("yuv_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("yuv_params_ubo"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            pipeline,
            bind_group_layout: bgl,
            sampler,
            uniform_buf,
            tex_y: None,
            tex_u: None,
            tex_v: None,
            bind_group: None,
            dims: None,
            ten_bit: false,
            pending: None,
        })
    }

    /// Faz upload das texturas YUV e atualiza o UBO a partir do frame pendente.
    ///
    /// SPEC-AV-003
    fn upload_pending(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let frame = match self.pending.take() {
            Some(f) => f,
            None => return,
        };
        if frame.width == 0 || frame.height == 0 {
            return;
        }

        let new_dims = (frame.width, frame.height);
        if self.dims != Some(new_dims) || self.ten_bit != frame.ten_bit {
            let fmt = if frame.ten_bit {
                wgpu::TextureFormat::R16Unorm
            } else {
                wgpu::TextureFormat::R8Unorm
            };
            let uv_w = frame.width.div_ceil(2);
            let uv_h = frame.height.div_ceil(2);

            let make_tex = |w: u32, h: u32, label: &str| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: fmt,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };

            let ty = make_tex(frame.width, frame.height, "yuv_y");
            let tu = make_tex(uv_w, uv_h, "yuv_u");
            let tv = make_tex(uv_w, uv_h, "yuv_v");

            let view_y = ty.create_view(&wgpu::TextureViewDescriptor::default());
            let view_u = tu.create_view(&wgpu::TextureViewDescriptor::default());
            let view_v = tv.create_view(&wgpu::TextureViewDescriptor::default());

            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("yuv_bind_group"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view_y),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view_u),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&view_v),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: self.uniform_buf.as_entire_binding(),
                    },
                ],
            });

            self.tex_y = Some(ty);
            self.tex_u = Some(tu);
            self.tex_v = Some(tv);
            self.bind_group = Some(bg);
            self.dims = Some(new_dims);
            self.ten_bit = frame.ten_bit;
        }

        let uv_w = frame.width.div_ceil(2);
        let uv_h = frame.height.div_ceil(2);
        let bps: u32 = if frame.ten_bit { 2 } else { 1 };

        if let Some(tex) = &self.tex_y {
            let data = prepare_plane_data(&frame.planes[0], frame.ten_bit);
            queue.write_texture(
                tex.as_image_copy(),
                &data,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(frame.width * bps),
                    rows_per_image: Some(frame.height),
                },
                wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
            );
        }
        if let Some(tex) = &self.tex_u {
            let data = prepare_plane_data(&frame.planes[1], frame.ten_bit);
            queue.write_texture(
                tex.as_image_copy(),
                &data,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(uv_w * bps),
                    rows_per_image: Some(uv_h),
                },
                wgpu::Extent3d {
                    width: uv_w,
                    height: uv_h,
                    depth_or_array_layers: 1,
                },
            );
        }
        if let Some(tex) = &self.tex_v {
            let data = prepare_plane_data(&frame.planes[2], frame.ten_bit);
            queue.write_texture(
                tex.as_image_copy(),
                &data,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(uv_w * bps),
                    rows_per_image: Some(uv_h),
                },
                wgpu::Extent3d {
                    width: uv_w,
                    height: uv_h,
                    depth_or_array_layers: 1,
                },
            );
        }

        let params = YuvParamsGpu::for_sw_frame(frame.colorspace, frame.color_range, frame.ten_bit);
        queue.write_buffer(&self.uniform_buf, 0, &params.to_bytes());
    }

    /// Emite os draw calls no render pass do egui.
    ///
    /// SPEC-AV-003
    fn paint(&self, info: &PaintCallbackInfo, render_pass: &mut wgpu::RenderPass<'_>) {
        let Some(bg) = &self.bind_group else {
            return;
        };
        let vp = info.viewport_in_pixels();
        if vp.width_px <= 0 || vp.height_px <= 0 {
            return;
        }
        render_pass.set_viewport(
            vp.left_px as f32,
            vp.top_px as f32,
            vp.width_px as f32,
            vp.height_px as f32,
            0.0,
            1.0,
        );
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, bg, &[]);
        render_pass.draw(0..3, 0..1);
    }
}

// ─── Helper: escala plano 10-bit para R16Unorm ───────────────────────────────

/// Prepara os dados de um plano para upload: retorna slice original (8-bit) ou
/// vetor escalado (10-bit → R16Unorm).
///
/// SPEC-AV-003
fn prepare_plane_data(plane: &[u8], ten_bit: bool) -> std::borrow::Cow<'_, [u8]> {
    if ten_bit {
        std::borrow::Cow::Owned(scale_10bit_plane(plane))
    } else {
        std::borrow::Cow::Borrowed(plane)
    }
}

/// Escala cada amostra de 10-bit (armazenada em u16 LE no range [0..1023],
/// nos 10 bits inferiores) para ocupar todo o range R16Unorm.
///
/// O shader normaliza dividindo por 65535.0 e espera o valor em [0..1].
/// 10-bit → 16-bit equivale a `value << 6` (×64). Ex.: 1023 × 64 = 65472.
/// Usar um multiplicador menor (ex.: 16) deixa o chroma neutro em ~0.125
/// em vez de 0.5, e o YUV→RGB produz tela verde com R e B negativos.
///
/// SPEC-AV-003
fn scale_10bit_plane(plane: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; plane.len()];
    for (i, chunk) in plane.chunks_exact(2).enumerate() {
        let raw = u16::from_le_bytes([chunk[0], chunk[1]]);
        let scaled = raw.saturating_mul(64);
        let bytes = scaled.to_le_bytes();
        out[i * 2] = bytes[0];
        out[i * 2 + 1] = bytes[1];
    }
    out
}

// ─── YuvPaintCallback ─────────────────────────────────────────────────────────

/// `egui_wgpu::CallbackTrait` que gerencia o pipeline YUV→RGB.
///
/// SPEC-AV-003
struct YuvPaintCallback {
    state: Arc<Mutex<YuvPipelineInner>>,
}

impl CallbackTrait for YuvPaintCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        _callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Ok(mut inner) = self.state.lock() {
            inner.upload_pending(device, queue);
        }
        Vec::new()
    }

    fn paint<'a>(
        &'a self,
        info: PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        _callback_resources: &'a egui_wgpu::CallbackResources,
    ) {
        if let Ok(inner) = self.state.lock() {
            inner.paint(&info, render_pass);
        }
    }
}

// ─── GpuRenderer ─────────────────────────────────────────────────────────────

struct GpuRenderer {
    state: Arc<Mutex<YuvPipelineInner>>,
}

impl GpuRenderer {
    fn new(device: Arc<wgpu::Device>, target_format: wgpu::TextureFormat) -> Result<Self, AvError> {
        let inner = YuvPipelineInner::create(&device, target_format)?;
        Ok(Self {
            state: Arc::new(Mutex::new(inner)),
        })
    }

    fn upload(&self, frame: &YuvFrame) {
        if let Ok(mut inner) = self.state.lock() {
            inner.pending = Some(frame.clone());
        }
    }

    fn paint_callback(&self, rect: egui::Rect) -> egui::PaintCallback {
        egui_wgpu::Callback::new_paint_callback(
            rect,
            YuvPaintCallback {
                state: Arc::clone(&self.state),
            },
        )
    }

    fn has_frame(&self) -> bool {
        self.state
            .lock()
            .map(|inner| inner.bind_group.is_some() || inner.pending.is_some())
            .unwrap_or(false)
    }
}

// ─── CPU renderer ─────────────────────────────────────────────────────────────

struct CpuRenderer {
    ctx: egui::Context,
    handle: Option<TextureHandle>,
}

impl CpuRenderer {
    fn upload(&mut self, frame: &YuvFrame) -> Result<(), AvError> {
        let rgba = yuv420p_to_rgba8(frame);
        let pixels: Vec<egui::Color32> = rgba
            .chunks_exact(4)
            .map(|c| egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]))
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

    fn texture_id(&self) -> Option<TextureId> {
        self.handle.as_ref().map(|h| h.id())
    }
}

// ─── NV12 pipeline (Fase C zero-copy) ────────────────────────────────────────

/// Dados de um frame NV12 prontos para upload à GPU.
struct NvPendingFrame {
    y_data: Vec<u8>,
    uv_data: Vec<u8>,
    width: u32,
    height: u32,
    /// Espaço de cor para calcular a matriz YUV→RGB.
    colorspace: YuvColorspace,
    /// Color range para ajuste de range_scale.
    color_range: YuvColorRange,
    /// TRC do conteúdo (BT.1886, PQ, HLG ou sRGB).
    transfer: TransferFunction,
    /// `true` quando os planos estão em 10-bit (P010).
    ten_bit: bool,
}

/// Estado interno do pipeline NV12.
///
/// Bindings:
/// - 0: `tex_y`  (R8Unorm — luma)
/// - 1: `tex_uv` (Rg8Unorm — croma UV interleaved)
/// - 2: `samp`   (sampler linear)
/// - 3: `params` (UBO 64 bytes — mesma `YuvParamsGpu`)
///
/// SPEC-AV-RENDER-NV12-001
struct NvPipelineInner {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buf: wgpu::Buffer,
    tex_y: Option<wgpu::Texture>,
    tex_uv: Option<wgpu::Texture>,
    bind_group: Option<wgpu::BindGroup>,
    dims: Option<(u32, u32)>,
    ten_bit: bool,
    pending: Option<NvPendingFrame>,
}

impl NvPipelineInner {
    fn create(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Result<Self, AvError> {
        let mut constants: HashMap<String, f64> = HashMap::new();
        if target_format.is_srgb() {
            constants.insert("DECODE_SRGB".to_string(), 1.0_f64);
        }

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nv12_to_rgb"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(NV12_SHADER_SRC)),
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12_bgl"),
            entries: &[
                // binding 0 — tex_y (R8Unorm)
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // binding 1 — tex_uv (Rg8Unorm)
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // binding 2 — sampler
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // binding 3 — UBO
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: NonZeroU64::new(64),
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nv12_pipeline_layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });

        let compilation_options = wgpu::PipelineCompilationOptions {
            constants: &constants,
            ..Default::default()
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nv12_to_rgb_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[],
                compilation_options: compilation_options.clone(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options,
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nv12_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nv12_params_ubo"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            pipeline,
            bind_group_layout: bgl,
            sampler,
            uniform_buf,
            tex_y: None,
            tex_uv: None,
            bind_group: None,
            dims: None,
            ten_bit: false,
            pending: None,
        })
    }

    /// Faz upload dos planos NV12 para as texturas wgpu.
    ///
    /// SPEC-AV-RENDER-NV12-001
    fn upload_pending(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let frame = match self.pending.take() {
            Some(f) => f,
            None => return,
        };
        if frame.width == 0 || frame.height == 0 {
            return;
        }
        let w = frame.width;
        let h = frame.height;
        let uv_w = w.div_ceil(2);
        let uv_h = h.div_ceil(2);
        let bytes_per_sample = if frame.ten_bit { 2 } else { 1 };

        // Recria as texturas somente se as dimensões mudaram.
        let need_new = self.dims != Some((w, h)) || self.ten_bit != frame.ten_bit;
        if need_new {
            self.tex_y = Some(device.create_texture(&wgpu::TextureDescriptor {
                label: Some("nv12_y"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: if frame.ten_bit {
                    wgpu::TextureFormat::R16Unorm
                } else {
                    wgpu::TextureFormat::R8Unorm
                },
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
            self.tex_uv = Some(device.create_texture(&wgpu::TextureDescriptor {
                label: Some("nv12_uv"),
                size: wgpu::Extent3d {
                    width: uv_w,
                    height: uv_h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: if frame.ten_bit {
                    wgpu::TextureFormat::Rg16Unorm
                } else {
                    wgpu::TextureFormat::Rg8Unorm
                },
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
            self.dims = Some((w, h));
            self.ten_bit = frame.ten_bit;
        }

        let tex_y = match &self.tex_y {
            Some(t) => t,
            None => return,
        };
        let tex_uv = match &self.tex_uv {
            Some(t) => t,
            None => return,
        };

        // Upload plano Y (R8Unorm — 1 byte/pixel).
        queue.write_texture(
            tex_y.as_image_copy(),
            &frame.y_data,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(w * bytes_per_sample),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );

        // Upload plano UV (Rg8Unorm — 2 bytes/pixel, U em R, V em G).
        // Cada linha UV do NvPlanes tem `width` bytes (uv_w pares × 2 = w bytes).
        queue.write_texture(
            tex_uv.as_image_copy(),
            &frame.uv_data,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(w * bytes_per_sample),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: uv_w,
                height: uv_h,
                depth_or_array_layers: 1,
            },
        );

        // Atualiza UBO com parâmetros YUV→RGB (reutiliza YuvParamsGpu).
        let params = YuvParamsGpu::for_frame(
            frame.colorspace,
            frame.color_range,
            frame.transfer,
            frame.ten_bit,
        );
        queue.write_buffer(&self.uniform_buf, 0, &params.to_bytes());

        // Reconstrói o bind group se as texturas foram recriadas.
        if need_new || self.bind_group.is_none() {
            let view_y = tex_y.create_view(&wgpu::TextureViewDescriptor::default());
            let view_uv = tex_uv.create_view(&wgpu::TextureViewDescriptor::default());
            self.bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("nv12_bind_group"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view_y),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view_uv),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: self.uniform_buf.as_entire_binding(),
                    },
                ],
            }));
        }
    }

    fn paint(&self, info: &PaintCallbackInfo, render_pass: &mut wgpu::RenderPass<'static>) {
        let bg = match &self.bind_group {
            Some(bg) => bg,
            None => return,
        };
        let viewport = info.viewport_in_pixels();
        render_pass.set_viewport(
            viewport.left_px as f32,
            viewport.top_px as f32,
            viewport.width_px as f32,
            viewport.height_px as f32,
            0.0,
            1.0,
        );
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, bg, &[]);
        render_pass.draw(0..3, 0..1);
    }

    fn has_frame(&self) -> bool {
        self.bind_group.is_some() || self.pending.is_some()
    }
}

/// `egui_wgpu::CallbackTrait` para o pipeline NV12.
///
/// SPEC-AV-RENDER-NV12-001
struct NvPaintCallback {
    state: Arc<Mutex<NvPipelineInner>>,
}

impl CallbackTrait for NvPaintCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        _callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Ok(mut inner) = self.state.lock() {
            inner.upload_pending(device, queue);
        }
        Vec::new()
    }

    fn paint<'a>(
        &'a self,
        info: PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        _callback_resources: &'a egui_wgpu::CallbackResources,
    ) {
        if let Ok(inner) = self.state.lock() {
            inner.paint(&info, render_pass);
        }
    }
}

/// Renderer HW NV12: extrai planos NV12 via staging D3D11 e renderiza no wgpu.
///
/// SPEC-AV-RENDER-NV12-001
struct NvRenderer {
    state: Arc<Mutex<NvPipelineInner>>,
    d3d11_dev: Arc<D3d11Device>,
}

impl NvRenderer {
    fn new(
        device: &wgpu::Device,
        d3d11_dev: Arc<D3d11Device>,
        target_format: wgpu::TextureFormat,
    ) -> Result<Self, AvError> {
        let inner = NvPipelineInner::create(device, target_format)?;
        Ok(Self {
            state: Arc::new(Mutex::new(inner)),
            d3d11_dev,
        })
    }

    fn upload_hw(&self, hw: &HwVideoFrame) -> Result<(), AvError> {
        let planes = self.d3d11_dev.extract_nv12_planes(&hw.tex)?;
        let pending = NvPendingFrame {
            y_data: planes.y_data,
            uv_data: planes.uv_data,
            width: planes.width,
            height: planes.height,
            colorspace: yuv_colorspace_from_hw(hw.tex.color_space),
            color_range: if hw.tex.full_range {
                YuvColorRange::Full
            } else {
                YuvColorRange::Limited
            },
            transfer: hw.tex.transfer,
            ten_bit: planes.ten_bit,
        };
        if let Ok(mut inner) = self.state.lock() {
            inner.pending = Some(pending);
        }
        Ok(())
    }

    fn paint_callback(&self, rect: egui::Rect) -> egui::PaintCallback {
        egui_wgpu::Callback::new_paint_callback(
            rect,
            NvPaintCallback {
                state: Arc::clone(&self.state),
            },
        )
    }

    fn has_frame(&self) -> bool {
        self.state
            .lock()
            .map(|inner| inner.has_frame())
            .unwrap_or(false)
    }
}

// ─── Helpers YUV (CPU) ────────────────────────────────────────────────────────

/// Converte YUV420P (8-bit ou 10-bit) para RGBA8 na CPU usando BT.709.
///
/// Ponte software para o modo CPU (fallback).
///
/// SPEC-AV-002b
fn yuv420p_to_rgba8(frame: &YuvFrame) -> Vec<u8> {
    let w = frame.width as usize;
    let h = frame.height as usize;
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let uv_w = w.div_ceil(2);
    let full_range = matches!(frame.color_range, YuvColorRange::Full);
    let mut rgba = vec![0u8; w * h * 4];

    for row in 0..h {
        let uv_row = row / 2;
        for col in 0..w {
            let uv_col = col / 2;
            let y_idx = row * w + col;
            let uv_idx = uv_row * uv_w + uv_col;

            let y_s = read_yuv_sample(&frame.planes[0], y_idx, frame.ten_bit);
            let u_s = read_yuv_sample(&frame.planes[1], uv_idx, frame.ten_bit);
            let v_s = read_yuv_sample(&frame.planes[2], uv_idx, frame.ten_bit);

            let c = if full_range { y_s } else { y_s - 16 };
            let d = u_s - 128;
            let e = v_s - 128;

            let r = clamp_u8((298 * c + 409 * e + 128) >> 8);
            let g = clamp_u8((298 * c - 100 * d - 208 * e + 128) >> 8);
            let b = clamp_u8((298 * c + 516 * d + 128) >> 8);

            let i = (row * w + col) * 4;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 255;
        }
    }

    rgba
}

#[inline]
fn read_yuv_sample(plane: &[u8], sample_idx: usize, ten_bit: bool) -> i32 {
    if ten_bit {
        let byte_idx = sample_idx * 2;
        let lo = plane[byte_idx] as i32;
        let hi = plane[byte_idx + 1] as i32;
        (hi << 8 | lo) >> 2
    } else {
        plane[sample_idx] as i32
    }
}

#[inline]
fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

// ─── VideoRenderer ────────────────────────────────────────────────────────────

// ─── VideoRenderer ────────────────────────────────────────────────────────────

enum RendererInner {
    Gpu(GpuRenderer),
    HwGpu(NvRenderer),
    Cpu(CpuRenderer),
}

#[derive(Clone)]
struct GpuRendererContext {
    device: Arc<wgpu::Device>,
    target_format: wgpu::TextureFormat,
    d3d11_dev: Option<Arc<D3d11Device>>,
}

/// Renderizador de frames de vídeo: modo GPU (wgpu PaintCallback) ou CPU (fallback).
///
/// ## Modo GPU
/// Upload dos planos YUV para 3 texturas (`R8Unorm` / `R16Unorm`), aplicação
/// do shader WGSL YUV→RGB via `egui::PaintCallback`. Use [`VideoRenderer::paint_callback`]
/// para obter o callback e adicione-o ao painter do egui.
///
/// ## Modo CPU
/// Converte para `egui::ColorImage` e retorna `TextureId` via
/// [`VideoRenderer::texture_id`] para uso com `painter.image()`.
///
/// SPEC-AV-003 · SPEC-AV-003c
pub struct VideoRenderer {
    inner: RendererInner,
    gpu_context: Option<GpuRendererContext>,
    cpu_ctx: Option<egui::Context>,
    /// Bytes enviados à GPU desde `upload_window_start`.
    upload_bytes_window: u64,
    /// Início da janela de medição de bytes por segundo.
    upload_window_start: std::time::Instant,
    /// Valor estabilizado de bytes/s calculado ao fechar a janela de 1 s.
    upload_bytes_per_sec: u64,
    /// Colorspace do último frame recebido.
    last_colorspace: Option<YuvColorspace>,
    /// Color range do último frame recebido.
    last_color_range: Option<YuvColorRange>,
}

impl VideoRenderer {
    /// Cria um `VideoRenderer` em modo GPU HW NV12 zero-copy (Fase C D3D11VA).
    ///
    /// `device`       : dispositivo wgpu do `eframe::CreationContext::wgpu_render_state`.
    /// `d3d11_dev`    : device D3D11 para staging copy NV12.
    /// `target_format`: formato do framebuffer de saída.
    ///
    /// SPEC-AV-RENDER-NV12-001
    pub fn new_hw_gpu(
        device: Arc<wgpu::Device>,
        d3d11_dev: Arc<D3d11Device>,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        match NvRenderer::new(&device, Arc::clone(&d3d11_dev), target_format) {
            Ok(nv) => Self {
                inner: RendererInner::HwGpu(nv),
                gpu_context: Some(GpuRendererContext {
                    device,
                    target_format,
                    d3d11_dev: Some(d3d11_dev),
                }),
                cpu_ctx: None,
                upload_bytes_window: 0,
                upload_window_start: std::time::Instant::now(),
                upload_bytes_per_sec: 0,
                last_colorspace: None,
                last_color_range: None,
            },
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "VideoRenderer::new_hw_gpu falhou — recaindo para modo CPU"
                );
                let ctx = egui::Context::default();
                Self {
                    inner: RendererInner::Cpu(CpuRenderer { ctx, handle: None }),
                    gpu_context: None,
                    cpu_ctx: None,
                    upload_bytes_window: 0,
                    upload_window_start: std::time::Instant::now(),
                    upload_bytes_per_sec: 0,
                    last_colorspace: None,
                    last_color_range: None,
                }
            }
        }
    }

    /// Envia um `HwVideoFrame` (D3D11VA NV12) para renderização zero-copy.
    ///
    /// Extrai os planos NV12 via staging D3D11 e armazena pendente para upload
    /// no `prepare()` do próximo frame egui.  Retorna erro se o renderer não
    /// estiver no modo `HwGpu` ou se a extração D3D11 falhar.
    ///
    /// SPEC-AV-RENDER-NV12-001
    pub fn upload_hw(&mut self, hw: &HwVideoFrame) -> Result<(), AvError> {
        self.ensure_hw_renderer(Arc::clone(&hw.d3d11_dev))?;
        self.last_colorspace = Some(yuv_colorspace_from_hw(hw.tex.color_space));
        self.last_color_range = Some(if hw.tex.full_range {
            YuvColorRange::Full
        } else {
            YuvColorRange::Limited
        });

        // Contabiliza bytes: w × h (Y) + w × h/2 (UV interleaved) = w × h × 3/2.
        let bytes_per_sample: u64 = if matches!(hw.tex.format, HwPixelFormat::P010) {
            2
        } else {
            1
        };
        let frame_bytes = (hw.width as u64 * hw.height as u64
            + hw.width as u64 * hw.height.div_ceil(2) as u64)
            * bytes_per_sample;
        self.upload_bytes_window = self.upload_bytes_window.saturating_add(frame_bytes);

        let elapsed = self.upload_window_start.elapsed();
        if elapsed >= std::time::Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64().max(0.001);
            self.upload_bytes_per_sec = (self.upload_bytes_window as f64 / secs) as u64;
            self.upload_bytes_window = 0;
            self.upload_window_start = std::time::Instant::now();
        }

        match &self.inner {
            RendererInner::HwGpu(nv) => nv.upload_hw(hw),
            _ => Err(AvError::HwInitFailed(
                "upload_hw chamado em renderer não-HwGpu".into(),
            )),
        }
    }

    /// Cria um `VideoRenderer` em modo GPU (wgpu/D3D11).
    ///
    /// `device`       : dispositivo wgpu do `eframe::CreationContext::wgpu_render_state`.
    /// `_queue`       : fila wgpu (reservado; upload ocorre em `prepare()`).
    /// `target_format`: formato do framebuffer de saída (ex.: `Bgra8Unorm`).
    ///
    /// SPEC-AV-003
    pub fn new_gpu(
        device: Arc<wgpu::Device>,
        _queue: Arc<wgpu::Queue>,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        match GpuRenderer::new(Arc::clone(&device), target_format) {
            Ok(gpu) => Self {
                inner: RendererInner::Gpu(gpu),
                gpu_context: Some(GpuRendererContext {
                    device,
                    target_format,
                    d3d11_dev: None,
                }),
                cpu_ctx: None,
                upload_bytes_window: 0,
                upload_window_start: std::time::Instant::now(),
                upload_bytes_per_sec: 0,
                last_colorspace: None,
                last_color_range: None,
            },
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "VideoRenderer::new_gpu falhou — recaindo para modo CPU"
                );
                let ctx = egui::Context::default();
                Self {
                    inner: RendererInner::Cpu(CpuRenderer { ctx, handle: None }),
                    gpu_context: None,
                    cpu_ctx: None,
                    upload_bytes_window: 0,
                    upload_window_start: std::time::Instant::now(),
                    upload_bytes_per_sec: 0,
                    last_colorspace: None,
                    last_color_range: None,
                }
            }
        }
    }

    /// Cria um `VideoRenderer` em modo CPU (fallback via `egui::ColorImage`).
    ///
    /// SPEC-AV-003c
    pub fn new_cpu(ctx: egui::Context) -> Self {
        Self {
            inner: RendererInner::Cpu(CpuRenderer {
                ctx: ctx.clone(),
                handle: None,
            }),
            gpu_context: None,
            cpu_ctx: Some(ctx),
            upload_bytes_window: 0,
            upload_window_start: std::time::Instant::now(),
            upload_bytes_per_sec: 0,
            last_colorspace: None,
            last_color_range: None,
        }
    }

    /// Envia um `YuvFrame` para upload.
    ///
    /// - **GPU**: armazena o frame; upload ocorre em `prepare()` do PaintCallback.
    /// - **CPU**: converte YUV→RGBA8 imediatamente e atualiza a `TextureHandle`.
    ///
    /// SPEC-AV-003
    pub fn upload(&mut self, frame: &YuvFrame) -> Result<(), AvError> {
        self.ensure_sw_renderer()?;
        // Atualiza metadados do último frame.
        self.last_colorspace = Some(frame.colorspace);
        self.last_color_range = Some(frame.color_range);

        // Contabiliza bytes enviados à GPU: Y + U + V planes.
        // Para YUV420P 8-bit: Y = w*h, U = V = (w/2)*(h/2).
        // Para 10-bit: cada amostra ocupa 2 bytes.
        let bps: u64 = if frame.ten_bit { 2 } else { 1 };
        let uv_w = (frame.width as u64).div_ceil(2);
        let uv_h = (frame.height as u64).div_ceil(2);
        let frame_bytes = frame.width as u64 * frame.height as u64 * bps + 2 * uv_w * uv_h * bps;
        self.upload_bytes_window = self.upload_bytes_window.saturating_add(frame_bytes);

        // Fecha a janela de medição a cada segundo e atualiza o valor estabilizado.
        let elapsed = self.upload_window_start.elapsed();
        if elapsed >= std::time::Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64().max(0.001);
            self.upload_bytes_per_sec = (self.upload_bytes_window as f64 / secs) as u64;
            self.upload_bytes_window = 0;
            self.upload_window_start = std::time::Instant::now();
        }

        match &mut self.inner {
            RendererInner::Gpu(gpu) => {
                gpu.upload(frame);
                Ok(())
            }
            RendererInner::HwGpu(_) => Err(AvError::HwInitFailed(
                "use upload_hw para frames HW (VideoFrame::Hw)".into(),
            )),
            RendererInner::Cpu(cpu) => cpu.upload(frame),
        }
    }

    /// Retorna a taxa de bytes enviados à GPU nos últimos ~1 s.
    ///
    /// SPEC-METRICS-PIPELINE-001
    pub fn gpu_upload_bytes_per_sec(&self) -> u64 {
        self.upload_bytes_per_sec
    }

    /// Retorna o label do colorspace do último frame recebido.
    ///
    /// SPEC-METRICS-PIPELINE-001
    pub fn current_colorspace_label(&self) -> Option<&'static str> {
        self.last_colorspace.map(|cs| match cs {
            YuvColorspace::Bt709 => "BT.709",
            YuvColorspace::Bt601 => "BT.601",
            YuvColorspace::Bt2020 => "BT.2020",
            YuvColorspace::Unspecified => "Unspecified",
        })
    }

    /// Retorna o label do color range do último frame recebido.
    ///
    /// SPEC-METRICS-PIPELINE-001
    pub fn current_color_range_label(&self) -> Option<&'static str> {
        self.last_color_range.map(|cr| match cr {
            YuvColorRange::Limited => "Limited",
            YuvColorRange::Full => "Full",
        })
    }

    /// Retorna um `egui::PaintCallback` para o rect dado (somente modo GPU).
    ///
    /// O caller deve adicionar o callback ao painter via `painter.add(cb)`.
    /// Retorna `None` em modo CPU.
    ///
    /// SPEC-AV-003
    pub fn paint_callback(&self, rect: egui::Rect) -> Option<egui::PaintCallback> {
        match &self.inner {
            RendererInner::Gpu(gpu) => Some(gpu.paint_callback(rect)),
            RendererInner::HwGpu(nv) => Some(nv.paint_callback(rect)),
            RendererInner::Cpu(_) => None,
        }
    }

    /// Retorna o `egui::TextureId` atual (somente modo CPU).
    ///
    /// Usado como fallback com `painter.image()` quando `paint_callback()` é `None`.
    /// Em modo GPU retorna sempre `None`.
    ///
    /// SPEC-AV-003c
    pub fn texture_id(&self) -> Option<TextureId> {
        match &self.inner {
            RendererInner::Gpu(_) | RendererInner::HwGpu(_) => None,
            RendererInner::Cpu(cpu) => cpu.texture_id(),
        }
    }

    /// Retorna `true` se o renderer está em modo GPU.
    ///
    /// SPEC-AV-003c
    pub fn is_gpu_mode(&self) -> bool {
        matches!(&self.inner, RendererInner::Gpu(_) | RendererInner::HwGpu(_))
    }

    /// Retorna `true` se um frame já foi recebido.
    ///
    /// SPEC-AV-003
    pub fn has_frame(&self) -> bool {
        match &self.inner {
            RendererInner::Gpu(gpu) => gpu.has_frame(),
            RendererInner::HwGpu(nv) => nv.has_frame(),
            RendererInner::Cpu(cpu) => cpu.texture_id().is_some(),
        }
    }

    /// Rebaixa o renderer para o caminho SW renderizável após perda do device HW.
    pub fn fallback_to_software(&mut self) -> Result<(), AvError> {
        self.ensure_sw_renderer()
    }

    fn ensure_sw_renderer(&mut self) -> Result<(), AvError> {
        if !matches!(self.inner, RendererInner::HwGpu(_)) {
            return Ok(());
        }

        if let Some(context) = self.gpu_context.clone() {
            self.inner =
                RendererInner::Gpu(GpuRenderer::new(context.device, context.target_format)?);
            return Ok(());
        }

        if let Some(ctx) = self.cpu_ctx.clone() {
            self.inner = RendererInner::Cpu(CpuRenderer { ctx, handle: None });
            return Ok(());
        }

        Err(AvError::HwInitFailed(
            "renderer não possui contexto para fallback SW".into(),
        ))
    }

    fn ensure_hw_renderer(&mut self, d3d11_dev: Arc<D3d11Device>) -> Result<(), AvError> {
        if matches!(self.inner, RendererInner::HwGpu(_)) {
            return Ok(());
        }

        let Some(mut context) = self.gpu_context.clone() else {
            return Err(AvError::HwInitFailed(
                "renderer não possui contexto wgpu para ativar upload HW".into(),
            ));
        };
        context.d3d11_dev = Some(Arc::clone(&d3d11_dev));
        let nv = NvRenderer::new(&context.device, d3d11_dev, context.target_format)?;
        self.gpu_context = Some(context);
        self.inner = RendererInner::HwGpu(nv);
        Ok(())
    }
}

// ─── Testes ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video_queue::{YuvColorRange, YuvColorspace};

    fn make_yuv_frame(width: u32, height: u32, pts: Option<u64>) -> YuvFrame {
        let w = width as usize;
        let h = height as usize;
        let y = vec![16u8; w * h];
        let uv = vec![128u8; (w / 2).max(1) * (h / 2).max(1)];
        YuvFrame {
            planes: [y, uv.clone(), uv],
            width,
            height,
            pts,
            sar_num: 1,
            sar_den: 1,
            colorspace: YuvColorspace::Bt709,
            color_range: YuvColorRange::Limited,
            ten_bit: false,
        }
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
        let frame = make_yuv_frame(4, 4, None);
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

        let frame1 = make_yuv_frame(8, 8, Some(1000));
        renderer.upload(&frame1).expect("primeiro upload");
        let id1 = renderer.texture_id();

        let frame2 = make_yuv_frame(8, 8, Some(2000));
        renderer.upload(&frame2).expect("segundo upload");
        let id2 = renderer.texture_id();

        assert_eq!(
            id1, id2,
            "TextureId deve ser estável entre uploads de mesma dimensão"
        );
    }

    /// Upload de frame 0×0 não deve falhar no modo CPU.
    #[test]
    fn spec_av_003_cpu_zero_dim_upload() {
        let ctx = egui::Context::default();
        let mut renderer = VideoRenderer::new_cpu(ctx);
        let frame = make_yuv_frame(0, 0, None);
        renderer
            .upload(&frame)
            .expect("upload de frame 0×0 não deve falhar");
    }

    /// Mudança de dimensão no modo CPU mantém o mesmo `TextureId` (handle reutilizado).
    #[test]
    fn spec_av_003_cpu_dimension_change_reuses_handle() {
        let ctx = egui::Context::default();
        let mut renderer = VideoRenderer::new_cpu(ctx);

        let frame_small = make_yuv_frame(4, 4, None);
        renderer.upload(&frame_small).expect("upload 4×4");
        let id_before = renderer.texture_id();

        let frame_large = make_yuv_frame(8, 8, None);
        renderer.upload(&frame_large).expect("upload 8×8");
        let id_after = renderer.texture_id();

        assert_eq!(
            id_before, id_after,
            "CpuRenderer reutiliza o mesmo TextureHandle (mesmo ID) ao mudar dimensão"
        );
    }

    /// Em modo CPU, `paint_callback()` deve retornar `None`.
    #[test]
    fn spec_av_003_paint_callback_none_in_cpu_mode() {
        let ctx = egui::Context::default();
        let renderer = VideoRenderer::new_cpu(ctx);
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 100.0));
        assert!(
            renderer.paint_callback(rect).is_none(),
            "paint_callback() deve ser None em modo CPU"
        );
    }

    // ── yuv420p_to_rgba8 ────────────────────────────────────────────────────

    /// Frame YUV420P preto (Y=16, U=V=128) deve produzir pixels RGBA8 próximos de preto.
    #[test]
    fn spec_av_003_yuv420p_black_frame_is_near_black_rgba() {
        let frame = make_yuv_frame(4, 4, None);
        let rgba = yuv420p_to_rgba8(&frame);
        assert_eq!(rgba.len(), 4 * 4 * 4);
        for chunk in rgba.chunks_exact(4) {
            assert!(chunk[0] <= 4, "R deve ser próximo de 0");
            assert!(chunk[1] <= 4, "G deve ser próximo de 0");
            assert!(chunk[2] <= 4, "B deve ser próximo de 0");
            assert_eq!(chunk[3], 255, "Alpha deve ser 255");
        }
    }

    /// Frame 0×0 deve retornar buffer vazio sem panic.
    #[test]
    fn spec_av_003_yuv420p_zero_dim_returns_empty() {
        let frame = make_yuv_frame(0, 0, None);
        let rgba = yuv420p_to_rgba8(&frame);
        assert!(rgba.is_empty());
    }

    // ── YuvParamsGpu ────────────────────────────────────────────────────────

    /// Serialização de `YuvParamsGpu` deve produzir exatamente 64 bytes.
    #[test]
    fn spec_av_003_yuv_params_size() {
        let p = YuvParamsGpu::for_sw_frame(YuvColorspace::Bt709, YuvColorRange::Limited, false);
        assert_eq!(p.to_bytes().len(), 64);
    }

    /// `range_scale` para limited range (BT.709) deve ser ≈ 255/219 ≈ 1.1644.
    #[test]
    fn spec_av_003_yuv_params_range_scale_limited() {
        let p = YuvParamsGpu::for_sw_frame(YuvColorspace::Bt709, YuvColorRange::Limited, false);
        let range_scale = p.offset_and_range[3];
        let expected = 255.0_f32 / 219.0;
        assert!(
            (range_scale - expected).abs() < 1e-4,
            "range_scale BT.709 limited deve ser ≈ {expected}, got {range_scale}"
        );
    }

    #[test]
    fn spec_av_003_yuv_params_luma_offset_limited_8bit() {
        let p = YuvParamsGpu::for_sw_frame(YuvColorspace::Bt709, YuvColorRange::Limited, false);
        let expected = 16.0_f32 / 255.0_f32;
        assert!((p.offset_and_range[0] - expected).abs() < 1e-6);
        assert!((p.offset_and_range[1] - 0.5_f32).abs() < 1e-6);
    }

    #[test]
    fn spec_av_003_yuv_params_luma_offset_limited_10bit() {
        let p = YuvParamsGpu::for_sw_frame(YuvColorspace::Bt2020, YuvColorRange::Limited, true);
        let expected = 64.0_f32 / 1023.0_f32;
        assert!((p.offset_and_range[0] - expected).abs() < 1e-6);
    }

    /// `range_scale` para full range deve ser 1.0.
    #[test]
    fn spec_av_003_yuv_params_range_scale_full() {
        let p = YuvParamsGpu::for_sw_frame(YuvColorspace::Bt709, YuvColorRange::Full, false);
        let range_scale = p.offset_and_range[3];
        assert!(
            (range_scale - 1.0_f32).abs() < 1e-6,
            "range_scale full deve ser 1.0, got {range_scale}"
        );
    }

    #[test]
    fn spec_av_003_yuv_params_hdr_transfer_sets_shader_metadata() {
        let p = YuvParamsGpu::for_frame(
            YuvColorspace::Bt2020,
            YuvColorRange::Limited,
            TransferFunction::Pq,
            true,
        );
        assert_eq!(p.col0[3], 1.0, "PQ deve mapear para transfer_mode=1");
        assert_eq!(p.col1[3], 1.0, "PQ deve habilitar clipping SDR documentado");
    }

    // ── scale_10bit_plane ───────────────────────────────────────────────────

    /// Máximo 10-bit (raw = 1023, valor direto nos bits [9:0]) deve escalar
    /// para 65472 (= 1023 × 64).
    #[test]
    fn spec_av_003_scale_10bit_max() {
        let max_10bit: u16 = 1023;
        let input = max_10bit.to_le_bytes();
        let out = scale_10bit_plane(&input);
        let result = u16::from_le_bytes([out[0], out[1]]);
        assert_eq!(result, 65472, "10-bit max deve escalar para 65472");
    }

    /// Zero deve permanecer zero.
    #[test]
    fn spec_av_003_scale_10bit_zero() {
        let input = 0u16.to_le_bytes();
        let out = scale_10bit_plane(&input);
        let result = u16::from_le_bytes([out[0], out[1]]);
        assert_eq!(result, 0);
    }
}
