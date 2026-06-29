//! Pipeline de renderização GPU headless: upload YUV/NV12 planar → shader WGSL
//! YUV→RGB → textura RGBA offscreen, importável como `slint::Image` (zero-copy
//! no lado da exibição).
//!
//! Arquitetura:
//! - `YuvPipeline`: 3 texturas R8/R16Unorm (Y, U, V) + `yuv_to_rgb.wgsl`.
//! - `NvPipeline`:  R8/R16 (Y) + Rg8/Rg16 (UV interleaved) + `nv12_to_rgb.wgsl`.
//! - `VideoRenderer`: orquestra o upload + render pass próprio (sem egui) e
//!   devolve uma `wgpu::Texture` `Rgba8Unorm` por frame, de um pool em anel.
//!
//! O fragment shader já decodifica TRC (BT.1886/PQ/HLG/sRGB), faz tone mapping
//! HDR→SDR e gamut BT.2020→BT.709. Com o alvo `Rgba8Unorm` (não-sRGB) ele emite
//! pixels já codificados para exibição, prontos para a Slint amostrar.
//!
//! SPEC-AV-003 · SPEC-AV-RENDER-NV12-001

use std::num::NonZeroU64;
use std::sync::Arc;

use crate::error::AvError;
use crate::hw::TransferFunction;
use crate::video_queue::{HwSurface, VideoFrame, YuvColorRange, YuvColorspace, YuvFrame};

// ─── Constantes ──────────────────────────────────────────────────────────────

const YUV_SHADER_SRC: &str = include_str!("yuv_to_rgb.wgsl");
const NV12_SHADER_SRC: &str = include_str!("nv12_to_rgb.wgsl");

/// Formato da textura de saída entregue à Slint. Não-sRGB: o shader já codifica
/// para exibição (ver `DECODE_SRGB` em `yuv_to_rgb.wgsl`).
const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Texturas de saída em anel. A Slint pode amostrar o frame N enquanto
/// produzimos N+1/N+2; 3 slots evitam sobrescrever um frame ainda em uso.
const OUTPUT_POOL_LEN: usize = 3;

/// Qual pipeline/fonte usar no render pass de um frame.
enum RenderKind {
    /// YUV420P software (3 planos).
    Yuv,
    /// NV12 com planos baixados para a CPU (Fase 1).
    NvCpu,
    /// NV12 de textura compartilhada na GPU (Fase 2, zero-copy).
    NvShared,
}

// ─── Uniform GPU struct ──────────────────────────────────────────────────────

/// Layout do uniform buffer `YuvParams` no shader WGSL (std140, 64 bytes).
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
    /// x=luma_offset, y=centro UV, z=gamut_map (BT.2020→709), w=range_scale.
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
        let gamut_map = matches!(colorspace, YuvColorspace::Bt2020) as u8 as f32;

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
                    offset_and_range: [luma_min, 0.5, gamut_map, range_scale],
                }
            }

            // ─ Full-range: Y, U, V ∈ [0,255]; sem escala adicional.
            YuvColorRange::Full => Self {
                col0: [cy[0], cy[1], cy[2], transfer_mode],
                col1: [cu[0], cu[1], cu[2], hdr_clip],
                col2: [cv[0], cv[1], cv[2], 0.0],
                offset_and_range: [0.0, 0.5, gamut_map, 1.0],
            },
        }
    }
}

// ─── Helpers: plano 10-bit → R16Unorm ────────────────────────────────────────

/// Prepara os dados de um plano para upload: slice original (8-bit) ou vetor
/// escalado (10-bit → R16Unorm).
fn prepare_plane_data(plane: &[u8], ten_bit: bool) -> std::borrow::Cow<'_, [u8]> {
    if ten_bit {
        std::borrow::Cow::Owned(scale_10bit_plane(plane))
    } else {
        std::borrow::Cow::Borrowed(plane)
    }
}

/// Escala cada amostra 10-bit (u16 LE em [0..1023]) para ocupar todo o range
/// R16Unorm (`value << 6`, ×64). O shader normaliza por 65535.0.
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

// ─── Pipeline YUV420P (software) ─────────────────────────────────────────────

/// Pipeline YUV planar: 3 texturas (Y, U, V) + shader WGSL.
struct YuvPipeline {
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
}

