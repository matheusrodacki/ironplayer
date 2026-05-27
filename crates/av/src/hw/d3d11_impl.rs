//! Implementação D3D11 real (Windows).
//!
//! Usa o crate `windows = "0.54"` para bindings COM/D3D11/DXGI.
//!
//! SPEC-AV-HW-001

use std::sync::Arc;

use tracing::{debug, info};
use windows::{
    core::{Error as WinError, Interface},
    Win32::Graphics::{
        Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
        Direct3D11::{
            D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
            D3D11_BIND_FLAG, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_FLAG, D3D11_SDK_VERSION,
            D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
        },
        Dxgi::{
            Common::{DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_SAMPLE_DESC},
            CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1,
            DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
        },
    },
};

use crate::error::AvError;

fn map_d3d11_error(context: &str, error: WinError) -> AvError {
    let code = error.code();
    if code == DXGI_ERROR_DEVICE_REMOVED || code == DXGI_ERROR_DEVICE_RESET {
        AvError::HwDeviceRemoved(format!("{context}: {error}"))
    } else {
        AvError::HwInitFailed(format!("{context}: {error}"))
    }
}

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

impl std::fmt::Debug for D3d11Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("D3d11Device")
            .field("adapter", &self.adapter_desc)
            .field("luid", &self.adapter_luid)
            .finish()
    }
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
                // Microsoft requer DRIVER_TYPE_UNKNOWN quando o adapter é
                // passado explicitamente; HARDWARE+adapter causa E_INVALIDARG.
                D3D_DRIVER_TYPE_UNKNOWN,
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

impl ColorSpace {
    /// Converte o `colorspace` avutil (inteiro) para `ColorSpace`.
    /// Valores: 1=BT.709, 5/6=BT.601, 9=BT.2020; padrão BT.709.
    pub fn from_avutil(cs: i32) -> Self {
        match cs {
            5 | 6 => Self::Bt601,
            9 => Self::Bt2020,
            _ => Self::Bt709,
        }
    }
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

impl TransferFunction {
    /// Converte `AVColorTransferCharacteristic` bruto para a TRC usada no shader.
    pub fn from_avutil(trc: i32) -> Self {
        match trc {
            16 => Self::Pq,
            18 => Self::Hlg,
            13 => Self::Srgb,
            _ => Self::Bt1886,
        }
    }
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

impl std::fmt::Debug for D3d11Texture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("D3d11Texture")
            .field("array_slice", &self.array_slice)
            .field("format", &self.format)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

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

    /// Cria um `D3d11Texture` a partir de um ponteiro bruto `ID3D11Texture2D*`
    /// proveniente do AVFrame HW D3D11VA, chamando `AddRef` para garantir
    /// que a textura permanece válida após o `FfmpegFrame::unref()`.
    ///
    /// # Safety
    ///
    /// `tex_ptr` deve ser um ponteiro válido para um `ID3D11Texture2D` vivo,
    /// com o formato `DXGI_FORMAT_NV12` ou `DXGI_FORMAT_P010`.
    ///
    /// SPEC-AV-HW-TEX-001
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn from_raw_addref(
        tex_ptr: *mut std::ffi::c_void,
        array_slice: u32,
        width: u32,
        height: u32,
        color_space: ColorSpace,
        transfer: TransferFunction,
        full_range: bool,
    ) -> Result<Self, AvError> {
        if tex_ptr.is_null() {
            return Err(AvError::HwInitFailed("ponteiro de textura HW é nulo".into()));
        }
        // ManuallyDrop evita o Release automático do temporário criado por from_raw;
        // em seguida clone() chama AddRef e retorna nossa referência própria.
        let texture = {
            let borrowed =
                std::mem::ManuallyDrop::new(ID3D11Texture2D::from_raw(tex_ptr as *mut _));
            (*borrowed).clone()
        };
        let format = detect_texture_format(&texture)?;
        Ok(Self::new(
            texture,
            array_slice,
            format,
            width,
            height,
            color_space,
            transfer,
            full_range,
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

fn detect_texture_format(texture: &ID3D11Texture2D) -> Result<HwPixelFormat, AvError> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    if desc.Format == DXGI_FORMAT_NV12 {
        Ok(HwPixelFormat::Nv12)
    } else if desc.Format == DXGI_FORMAT_P010 {
        Ok(HwPixelFormat::P010)
    } else {
        Err(AvError::HwInitFailed(format!(
            "formato D3D11VA não suportado: {:?}",
            desc.Format
        )))
    }
}

// ── NvPlanes + extração via staging ──────────────────────────────────────────

/// Planos NV12 extraídos via textura D3D11 staging (GPU→CPU).
///
/// Dados compactados (sem padding de row alignment do driver).
///
/// SPEC-AV-HW-TEX-001
pub struct NvPlanes {
    /// Plano luma Y compactado (`width × height × bytes_per_sample`).
    pub y_data: Vec<u8>,
    /// Plano croma UV interleaved (`width × ceil(height/2) × bytes_per_sample`).
    /// Layout: U0 V0 U1 V1 … por linha, `width/2` pares por linha.
    pub uv_data: Vec<u8>,
    /// Largura em pixels.
    pub width: u32,
    /// Altura em pixels.
    pub height: u32,
    /// `true` quando a textura origem é P010.
    pub ten_bit: bool,
}

impl D3d11Device {
    /// Extrai os planos NV12 de uma textura HW D3D11VA via textura staging.
    ///
    /// Fluxo GPU (sem CPU round-trip FFmpeg):
    /// 1. Cria textura NV12 `D3D11_USAGE_STAGING` (CPU-readable).
    /// 2. `CopySubresourceRegion` — copia Y e UV do slice do array para staging.
    /// 3. `Map` subresource 0 (Y) e subresource 1 (UV) — lê dados do driver.
    /// 4. Compacta em `Vec<u8>` sem padding de linha.
    /// 5. `Unmap` ambos os subresources.
    ///
    /// Não chama `av_hwframe_transfer_data` em nenhum momento.
    ///
    /// SPEC-AV-HW-TEX-001
    pub fn extract_nv12_planes(&self, tex: &D3d11Texture) -> Result<NvPlanes, AvError> {
        let width = tex.width;
        let height = tex.height;
        if width == 0 || height == 0 {
            return Err(AvError::HwInitFailed(
                "extract_nv12_planes: dimensões inválidas".into(),
            ));
        }

        // ── 1. Obtém ArraySize da textura fonte para calcular o índice UV ─────
        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.texture.GetDesc(&mut src_desc) };
        let array_size = src_desc.ArraySize;

        // ── 2. Cria textura staging NV12 (tamanho do frame individual) ────────
        let is_p010 = matches!(tex.format, HwPixelFormat::P010);
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: if is_p010 { DXGI_FORMAT_P010 } else { DXGI_FORMAT_NV12 },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: D3D11_BIND_FLAG(0).0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: D3D11_RESOURCE_MISC_FLAG(0).0 as u32,
        };
        let mut staging_opt: Option<ID3D11Texture2D> = None;
        unsafe {
            self.device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging_opt))
                .map_err(|e| map_d3d11_error("CreateTexture2D staging", e))?;
        }
        let staging = staging_opt
            .ok_or_else(|| AvError::HwInitFailed("staging texture nula".into()))?;

