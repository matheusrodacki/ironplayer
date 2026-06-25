//! Erros de parsing de cabeçalhos de codec.
//!
//! SPEC-MI-001

use thiserror::Error;

/// Erro ao analisar cabeçalho de codec elementar.
///
/// SPEC-MI-001
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MediaInfoError {
    #[error("dados insuficientes: precisava {expected} bytes, encontrou {found}")]
    InsufficientData { expected: usize, found: usize },
    #[error("bitstream truncado")]
    TruncatedBitstream,
    #[error("sync word não encontrado")]
    SyncNotFound,
    #[error("NAL/reserved inválido")]
    InvalidNal,
    #[error("codec não suportado para probe")]
    UnsupportedCodec,
}