impl YuvPipeline {
    fn create(device: &wgpu::Device) -> Result<Self, AvError> {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv_to_rgb"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(YUV_SHADER_SRC)),
        });

        let texture_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv_bgl"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                texture_entry(2),
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

        let pipeline = build_pipeline(device, &shader, &bgl, "yuv");

        let sampler = linear_sampler(device, "yuv_sampler");
        let uniform_buf = params_ubo(device, "yuv_params_ubo");

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
        })
    }

    /// Faz upload das texturas YUV e atualiza o UBO a partir do frame.
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, frame: &YuvFrame) {
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
                wgpu::TexelCopyBufferLayout {
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
        for (tex, plane) in [(&self.tex_u, 1usize), (&self.tex_v, 2usize)] {
            if let Some(tex) = tex {
                let data = prepare_plane_data(&frame.planes[plane], frame.ten_bit);
                queue.write_texture(
                    tex.as_image_copy(),
                    &data,
                    wgpu::TexelCopyBufferLayout {
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
        }

        let params = YuvParamsGpu::for_frame(
            frame.colorspace,
            frame.color_range,
            frame.transfer,
            frame.ten_bit,
        );
        queue.write_buffer(&self.uniform_buf, 0, &params.to_bytes());
    }

    fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        if let Some(bg) = &self.bind_group {
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, bg, &[]);
            pass.draw(0..3, 0..1);
        }
    }
}

// ─── Pipeline NV12 (hardware) ────────────────────────────────────────────────

/// Pipeline NV12: textura Y (R8/R16) + textura UV interleaved (Rg8/Rg16).
struct NvPipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buf: wgpu::Buffer,
    tex_y: Option<wgpu::Texture>,
    tex_uv: Option<wgpu::Texture>,
    bind_group: Option<wgpu::BindGroup>,
    dims: Option<(u32, u32)>,
    ten_bit: bool,
}

impl NvPipeline {
    fn create(device: &wgpu::Device) -> Result<Self, AvError> {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nv12_to_rgb"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(NV12_SHADER_SRC)),
        });

        let texture_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12_bgl"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
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

        let pipeline = build_pipeline(device, &shader, &bgl, "nv12");

        let sampler = linear_sampler(device, "nv12_sampler");
        let uniform_buf = params_ubo(device, "nv12_params_ubo");

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
        })
    }

    /// Faz upload dos planos NV12 da CPU (Y + UV interleaved) e atualiza o UBO.
    #[allow(clippy::too_many_arguments)]
    fn upload_cpu(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        planes: &crate::hw::NvPlanes,
        colorspace: YuvColorspace,
        color_range: YuvColorRange,
        transfer: TransferFunction,
    ) {
        let w = planes.width;
        let h = planes.height;
        if w == 0 || h == 0 {
            return;
        }
        let ten_bit = planes.ten_bit;
        let uv_w = w.div_ceil(2);
        let uv_h = h.div_ceil(2);
        let bps = if ten_bit { 2 } else { 1 };

        let need_new = self.dims != Some((w, h)) || self.ten_bit != ten_bit;
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
                format: if ten_bit {
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
                format: if ten_bit {
                    wgpu::TextureFormat::Rg16Unorm
                } else {
                    wgpu::TextureFormat::Rg8Unorm
                },
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
            self.dims = Some((w, h));
            self.ten_bit = ten_bit;
        }

        let (Some(tex_y), Some(tex_uv)) = (&self.tex_y, &self.tex_uv) else {
            return;
        };

        // Plano Y (R8/R16 — 1 amostra/pixel).
        queue.write_texture(
            tex_y.as_image_copy(),
            &planes.y_data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * bps),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );

        // Plano UV (Rg8/Rg16 — 2 amostras/pixel; cada linha tem `w` amostras).
        queue.write_texture(
            tex_uv.as_image_copy(),
            &planes.uv_data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * bps),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: uv_w,
                height: uv_h,
                depth_or_array_layers: 1,
            },
        );

        let params = YuvParamsGpu::for_frame(colorspace, color_range, transfer, ten_bit);
        queue.write_buffer(&self.uniform_buf, 0, &params.to_bytes());

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

    fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        if let Some(bg) = &self.bind_group {
            self.render_with(pass, bg);
        }
    }

    /// Renderiza usando um bind group externo (caminho compartilhado Fase 2).
    fn render_with(&self, pass: &mut wgpu::RenderPass<'_>, bind_group: &wgpu::BindGroup) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    /// Monta um bind group a partir de views de plano externos (textura NV12
    /// compartilhada importada da GPU) reusando sampler + UBO deste pipeline.
    #[cfg(windows)]
    fn make_bind_group(
        &self,
        device: &wgpu::Device,
        y_view: &wgpu::TextureView,
        uv_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12_shared_bind_group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(uv_view),
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
        })
    }
}

