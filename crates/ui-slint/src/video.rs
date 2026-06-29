//! Conversão de `VideoFrame` (planos YUV na CPU) para um `SharedPixelBuffer` RGBA.
//!
//! O decoder sempre entrega os planos na CPU — `VideoFrame::Sw` (YUV420P/10) e
//! `VideoFrame::Hw` (NV12/P010 já baixado). Convertemos para RGBA8 escrevendo
//! **direto** nos bytes do `SharedPixelBuffer` (sem `Vec` intermediário nem cópia
//! extra). A conversão roda numa thread worker (ver `lib.rs`), fora do event loop
//! do Slint. 10-bit é reduzido a 8-bit (refino HDR é follow-up).

use av::{HwSurface, VideoFrame, YuvColorRange};
use slint::{Rgba8Pixel, SharedPixelBuffer};

/// Converte o frame para um `SharedPixelBuffer` RGBA, ou `None` se inválido.
///
/// Escreve diretamente no buffer de pixels alocado (uma alocação por frame,
/// sem zero-fill redundante nem cópia intermediária).
pub fn convert(frame: &VideoFrame) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    match frame {
        VideoFrame::Sw(yuv) => {
            let w = yuv.width as usize;
            let h = yuv.height as usize;
            if w == 0 || h == 0 {
                return None;
            }
            let full = matches!(yuv.color_range, YuvColorRange::Full);
            let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w as u32, h as u32);
            if yuv420p_into(buf.make_mut_bytes(), &yuv.planes, w, h, yuv.ten_bit, full) {
                Some(buf)
            } else {
                None
            }
        }
        VideoFrame::Hw(hw) => {
            // Frames `Shared` (zero-copy GPU) só ocorrem em modo GPU; sem device
            // D3D11 aqui não há como convertê-los na CPU — ignorados (o flag de
            // zero-copy fica desligado no modo CPU, então não devem chegar).
            let planes = match &hw.surface {
                HwSurface::Cpu(p) => p,
                HwSurface::Shared(_) => return None,
            };
            let w = planes.width as usize;
            let h = planes.height as usize;
            if w == 0 || h == 0 {
                return None;
            }
            let full = matches!(hw.color_range, YuvColorRange::Full);
            let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w as u32, h as u32);
            if nv12_into(
                buf.make_mut_bytes(),
                &planes.y_data,
                &planes.uv_data,
                w,
                h,
                planes.ten_bit,
                full,
            ) {
                Some(buf)
            } else {
                None
            }
        }
    }
}

#[inline(always)]
fn sample8(plane: &[u8], idx: usize) -> i32 {
    plane[idx] as i32
}

#[inline(always)]
fn sample10(plane: &[u8], idx: usize) -> i32 {
    let b = idx * 2;
    ((plane[b + 1] as i32) << 8 | plane[b] as i32) >> 2
}

#[inline(always)]
fn write_rgb(out: &mut [u8], y: i32, u: i32, v: i32, full: bool) {
    let c = if full { y } else { y - 16 };
    let d = u - 128;
    let e = v - 128;
    out[0] = ((298 * c + 409 * e + 128) >> 8).clamp(0, 255) as u8;
    out[1] = ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8;
    out[2] = ((298 * c + 516 * d + 128) >> 8).clamp(0, 255) as u8;
    out[3] = 255;
}

/// YUV420P (planos Y/U/V separados, croma a 1/2 resolução) → RGBA no `dst`.
/// Retorna `false` se os planos forem menores que o esperado (frame ignorado).
fn yuv420p_into(dst: &mut [u8], planes: &[Vec<u8>; 3], w: usize, h: usize, ten: bool, full: bool) -> bool {
    let uv_w = w.div_ceil(2);
    let uv_h = h.div_ceil(2);
    let bpp = if ten { 2 } else { 1 };
    if planes[0].len() < w * h * bpp || planes[1].len() < uv_w * uv_h * bpp || planes[2].len() < uv_w * uv_h * bpp {
        return false;
    }
    for row in 0..h {
        let uv_row = row / 2;
        let y_base = row * w;
        let uv_base = uv_row * uv_w;
        let dst_base = row * w * 4;
        for col in 0..w {
            let uv_idx = uv_base + col / 2;
            let (y, u, v) = if ten {
                (
                    sample10(&planes[0], y_base + col),
                    sample10(&planes[1], uv_idx),
                    sample10(&planes[2], uv_idx),
                )
            } else {
                (
                    sample8(&planes[0], y_base + col),
                    sample8(&planes[1], uv_idx),
                    sample8(&planes[2], uv_idx),
                )
            };
            let o = dst_base + col * 4;
            write_rgb(&mut dst[o..o + 4], y, u, v, full);
        }
    }
    true
}

/// NV12 (Y planar + UV intercalado a 1/2 resolução) → RGBA no `dst`.
fn nv12_into(dst: &mut [u8], y_plane: &[u8], uv_plane: &[u8], w: usize, h: usize, ten: bool, full: bool) -> bool {
    let uv_w = w.div_ceil(2);
    let uv_h = h.div_ceil(2);
    let bpp = if ten { 2 } else { 1 };
    // UV intercalado: cada par (U,V) ocupa 2 amostras.
    if y_plane.len() < w * h * bpp || uv_plane.len() < uv_w * uv_h * 2 * bpp {
        return false;
    }
    for row in 0..h {
        let uv_row = row / 2;
        let y_base = row * w;
        let pair_base = uv_row * uv_w;
        let dst_base = row * w * 4;
        for col in 0..w {
            let pair = pair_base + col / 2;
            let (y, u, v) = if ten {
                (
                    sample10(y_plane, y_base + col),
                    sample10(uv_plane, pair * 2),
                    sample10(uv_plane, pair * 2 + 1),
                )
            } else {
                (
                    sample8(y_plane, y_base + col),
                    sample8(uv_plane, pair * 2),
                    sample8(uv_plane, pair * 2 + 1),
                )
            };
            let o = dst_base + col * 4;
            write_rgb(&mut dst[o..o + 4], y, u, v, full);
        }
    }
    true
}
