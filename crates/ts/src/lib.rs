//! Crate `ts` — Demuxer e Parser MPEG-TS puro Rust.
//!
//! SPEC-TS-001 · SPEC-TS-002 · SPEC-TS-003 · SPEC-TS-004
//!
//! Zero FFI, zero `unsafe`. Rust 1.78 stable.

pub mod adaptation;
pub mod crc;
pub mod demux;
pub mod error;
pub mod packet;
pub mod pcr;
pub mod section;

pub use adaptation::{pcr_to_duration, AdaptationField};
pub use crc::{crc32_mpeg2, verify_crc32_mpeg2};
pub use demux::{PesData, SectionData, TsDemuxer};
pub use error::{DiscontinuityReason, PcrEvent, TsError, TsEvent};
pub use packet::TsPacket;
pub use pcr::PcrTracker;
pub use section::{CompleteSection, SectionAssembler};

/// Identificador de PID MPEG-TS (13 bits; faixa válida: 0x0000–0x1FFF).
///
/// SPEC-TS-001
pub type Pid = u16;