// ─── Importador de texturas NV12 compartilhadas (Fase 2, Windows) ─────────────

/// Abre texturas NV12 compartilhadas (D3D11) na GPU wgpu/DX12 e sincroniza via
/// fence — sem download CPU. Cacheia as texturas importadas por handle.
#[cfg(windows)]
struct SharedNvImporter {
    /// `ID3D12Device` bruto do wgpu (clonado uma vez).
    d3d12: Option<windows::Win32::Graphics::Direct3D12::ID3D12Device>,
    /// Fence compartilhada aberta como `ID3D12Fence` (uma vez).
    fence: Option<windows::Win32::Graphics::Direct3D12::ID3D12Fence>,
    /// Texturas importadas, indexadas pelo handle NT da textura.
    cache: std::collections::HashMap<isize, ImportedNv>,
    last_dims: Option<(u32, u32)>,
}

#[cfg(windows)]
struct ImportedNv {
    /// Mantém a `wgpu::Texture` viva (as views dependem dela).
    #[allow(dead_code)]
    texture: wgpu::Texture,
    y_view: wgpu::TextureView,
    uv_view: wgpu::TextureView,
}

#[cfg(windows)]
impl SharedNvImporter {
    fn new() -> Self {
        Self {
            d3d12: None,
            fence: None,
            cache: std::collections::HashMap::new(),
            last_dims: None,
        }
    }

    /// Prepara as views de plano (Y/UV) de um frame compartilhado, esperando a
    /// fence. Devolve `None` se a fence ainda não sinalizou (pula o frame) ou se
    /// o import falhar (nesse caso desabilita o zero-copy globalmente).
    fn prepare(
        &mut self,
        device: &wgpu::Device,
        shared: &crate::hw::SharedNvFrame,
    ) -> Option<(wgpu::TextureView, wgpu::TextureView)> {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Graphics::Direct3D12::ID3D12Fence;

        let dims = (shared.width, shared.height);
        if self.last_dims != Some(dims) {
            self.cache.clear();
            self.last_dims = Some(dims);
        }

        // Device D3D12 bruto do wgpu (clone COM, uma vez).
        if self.d3d12.is_none() {
            self.d3d12 = unsafe {
                device
                    .as_hal::<wgpu::hal::api::Dx12>()
                    .map(|hal| hal.raw_device().clone())
            };
        }
        let d3d12 = self.d3d12.clone()?;

        // Abre a fence compartilhada (uma vez).
        if self.fence.is_none() {
            let mut f: Option<ID3D12Fence> = None;
            let h = HANDLE(shared.fence_handle as *mut _);
            if unsafe { d3d12.OpenSharedHandle(h, &mut f) }.is_err() {
                tracing::warn!("zero-copy: OpenSharedHandle(fence) falhou; desabilitando");
                crate::hw::set_gpu_zero_copy_enabled(false);
                return None;
            }
            self.fence = f;
        }
        let fence = self.fence.as_ref()?;

        // A cópia do produtor já terminou? (Quase sempre sim — frame ficou na
        // fila por dezenas de ms.) Se não, pula este tick.
        if unsafe { fence.GetCompletedValue() } < shared.fence_value {
            return None;
        }

        // Importa a textura (uma vez por handle); cacheada por handle.
        use std::collections::hash_map::Entry;
        let imp = match self.cache.entry(shared.texture_handle) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => match Self::import_texture(device, &d3d12, shared) {
                Some(tex) => e.insert(tex),
                None => {
                    tracing::warn!("zero-copy: import da textura NV12 falhou; desabilitando");
                    crate::hw::set_gpu_zero_copy_enabled(false);
                    return None;
                }
            },
        };
        Some((imp.y_view.clone(), imp.uv_view.clone()))
    }

    /// Abre o handle NT da textura na GPU DX12 e cria a `wgpu::Texture` NV12 +
    /// views de plano (Y=R8 Plane0, UV=Rg8 Plane1).
    fn import_texture(
        device: &wgpu::Device,
        d3d12: &windows::Win32::Graphics::Direct3D12::ID3D12Device,
        shared: &crate::hw::SharedNvFrame,
    ) -> Option<ImportedNv> {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

        let mut res: Option<ID3D12Resource> = None;
        let h = HANDLE(shared.texture_handle as *mut _);
        unsafe { d3d12.OpenSharedHandle(h, &mut res) }.ok()?;
        let resource = res?;

        let size = wgpu::Extent3d {
            width: shared.width,
            height: shared.height,
            depth_or_array_layers: 1,
        };
        let hal_tex = unsafe {
            wgpu::hal::dx12::Device::texture_from_raw(
                resource,
                wgpu::TextureFormat::NV12,
                wgpu::TextureDimension::D2,
                size,
                1,
                1,
            )
        };
        let texture = unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
                hal_tex,
                &wgpu::TextureDescriptor {
                    label: Some("shared_nv12"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::NV12,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
            )
        };
        let y_view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("shared_nv12_y"),
            format: Some(wgpu::TextureFormat::R8Unorm),
            aspect: wgpu::TextureAspect::Plane0,
            ..Default::default()
        });
        let uv_view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("shared_nv12_uv"),
            format: Some(wgpu::TextureFormat::Rg8Unorm),
            aspect: wgpu::TextureAspect::Plane1,
            ..Default::default()
        });
        Some(ImportedNv {
            texture,
            y_view,
            uv_view,
        })
    }
}

