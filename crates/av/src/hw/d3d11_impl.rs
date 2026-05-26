//! Implementação D3D11 real (Windows).
//!
//! Usa o crate `windows = "0.54"` para bindings COM/D3D11/DXGI.
//!
//! SPEC-AV-HW-001

use std::sync::Arc;

use tracing::{debug, info};
use windows::{
    core::Interface,
    Win32::Graphics::{
        Direct3D::D3D_DRIVER_TYPE_HARDWARE,
        Direct3D11::{
            D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
        },
        Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1},
    },
};

use crate::error::AvError;

// ── Tipos públicos ─────────────────────────────────────────────────────────────

/// Formato de pixel de hardware suportado pelo decoder D3D11VA.
///
/// SPEC-AV-HW-001
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwPixelFormat {
    /// YUV 4:2:0 planar 8-bit (NV12).
    Nv12,
    /// YUV 4:2:0 planar 10-bit (P010).
    P010,
}

/// LUID de um adaptador DXGI — identifica univocamente um adapter D3D no sistema.
///
/// SPEC-AV-HW-001
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterLuid {
    pub low_part: u32,
    pub high_part: i32,
}

impl AdapterLuid {
    /// Converte em `u64` (LowPart nos 32 bits baixos, HighPart cast para u32 nos 32 bits altos).
    pub fn as_u64(self) -> u64 {
        ((self.high_part as u32 as u64) << 32) | self.low_part as u64
    }
}

impl std::fmt::Display for AdapterLuid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#010x}:{:#010x}", self.high_part, self.low_part)
    }
}

// ── D3d11Device ───────────────────────────────────────────────────────────────

/// Encapsula um `ID3D11Device` criado standalone para hwaccel D3D11VA.
///
/// Exatamente **uma** instância deve existir no processo.  Tanto o `FfmpegDecoder`
/// quanto o backend wgpu referenciam o adapter por LUID a partir desta instância.
///
/// # Thread-safety
///
/// `ID3D11Multithread::SetMultithreadProtected(true)` é chamado durante a
/// construção, tornando seguro o acesso concorrente entre a thread do decoder e
/// a thread de renderização.
///
/// SPEC-AV-HW-001
pub struct D3d11Device {
    device: ID3D11Device,
    #[allow(dead_code)]
    context: ID3D11DeviceContext,
    adapter_luid: AdapterLuid,
    adapter_desc: String,
    vendor_id: u32,
}

// SAFETY: ID3D11Device com SetMultithreadProtected(true) é seguro para Send+Sync.
// O immediate context é usado apenas pela thread do decoder com
// serialização interna do D3D11 runtime.
unsafe impl Send for D3d11Device {}
unsafe impl Sync for D3d11Device {}

impl D3d11Device {
    /// Cria um `ID3D11Device` no adapter de hardware primário.
    ///
    /// Seleciona o primeiro adapter DXGI com hardware real; em
    /// sistemas multi-GPU, isso corresponde ao adapter de maior performance
    /// enumerado pela DXGI (ordem determinada pelo driver/OS).
    ///
    /// Retorna erro se:
    /// - Nenhum adapter D3D11 disponível (modo seguro, RDP sem GPU virtual, etc.)
    /// - `D3D11CreateDevice` falha (driver corrompido / sem suporte D3D11)
    ///
    /// SPEC-AV-HW-001
    pub fn new() -> Result<Arc<Self>, AvError> {
        // ── 1. Enumerar adapters via DXGI ──────────────────────────────────────
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
            .map_err(|e| AvError::HwInitFailed(format!("CreateDXGIFactory1 falhou: {e}")))?;

        let adapter1: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(0) }
            .map_err(|_| AvError::HwInitFailed("Nenhum adapter DXGI encontrado".into()))?;

        // Obtém descrição e LUID do adapter
        let mut desc = windows::Win32::Graphics::Dxgi::DXGI_ADAPTER_DESC1::default();
        unsafe { adapter1.GetDesc1(&mut desc) }
            .map_err(|e| AvError::HwInitFailed(format!("GetDesc1 falhou: {e}")))?;

        let adapter_luid = AdapterLuid {
            low_part: desc.AdapterLuid.LowPart,
            high_part: desc.AdapterLuid.HighPart,
        };
        let vendor_id = desc.VendorId;

        // Converte descrição UTF-16 para String (trunca no primeiro nul)
        let name_end = desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
        let adapter_desc = String::from_utf16_lossy(&desc.Description[..name_end]);