        // ── 3. Copia Y e UV do array slice para o staging ──────────────────────
        let staging_res: ID3D11Resource = staging
            .cast()
            .map_err(|e| AvError::HwInitFailed(format!("cast staging→Resource: {e}")))?;
        let src_res: ID3D11Resource = tex
            .texture
            .cast()
            .map_err(|e| AvError::HwInitFailed(format!("cast src→Resource: {e}")))?;

        unsafe {
            // Subresource Y do array: slice_index × MipLevels (=1) + mip_level (=0)
            self.context
                .CopySubresourceRegion(&staging_res, 0, 0, 0, 0, &src_res, tex.array_slice, None);
            // Subresource UV do array: array_size + slice_index
            self.context.CopySubresourceRegion(
                &staging_res,
                1,
                0,
                0,
                0,
                &src_res,
                array_size + tex.array_slice,
                None,
            );
        }

        // ── 4. Map + extração compacta ─────────────────────────────────────────
        let mut mapped_y = D3D11_MAPPED_SUBRESOURCE::default();
        let mut mapped_uv = D3D11_MAPPED_SUBRESOURCE::default();

        unsafe {
            self.context
                .Map(&staging_res, 0, D3D11_MAP_READ, 0, Some(&mut mapped_y))
                .map_err(|e| map_d3d11_error("Map Y", e))?;
        }

        // Map UV — se falhar, deve Unmap Y antes de retornar
        let uv_map_result = unsafe {
            self.context
                .Map(&staging_res, 1, D3D11_MAP_READ, 0, Some(&mut mapped_uv))
        };
        if let Err(e) = uv_map_result {
            unsafe { self.context.Unmap(&staging_res, 0) };
            return Err(map_d3d11_error("Map UV", e));
        }

        // Copia Y compactado (sem row padding do driver)
        let w = width as usize;
        let h = height as usize;
        let h_uv = h.div_ceil(2);
        let bytes_per_sample = if is_p010 { 2 } else { 1 };
        let row_bytes = w * bytes_per_sample;
        let mut y_data = vec![0u8; row_bytes * h];
        let mut uv_data = vec![0u8; row_bytes * h_uv];

        let y_row_pitch = mapped_y.RowPitch as usize;
        let uv_row_pitch = mapped_uv.RowPitch as usize;

        unsafe {
            let y_src = mapped_y.pData as *const u8;
            for row in 0..h {
                let src = y_src.add(row * y_row_pitch);
                let dst = y_data[row * row_bytes..].as_mut_ptr();
                std::ptr::copy_nonoverlapping(src, dst, row_bytes);
            }

            let uv_src = mapped_uv.pData as *const u8;
            for row in 0..h_uv {
                // Cada linha UV tem `width × bytes_per_sample` bytes compactados.
                let src = uv_src.add(row * uv_row_pitch);
                let dst = uv_data[row * row_bytes..].as_mut_ptr();
                std::ptr::copy_nonoverlapping(src, dst, row_bytes);
            }
        }

        // ── 5. Unmap ───────────────────────────────────────────────────────────
        unsafe {
            self.context.Unmap(&staging_res, 1);
            self.context.Unmap(&staging_res, 0);
        }

        Ok(NvPlanes {
            y_data,
            uv_data,
            width,
            height,
            ten_bit: is_p010,
        })
    }
}

// ─── Testes ───────────────────────────────────────────────────────────────────

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

    #[test]
    fn spec_av_hw_001_transfer_function_from_avutil_maps_hdr_values() {
        assert_eq!(TransferFunction::from_avutil(16), TransferFunction::Pq);
        assert_eq!(TransferFunction::from_avutil(18), TransferFunction::Hlg);
        assert_eq!(TransferFunction::from_avutil(1), TransferFunction::Bt1886);
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
