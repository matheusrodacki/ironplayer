//! Parsers de tabelas PSI/SI e DVB para MPEG-TS.
//!
//! SPEC-TABLE-001 a SPEC-TABLE-008
//!
//! Todos os parsers são funções puras: recebem `&[u8]` e retornam
//! `Result<T, TableError>`. Zero estado, zero side-effects.

pub mod bat;
pub mod descriptor;
pub mod dvb_string;
pub mod eit;
pub mod nit;
pub mod pat;
pub mod pmt;
pub mod sdt;
pub mod tdt;

pub use bat::{Bat, BatTransportStream};
pub use descriptor::{Descriptor, KnownDescriptor};
pub use eit::{Eit, EitEvent};
pub use nit::{Nit, NitTransportStream};
pub use pat::{Pat, PatProgram};
pub use pmt::{stream_type_label, Pmt, PmtStream};
pub use sdt::{RunningStatus, Sdt, SdtService};
pub use tdt::Tdt;

use thiserror::Error;

// ── TableError ────────────────────────────────────────────────────────────────

/// Erros retornados pelos parsers de tabelas PSI/SI.
///
/// SPEC-TABLE-001 a SPEC-TABLE-008
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TableError {
    /// O slice fornecido é menor do que o mínimo esperado para a tabela.
    ///
    /// SPEC-TABLE-001
    #[error("dados insuficientes: esperado ao menos {expected} bytes, encontrado {found}")]
    InsufficientData { expected: usize, found: usize },

    /// O `table_id` da seção não corresponde ao parser invocado.
    ///
    /// SPEC-TABLE-001
    #[error("table_id inválido: esperado 0x{expected:02X}, encontrado 0x{found:02X}")]
    WrongTableId { expected: u8, found: u8 },

    /// O `table_id` está fora do conjunto de valores aceitos pelo parser.
    ///
    /// Usado por parsers que aceitam múltiplos table_ids (ex: NIT, SDT, EIT).
    #[error("table_id inválido: 0x{found:02X} não é aceito por este parser")]
    WrongTableIdMulti { found: u8 },

    /// O campo `section_length` da seção é inconsistente com o tamanho real do slice.
    ///
    /// SPEC-TABLE-001
    #[error("section_length inconsistente: declarado {declared}, disponível {available}")]
    InvalidSectionLength { declared: usize, available: usize },
}

// ── SectionParser ─────────────────────────────────────────────────────────────

/// Trait implementado por todos os parsers de tabelas PSI/SI.
///
/// `section_body` é o conteúdo **sem** os 3 bytes de cabeçalho da seção TS
/// e **sem** os 4 bytes de CRC-32 (já validados pelo `SectionAssembler`).
///
/// SPEC-TABLE-001
#[allow(dead_code)]
pub(crate) trait SectionParser: Sized {
    fn parse(section_body: &[u8]) -> Result<Self, TableError>;
}
