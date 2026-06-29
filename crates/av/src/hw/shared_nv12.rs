//! Fase 2 — zero-copy de hardware: pool de texturas NV12 **compartilhadas**
//! (D3D11 → wgpu/DX12) com sincronização por fence compartilhada.
//!
//! O decoder (produtor, thread `av-decode`) copia o slice NV12 do pool D3D11VA
//! para uma textura compartilhável (`SHARED_NTHANDLE`), `Signal`a uma fence e
//! entrega um [`SharedNvFrame`] no `HwVideoFrame`. O renderer (consumidor, thread
//! da UI) abre o handle na GPU wgpu/DX12 (`renderer.rs`), espera a fence e
//! amostra a textura — **sem download CPU**.
//!
//! ## Sincronização
//! D3D12 não suporta keyed mutex em recursos abertos; por isso usamos uma fence
//! compartilhada D3D11↔D3D12. Como cada frame fica na fila (`VideoQueue`) por
//! dezenas de ms antes de exibir, a cópia já terminou quando o consumidor lê
//! `GetCompletedValue` — a espera é praticamente sempre imediata.
//!
//! ## Ciclo de vida (lifetime)
//! Um slot do pool só é reutilizado quando o `SharedNvFrame` correspondente é
//! liberado (Drop devolve o slot à free-list). Como o `VideoQueue` mantém o
//! `HwVideoFrame` vivo até exibir/descartar, o produtor nunca sobrescreve uma
//! textura ainda referenciada — sem "zig-zag".
//!
//! Somente **NV12 8-bit**. P010/10-bit cai no caminho de planos (Fase 1).
//!
//! SPEC-AV-HW-ZEROCOPY-001

use std::sync::{Arc, Mutex, Weak};

use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence,
    ID3D11Resource, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE, D3D11_FENCE_FLAG_SHARED,
    D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIResource1;

use crate::error::AvError;

/// Capacidade máxima do pool de texturas compartilhadas.
///
/// Dimensionada para cobrir a profundidade da `VideoQueue` (até ~160 frames).
/// Cada slot 4K NV12 ≈ 12 MB; o pool cresce sob demanda até este teto. Se
/// esgotar, o frame cai no caminho de planos (CPU) da Fase 1.
const MAX_SLOTS: usize = 96;

/// Uma textura NV12 compartilhável (NT handle) no device D3D11 de decode.
struct Slot {
    #[allow(dead_code)]
    texture: ID3D11Texture2D,
    resource: ID3D11Resource,
    handle: HANDLE,
}

// SAFETY: COM objects D3D11 num device com `SetMultithreadProtected(true)`;
// acesso serializado pelo runtime D3D11. O handle NT é global ao processo.
unsafe impl Send for Slot {}

struct PoolState {
    slots: Vec<Slot>,
    free: Vec<usize>,
    /// Dimensões atuais; mudança de resolução recria o pool.
    dims: Option<(u32, u32)>,
    next_fence_value: u64,
}

/// Pool de texturas NV12 compartilhadas + fence compartilhada, no device de decode.
pub struct SharedNvPool {
    device: ID3D11Device5,
    context: ID3D11DeviceContext4,
    fence: ID3D11Fence,
    /// Handle NT da fence (aberto como `ID3D12Fence` pelo consumidor).
    fence_handle: HANDLE,
    state: Mutex<PoolState>,
}

// SAFETY: device com multithread protection; fence/handle são thread-safe.
unsafe impl Send for SharedNvPool {}
unsafe impl Sync for SharedNvPool {}

impl Drop for SharedNvPool {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        for slot in state.slots.drain(..) {
            unsafe {
                let _ = CloseHandle(slot.handle);
            }
        }
        unsafe {
            let _ = CloseHandle(self.fence_handle);
        }
    }
}

impl SharedNvPool {
    /// Cria o pool a partir do `ID3D11Device`/context do decoder.
    ///
    /// Requer D3D11.4 (`ID3D11Device5`/`ID3D11DeviceContext4`) para fences. Em
    /// falha, o chamador deve cair no caminho de planos (CPU).
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> Result<Arc<Self>, AvError> {
        let device5: ID3D11Device5 = device
            .cast()
            .map_err(|e| AvError::HwInitFailed(format!("cast ID3D11Device5 (D3D11.4): {e}")))?;
        let context4: ID3D11DeviceContext4 = context
            .cast()
            .map_err(|e| AvError::HwInitFailed(format!("cast ID3D11DeviceContext4: {e}")))?;

        // Fence compartilhada (D3D11 sinaliza; D3D12 espera via handle).
        let mut fence_opt: Option<ID3D11Fence> = None;
        unsafe { device5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence_opt) }
            .map_err(|e| AvError::HwInitFailed(format!("CreateFence: {e}")))?;
        let fence =
            fence_opt.ok_or_else(|| AvError::HwInitFailed("CreateFence retornou nulo".into()))?;
        let fence_handle = unsafe { fence.CreateSharedHandle(None, GENERIC_ALL.0, None) }
            .map_err(|e| AvError::HwInitFailed(format!("fence CreateSharedHandle: {e}")))?;

