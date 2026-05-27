//! Tipos de erro do crate `av`.
//!
//! SPEC-AV-002

use thiserror::Error;

/// Erros retornados pelo pipeline A/V.
///
/// SPEC-AV-002
#[derive(Debug, Error)]
pub enum AvError {
    /// Codec de vídeo ou áudio não suportado pelo decodificador.
    ///
    /// SPEC-AV-002a
    #[error("codec não suportado: stream_type=0x{stream_type:02X}")]
    UnsupportedCodec { stream_type: u8 },

    /// PES packet inválido ou truncado.
    ///
    /// SPEC-AV-001
    #[error("PES packet inválido: {reason}")]
    InvalidPes { reason: &'static str },

    /// Erro interno do decodificador FFmpeg.
    ///
    /// SPEC-AV-002b
    #[error("erro FFmpeg: código {code}")]
    FfmpegError { code: i32 },

    /// DLL FFmpeg não encontrada ou versão incompatível.
    ///
    /// SPEC-AV-002
    #[error("FFmpeg indisponível: {message}")]
    FfmpegUnavailable { message: String },

    /// Canal bounded cheio; frame descartado.
    ///
    /// SPEC-AV-001
    #[error("canal cheio; frame descartado no PID {pid}")]
    ChannelFull { pid: u16 },

    /// Falha na inicialização do hardware hwaccel (D3D11VA).
    ///
    /// SPEC-AV-HW-001
    #[error("falha na inicialização de hwaccel: {0}")]
    HwInitFailed(String),

    /// O device D3D11/DXGI foi removido ou resetado pelo driver.
    ///
    /// Usado para distinguir TDR (`DXGI_ERROR_DEVICE_REMOVED` /
    /// `DXGI_ERROR_DEVICE_RESET`) de erros genéricos do caminho HW.
    #[error("device D3D11 removido/resetado: {0}")]
    HwDeviceRemoved(String),

    /// Erro genérico encapsulado com contexto.
    ///
    /// SPEC-AV-002
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl AvError {
    /// `true` quando o erro representa perda/reset do device D3D11/DXGI.
    pub fn is_device_removed(&self) -> bool {
        matches!(self, Self::HwDeviceRemoved(_))
    }
}

#[cfg(test)]
mod tests {
    use super::AvError;

    #[test]
    fn spec_av_hw_001_hw_device_removed_is_detected() {
        let err = AvError::HwDeviceRemoved("DXGI_ERROR_DEVICE_REMOVED".into());
        assert!(err.is_device_removed());
    }

    #[test]
    fn spec_av_hw_001_non_device_removed_error_is_not_detected() {
        let err = AvError::HwInitFailed("generic hw init failure".into());
        assert!(!err.is_device_removed());
    }
}
