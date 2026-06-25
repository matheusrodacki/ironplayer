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
pub(crate) use d3d11_impl::com_addref;
#[cfg(windows)]
pub use d3d11_impl::{
    AdapterLuid, ColorSpace, D3d11Device, D3d11Texture, HwPixelFormat, NvPlanes, TransferFunction,
};

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::{
    AdapterLuid, ColorSpace, D3d11Device, D3d11Texture, HwPixelFormat, NvPlanes, TransferFunction,
};

mod hwaccel_mode;
pub use hwaccel_mode::{HwAccelMode, HwAccelState, HW_FALLBACK_THRESHOLD};

mod tdr;
pub use tdr::{TdrState, TDR_MAX_ATTEMPTS, TDR_RETRY_COOLDOWN};

// ── AdapterInfo: snapshot legível do adapter D3D11 ativo ──────────────────────

/// Snapshot legível do adapter GPU em uso, para alimentar telemetria.
///
/// Disponível em todas as plataformas (em não-Windows, `name` é vazio e
/// `luid` é zero).  Consumido por [`apply_to_metrics`] para atualizar os
/// campos `gpu_adapter_*` em `PipelineMetrics`.
///
/// SPEC-METRICS-HW-001
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdapterInfo {
    /// Descrição do adapter (ex.: `"NVIDIA GeForce RTX 4060"`).
    pub name: String,
    /// LUID DXGI codificado como u64 (LowPart nos 32 bits baixos).
    pub luid: u64,
    /// PCI Vendor ID (ex.: `0x10de` NVIDIA, `0x8086` Intel, `0x1002` AMD).
    pub vendor_id: u32,
}

impl AdapterInfo {
    /// Constrói um `AdapterInfo` a partir do `D3d11Device` ativo.
    ///
    /// SPEC-METRICS-HW-001
    pub fn from_device(device: &D3d11Device) -> Self {
        Self {
            name: device.adapter_description().to_string(),
            luid: device.adapter_luid().as_u64(),
            vendor_id: device.vendor_id(),
        }
    }

    /// Aplica este snapshot aos campos `gpu_adapter_*` de `PipelineMetrics`.
    ///
    /// Mantém os demais campos inalterados (e.g. `hw_decode_active`,
    /// `tdr_recoveries`).
    ///
    /// SPEC-METRICS-HW-001
    pub fn apply_to_metrics(&self, metrics: &mut ts::metrics::PipelineMetrics) {
        metrics.gpu_adapter_name = if self.name.is_empty() {
            None
        } else {
            Some(self.name.clone())
        };
        metrics.gpu_adapter_luid = self.luid;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ts::metrics::PipelineMetrics;

    /// AdapterInfo::default produz nome vazio e LUID zero.
    ///
    /// SPEC-METRICS-HW-001
    #[test]
    fn spec_metrics_hw_001_adapter_info_default_is_empty() {
        let info = AdapterInfo::default();
        assert!(info.name.is_empty());
        assert_eq!(info.luid, 0);
        assert_eq!(info.vendor_id, 0);
    }

    /// apply_to_metrics popula gpu_adapter_name e gpu_adapter_luid.
    ///
    /// SPEC-METRICS-HW-001
    #[test]
    fn spec_metrics_hw_001_apply_to_metrics_populates_fields() {
        let info = AdapterInfo {
            name: "NVIDIA GeForce RTX 4060".into(),
            luid: 0x0000_0001_0000_1234,
            vendor_id: 0x10de,
        };
        let mut m = PipelineMetrics::default();
        info.apply_to_metrics(&mut m);
        assert_eq!(
            m.gpu_adapter_name.as_deref(),
            Some("NVIDIA GeForce RTX 4060")
        );
        assert_eq!(m.gpu_adapter_luid, 0x0000_0001_0000_1234);
    }

    /// Nome vazio resulta em None (sentinela para "sem adapter ativo").
    ///
    /// SPEC-METRICS-HW-001
    #[test]
    fn spec_metrics_hw_001_apply_to_metrics_empty_name_yields_none() {
        let info = AdapterInfo::default();
        let mut m = PipelineMetrics {
            gpu_adapter_name: Some("stale".into()),
            ..PipelineMetrics::default()
        };
        info.apply_to_metrics(&mut m);
        assert_eq!(m.gpu_adapter_name, None);
        assert_eq!(m.gpu_adapter_luid, 0);
    }
}