        info!(
            adapter = %adapter_desc,
            luid = %adapter_luid,
            vendor_id = format!("{:#06x}", vendor_id),
            "hw: adapter D3D11 selecionado"
        );

        // ── 2. Cast IDXGIAdapter1 → IDXGIAdapter para D3D11CreateDevice ───────
        let adapter: IDXGIAdapter = adapter1.cast().map_err(|e| {
            AvError::HwInitFailed(format!("cast IDXGIAdapter1→IDXGIAdapter falhou: {e}"))
        })?;

        // ── 3. Criar ID3D11Device ──────────────────────────────────────────────
        //
        // D3D11_CREATE_DEVICE_BGRA_SUPPORT: necessário para interop DXGI/wgpu (surface sharing).
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;

        unsafe {
            D3D11CreateDevice(
                Some(&adapter),
                D3D_DRIVER_TYPE_HARDWARE,
                None, // software rasterizer module (não usado com adapter explícito)
                flags,
                None, // feature levels (usa default D3D11_0+)
                D3D11_SDK_VERSION,
                Some(&mut device),
                None, // feature level selecionada (não necessário para Fase A)
                Some(&mut context),
            )
        }
        .map_err(|e| AvError::HwInitFailed(format!("D3D11CreateDevice falhou: {e}")))?;

        let device = device.ok_or_else(|| {
            AvError::HwInitFailed("D3D11CreateDevice retornou device nulo".into())
        })?;

        let context = context.ok_or_else(|| {
            AvError::HwInitFailed("D3D11CreateDevice retornou context nulo".into())
        })?;

        // ── 4. Habilitar proteção multithread no device context ────────────────
        //
        // Necessário para acesso concorrente entre decoder thread e render thread.
        // Ref: https://learn.microsoft.com/en-us/windows/win32/api/d3d11/nn-d3d11-id3d11multithread
        use windows::Win32::Graphics::Direct3D11::ID3D11Multithread;
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            unsafe { mt.SetMultithreadProtected(true) };
            debug!("hw: ID3D11Multithread::SetMultithreadProtected(true) OK");
        }

        info!(luid = %adapter_luid, "hw: D3d11Device criado com sucesso");

        Ok(Arc::new(Self {
            device,
            context,
            adapter_luid,
            adapter_desc,
            vendor_id,
        }))
    }

    /// Retorna o LUID do adapter, usado para sincronizar seleção com wgpu.
    ///
    /// SPEC-AV-HW-001
    pub fn adapter_luid(&self) -> AdapterLuid {
        self.adapter_luid
    }

    /// Retorna o `VendorId` DXGI do adapter (PCI vendor ID).
    ///
    /// Exemplos: `0x10de` = NVIDIA, `0x8086` = Intel, `0x1002` = AMD.
    ///
    /// SPEC-AV-HW-001
    pub fn vendor_id(&self) -> u32 {
        self.vendor_id
    }

    /// Descrição legível do adapter (ex.: "NVIDIA GeForce RTX 4060").
    pub fn adapter_description(&self) -> &str {
        &self.adapter_desc
    }

    /// Ponteiro bruto para o `ID3D11Device` (consumido pelo FFmpeg hwaccel context).
    ///
    /// # Safety
    ///
    /// O chamador deve chamar `AddRef` se armazenar o ponteiro além do tempo de
    /// vida deste `D3d11Device`.  O ponteiro permanece válido enquanto este
    /// `Arc<D3d11Device>` existir.
    ///
    /// SPEC-AV-HW-001
    pub unsafe fn as_raw(&self) -> *mut std::ffi::c_void {
        self.device.as_raw()
    }
}

impl Drop for D3d11Device {
    fn drop(&mut self) {
        debug!(
            "hw: D3d11Device sendo dropado (adapter: {})",
            self.adapter_desc
        );
        // ID3D11Device/ID3D11DeviceContext implementam Drop via windows crate (Release automático)
    }
}

// ── D3d11Texture ──────────────────────────────────────────────────────────────

/// Espaço de cor do conteúdo da textura.
///
/// SPEC-AV-HW-001
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpace {
    Bt601,
    Bt709,
    Bt2020,
}

/// Função de transferência (curva eletro-óptica).
///
/// SPEC-AV-HW-001
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferFunction {
    Bt1886,
    Pq,
    Hlg,
    Srgb,
}

