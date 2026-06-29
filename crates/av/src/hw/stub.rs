//! Stub para plataformas não-Windows (compilação cruzada / CI Linux).
//!
//! Todos os tipos retornam erro imediatamente — D3D11VA é exclusivamente Windows.

use crate::error::AvError;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwPixelFormat {
    Nv12,
    P010,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ColorSpace {
    Bt601,
    Bt709,
    Bt2020,
}

impl ColorSpace {
    pub fn from_avutil(cs: i32) -> Self {
        match cs {
            5 | 6 => Self::Bt601,
            9 => Self::Bt2020,
            _ => Self::Bt709,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum TransferFunction {
    Bt1886,
    Pq,
    Hlg,
    Srgb,
}

impl TransferFunction {
    pub fn from_avutil(trc: i32) -> Self {
        match trc {
            16 => Self::Pq,
            18 => Self::Hlg,
            13 => Self::Srgb,
            _ => Self::Bt1886,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterLuid {
    pub low_part: u32,
    pub high_part: i32,
}

impl AdapterLuid {
    pub fn as_u64(self) -> u64 {
        ((self.high_part as u32 as u64) << 32) | self.low_part as u64
    }
}

impl std::fmt::Display for AdapterLuid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#010x}:{:#010x}", self.high_part, self.low_part)
    }
}

/// Stub de `D3d11Device` para plataformas não-Windows.
#[derive(Debug)]
pub struct D3d11Device;

impl D3d11Device {
    pub fn new() -> Result<Arc<Self>, AvError> {
        Err(AvError::HwInitFailed(
            "D3D11 não suportado nesta plataforma".into(),
        ))
    }
    pub fn adapter_luid(&self) -> AdapterLuid {
        AdapterLuid {
            low_part: 0,
            high_part: 0,
        }
    }
    pub fn vendor_id(&self) -> u32 {
        0
    }
    pub fn adapter_description(&self) -> &str {
        "stub"
    }
    pub fn extract_nv12_planes(&self, _tex: &D3d11Texture) -> Result<NvPlanes, AvError> {
        Err(AvError::HwInitFailed(
            "D3D11 não suportado nesta plataforma".into(),
        ))
    }
}

/// Stub de planos NV12 extraídos via staging (plataformas não-Windows).
pub struct NvPlanes {
    pub y_data: Vec<u8>,
    pub uv_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub ten_bit: bool,
}

/// Stub não-habitado de `SharedNvFrame` (zero-copy HW é exclusivo de Windows).
///
/// `HwSurface::Shared` nunca é construído em não-Windows.
#[derive(Debug)]
pub enum SharedNvFrame {}

/// Stub de `D3d11Texture` para plataformas não-Windows.
#[derive(Debug)]
pub struct D3d11Texture;

impl D3d11Texture {
    pub fn into_wgpu(&self, _device: &wgpu::Device) -> Result<wgpu::Texture, AvError> {
        Err(AvError::HwInitFailed(
            "D3D11 não suportado nesta plataforma".into(),
        ))
    }

    /// Stub: nunca é chamado em não-Windows (sem frames HW).
    ///
    /// SPEC-AV-HW-TEX-001
    pub unsafe fn from_raw_addref(
        _tex_ptr: *mut std::ffi::c_void,
        _array_slice: u32,
        _width: u32,
        _height: u32,
        _color_space: ColorSpace,
        _transfer: TransferFunction,
        _full_range: bool,
    ) -> Result<Self, AvError> {
        Err(AvError::HwInitFailed(
            "D3D11 não suportado nesta plataforma".into(),
        ))
    }
}