// ─── Helpers de criação wgpu ─────────────────────────────────────────────────

fn build_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    bgl: &wgpu::BindGroupLayout,
    label: &str,
) -> wgpu::RenderPipeline {
    // O alvo é `Rgba8Unorm` (não-sRGB), então `DECODE_SRGB` permanece no default
    // (0.0) do shader — sem constantes de override a passar.
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(bgl)],
        immediate_size: 0,
    });

    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: OUTPUT_FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

fn linear_sampler(device: &wgpu::Device, label: &str) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some(label),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    })
}

fn params_ubo(device: &wgpu::Device, label: &str) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: 64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

// ─── VideoRenderer ───────────────────────────────────────────────────────────

/// Renderizador de frames de vídeo na GPU (wgpu, headless).
///
/// Recebe `device`/`queue` compartilhados com a Slint, converte cada
/// `VideoFrame` (YUV/NV12) para uma `wgpu::Texture` `Rgba8Unorm` via shader e a
/// devolve para import zero-copy como `slint::Image`.
///
/// SPEC-AV-003 · SPEC-AV-RENDER-NV12-001
pub struct VideoRenderer {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    yuv: YuvPipeline,
    nv: NvPipeline,
    /// Pool em anel de texturas de saída `Rgba8Unorm`.
    out_pool: Vec<wgpu::Texture>,
    out_dims: Option<(u32, u32)>,
    pool_idx: usize,
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
    /// Importador de texturas NV12 compartilhadas (Fase 2, zero-copy HW).
    #[cfg(windows)]
    shared_importer: SharedNvImporter,
}

impl VideoRenderer {
    /// Cria o renderer com o `device`/`queue` wgpu compartilhados com a Slint.
    ///
    /// SPEC-AV-003
    pub fn new(device: Arc<wgpu::Device>, queue: Arc<wgpu::Queue>) -> Result<Self, AvError> {
        let yuv = YuvPipeline::create(&device)?;
        let nv = NvPipeline::create(&device)?;
        Ok(Self {
            device,
            queue,
            yuv,
            nv,
            out_pool: Vec::new(),
            out_dims: None,
            pool_idx: 0,
            upload_bytes_window: 0,
            upload_window_start: std::time::Instant::now(),
            upload_bytes_per_sec: 0,
            last_colorspace: None,
            last_color_range: None,
            #[cfg(windows)]
            shared_importer: SharedNvImporter::new(),
        })
    }

