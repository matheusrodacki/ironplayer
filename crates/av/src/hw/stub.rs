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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum TransferFunction {
    Bt1886,
    Pq,
    Hlg,
    Srgb,
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
}

/// Stub de `D3d11Texture` para plataformas não-Windows.
pub struct D3d11Texture;

impl D3d11Texture {
    pub fn into_wgpu(&self, _device: &wgpu::Device) -> Result<wgpu::Texture, AvError> {
        Err(AvError::HwInitFailed(
            "D3D11 não suportado nesta plataforma".into(),
        ))
    }
}