/// Referência a uma textura D3D11VA produzida pelo decoder FFmpeg.
///
/// Encapsula um `ID3D11Texture2D` de array + índice de slice.  O caller
/// (VideoRenderer) a consome via `into_wgpu` sem nenhuma cópia CPU↔GPU.
///
/// # Ciclo de vida
///
/// A textura é válida enquanto o frame FFmpeg subjacente estiver vivo.
/// O `VideoQueue` mantém o frame referenciado até o render pass completar.
///
/// SPEC-AV-HW-001
pub struct D3d11Texture {
    texture: ID3D11Texture2D,
    /// Índice no array texture (D3D11VA usa texture arrays para o frame pool).
    pub array_slice: u32,
    /// Formato de pixel do conteúdo.
    pub format: HwPixelFormat,
    pub width: u32,
    pub height: u32,
    pub color_space: ColorSpace,
    pub transfer: TransferFunction,
    pub full_range: bool,
}

// SAFETY: ID3D11Texture2D é seguro para Send — não tem estado por-thread;
// o acesso é serializado pelo device multithread.
unsafe impl Send for D3d11Texture {}
unsafe impl Sync for D3d11Texture {}

impl D3d11Texture {
    /// Cria um `D3d11Texture` a partir de um `ID3D11Texture2D` e metadados de frame.
    ///
    /// SPEC-AV-HW-001
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        texture: ID3D11Texture2D,
        array_slice: u32,
        format: HwPixelFormat,
        width: u32,
        height: u32,
        color_space: ColorSpace,
        transfer: TransferFunction,
        full_range: bool,
    ) -> Self {
        Self {
            texture,
            array_slice,
            format,
            width,
            height,
            color_space,
            transfer,
            full_range,
        }
    }

    /// Converte para `wgpu::Texture` via interop D3D11↔wgpu.
    ///
    /// **Fase C** implementará o caminho zero-copy real.  Nesta Fase A, retorna
    /// erro para documentar a interface — o `VideoRenderer` usa apenas frames SW.
    ///
    /// SPEC-AV-HW-001
    pub fn into_wgpu(&self, _device: &wgpu::Device) -> Result<wgpu::Texture, AvError> {
        Err(AvError::HwInitFailed(
            "into_wgpu não implementado na Fase A — apenas Fase C".into(),
        ))
    }

    /// Ponteiro bruto para o `ID3D11Texture2D` (consumido pelo FFmpeg hwaccel context).
    ///
    /// # Safety
    ///
    /// O chamador deve garantir que o `D3d11Texture` permaneça vivo enquanto
    /// o ponteiro for usado.
    pub unsafe fn as_raw(&self) -> *mut std::ffi::c_void {
        self.texture.as_raw()
    }
}

// ── Testes ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Valida que AdapterLuid::as_u64 arredonda corretamente.
    ///
    /// SPEC-AV-HW-001
    #[test]
    fn spec_av_hw_001_luid_roundtrip() {
        let luid = AdapterLuid {
            low_part: 0x0000_1234,
            high_part: 0x0000_0001,
        };
        let u = luid.as_u64();
        assert_eq!(
            u & 0xFFFF_FFFF,
            0x0000_1234u64,
            "LowPart deve estar nos 32 bits baixos"
        );
        assert_eq!(
            u >> 32,
            0x0000_0001u64,
            "HighPart deve estar nos 32 bits altos"
        );
    }

    /// Valida que AdapterLuid::as_u64 funciona com high_part negativo.
    #[test]
    fn spec_av_hw_001_luid_negative_high() {
        let luid = AdapterLuid {
            low_part: 0xDEAD_BEEF,
            high_part: -1,
        };
        let u = luid.as_u64();
        // high_part -1 como u32 = 0xFFFFFFFF
        assert_eq!(u >> 32, 0xFFFF_FFFFu64);
        assert_eq!(u & 0xFFFF_FFFF, 0xDEAD_BEEFu64);
    }

    /// Valida que D3d11Device pode ser criado em ambiente Windows com GPU real.
    ///
    /// Marcado `#[ignore]` para não rodar em CI sem GPU.
    ///
    /// SPEC-AV-HW-001
    #[test]
    #[ignore = "requer GPU D3D11 real; rodar em runner self-hosted"]
    fn spec_av_hw_001_device_creation() {
        let dev = D3d11Device::new().expect("D3d11Device::new falhou");
        let luid = dev.adapter_luid();
        assert_ne!(luid.as_u64(), 0, "LUID deve ser não-zero");
        println!(
            "Adapter: {}, LUID: {}, VendorId: {:#06x}",
            dev.adapter_description(),
            luid,
            dev.vendor_id()
        );
    }
}
