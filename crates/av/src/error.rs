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

    /// Erro genérico encapsulado com contexto.
    ///
    /// SPEC-AV-002
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
