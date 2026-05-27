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
pub mod clock;
pub mod codec;
pub mod decoder;
pub(crate) mod deinterlace;
pub mod error;
pub mod ffi;
pub mod hw;
pub mod pes;
pub mod renderer;
pub mod video_queue;

// ── Re-exportações públicas ───────────────────────────────────────────────────

pub use audio::{AudioFrame, AudioOutput, AudioRingBuffer};
pub use clock::{AudioClockHandle, Clock, MasterClock, Pts90, WallClockHandle};
pub use codec::{AudioCodec, CodecConfig, MediaCodec, ThreadType, VideoCodec};
pub use decoder::{DecodedFrame, FfmpegDecoder};
pub use error::AvError;
pub use hw::{
    AdapterInfo, AdapterLuid, ColorSpace, D3d11Device, D3d11Texture, HwAccelMode, HwAccelState,
    HwPixelFormat, TdrState, TransferFunction, HW_FALLBACK_THRESHOLD, TDR_MAX_ATTEMPTS,
    TDR_RETRY_COOLDOWN,
};
pub use pes::{PesAssembler, PesPacket};
pub use renderer::VideoRenderer;
pub use video_queue::{
    PopResult, PushResult, VideoQueue, YuvColorRange, YuvColorspace, YuvFrame,
    DEFAULT_CAPACITY as VIDEO_QUEUE_CAPACITY, DROP_PTS, HOLD_PTS, RESYNC_PTS,
};