        Ok(Arc::new(Self {
            device: device5,
            context: context4,
            fence,
            fence_handle,
            state: Mutex::new(PoolState {
                slots: Vec::new(),
                free: Vec::new(),
                dims: None,
                next_fence_value: 0,
            }),
        }))
    }

    /// Cria uma textura NV12 compartilhável (`SHARED_NTHANDLE`) e seu handle.
    fn create_slot(&self, w: u32, h: u32) -> Result<Slot, AvError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED.0) as u32,
        };
        let mut tex_opt: Option<ID3D11Texture2D> = None;
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut tex_opt))
                .map_err(|e| AvError::HwInitFailed(format!("CreateTexture2D shared NV12: {e}")))?;
        }
        let texture = tex_opt
            .ok_or_else(|| AvError::HwInitFailed("textura NV12 compartilhada nula".into()))?;
        let dxgi: IDXGIResource1 = texture
            .cast()
            .map_err(|e| AvError::HwInitFailed(format!("cast IDXGIResource1: {e}")))?;
        let handle = unsafe { dxgi.CreateSharedHandle(None, GENERIC_ALL.0, None) }
            .map_err(|e| AvError::HwInitFailed(format!("texture CreateSharedHandle: {e}")))?;
        let resource: ID3D11Resource = texture
            .cast()
            .map_err(|e| AvError::HwInitFailed(format!("cast textura→Resource: {e}")))?;
        Ok(Slot {
            texture,
            resource,
            handle,
        })
    }

    /// Copia o slice NV12 do `src` (pool D3D11VA) para um slot compartilhado,
    /// sinaliza a fence e devolve um [`SharedNvFrame`] zero-copy.
    ///
    /// `array_size` e `array_slice` localizam os subresources Y (slice) e UV
    /// (array_size + slice) na textura-array do decoder — mesmo esquema do
    /// `extract_nv12_planes`.
    pub fn acquire_copy(
        self: &Arc<Self>,
        src: &ID3D11Texture2D,
        array_slice: u32,
        array_size: u32,
        w: u32,
        h: u32,
    ) -> Result<SharedNvFrame, AvError> {
        let (slot_idx, fence_value) = {
            let mut state = self.state.lock().unwrap();

            // Mudança de resolução: descarta os slots livres com dims antigas.
            if state.dims != Some((w, h)) {
                for slot in state.slots.drain(..) {
                    unsafe {
                        let _ = CloseHandle(slot.handle);
                    }
                }
                state.free.clear();
                state.dims = Some((w, h));
            }

            // Pega um slot livre ou aloca um novo (até o teto).
            let idx = match state.free.pop() {
                Some(i) => i,
                None => {
                    if state.slots.len() >= MAX_SLOTS {
                        return Err(AvError::HwInitFailed(
                            "pool de texturas compartilhadas esgotado".into(),
                        ));
                    }
                    let slot = self.create_slot(w, h)?;
                    state.slots.push(slot);
                    state.slots.len() - 1
                }
            };

            state.next_fence_value += 1;
            (idx, state.next_fence_value)
        };

        // Copia Y (subresource 0) e UV (subresource 1) do array slice → slot.
        let src_res: ID3D11Resource = src
            .cast()
            .map_err(|e| AvError::HwInitFailed(format!("cast src→Resource: {e}")))?;
        let (dst_res, tex_handle) = {
            let state = self.state.lock().unwrap();
            let slot = &state.slots[slot_idx];
            (slot.resource.clone(), slot.handle)
        };
        unsafe {
            self.context.CopySubresourceRegion(
                &dst_res,
                0,
                0,
                0,
                0,
                &src_res,
                array_slice,
                None,
            );
            self.context.CopySubresourceRegion(
                &dst_res,
                1,
                0,
                0,
                0,
                &src_res,
                array_size + array_slice,
                None,
            );
            // Sinaliza a fence após a cópia e empurra o trabalho para a GPU,
            // tornando a textura visível ao consumidor D3D12.
            self.context
                .Signal(&self.fence, fence_value)
                .map_err(|e| AvError::HwInitFailed(format!("fence Signal: {e}")))?;
            self.context.Flush();
        }

        Ok(SharedNvFrame {
            pool: Arc::downgrade(self),
            slot: slot_idx,
            texture_handle: tex_handle.0 as isize,
            fence_handle: self.fence_handle.0 as isize,
            fence_value,
            width: w,
            height: h,
        })
    }

    /// Devolve um slot à free-list (chamado pelo Drop do `SharedNvFrame`).
    fn release(&self, slot: usize) {
        if let Ok(mut state) = self.state.lock() {
            // Só recicla se ainda pertence às dims atuais (senão já foi drenado).
            if slot < state.slots.len() {
                state.free.push(slot);
            }
        }
    }
}

/// Referência zero-copy a um frame NV12 numa textura D3D11 compartilhada.
///
/// Carrega os handles NT (textura + fence) e o valor de fence que o consumidor
/// (`renderer.rs`) usa para abrir a textura na GPU wgpu/DX12 e sincronizar.
/// O Drop devolve o slot ao pool.
pub struct SharedNvFrame {
    pool: Weak<SharedNvPool>,
    slot: usize,
    /// Handle NT da textura compartilhada (`HANDLE.0 as isize`).
    pub texture_handle: isize,
    /// Handle NT da fence compartilhada (`HANDLE.0 as isize`).
    pub fence_handle: isize,
    /// Valor de fence a aguardar antes de amostrar.
    pub fence_value: u64,
    pub width: u32,
    pub height: u32,
}

// SAFETY: handles são valores globais ao processo; `Weak<SharedNvPool>` é Send
// porque `SharedNvPool: Send + Sync`.
unsafe impl Send for SharedNvFrame {}

impl std::fmt::Debug for SharedNvFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedNvFrame")
            .field("slot", &self.slot)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fence_value", &self.fence_value)
            .finish()
    }
}

impl Drop for SharedNvFrame {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            pool.release(self.slot);
        }
    }
}