    /// `true` se o device wgpu suporta o caminho zero-copy de hardware
    /// (textura NV12). A UI usa isto para habilitar o flag global.
    ///
    /// SPEC-AV-HW-ZEROCOPY-001
    pub fn supports_hw_zero_copy(&self) -> bool {
        cfg!(windows)
            && self
                .device
                .features()
                .contains(wgpu::Features::TEXTURE_FORMAT_NV12)
    }

    /// Converte um `VideoFrame` para uma textura `Rgba8Unorm` na GPU.
    ///
    /// Devolve `None` para frames inválidos (dimensão 0). A textura vem de um
    /// pool em anel — não a mantenha além de ~3 frames.
    ///
    /// SPEC-AV-003 · SPEC-AV-RENDER-NV12-001
    pub fn render_to_texture(&mut self, frame: &VideoFrame) -> Option<wgpu::Texture> {
        let w = frame.width();
        let h = frame.height();
        if w == 0 || h == 0 {
            return None;
        }

        self.account_bytes(frame);

        // Prepara a fonte (upload CPU ou import GPU) e decide o pipeline.
        // `shared_bg` mantém o bind group do caminho compartilhado vivo durante
        // o render pass.
        let mut shared_bg: Option<wgpu::BindGroup> = None;
        let kind = match frame {
            VideoFrame::Sw(yuv) => {
                self.last_colorspace = Some(yuv.colorspace);
                self.last_color_range = Some(yuv.color_range);
                self.yuv.upload(&self.device, &self.queue, yuv);
                RenderKind::Yuv
            }
            VideoFrame::Hw(hw) => {
                self.last_colorspace = Some(hw.colorspace);
                self.last_color_range = Some(hw.color_range);
                match &hw.surface {
                    HwSurface::Cpu(planes) => {
                        self.nv.upload_cpu(
                            &self.device,
                            &self.queue,
                            planes,
                            hw.colorspace,
                            hw.color_range,
                            hw.transfer,
                        );
                        RenderKind::NvCpu
                    }
                    #[cfg(windows)]
                    HwSurface::Shared(shared) => {
                        // Importa as views da textura NV12 compartilhada; pula o
                        // frame se a fence não sinalizou (raro) ou import falhar.
                        let (y_view, uv_view) =
                            self.shared_importer.prepare(&self.device, shared)?;
                        let params = YuvParamsGpu::for_frame(
                            hw.colorspace,
                            hw.color_range,
                            hw.transfer,
                            false,
                        );
                        self.queue
                            .write_buffer(&self.nv.uniform_buf, 0, &params.to_bytes());
                        shared_bg =
                            Some(self.nv.make_bind_group(&self.device, &y_view, &uv_view));
                        RenderKind::NvShared
                    }
                    #[cfg(not(windows))]
                    HwSurface::Shared(_shared) => {
                        unreachable!("HwSurface::Shared não existe em não-Windows")
                    }
                }
            }
        };

        self.ensure_output_pool(w, h);
        let tex = self.out_pool[self.pool_idx].clone();
        self.pool_idx = (self.pool_idx + 1) % self.out_pool.len();
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("yuv_to_rgba"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("yuv_to_rgba_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            match kind {
                RenderKind::Yuv => self.yuv.render(&mut pass),
                RenderKind::NvCpu => self.nv.render(&mut pass),
                RenderKind::NvShared => {
                    if let Some(bg) = &shared_bg {
                        self.nv.render_with(&mut pass, bg);
                    }
                }
            }
        }
        self.queue.submit([encoder.finish()]);

        Some(tex)
    }

