//! D3D11 Video Processor — deinterlace GPU em streams 1080i (perfil Performance).
//!
//! SPEC-AV-006

use std::sync::Arc;

use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_VIDEO_FRAME_FORMAT_INTERLACED_BOTTOM_FIELD_FIRST,
    D3D11_VIDEO_FRAME_FORMAT_INTERLACED_TOP_FIELD_FIRST, D3D11_VIDEO_FRAME_FORMAT,
    D3D11_VIDEO_PROCESSOR_CAPS, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_HALF,
    D3D11_VIDEO_PROCESSOR_RATE_CONVERSION_CAPS, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL;

use crate::error::AvError;
use crate::hw::{D3d11Texture, SharedNvFrame, SharedNvPool};

const DEINT_MODE_ADAPTIVE: u32 = 0x4;
const DEINT_MODE_BOB: u32 = 0x2;

/// Frame de saída do Video Processor com PTS reescalado.
///
/// SPEC-AV-006
pub struct VpOutput {
    pub shared: SharedNvFrame,
    pub pts: Option<u64>,
}

/// Processador D3D11 VP por PID.
///
/// NOTA (driver Intel Arc): `VideoProcessorBlt` retorna `E_INVALIDARG` quando
/// `ppPastSurfaces`/`ppFutureSurfaces` apontam para subresources diferentes do
/// frame atual — mesmo com contagens iguais aos caps reportados (validado em
/// `examples/vp_probe.rs`, casos G/N/Q/R–X). Por isso o Blt é submetido **sem**
/// frames de referência (mesmo approach do mpv `vf_d3d11vpp`); o driver aplica
/// o deinterlace adaptativo com histórico interno próprio.
///
/// SPEC-AV-006
pub struct D3d11VideoProcessor {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    d3d_context: ID3D11DeviceContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    width: u32,
    height: u32,
    input_format: D3D11_VIDEO_FRAME_FORMAT,
    time_base_num: i32,
    time_base_den: i32,
    last_field_pts: Option<i64>,
    output_seq: u32,
}

