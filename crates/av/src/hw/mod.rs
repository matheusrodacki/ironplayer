//! Módulo `hw` — Bootstrap D3D11 e tipos de textura de hardware.
//!
//! Cria e encapsula o `ID3D11Device` usado tanto pelo FFmpeg (hwaccel D3D11VA)
//! quanto pelo wgpu (via adapter LUID compartilhado).
//!
//! # Segurança
//!
//! Todo `unsafe` COM/D3D11 está confinado neste módulo.  Ponteiros COM são
//! gerenciados com `Arc` + `AddRef/Release` em `Drop`.
//!
//! SPEC-AV-HW-001

#[cfg(windows)]
mod d3d11_impl;
#[cfg(windows)]
pub use d3d11_impl::{D3d11Device, D3d11Texture, HwPixelFormat};

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::{D3d11Device, D3d11Texture, HwPixelFormat};
