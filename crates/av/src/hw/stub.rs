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
pub enum ColorSpace {
    Bt601,
    Bt709,
    Bt2020,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferFunction {
    Bt1886,
    Pq,
    Hlg,
    Srgb,
}

/// Stub de `D3d11Device` para plataformas não-Windows.
pub struct D3d11Device;

impl D3d11Device {
    pub fn new() -> Result<Arc<Self>, AvError> {
        Err(AvError::HwInitFailed(
            "D3D11 não suportado nesta plataforma".into(),
        ))
    }
    pub fn adapter_luid(&self) -> u64 {
        0
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