impl D3d11VideoProcessor {
    /// Cria o Video Processor para as dimensões dadas.
    ///
    /// SPEC-AV-006
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        top_field_first: bool,
        time_base_num: i32,
        time_base_den: i32,
    ) -> Result<Self, AvError> {
        let video_device: ID3D11VideoDevice = device
            .cast()
            .map_err(|e| AvError::HwInitFailed(format!("cast ID3D11VideoDevice: {e}")))?;
        let video_context: ID3D11VideoContext = context
            .cast()
            .map_err(|e| AvError::HwInitFailed(format!("cast ID3D11VideoContext: {e}")))?;

        let input_format = if top_field_first {
            D3D11_VIDEO_FRAME_FORMAT_INTERLACED_TOP_FIELD_FIRST
        } else {
            D3D11_VIDEO_FRAME_FORMAT_INTERLACED_BOTTOM_FIELD_FIRST
        };

        // Taxas de frame deixadas em 0 (driver infere) — ver mpv vf_d3d11vpp.c.
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: input_format,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 0,
                Denominator: 0,
            },
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 0,
                Denominator: 0,
            },
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        let enumerator = unsafe {
            video_device
                .CreateVideoProcessorEnumerator(&content_desc)
                .map_err(|e| AvError::HwInitFailed(format!("CreateVideoProcessorEnumerator: {e}")))?
        };

        let mut vp_caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
        unsafe {
            enumerator
                .GetVideoProcessorCaps(&mut vp_caps)
                .map_err(|e| AvError::HwInitFailed(format!("GetVideoProcessorCaps: {e}")))?;
        }

        let (rate_index, rc_caps) =
            select_rate_conversion_cap(&enumerator, &vp_caps, DEINT_MODE_ADAPTIVE)
                .or_else(|| select_rate_conversion_cap(&enumerator, &vp_caps, DEINT_MODE_BOB))
                .unwrap_or((0, D3D11_VIDEO_PROCESSOR_RATE_CONVERSION_CAPS::default()));

        let processor = unsafe {
            video_device
                .CreateVideoProcessor(&enumerator, rate_index)
                .map_err(|e| AvError::HwInitFailed(format!("CreateVideoProcessor: {e}")))?
        };

        let processor_caps = rc_caps.ProcessorCaps;

        configure_processor(
            &video_context,
            &processor,
            width,
            height,
            input_format,
        );

        tracing::debug!(
            rate_index,
            processor_caps,
            caps_past = rc_caps.PastFrames,
            caps_future = rc_caps.FutureFrames,
            "D3D11 VP: rate conversion cap selecionado"
        );

        Ok(Self {
            video_device,
            video_context,
            d3d_context: context.clone(),
            enumerator,
            processor,
            width,
            height,
            input_format,
            time_base_num,
            time_base_den,
            last_field_pts: None,
            output_seq: 0,
        })
    }

    /// Meio período de um frame entrelaçado em unidades de PTS do stream.
    fn half_frame_pts_step(&self, field_pts: i64) -> i64 {
        if let Some(last) = self.last_field_pts {
            let delta = field_pts.saturating_sub(last);
            if (1500..=5000).contains(&delta) {
                return delta / 2;
            }
        }
        vp_half_frame_step_fallback(self.time_base_num, self.time_base_den)
    }

    /// Processa um frame HW entrelaçado → saída progressiva zero-copy.
    ///
    /// SPEC-AV-006
    pub fn process(
        &mut self,
        pool: &Arc<SharedNvPool>,
        tex: &D3d11Texture,
        field_pts: i64,
        top_field_first: bool,
    ) -> Result<Vec<VpOutput>, AvError> {
        let input_format = if top_field_first {
            D3D11_VIDEO_FRAME_FORMAT_INTERLACED_TOP_FIELD_FIRST
        } else {
            D3D11_VIDEO_FRAME_FORMAT_INTERLACED_BOTTOM_FIELD_FIRST
        };
        if input_format != self.input_format {
            self.input_format = input_format;
            configure_processor(
                &self.video_context,
                &self.processor,
                self.width,
                self.height,
                input_format,
            );
        }

        let half_step = self.half_frame_pts_step(field_pts);
        self.last_field_pts = Some(field_pts);

        let current_pts = field_pts;
        let input_view = create_input_view(
            &self.video_device,
            &self.enumerator,
            tex.d3d11_texture(),
            tex.array_slice,
        )?;

        unsafe {
            self.video_context.VideoProcessorSetStreamFrameFormat(
                &self.processor,
                0,
                input_format,
            );
        }

        let (out_tex, slot_idx, fence_value, tex_handle) =
            pool.reserve_output_slot(self.width, self.height)?;
        let output_view = create_output_view(
            &self.video_device,
            &self.enumerator,
            &out_tex,
        )?;

        // HALF rate: 1 frame progressivo por frame entrelaçado (25p/29.97p).
        let input_frame_or_field = self.output_seq * 2;

        // Sem refs past/future: ver NOTA no doc da struct (quirk driver Intel).
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: input_frame_or_field,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(input_view)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };

        let blt_result = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &output_view,
                self.output_seq,
                std::slice::from_ref(&stream),
            )
        };
        // Libera a input view retida pelo ManuallyDrop antes de propagar erro.
        let mut stream = stream;
        unsafe {
            std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
        }
        blt_result.map_err(|e| AvError::HwInitFailed(format!("VideoProcessorBlt: {e}")))?;

        self.output_seq = self.output_seq.wrapping_add(1);

        let shared = pool.finalize_output_slot(
            slot_idx,
            fence_value,
            self.width,
            self.height,
            tex_handle,
        )?;

        let pts = vp_output_pts(current_pts, 0, half_step);

        unsafe {
            self.d3d_context.Flush();
        }

        Ok(vec![VpOutput {
            shared,
            pts: pts_raw_to_option(pts),
        }])
    }
}

fn configure_processor(
    video_context: &ID3D11VideoContext,
    processor: &ID3D11VideoProcessor,
    width: u32,
    height: u32,
    input_format: D3D11_VIDEO_FRAME_FORMAT,
) {
    let src_rc = RECT {
        left: 0,
        top: 0,
        right: width as i32,
        bottom: height as i32,
    };
    unsafe {
        video_context.VideoProcessorSetStreamSourceRect(processor, 0, true, Some(&src_rc));
        // mpv desliga auto-processing para o driver não "melhorar" sem deinterlace real.
        video_context.VideoProcessorSetStreamAutoProcessingMode(processor, 0, false);
        video_context.VideoProcessorSetStreamFrameFormat(processor, 0, input_format);
        video_context.VideoProcessorSetStreamOutputRate(
            processor,
            0,
            D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_HALF,
            false,
            None,
        );
    }
}

