//! Crate `av` — Bridge FFmpeg, Renderização e Áudio.
//!
//! SPEC-AV-001 · SPEC-AV-002 · SPEC-AV-003 · SPEC-AV-004
//!
//! # Responsabilidades
//!
//! - `pes`: remonta `PesPacket` a partir de fragmentos TS (`PesAssembler`, Task 2).
//! - `codec`: mapeia `stream_type` MPEG-TS para `VideoCodec` / `AudioCodec`.
//! - `decoder`: decodifica `PesPacket` → `DecodedFrame` via FFmpeg (Task 3).
//! - `renderer`: renderiza `VideoFrame` RGB24 em textura wgpu (Task 4).
//! - `audio`: reproduz `AudioFrame` PCM via WASAPI/cpal (Task 5).
//! - `ffi`: todo `unsafe` confinado aqui; zero FFI fora deste módulo.
//!
//! # Regra de segurança
//!
//! Todo `unsafe` **deve** residir exclusivamente em `av::ffi`.  Os módulos
//! `ts` e `net` permanecem zero-FFI.

pub mod audio;
pub mod codec;
pub mod decoder;
pub mod error;
pub mod ffi;
pub mod pes;
pub mod renderer;

// ── Re-exportações públicas ───────────────────────────────────────────────────

pub use audio::{AudioFrame, AudioOutput, AudioRingBuffer};
pub use codec::{AudioCodec, MediaCodec, VideoCodec};
pub use decoder::{DecodedFrame, FfmpegDecoder};
pub use error::AvError;
pub use pes::{PesAssembler, PesPacket};
pub use renderer::{VideoFrame, VideoRenderer};