    /// (Re)cria o pool de texturas de saída quando a resolução muda.
    fn ensure_output_pool(&mut self, w: u32, h: u32) {
        if self.out_dims == Some((w, h)) {
            return;
        }
        self.out_pool.clear();
        for i in 0..OUTPUT_POOL_LEN {
            self.out_pool
                .push(self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("video_rgba_out"),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: OUTPUT_FORMAT,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                }));
            let _ = i;
        }
        self.out_dims = Some((w, h));
        self.pool_idx = 0;
    }

    /// Contabiliza bytes enviados à GPU (Y + croma) na janela de 1 s.
    ///
    /// Frames `Shared` (zero-copy) não fazem upload CPU→GPU: contabilizam 0.
    fn account_bytes(&mut self, frame: &VideoFrame) {
        let (w, h, ten) = match frame {
            VideoFrame::Sw(f) => (f.width as u64, f.height as u64, f.ten_bit),
            VideoFrame::Hw(f) => match &f.surface {
                HwSurface::Cpu(p) => (p.width as u64, p.height as u64, p.ten_bit),
                // Zero-copy: nenhum byte sobe via write_texture.
                HwSurface::Shared(_) => {
                    return;
                }
            },
        };
        let bps: u64 = if ten { 2 } else { 1 };
        let uv_w = w.div_ceil(2);
        let uv_h = h.div_ceil(2);
        // YUV420P: Y + U + V; NV12: Y + UV interleaved (mesma soma total).
        let frame_bytes = (w * h + 2 * uv_w * uv_h) * bps;
        self.upload_bytes_window = self.upload_bytes_window.saturating_add(frame_bytes);

        let elapsed = self.upload_window_start.elapsed();
        if elapsed >= std::time::Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64().max(0.001);
            self.upload_bytes_per_sec = (self.upload_bytes_window as f64 / secs) as u64;
            self.upload_bytes_window = 0;
            self.upload_window_start = std::time::Instant::now();
        }
    }

    /// Taxa de bytes enviados à GPU nos últimos ~1 s.
    ///
    /// SPEC-METRICS-PIPELINE-001
    pub fn gpu_upload_bytes_per_sec(&self) -> u64 {
        self.upload_bytes_per_sec
    }

    /// Label do colorspace do último frame recebido.
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

    /// Label do color range do último frame recebido.
    ///
    /// SPEC-METRICS-PIPELINE-001
    pub fn current_color_range_label(&self) -> Option<&'static str> {
        self.last_color_range.map(|cr| match cr {
            YuvColorRange::Limited => "Limited",
            YuvColorRange::Full => "Full",
        })
    }
}

// ─── Testes ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video_queue::{YuvColorRange, YuvColorspace};

    // ── YuvParamsGpu ────────────────────────────────────────────────────────

    /// Serialização de `YuvParamsGpu` deve produzir exatamente 64 bytes.
    #[test]
    fn spec_av_003_yuv_params_size() {
        let p = YuvParamsGpu::for_frame(
            YuvColorspace::Bt709,
            YuvColorRange::Limited,
            TransferFunction::Bt1886,
            false,
        );
        assert_eq!(p.to_bytes().len(), 64);
    }

    /// `range_scale` para limited range (BT.709) deve ser ≈ 255/219 ≈ 1.1644.
    #[test]
    fn spec_av_003_yuv_params_range_scale_limited() {
        let p = YuvParamsGpu::for_frame(
            YuvColorspace::Bt709,
            YuvColorRange::Limited,
            TransferFunction::Bt1886,
            false,
        );
        let range_scale = p.offset_and_range[3];
        let expected = 255.0_f32 / 219.0;
        assert!(
            (range_scale - expected).abs() < 1e-4,
            "range_scale BT.709 limited deve ser ≈ {expected}, got {range_scale}"
        );
    }

    #[test]
    fn spec_av_003_yuv_params_luma_offset_limited_8bit() {
        let p = YuvParamsGpu::for_frame(
            YuvColorspace::Bt709,
            YuvColorRange::Limited,
            TransferFunction::Bt1886,
            false,
        );
        let expected = 16.0_f32 / 255.0_f32;
        assert!((p.offset_and_range[0] - expected).abs() < 1e-6);
        assert!((p.offset_and_range[1] - 0.5_f32).abs() < 1e-6);
    }

    #[test]
    fn spec_av_003_yuv_params_luma_offset_limited_10bit() {
        let p = YuvParamsGpu::for_frame(
            YuvColorspace::Bt2020,
            YuvColorRange::Limited,
            TransferFunction::Bt1886,
            true,
        );
        let expected = 64.0_f32 / 1023.0_f32;
        assert!((p.offset_and_range[0] - expected).abs() < 1e-6);
    }

    /// `range_scale` para full range deve ser 1.0.
    #[test]
    fn spec_av_003_yuv_params_range_scale_full() {
        let p = YuvParamsGpu::for_frame(
            YuvColorspace::Bt709,
            YuvColorRange::Full,
            TransferFunction::Bt1886,
            false,
        );
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

    /// Máximo 10-bit (raw = 1023) deve escalar para 65472 (= 1023 × 64).
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