fn select_rate_conversion_cap(
    enumerator: &ID3D11VideoProcessorEnumerator,
    vp_caps: &D3D11_VIDEO_PROCESSOR_CAPS,
    desired_mode: u32,
) -> Option<(u32, D3D11_VIDEO_PROCESSOR_RATE_CONVERSION_CAPS)> {
    let mut fallback = None;
    for n in 0..vp_caps.RateConversionCapsCount {
        let mut rc_caps = D3D11_VIDEO_PROCESSOR_RATE_CONVERSION_CAPS::default();
        unsafe {
            if enumerator
                .GetVideoProcessorRateConversionCaps(n, &mut rc_caps)
                .is_err()
            {
                continue;
            }
        }
        if fallback.is_none() {
            fallback = Some((n, rc_caps));
        }
        if (rc_caps.ProcessorCaps & desired_mode) == desired_mode {
            return Some((n, rc_caps));
        }
    }
    fallback
}

fn create_input_view(
    video_device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    texture: &ID3D11Texture2D,
    array_slice: u32,
) -> Result<ID3D11VideoProcessorInputView, AvError> {
    let resource: ID3D11Resource = texture
        .cast()
        .map_err(|e| AvError::HwInitFailed(format!("cast texture→Resource: {e}")))?;
    let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
        FourCC: 0,
        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
        Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPIV {
                MipSlice: 0,
                ArraySlice: array_slice,
            },
        },
    };
    let mut view = None;
    unsafe {
        video_device
            .CreateVideoProcessorInputView(&resource, enumerator, &desc, Some(&mut view))
            .map_err(|e| AvError::HwInitFailed(format!("CreateVideoProcessorInputView: {e}")))?;
    }
    view.ok_or_else(|| AvError::HwInitFailed("input view VP nula".into()))
}

fn create_output_view(
    video_device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11VideoProcessorOutputView, AvError> {
    let resource: ID3D11Resource = texture
        .cast()
        .map_err(|e| AvError::HwInitFailed(format!("cast out texture→Resource: {e}")))?;
    let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
        Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
        },
    };
    let mut view = None;
    unsafe {
        video_device
            .CreateVideoProcessorOutputView(&resource, enumerator, &desc, Some(&mut view))
            .map_err(|e| AvError::HwInitFailed(format!("CreateVideoProcessorOutputView: {e}")))?;
    }
    view.ok_or_else(|| AvError::HwInitFailed("output view VP nula".into()))
}

/// PTS de saída bob: `field_pts + field_idx * half_step` (50p/59.94p em 90 kHz).
///
/// `half_step` é metade do delta PTS entre frames entrelaçados consecutivos.
///
/// SPEC-AV-006
pub(crate) fn vp_output_pts(field_pts: i64, field_idx: u32, half_step: i64) -> i64 {
    if field_pts == i64::MIN {
        return i64::MIN;
    }
    field_pts + (field_idx as i64) * half_step
}

/// Fallback quando ainda não há delta PTS (primeiro frame): ~1800 ticks @ 25 fps / 90 kHz.
fn vp_half_frame_step_fallback(time_base_num: i32, time_base_den: i32) -> i64 {
    if time_base_num > 0 && time_base_den > 0 {
        let fps = 25i64;
        (time_base_den as i64) / ((time_base_num as i64) * fps * 2)
    } else {
        1800
    }
}

#[inline]
fn pts_raw_to_option(pts_raw: i64) -> Option<u64> {
    if pts_raw == i64::MIN {
        None
    } else {
        Some(pts_raw as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_av_006_vp_output_pts_bob_halves_timebase() {
        let field_pts = 90_000i64;
        let half = 1800i64;
        let first = vp_output_pts(field_pts, 0, half);
        let second = vp_output_pts(field_pts, 1, half);
        assert_eq!(first, field_pts);
        assert_eq!(second, field_pts + half);
    }

    #[test]
    fn spec_av_006_vp_output_pts_monotonic_across_frames() {
        let p0 = 100_000i64;
        let p1 = p0 + 3600;
        let p2 = p1 + 3600;
        let seq = [vp_output_pts(p0, 0, 1800), vp_output_pts(p1, 0, 1800), vp_output_pts(p2, 0, 1800)];
        for w in seq.windows(2) {
            assert!(w[1] > w[0], "PTS deve ser estritamente crescente: {:?}", seq);
        }
    }

    #[test]
    fn spec_av_006_vp_output_pts_nopreserves() {
        assert_eq!(vp_output_pts(i64::MIN, 0, 1800), i64::MIN);
    }
}
