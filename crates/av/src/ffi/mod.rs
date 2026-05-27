//! Módulo FFI — todo `unsafe` do crate `av` é confinado aqui.
//!
//! Carrega dinamicamente `avcodec-62.dll`, `avutil-60.dll` e `swscale-9.dll`
//! via `libloading`. Expõe tipos Rust seguros que encapsulam os ponteiros FFI.
//!
//! # Invariantes de segurança
//!
//! - Nenhum ponteiro FFI escapa deste módulo sem estar encapsulado em um tipo
//!   seguro com drop explícito.
//! - Todos os retornos de erro da libavcodec são convertidos em `AvError`
//!   antes de cruzar o limite deste módulo.
//! - Nenhum `unwrap()` ou `expect()` fora de contextos de teste.
//! - As Libraries são mantidas vivas enquanto existir algum `Arc<FfmpegLib>`.
//!
//! # Layout dos structs FFmpeg (FFmpeg 8.x / avutil-60, Windows x86-64)
//!
//! Os offsets foram derivados dos headers públicos do FFmpeg 8.0.  Campos
//! marcados `#[deprecated]` continuam presentes nas builds shared padrão.
//!
//! SPEC-AV-002b

use std::ffi::{c_int, c_void, CString};
use std::path::Path;
use std::sync::Arc;

use crate::codec::{CodecConfig, ThreadType};

use libloading::Library;

use crate::error::AvError;

// ─── Nomes das DLLs (Windows FFmpeg 8.x) ─────────────────────────────────────

#[cfg(windows)]
const DLL_AVUTIL: &str = "avutil-60.dll";
#[cfg(windows)]
const DLL_AVCODEC: &str = "avcodec-62.dll";
#[cfg(windows)]
const DLL_SWRESAMPLE: &str = "swresample-6.dll";
#[cfg(windows)]
const DLL_AVFILTER: &str = "avfilter-11.dll";

#[cfg(not(windows))]
const DLL_AVUTIL: &str = "libavutil.so.60";
#[cfg(not(windows))]
const DLL_AVCODEC: &str = "libavcodec.so.62";
#[cfg(not(windows))]
const DLL_SWRESAMPLE: &str = "libswresample.so.6";
#[cfg(not(windows))]
const DLL_AVFILTER: &str = "libavfilter.so.11";

// ─── Constantes FFmpeg ────────────────────────────────────────────────────────

/// IDs de codec AVCodecID para os codecs suportados.
///
/// SPEC-AV-002a
pub const AV_CODEC_ID_MPEG2VIDEO: u32 = 2;
pub const AV_CODEC_ID_H264: u32 = 27;
pub const AV_CODEC_ID_HEVC: u32 = 173;
pub const AV_CODEC_ID_MP2: u32 = 0x15000;
pub const AV_CODEC_ID_AAC: u32 = 0x15002;
pub const AV_CODEC_ID_AC3: u32 = 0x15003;
pub const AV_CODEC_ID_EAC3: u32 = 0x15028;
pub const AV_CODEC_ID_AAC_LATM: u32 = 0x15031;

/// Formatos de pixel YUV planar suportados pelo decoder.
pub const AV_PIX_FMT_YUV420P: c_int = 0;
pub const AV_PIX_FMT_YUV420P10LE: c_int = 63;

/// Formatos de sample de áudio.
pub const AV_SAMPLE_FMT_FLT: c_int = 3;
pub const AV_SAMPLE_FMT_S16P: c_int = 6;
pub const AV_SAMPLE_FMT_FLTP: c_int = 8;

/// Códigos de erro FFmpeg.
pub const AVERROR_EAGAIN: c_int = -11;
/// AVERROR_EOF = FFERRTAG(0xF8,'E','O','F') = -0x20464F45
pub const AVERROR_EOF: c_int = -541_478_725_i32;

/// `AV_FRAME_FLAG_INTERLACED` — frame tem campos entrelaçados (bit 0 de `AVFrame::flags`).
///
/// Introduzido em FFmpeg 6.x em substituição ao campo deprecated `interlaced_frame`.
pub const AV_FRAME_FLAG_INTERLACED: c_int = 1;

/// `AV_PIX_FMT_NV12` — semi-planar YCbCr 4:2:0 (plano Y + UV intercalado).
/// Formato nativo de saída do decoder D3D11VA.
pub const AV_PIX_FMT_NV12: c_int = 23;

/// `AV_PIX_FMT_D3D11` — frame em textura D3D11 (hardware surface).
/// Retornado por `avcodec_receive_frame` quando D3D11VA está ativo.
pub const AV_PIX_FMT_D3D11: c_int = 224;

/// `AV_HWDEVICE_TYPE_D3D11VA` — identificador do tipo de device hardware D3D11VA.
pub const AV_HWDEVICE_TYPE_D3D11VA: c_int = 7;

/// Offset do campo `hw_device_ctx` em `AVCodecContext` (FFmpeg 7.x / avcodec-62, x86-64).
///
/// Confirmado via sondagem runtime: `hwaccel_flags` (AVOption) está em offset +568,
/// portanto o ponteiro `AVBufferRef*` imediatamente anterior fica em +560.
pub(crate) const AV_CTX_HW_DEVICE_CTX_OFFSET: usize = 560;

// ─── Tipos opacos FFmpeg ──────────────────────────────────────────────────────

/// Tipo opaco para `AVCodec*`.
#[repr(C)]
pub struct AvCodec {
    _opaque: [u8; 0],
}

/// Tipo opaco para `AVCodecContext*`.
#[repr(C)]
pub struct AvCodecContext {
    _opaque: [u8; 0],
}

/// Tipo opaco para `SwrContext*`.
#[repr(C)]
pub struct SwrContext {
    _opaque: [u8; 0],
}

/// Tipo opaco para `AVDictionary*`.
#[repr(C)]
pub struct AvDictionary {
    _opaque: [u8; 0],
}

/// Layout público de `AVChannelLayout` em FFmpeg 8.x.
#[repr(C)]
pub struct AvChannelLayout {
    pub order: c_int,
    pub nb_channels: c_int,
    pub channels: AvChannelLayoutChannels,
    pub opaque: *mut c_void,
}

/// União pública de `AVChannelLayout`.
#[repr(C)]
pub union AvChannelLayoutChannels {
    pub mask: u64,
    pub custom_channel: *mut c_void,
}

// ─── AVPacket (layout estável desde FFmpeg 4.x, offsets x86-64) ──────────────
//
// Campo             Offset  Tamanho
// buf (*AVBufferRef)   0       8
// pts (int64_t)        8       8
// dts (int64_t)       16       8
// data (*uint8_t)     24       8
// size (int)          32       4
// stream_index (int)  36       4
// flags (int)         40       4
// _pad                44       4
// side_data ptr       48       8  (não precisamos)
// ...

/// Layout do `AVPacket` na ABI do FFmpeg 8.x (x86-64).
///
/// Apenas os campos que precisamos ler/escrever são declarados.
/// Os campos além de `flags` não são acessados diretamente.
#[repr(C)]
pub struct AvPacket {
    buf: *mut c_void,    // 0: AVBufferRef*
    pub pts: i64,        // 8
    pub dts: i64,        // 16
    pub data: *mut u8,   // 24
    pub size: c_int,     // 32
    stream_index: c_int, // 36
    flags: c_int,        // 40
}

// ─── AVFrame helper ───────────────────────────────────────────────────────────
//
// Acessamos campos do AVFrame via funções auxiliares que usam offsets de byte
// explícitos. O layout abaixo é válido para FFmpeg 8.x (avutil-60) em x86-64
// com fields deprecated incluídos (builds shared padrão).
//
// Offset  Campo
//   0..64 data[8]  (*uint8_t por ponteiro)
//  64..96 linesize[8] (int)
//  96     extended_data (**uint8_t)
// 104     width (int)
// 108     height (int)
// 112     nb_samples (int)
// 116     format (int)
// 120     key_frame (int) [deprecated, presente em FFmpeg 8]
// 124     pict_type (enum, int)
// 128     sample_aspect_ratio (AVRational = 2×int)
// 136     pts (int64_t)
// 144     pkt_dts (int64_t) [deprecated, presente em FFmpeg 8]
// 152     time_base (AVRational)
// 160     coded_picture_number (int) [deprecated]
// 164     display_picture_number (int) [deprecated]
// 168     quality (int)
// 172     _pad (int)
// 176     opaque (*void)
// 168     opaque (*void)
// 176     repeat_pict (int)
// 180     sample_rate (int)
// 184     buf[8] (*AVBufferRef)
// 384     ch_layout (AVChannelLayout)
// 388     ch_layout.nb_channels (int)

/// Lê `data[i]` de um `AVFrame*` opaco.
///
/// SAFETY: `frame` deve ser um ponteiro válido para `AVFrame`.
#[inline]
pub(crate) unsafe fn frame_data_ptr(frame: *mut c_void, i: usize) -> *mut u8 {
    debug_assert!(i < 8, "data index out of bounds");
    let base = frame as *const *mut u8;
    // SAFETY: data é o primeiro campo, array de 8 ponteiros.
    *base.add(i)
}

/// Lê `linesize[i]` de um `AVFrame*` opaco.
///
/// SAFETY: `frame` deve ser um ponteiro válido para `AVFrame`.
#[inline]
pub(crate) unsafe fn frame_linesize(frame: *mut c_void, i: usize) -> c_int {
    debug_assert!(i < 8, "linesize index out of bounds");
    let base = (frame as *const u8).add(64) as *const c_int;
    // SAFETY: linesize é o segundo campo, array de 8 ints, offset=64.
    *base.add(i)
}

/// Lê `width` de um `AVFrame*` opaco (offset 104).
///
/// SAFETY: `frame` deve ser um ponteiro válido para `AVFrame`.
#[inline]
pub(crate) unsafe fn frame_width(frame: *mut c_void) -> c_int {
    *((frame as *const u8).add(104) as *const c_int)
}

/// Lê `height` de um `AVFrame*` opaco (offset 108).
///
/// SAFETY: `frame` deve ser um ponteiro válido para `AVFrame`.
#[inline]
pub(crate) unsafe fn frame_height(frame: *mut c_void) -> c_int {
    *((frame as *const u8).add(108) as *const c_int)
}

/// Lê `nb_samples` de um `AVFrame*` opaco (offset 112).
///
/// SAFETY: `frame` deve ser um ponteiro válido para `AVFrame`.
#[inline]
pub(crate) unsafe fn frame_nb_samples(frame: *mut c_void) -> c_int {
    *((frame as *const u8).add(112) as *const c_int)
}

/// Lê `format` de um `AVFrame*` opaco (offset 116).
///
/// SAFETY: `frame` deve ser um ponteiro válido para `AVFrame`.
#[inline]
pub(crate) unsafe fn frame_format(frame: *mut c_void) -> c_int {
    *((frame as *const u8).add(116) as *const c_int)
}

/// Lê `pts` de um `AVFrame*` opaco (offset 136).
///
/// SAFETY: `frame` deve ser um ponteiro válido para `AVFrame`.
#[inline]
pub(crate) unsafe fn frame_pts(frame: *mut c_void) -> i64 {
    *((frame as *const u8).add(136) as *const i64)
}

/// Lê `sample_aspect_ratio` de um `AVFrame*` opaco (offset 128).
///
/// Retorna `(num, den)`.  Quando `den == 0` ou `num <= 0`, o SAR não está
/// definido; o chamador deve tratar como pixels quadrados (1:1).
///
/// SAFETY: `frame` deve ser um ponteiro válido para `AVFrame` FFmpeg 8.x.
#[inline]
pub(crate) unsafe fn frame_sar(frame: *mut c_void) -> (i32, i32) {
    let num = *((frame as *const u8).add(128) as *const i32);
    let den = *((frame as *const u8).add(132) as *const i32);
    (num, den)
}

/// Lê `sample_rate` de um `AVFrame*` opaco (offset 180).
///
/// SAFETY: `frame` deve ser um ponteiro válido para `AVFrame` FFmpeg 8.x.
#[inline]
pub(crate) unsafe fn frame_sample_rate(frame: *mut c_void) -> c_int {
    *((frame as *const u8).add(180) as *const c_int)
}

/// Lê `ch_layout.nb_channels` de um `AVFrame*` opaco (offset 388).
///
/// SAFETY: `frame` deve ser um ponteiro válido para `AVFrame` FFmpeg 8.x.
#[inline]
pub(crate) unsafe fn frame_channel_count(frame: *mut c_void) -> c_int {
    *((frame as *const u8).add(388) as *const c_int)
}

/// Lê `color_range` de um `AVFrame*` opaco (offset 280).
///
/// Valores: `0` = não especificado, `1` = MPEG/TV range (16..235),
/// `2` = JPEG/full range (0..255).
///
/// Offset derivado do layout x86-64 de FFmpeg 8.0 (avutil-60):
/// após `buf[8]`(184..248) + `extended_buf`(8) + `nb_extended_buf+pad`(8)
/// + `side_data`(8) + `nb_side_data+flags`(8) = offset 280.
///
/// SAFETY: `frame` deve ser um ponteiro válido para `AVFrame` FFmpeg 8.x.
#[inline]
pub(crate) unsafe fn frame_color_range(frame: *mut c_void) -> i32 {
    *((frame as *const u8).add(280) as *const i32)
}

/// Lê `colorspace` de um `AVFrame*` opaco (offset 292).
///
/// Valores relevantes: `1` = BT.709, `2` = não especificado,
/// `5`/`6` = BT.601, `9` = BT.2020 NCL.
///
/// Offset derivado do layout x86-64 de FFmpeg 8.0 (avutil-60):
/// `color_range`(280, 4) + `color_primaries`(284, 4) + `color_trc`(288, 4)
/// → `colorspace` em 292.
///
/// SAFETY: `frame` deve ser um ponteiro válido para `AVFrame` FFmpeg 8.x.
#[inline]
pub(crate) unsafe fn frame_colorspace(frame: *mut c_void) -> i32 {
    *((frame as *const u8).add(292) as *const i32)
}

/// Lê `color_trc` de um `AVFrame*` opaco (offset 288).
///
/// Valores relevantes: `1` = SDR (BT.1886), `16` = SMPTE ST 2084 (PQ),
/// `18` = ARIB STD-B67 (HLG).
///
/// SAFETY: `frame` deve ser um ponteiro válido para `AVFrame` FFmpeg 8.x.
#[inline]
pub(crate) unsafe fn frame_color_trc(frame: *mut c_void) -> i32 {
    *((frame as *const u8).add(288) as *const i32)
}

/// Lê `flags` de um `AVFrame*` opaco (offset 276).
///
/// Contém flags como `AV_FRAME_FLAG_INTERLACED (1 << 0)` e
/// `AV_FRAME_FLAG_TOP_FIELD_FIRST (1 << 1)`.
///
/// Offset derivado do layout x86-64 de FFmpeg 8.0 (avutil-60):
/// `nb_side_data`(272, 4) + `flags`(276, 4).
///
/// SAFETY: `frame` deve ser um ponteiro válido para `AVFrame` FFmpeg 8.x.
#[inline]
pub(crate) unsafe fn frame_flags(frame: *mut c_void) -> c_int {
    *((frame as *const u8).add(276) as *const c_int)
}

// ─── Tipos de ponteiro de função ──────────────────────────────────────────────

type FnAvcodecFindDecoder = unsafe extern "C" fn(id: u32) -> *mut AvCodec;
type FnAvcodecAllocContext3 = unsafe extern "C" fn(codec: *const AvCodec) -> *mut AvCodecContext;
type FnAvcodecFreeContext = unsafe extern "C" fn(avctx: *mut *mut AvCodecContext);
type FnAvcodecOpen2 = unsafe extern "C" fn(
    avctx: *mut AvCodecContext,
    codec: *const AvCodec,
    options: *mut *mut c_void,
) -> c_int;
type FnAvcodecSendPacket =
    unsafe extern "C" fn(avctx: *mut AvCodecContext, avpkt: *const AvPacket) -> c_int;
type FnAvcodecReceiveFrame =
    unsafe extern "C" fn(avctx: *mut AvCodecContext, frame: *mut c_void) -> c_int;

type FnAvPacketAlloc = unsafe extern "C" fn() -> *mut AvPacket;
type FnAvPacketFree = unsafe extern "C" fn(pkt: *mut *mut AvPacket);
type FnAvNewPacket = unsafe extern "C" fn(pkt: *mut AvPacket, size: c_int) -> c_int;
type FnAvFrameAlloc = unsafe extern "C" fn() -> *mut c_void;
type FnAvFrameFree = unsafe extern "C" fn(frame: *mut *mut c_void);
type FnAvFrameUnref = unsafe extern "C" fn(frame: *mut c_void);
type FnAvChannelLayoutDefault =
    unsafe extern "C" fn(layout: *mut AvChannelLayout, nb_channels: c_int);
type FnAvChannelLayoutUninit = unsafe extern "C" fn(channel_layout: *mut AvChannelLayout);
type FnAvStrerror =
    unsafe extern "C" fn(errnum: c_int, errbuf: *mut i8, errbuf_size: usize) -> c_int;

type FnSwrAllocSetOpts2 = unsafe extern "C" fn(
    swr_ctx: *mut *mut SwrContext,
    out_ch_layout: *const AvChannelLayout,
    out_sample_fmt: c_int,
    out_sample_rate: c_int,
    in_ch_layout: *const AvChannelLayout,
    in_sample_fmt: c_int,
    in_sample_rate: c_int,
    log_offset: c_int,
    log_ctx: *mut c_void,
) -> c_int;
type FnSwrInit = unsafe extern "C" fn(swr_ctx: *mut SwrContext) -> c_int;
type FnSwrGetOutSamples =
    unsafe extern "C" fn(swr_ctx: *mut SwrContext, in_samples: c_int) -> c_int;
type FnSwrConvert = unsafe extern "C" fn(
    swr_ctx: *mut SwrContext,
    out: *mut *mut u8,
    out_count: c_int,
    input: *const *const u8,
    in_count: c_int,
) -> c_int;
type FnSwrFree = unsafe extern "C" fn(swr_ctx: *mut *mut SwrContext);

type FnAvDictSet = unsafe extern "C" fn(
    pm: *mut *mut AvDictionary,
    key: *const i8,
    value: *const i8,
    flags: c_int,
) -> c_int;

type FnAvDictFree = unsafe extern "C" fn(m: *mut *mut AvDictionary);

// ─── Tipos de função para hardware acceleration ────────────────────────────────

/// `av_hwdevice_ctx_create` — cria um `AVBufferRef*` wrapping um `AVHWDeviceContext`.
type FnAvHwdeviceCtxCreate = unsafe extern "C" fn(
    device_ctx: *mut *mut c_void,
    hw_type: c_int,
    device: *const std::ffi::c_char,
    opts: *mut c_void,
    flags: c_int,
) -> c_int;

/// `av_hwframe_transfer_data` — copia dados de um frame HW para um frame SW.
type FnAvHwframeTransferData =
    unsafe extern "C" fn(dst: *mut c_void, src: *const c_void, flags: c_int) -> c_int;

/// `av_buffer_ref` — incrementa refcount e retorna novo `AVBufferRef*`.
type FnAvBufferRef = unsafe extern "C" fn(buf: *mut c_void) -> *mut c_void;

/// `av_buffer_unref` — decrementa refcount; libera se chegar a zero.
type FnAvBufferUnref = unsafe extern "C" fn(buf: *mut *mut c_void);

// ─── FfmpegLib ────────────────────────────────────────────────────────────────

/// Bibliotecas FFmpeg carregadas e ponteiros de função resolvidos.
///
/// SAFETY: Os ponteiros de função são válidos enquanto as `Library` estiverem
/// vivas. A struct mantém as `Library` em campo, garantindo o invariante.
///
/// SPEC-AV-002b
#[allow(dead_code)]
pub struct FfmpegLib {
    // Libraries mantidas vivas para garantir validade dos fn pointers.
    _avutil: Library,
    _avcodec: Library,
    _swresample: Library,

    // Funções avcodec
    pub(crate) avcodec_find_decoder: FnAvcodecFindDecoder,
    pub(crate) avcodec_alloc_context3: FnAvcodecAllocContext3,
    pub(crate) avcodec_free_context: FnAvcodecFreeContext,
    pub(crate) avcodec_open2: FnAvcodecOpen2,
    pub(crate) avcodec_send_packet: FnAvcodecSendPacket,
    pub(crate) avcodec_receive_frame: FnAvcodecReceiveFrame,

    // Funções avutil
    pub(crate) av_packet_alloc: FnAvPacketAlloc,
    pub(crate) av_packet_free: FnAvPacketFree,
    pub(crate) av_new_packet: FnAvNewPacket,
    pub(crate) av_frame_alloc: FnAvFrameAlloc,
    pub(crate) av_frame_free: FnAvFrameFree,
    pub(crate) av_frame_unref: FnAvFrameUnref,
    pub(crate) av_channel_layout_default: FnAvChannelLayoutDefault,
    pub(crate) av_channel_layout_uninit: FnAvChannelLayoutUninit,
    pub(crate) av_strerror: FnAvStrerror,

    // Funções swresample
    pub(crate) swr_alloc_set_opts2: FnSwrAllocSetOpts2,
    pub(crate) swr_init: FnSwrInit,
    pub(crate) swr_get_out_samples: FnSwrGetOutSamples,
    pub(crate) swr_convert: FnSwrConvert,
    pub(crate) swr_free: FnSwrFree,

    // Funções de dicionário avutil (usadas para opções de codec)
    pub(crate) av_dict_set: FnAvDictSet,
    pub(crate) av_dict_free: FnAvDictFree,

    // Funções hw-accel (D3D11VA, SPEC-AV-HW-DEC-001)
    pub(crate) av_hwdevice_ctx_create: FnAvHwdeviceCtxCreate,
    pub(crate) av_hwframe_transfer_data: FnAvHwframeTransferData,
    pub(crate) av_buffer_ref: FnAvBufferRef,
    pub(crate) av_buffer_unref: FnAvBufferUnref,
}

// SAFETY: Os ponteiros de função são obtidos de DLLs thread-safe do FFmpeg.
// FFmpeg garante que suas funções são thread-safe para contextos distintos.
unsafe impl Send for FfmpegLib {}
unsafe impl Sync for FfmpegLib {}

impl FfmpegLib {
    /// Carrega as DLLs FFmpeg a partir de `dll_dir` e resolve todos os
    /// símbolos necessários.
    ///
    /// No Windows, configura temporariamente o diretório de busca de DLLs
    /// para que as dependências transitivas (avutil, swresample etc.) sejam
    /// encontradas na mesma pasta.
    ///
    /// SPEC-AV-002b
    pub fn load(dll_dir: &Path) -> Result<Arc<Self>, AvError> {
        #[cfg(windows)]
        set_dll_search_dir(Some(dll_dir));

        let result = Self::load_inner(dll_dir);

        #[cfg(windows)]
        set_dll_search_dir(None);

        result
    }

    fn load_inner(dll_dir: &Path) -> Result<Arc<Self>, AvError> {
        // Carrega avutil primeiro (sem dependências externas)
        let avutil = unsafe { Library::new(dll_dir.join(DLL_AVUTIL)) }.map_err(|e| {
            AvError::FfmpegUnavailable {
                message: format!("falha ao carregar {DLL_AVUTIL}: {e}"),
            }
        })?;

        // Carrega avcodec (depende de avutil)
        let avcodec = unsafe { Library::new(dll_dir.join(DLL_AVCODEC)) }.map_err(|e| {
            AvError::FfmpegUnavailable {
                message: format!("falha ao carregar {DLL_AVCODEC}: {e}"),
            }
        })?;

        // Carrega swresample (depende de avutil)
        let swresample = unsafe { Library::new(dll_dir.join(DLL_SWRESAMPLE)) }.map_err(|e| {
            AvError::FfmpegUnavailable {
                message: format!("falha ao carregar {DLL_SWRESAMPLE}: {e}"),
            }
        })?;

        // Resolve símbolos — cada `*sym` extrai o fn pointer cru do Symbol,
        // que é válido enquanto a Library estiver viva (invariante da struct).
        macro_rules! sym {
            ($lib:expr, $name:literal, $ty:ty) => {{
                let s: libloading::Symbol<$ty> =
                    unsafe { $lib.get($name) }.map_err(|e| AvError::FfmpegUnavailable {
                        message: format!(
                            "símbolo '{}' não encontrado: {e}",
                            std::str::from_utf8(&$name[..$name.len() - 1]).unwrap_or("<invalid>")
                        ),
                    })?;
                *s
            }};
        }

        let avcodec_find_decoder = sym!(avcodec, b"avcodec_find_decoder\0", FnAvcodecFindDecoder);
        let avcodec_alloc_context3 =
            sym!(avcodec, b"avcodec_alloc_context3\0", FnAvcodecAllocContext3);
        let avcodec_free_context = sym!(avcodec, b"avcodec_free_context\0", FnAvcodecFreeContext);
        let avcodec_open2 = sym!(avcodec, b"avcodec_open2\0", FnAvcodecOpen2);
        let avcodec_send_packet = sym!(avcodec, b"avcodec_send_packet\0", FnAvcodecSendPacket);
        let avcodec_receive_frame =
            sym!(avcodec, b"avcodec_receive_frame\0", FnAvcodecReceiveFrame);

        let av_packet_alloc = sym!(avcodec, b"av_packet_alloc\0", FnAvPacketAlloc);
        let av_packet_free = sym!(avcodec, b"av_packet_free\0", FnAvPacketFree);
        let av_new_packet = sym!(avcodec, b"av_new_packet\0", FnAvNewPacket);
        let av_frame_alloc = sym!(avutil, b"av_frame_alloc\0", FnAvFrameAlloc);
        let av_frame_free = sym!(avutil, b"av_frame_free\0", FnAvFrameFree);
        let av_frame_unref = sym!(avutil, b"av_frame_unref\0", FnAvFrameUnref);
        let av_channel_layout_default = sym!(
            avutil,
            b"av_channel_layout_default\0",
            FnAvChannelLayoutDefault
        );
        let av_channel_layout_uninit = sym!(
            avutil,
            b"av_channel_layout_uninit\0",
            FnAvChannelLayoutUninit
        );
        let av_strerror = sym!(avutil, b"av_strerror\0", FnAvStrerror);

        let swr_alloc_set_opts2 = sym!(swresample, b"swr_alloc_set_opts2\0", FnSwrAllocSetOpts2);
        let swr_init = sym!(swresample, b"swr_init\0", FnSwrInit);
        let swr_get_out_samples = sym!(swresample, b"swr_get_out_samples\0", FnSwrGetOutSamples);
        let swr_convert = sym!(swresample, b"swr_convert\0", FnSwrConvert);
        let swr_free = sym!(swresample, b"swr_free\0", FnSwrFree);

        let av_dict_set = sym!(avutil, b"av_dict_set\0", FnAvDictSet);
        let av_dict_free = sym!(avutil, b"av_dict_free\0", FnAvDictFree);

        let av_hwdevice_ctx_create =
            sym!(avutil, b"av_hwdevice_ctx_create\0", FnAvHwdeviceCtxCreate);
        let av_hwframe_transfer_data = sym!(
            avutil,
            b"av_hwframe_transfer_data\0",
            FnAvHwframeTransferData
        );
        let av_buffer_ref = sym!(avutil, b"av_buffer_ref\0", FnAvBufferRef);
        let av_buffer_unref = sym!(avutil, b"av_buffer_unref\0", FnAvBufferUnref);

        Ok(Arc::new(Self {
            _avutil: avutil,
            _avcodec: avcodec,
            _swresample: swresample,
            avcodec_find_decoder,
            avcodec_alloc_context3,
            avcodec_free_context,
            avcodec_open2,
            avcodec_send_packet,
            avcodec_receive_frame,
            av_packet_alloc,
            av_packet_free,
            av_new_packet,
            av_frame_alloc,
            av_frame_free,
            av_frame_unref,
            av_channel_layout_default,
            av_channel_layout_uninit,
            av_strerror,
            swr_alloc_set_opts2,
            swr_init,
            swr_get_out_samples,
            swr_convert,
            swr_free,
            av_dict_set,
            av_dict_free,
            av_hwdevice_ctx_create,
            av_hwframe_transfer_data,
            av_buffer_ref,
            av_buffer_unref,
        }))
    }

    /// Converte um código de erro FFmpeg em string legível via `av_strerror`.
    ///
    /// SPEC-AV-002b
    #[allow(dead_code)]
    pub(crate) fn strerror(&self, code: c_int) -> String {
        let mut buf = [0i8; 256];
        // SAFETY: buf é válido, tamanho correto, código de erro é um i32.
        let ret = unsafe { (self.av_strerror)(code, buf.as_mut_ptr(), buf.len()) };
        if ret == 0 {
            // SAFETY: av_strerror garante nul-terminação dentro de buf.
            unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        } else {
            format!("código {code}")
        }
    }
}

// ─── RAII: FfmpegCodecContext ─────────────────────────────────────────────────

/// Wrapper RAII para `AVCodecContext*`.
///
/// O contexto é liberado via `avcodec_free_context` ao ser dropado.
///
/// SPEC-AV-002b
pub struct FfmpegCodecContext {
    /// Ponteiro opaco para `AVCodecContext`.
    ctx: *mut AvCodecContext,
    /// Mantém a biblioteca viva enquanto este contexto existir.
    lib: Arc<FfmpegLib>,
}

// SAFETY: `AVCodecContext` não é `Send` por si só, mas nós garantimos uso
// exclusivo em uma única thread (o decoder possui o contexto).
unsafe impl Send for FfmpegCodecContext {}

impl FfmpegCodecContext {
    /// Abre um decodificador FFmpeg para o `codec_id` especificado.
    ///
    /// Constrói um `AVDictionary` com as opções de `config` (threads,
    /// thread_type, skip_loop_filter, flag2_fast) e o passa como terceiro
    /// argumento de `avcodec_open2`. O dicionário é liberado após a abertura.
    ///
    /// SPEC-AV-002b
    pub fn open(lib: Arc<FfmpegLib>, codec_id: u32, config: &CodecConfig) -> Result<Self, AvError> {
        // SAFETY: avcodec_find_decoder é thread-safe e retorna um ponteiro
        // estático (não precisamos liberar).
        let codec = unsafe { (lib.avcodec_find_decoder)(codec_id) };
        if codec.is_null() {
            return Err(AvError::FfmpegUnavailable {
                message: format!("codec id={codec_id} não encontrado no FFmpeg"),
            });
        }

        // SAFETY: avcodec_alloc_context3 aloca com av_malloc; codec é válido.
        let ctx = unsafe { (lib.avcodec_alloc_context3)(codec) };
        if ctx.is_null() {
            return Err(AvError::FfmpegError { code: -12 }); // ENOMEM
        }

        // Monta AVDictionary com as opções de codec.
        // av_dict_set copia key e value internamente (flags=0), portanto as
        // CStrings podem ser descartadas logo após a chamada.
        //
        // SAFETY: ctx é não-nulo; todas as CStrings são nul-terminadas e válidas
        // durante a chamada; av_dict_set documenta que copia as strings.
        let mut opts: *mut AvDictionary = std::ptr::null_mut();

        if config.thread_count > 0 {
            // SAFETY: thread_count é sempre um número ASCII válido.
            let count_cstr =
                CString::new(config.thread_count.to_string()).expect("thread_count ASCII válido");
            unsafe {
                (lib.av_dict_set)(&mut opts, c"threads".as_ptr(), count_cstr.as_ptr(), 0);
            }
        }

        match config.thread_type {
            ThreadType::Frame => unsafe {
                (lib.av_dict_set)(&mut opts, c"thread_type".as_ptr(), c"frame".as_ptr(), 0);
            },
            ThreadType::Slice => unsafe {
                (lib.av_dict_set)(&mut opts, c"thread_type".as_ptr(), c"slice".as_ptr(), 0);
            },
            // Auto: deixa o FFmpeg escolher a estratégia ideal para o codec.
            ThreadType::Auto => {}
        }

        if config.skip_loop_filter {
            unsafe {
                (lib.av_dict_set)(
                    &mut opts,
                    c"skip_loop_filter".as_ptr(),
                    c"noref".as_ptr(),
                    0,
                );
            }
        }

        if config.flag2_fast {
            unsafe {
                (lib.av_dict_set)(&mut opts, c"flags2".as_ptr(), c"+fast".as_ptr(), 0);
            }
        }

        // SAFETY: avcodec_open2 configura o contexto com o codec e as opções.
        // O dicionário é sempre liberado após a chamada, independente do resultado.
        let ret = unsafe {
            (lib.avcodec_open2)(
                ctx,
                codec,
                &mut opts as *mut *mut AvDictionary as *mut *mut c_void,
            )
        };

        // Libera entradas restantes do dicionário (opções não consumidas pelo codec).
        // SAFETY: opts pode ser nulo (av_dict_free aceita *mut NULL).
        if !opts.is_null() {
            unsafe { (lib.av_dict_free)(&mut opts) };
        }

        if ret < 0 {
            // Libera o contexto antes de retornar erro.
            // SAFETY: ctx não-nulo, avcodec_free_context é o destrutor correto.
            let mut p = ctx;
            unsafe { (lib.avcodec_free_context)(&mut p) };
            return Err(AvError::FfmpegError { code: ret });
        }

        tracing::debug!(
            codec_id,
            threads = config.thread_count,
            skip_loop_filter = config.skip_loop_filter,
            flag2_fast = config.flag2_fast,
            "decodificador FFmpeg aberto"
        );
        Ok(Self { ctx, lib })
    }

    /// Abre um decodificador FFmpeg com aceleração de hardware D3D11VA.
    ///
    /// Cria um `AVHWDeviceContext` via `av_hwdevice_ctx_create` (o FFmpeg cria
    /// internamente um `ID3D11Device` próprio) e o escreve no campo
    /// `hw_device_ctx` de `AVCodecContext` (offset +560, verificado via sondagem
    /// em tempo de execução) antes de chamar `avcodec_open2`.
    ///
    /// O `avcodec_default_get_format` padrão do FFmpeg seleciona automaticamente
    /// `AV_PIX_FMT_D3D11` quando `hw_device_ctx` está preenchido — não é
    /// necessário registrar um callback `get_format` customizado.
    ///
    /// Retorna `Err` se o device HW não puder ser criado ou o codec falhar ao
    /// abrir com o device — o caller deve fazer fallback para `open()` (SW).
    ///
    /// SPEC-AV-HW-DEC-001
    pub fn open_with_hwaccel(
        lib: Arc<FfmpegLib>,
        codec_id: u32,
        config: &CodecConfig,
        hw_type: c_int,
    ) -> Result<Self, AvError> {
        // SAFETY: avcodec_find_decoder é thread-safe e retorna ponteiro estático.
        let codec = unsafe { (lib.avcodec_find_decoder)(codec_id) };
        if codec.is_null() {
            return Err(AvError::FfmpegUnavailable {
                message: format!("codec id={codec_id} não encontrado para hwaccel"),
            });
        }

        // SAFETY: avcodec_alloc_context3 aloca com av_malloc; codec é válido.
        let ctx = unsafe { (lib.avcodec_alloc_context3)(codec) };
        if ctx.is_null() {
            return Err(AvError::FfmpegError { code: -12 }); // ENOMEM
        }

        // Cria AVHWDeviceContext. O FFmpeg cria um ID3D11Device interno para o tipo
        // solicitado. No Windows, hw_type=7 (AV_HWDEVICE_TYPE_D3D11VA).
        let mut hw_ctx: *mut c_void = std::ptr::null_mut();
        let hw_ret = unsafe {
            (lib.av_hwdevice_ctx_create)(
                &mut hw_ctx,
                hw_type,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            )
        };
        if hw_ret < 0 {
            let mut p = ctx;
            unsafe { (lib.avcodec_free_context)(&mut p as *mut *mut AvCodecContext) };
            return Err(AvError::FfmpegError { code: hw_ret });
        }

        // Cria uma referência adicional ao hw_ctx para transferir ao AVCodecContext.
        // O FFmpeg AddRef-a internamente durante avcodec_open2; liberamos nossa cópia
        // original logo após a abertura. hw_ref passa a ser propriedade do contexto.
        let hw_ref = unsafe { (lib.av_buffer_ref)(hw_ctx) };
        if hw_ref.is_null() {
            let mut hw = hw_ctx;
            unsafe { (lib.av_buffer_unref)(&mut hw) };
            let mut p = ctx;
            unsafe { (lib.avcodec_free_context)(&mut p as *mut *mut AvCodecContext) };
            return Err(AvError::FfmpegError { code: -12 });
        }

        // Escreve hw_ref no campo hw_device_ctx do AVCodecContext (offset +560).
        // SAFETY: ctx é não-nulo e alinhado a 8 bytes; offset 560 é múltiplo de 8;
        //         hw_ref é um AVBufferRef* não-nulo.
        unsafe {
            std::ptr::write_unaligned(
                (ctx as *mut u8).add(AV_CTX_HW_DEVICE_CTX_OFFSET) as *mut *mut c_void,
                hw_ref,
            );
        }

        // Libera nossa referência original (hw_ctx). A cópia hw_ref já foi
        // transferida para o contexto — o FFmpeg a mantém viva enquanto o
        // AVCodecContext existir e a libera via avcodec_free_context.
        let mut hw = hw_ctx;
        unsafe { (lib.av_buffer_unref)(&mut hw) };

        // Monta AVDictionary com as opções de codec (mesma lógica de open()).
        let mut opts: *mut AvDictionary = std::ptr::null_mut();

        if config.thread_count > 0 {
            let count_cstr =
                CString::new(config.thread_count.to_string()).expect("thread_count ASCII");
            unsafe {
                (lib.av_dict_set)(&mut opts, c"threads".as_ptr(), count_cstr.as_ptr(), 0);
            }
        }
        match config.thread_type {
            ThreadType::Frame => unsafe {
                (lib.av_dict_set)(&mut opts, c"thread_type".as_ptr(), c"frame".as_ptr(), 0);
            },
            ThreadType::Slice => unsafe {
                (lib.av_dict_set)(&mut opts, c"thread_type".as_ptr(), c"slice".as_ptr(), 0);
            },
            ThreadType::Auto => {}
        }
        if config.skip_loop_filter {
            unsafe {
                (lib.av_dict_set)(
                    &mut opts,
                    c"skip_loop_filter".as_ptr(),
                    c"noref".as_ptr(),
                    0,
                );
            }
        }

        // SAFETY: avcodec_open2 usa o hw_device_ctx já configurado no contexto.
        let ret = unsafe {
            (lib.avcodec_open2)(
                ctx,
                codec,
                &mut opts as *mut *mut AvDictionary as *mut *mut c_void,
            )
        };

        if !opts.is_null() {
            unsafe { (lib.av_dict_free)(&mut opts) };
        }

        if ret < 0 {
            let mut p = ctx;
            unsafe { (lib.avcodec_free_context)(&mut p as *mut *mut AvCodecContext) };
            return Err(AvError::FfmpegError { code: ret });
        }

        tracing::debug!(codec_id, "decodificador FFmpeg aberto com D3D11VA");
        Ok(Self { ctx, lib })
    }

    /// Envia um `AvPacket` para o decodificador.
    ///
    /// SPEC-AV-002b
    pub(crate) fn send_packet(&self, pkt: &FfmpegPacket) -> Result<(), AvError> {
        // SAFETY: ctx e pkt são válidos e não-nulos.
        let ret = unsafe { (self.lib.avcodec_send_packet)(self.ctx, pkt.pkt) };
        if ret < 0 && ret != AVERROR_EAGAIN {
            return Err(AvError::FfmpegError { code: ret });
        }
        Ok(())
    }

    /// Recebe um frame decodificado do contexto.
    ///
    /// Retorna `None` quando o decoder precisa de mais dados (EAGAIN ou EOF).
    ///
    /// SPEC-AV-002b
    pub(crate) fn receive_frame(&self, frame: &mut FfmpegFrame) -> Result<bool, AvError> {
        // SAFETY: ctx e frame são válidos e não-nulos.
        let ret = unsafe { (self.lib.avcodec_receive_frame)(self.ctx, frame.frame) };
        if ret == 0 {
            Ok(true)
        } else if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
            Ok(false)
        } else {
            Err(AvError::FfmpegError { code: ret })
        }
    }
}

impl Drop for FfmpegCodecContext {
    fn drop(&mut self) {
        // SAFETY: ctx foi alocado por avcodec_alloc_context3 e é o único dono.
        unsafe { (self.lib.avcodec_free_context)(&mut self.ctx) };
    }
}

// ─── RAII: FfmpegPacket ───────────────────────────────────────────────────────

/// Wrapper RAII para `AVPacket*`.
///
/// SPEC-AV-002b
pub struct FfmpegPacket {
    pkt: *mut AvPacket,
    lib: Arc<FfmpegLib>,
}

// SAFETY: AVPacket contém apenas dados de payload; seguro para enviar entre
// threads enquanto nenhuma outra thread o usa concorrentemente.
unsafe impl Send for FfmpegPacket {}

impl FfmpegPacket {
    /// Aloca um novo `AVPacket` e copia `data` para dentro dele.
    ///
    /// SPEC-AV-002b
    pub fn from_bytes(lib: Arc<FfmpegLib>, data: &[u8], pts: Option<u64>) -> Result<Self, AvError> {
        // SAFETY: av_packet_alloc aloca com av_malloc e zera o struct.
        let pkt = unsafe { (lib.av_packet_alloc)() };
        if pkt.is_null() {
            return Err(AvError::FfmpegError { code: -12 });
        }

        let size = data.len();
        if size > i32::MAX as usize {
            // SAFETY: pkt é não-nulo; libera antes de retornar.
            let mut p = pkt;
            unsafe { (lib.av_packet_free)(&mut p) };
            return Err(AvError::InvalidPes {
                reason: "payload PES excede 2 GiB",
            });
        }

        // SAFETY: av_new_packet aloca internamente `size` bytes e inicializa
        // pkt->data e pkt->size.
        let ret = unsafe { (lib.av_new_packet)(pkt, size as c_int) };
        if ret < 0 {
            let mut p = pkt;
            unsafe { (lib.av_packet_free)(&mut p) };
            return Err(AvError::FfmpegError { code: ret });
        }

        // SAFETY: av_new_packet garantiu que pkt->data é não-nulo com `size` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), (*pkt).data, size);
            // Configura PTS (AV_NOPTS_VALUE = i64::MIN em FFmpeg).
            (*pkt).pts = pts.map(|v| v as i64).unwrap_or(i64::MIN);
            (*pkt).dts = i64::MIN;
        }

        Ok(Self { pkt, lib })
    }
}

impl Drop for FfmpegPacket {
    fn drop(&mut self) {
        // SAFETY: pkt foi alocado por av_packet_alloc; é o único dono.
        unsafe { (self.lib.av_packet_free)(&mut self.pkt) };
    }
}

// ─── RAII: FfmpegFrame ────────────────────────────────────────────────────────

/// Wrapper RAII para `AVFrame*`.
///
/// SPEC-AV-002b
pub struct FfmpegFrame {
    frame: *mut c_void,
    lib: Arc<FfmpegLib>,
}

// SAFETY: AVFrame contém apenas buffers de dados; seguro para enviar entre
// threads enquanto nenhuma outra thread o usa concorrentemente.
unsafe impl Send for FfmpegFrame {}

impl FfmpegFrame {
    /// Aloca um novo `AVFrame`.
    ///
    /// SPEC-AV-002b
    pub fn alloc(lib: Arc<FfmpegLib>) -> Result<Self, AvError> {
        // SAFETY: av_frame_alloc aloca com av_malloc e inicializa o struct.
        let frame = unsafe { (lib.av_frame_alloc)() };
        if frame.is_null() {
            return Err(AvError::FfmpegError { code: -12 });
        }
        Ok(Self { frame, lib })
    }

    /// Remove a referência aos dados do frame sem liberar o frame em si.
    ///
    /// SPEC-AV-002b
    pub(crate) fn unref(&mut self) {
        // SAFETY: frame é não-nulo e válido.
        unsafe { (self.lib.av_frame_unref)(self.frame) };
    }

    /// Extrai os planos YUV de um frame de vídeo decodificado.
    ///
    /// Suporta `YUV420P` (8-bit, `fmt == 0`) e `YUV420P10LE` (10-bit, `fmt == 63`).
    ///
    /// Retorna `(width, height, pts, [y_plane, u_plane, v_plane], (sar_num, sar_den),
    ///           raw_colorspace, raw_color_range, ten_bit)`.
    ///
    /// Os planos são compactados (sem padding de linesize): cada linha do plano Y
    /// tem exatamente `width * bytes_per_sample` bytes, e cada linha dos planos
    /// U/V tem `(width/2) * bytes_per_sample` bytes.
    ///
    /// `raw_colorspace` e `raw_color_range` são valores inteiros brutos dos campos
    /// `AVColorSpace` / `AVColorRange` do AVFrame (ver `YuvColorspace::from_avutil`).
    ///
    /// SPEC-AV-002b
    #[allow(clippy::type_complexity)]
    pub(crate) fn to_yuv_planes(
        &self,
    ) -> Result<(u32, u32, i64, [Vec<u8>; 3], (u32, u32), i32, i32, bool), AvError> {
        // SAFETY: offsets validados contra FFmpeg 8.x headers (ver comentário de layout).
        let (width, height, pts, fmt, raw_sar) = unsafe {
            (
                frame_width(self.frame),
                frame_height(self.frame),
                frame_pts(self.frame),
                frame_format(self.frame),
                frame_sar(self.frame),
            )
        };

        if width <= 0 || height <= 0 {
            return Err(AvError::FfmpegError { code: -22 }); // EINVAL
        }

        let ten_bit = fmt == AV_PIX_FMT_YUV420P10LE;
        if fmt != AV_PIX_FMT_YUV420P && fmt != AV_PIX_FMT_YUV420P10LE {
            tracing::warn!(fmt, "to_yuv_planes: formato de pixel inesperado");
            return Err(AvError::FfmpegError { code: -22 });
        }

        let bytes_per_sample: usize = if ten_bit { 2 } else { 1 };
        let w = width as usize;
        let h = height as usize;
        let w_uv = w / 2;
        let h_uv = h / 2;

        // ── Plano Y ──────────────────────────────────────────────────────────
        let ls_y = unsafe { frame_linesize(self.frame, 0) } as usize;
        let row_y = w * bytes_per_sample;
        let mut y_plane = vec![0u8; row_y * h];
        for row in 0..h {
            // SAFETY: frame->data[0] aponta para plano Y válido após receive_frame.
            unsafe {
                let src = frame_data_ptr(self.frame, 0).add(row * ls_y);
                std::ptr::copy_nonoverlapping(src, y_plane[row * row_y..].as_mut_ptr(), row_y);
            }
        }

        // ── Plano U ──────────────────────────────────────────────────────────
        let ls_u = unsafe { frame_linesize(self.frame, 1) } as usize;
        let row_uv = w_uv * bytes_per_sample;
        let mut u_plane = vec![0u8; row_uv * h_uv];
        for row in 0..h_uv {
            // SAFETY: frame->data[1] aponta para plano U válido após receive_frame.
            unsafe {
                let src = frame_data_ptr(self.frame, 1).add(row * ls_u);
                std::ptr::copy_nonoverlapping(src, u_plane[row * row_uv..].as_mut_ptr(), row_uv);
            }
        }

        // ── Plano V ──────────────────────────────────────────────────────────
        let ls_v = unsafe { frame_linesize(self.frame, 2) } as usize;
        let mut v_plane = vec![0u8; row_uv * h_uv];
        for row in 0..h_uv {
            // SAFETY: frame->data[2] aponta para plano V válido após receive_frame.
            unsafe {
                let src = frame_data_ptr(self.frame, 2).add(row * ls_v);
                std::ptr::copy_nonoverlapping(src, v_plane[row * row_uv..].as_mut_ptr(), row_uv);
            }
        }

        // ── Metadados ─────────────────────────────────────────────────────────
        let sar = if raw_sar.0 > 0 && raw_sar.1 > 0 {
            (raw_sar.0 as u32, raw_sar.1 as u32)
        } else {
            (1u32, 1u32)
        };

        // SAFETY: offsets de color_range (280) e colorspace (292) derivados do
        // layout x86-64 de FFmpeg 8.0 (avutil-60). Ver comentário nos helpers.
        let raw_colorspace = unsafe { frame_colorspace(self.frame) };
        let raw_color_range = unsafe { frame_color_range(self.frame) };

        Ok((
            width as u32,
            height as u32,
            pts,
            [y_plane, u_plane, v_plane],
            sar,
            raw_colorspace,
            raw_color_range,
            ten_bit,
        ))
    }

    /// Converte o frame de áudio para PCM f32 interleaved.
    ///
    /// Usa `swresample` para normalizar formatos planares/interleaved e fazer
    /// downmix para estéreo quando o frame tiver mais de 2 canais.
    ///
    /// SPEC-AV-002b
    pub(crate) fn to_pcm_f32(
        &self,
        sample_rate: u32,
        channels: u16,
    ) -> Result<(i64, u32, u16, Vec<f32>), AvError> {
        // SAFETY: offsets usados aqui se limitam a `nb_samples`, `format` e `pts`.
        let (nb_samples, fmt, pts) = unsafe {
            (
                frame_nb_samples(self.frame),
                frame_format(self.frame),
                frame_pts(self.frame),
            )
        };

        if nb_samples <= 0 || channels == 0 || sample_rate == 0 {
            tracing::error!(
                nb_samples,
                fmt,
                sample_rate,
                channels,
                pts,
                "to_pcm_f32: metadata de áudio inválida ao converter frame"
            );
            return Err(AvError::FfmpegError { code: -22 });
        }

        let out_channels = output_channels_for_input(channels);
        let mut in_layout = self.audio_channel_layout(channels)?;
        let mut out_layout = default_channel_layout(&self.lib, out_channels)?;
        let mut swr = std::ptr::null_mut();

        if out_channels != channels {
            tracing::debug!(
                input_channels = channels,
                output_channels = out_channels,
                "to_pcm_f32: aplicando downmix para estéreo"
            );
        }

        let alloc_ret = unsafe {
            (self.lib.swr_alloc_set_opts2)(
                &mut swr,
                &out_layout,
                AV_SAMPLE_FMT_FLT,
                sample_rate as c_int,
                &in_layout,
                fmt,
                sample_rate as c_int,
                0,
                std::ptr::null_mut(),
            )
        };
        if alloc_ret < 0 || swr.is_null() {
            uninit_channel_layout(&self.lib, &mut in_layout);
            uninit_channel_layout(&self.lib, &mut out_layout);
            return Err(AvError::FfmpegError {
                code: if alloc_ret < 0 { alloc_ret } else { -12 },
            });
        }

        let init_ret = unsafe { (self.lib.swr_init)(swr) };
        if init_ret < 0 {
            free_swr_context(&self.lib, &mut swr);
            uninit_channel_layout(&self.lib, &mut in_layout);
            uninit_channel_layout(&self.lib, &mut out_layout);
            return Err(AvError::FfmpegError { code: init_ret });
        }

        let out_samples_capacity = unsafe { (self.lib.swr_get_out_samples)(swr, nb_samples) };
        if out_samples_capacity < 0 {
            free_swr_context(&self.lib, &mut swr);
            uninit_channel_layout(&self.lib, &mut in_layout);
            uninit_channel_layout(&self.lib, &mut out_layout);
            return Err(AvError::FfmpegError {
                code: out_samples_capacity,
            });
        }

        let out_frames = (out_samples_capacity as usize)
            .max(nb_samples as usize)
            .max(1);
        let mut out = vec![0f32; out_frames * out_channels as usize];
        let mut out_planes = [
            out.as_mut_ptr() as *mut u8,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ];
        let in_planes = unsafe {
            [
                frame_data_ptr(self.frame, 0) as *const u8,
                frame_data_ptr(self.frame, 1) as *const u8,
                frame_data_ptr(self.frame, 2) as *const u8,
                frame_data_ptr(self.frame, 3) as *const u8,
                frame_data_ptr(self.frame, 4) as *const u8,
                frame_data_ptr(self.frame, 5) as *const u8,
                frame_data_ptr(self.frame, 6) as *const u8,
                frame_data_ptr(self.frame, 7) as *const u8,
            ]
        };

        let converted = unsafe {
            (self.lib.swr_convert)(
                swr,
                out_planes.as_mut_ptr(),
                out_frames as c_int,
                in_planes.as_ptr(),
                nb_samples,
            )
        };

        free_swr_context(&self.lib, &mut swr);
        uninit_channel_layout(&self.lib, &mut in_layout);
        uninit_channel_layout(&self.lib, &mut out_layout);

        if converted < 0 {
            return Err(AvError::FfmpegError { code: converted });
        }

        out.truncate(converted as usize * out_channels as usize);

        Ok((pts, sample_rate, out_channels, out))
    }
}

impl FfmpegFrame {
    /// Lê `sample_rate` e canais do AVFrame decodificado.
    pub(crate) fn audio_params(&self) -> Result<(u32, u16), AvError> {
        let (sample_rate, channels) = unsafe {
            (
                frame_sample_rate(self.frame) as i64,
                frame_channel_count(self.frame) as i64,
            )
        };

        normalize_audio_params(sample_rate, channels)
    }
}

fn normalize_audio_params(sample_rate: i64, channels: i64) -> Result<(u32, u16), AvError> {
    if sample_rate <= 0
        || sample_rate > u32::MAX as i64
        || channels <= 0
        || channels > u16::MAX as i64
    {
        return Err(AvError::FfmpegError { code: -22 });
    }

    Ok((sample_rate as u32, channels as u16))
}

fn output_channels_for_input(channels: u16) -> u16 {
    if channels > 2 {
        2
    } else {
        channels
    }
}

fn empty_channel_layout() -> AvChannelLayout {
    AvChannelLayout {
        order: 0,
        nb_channels: 0,
        channels: AvChannelLayoutChannels { mask: 0 },
        opaque: std::ptr::null_mut(),
    }
}

fn default_channel_layout(lib: &FfmpegLib, channels: u16) -> Result<AvChannelLayout, AvError> {
    let mut layout = empty_channel_layout();
    unsafe { (lib.av_channel_layout_default)(&mut layout, channels as c_int) };
    if layout.nb_channels <= 0 {
        return Err(AvError::FfmpegError { code: -22 });
    }
    Ok(layout)
}

fn uninit_channel_layout(lib: &FfmpegLib, layout: &mut AvChannelLayout) {
    unsafe { (lib.av_channel_layout_uninit)(layout) };
}

fn free_swr_context(lib: &FfmpegLib, swr: &mut *mut SwrContext) {
    unsafe { (lib.swr_free)(swr) };
}

impl FfmpegFrame {
    fn audio_channel_layout(&self, channels: u16) -> Result<AvChannelLayout, AvError> {
        default_channel_layout(&self.lib, channels)
    }
}

impl FfmpegFrame {
    /// Retorna o ponteiro bruto `*mut c_void` para o `AVFrame*` interno.
    ///
    /// Usado pelo deinterlacador para passar o frame ao grafo avfilter.
    ///
    /// SAFETY: o ponteiro é válido enquanto `self` existir e não tiver sido unref'd.
    #[inline]
    pub(crate) fn as_ptr(&self) -> *mut c_void {
        self.frame
    }

    /// Retorna `true` se o frame contém uma surface de hardware D3D11VA
    /// (`format == AV_PIX_FMT_D3D11`).
    ///
    /// SPEC-AV-HW-DEC-001
    #[inline]
    pub(crate) fn is_hw(&self) -> bool {
        // SAFETY: frame é não-nulo; frame_format lê offset 116 (int).
        unsafe { frame_format(self.frame) == AV_PIX_FMT_D3D11 }
    }

    /// Extrai o ponteiro bruto `ID3D11Texture2D*` e metadados de um frame HW
    /// **sem** chamar `av_hwframe_transfer_data` — zero cópia CPU.
    ///
    /// No D3D11VA o AVFrame armazena:
    /// - `data[0]` → `ID3D11Texture2D*` (textura array, COM não AddRef'd)
    /// - `data[1]` → índice de slice do array (cast de inteiro para ponteiro)
    ///
    /// O chamador é responsável por chamar `AddRef` na textura se precisar
    /// armazená-la além do tempo de vida do `FfmpegFrame`.
    ///
    /// Retorna `Err` se o frame não for HW (`!is_hw()`).
    ///
    /// SPEC-AV-HW-DEC-001
    #[allow(clippy::type_complexity)]
    pub(crate) fn hw_frame_info(
        &self,
    ) -> Result<(*mut c_void, u32, u32, u32, i64, (u32, u32), i32, i32, i32), crate::error::AvError>
    {
        if !self.is_hw() {
            return Err(crate::error::AvError::FfmpegError { code: -22 });
        }
        // SAFETY: frame HW D3D11VA válido; data[0]/[1] preenchidos pelo decoder.
        unsafe {
            let tex_ptr = frame_data_ptr(self.frame, 0) as *mut c_void;
            // data[1] é o índice do array armazenado como ponteiro (cast de usize).
            let slice = frame_data_ptr(self.frame, 1) as usize as u32;
            let width = frame_width(self.frame) as u32;
            let height = frame_height(self.frame) as u32;
            let pts = frame_pts(self.frame);
            let raw_sar = frame_sar(self.frame);
            let sar = if raw_sar.0 > 0 && raw_sar.1 > 0 {
                (raw_sar.0 as u32, raw_sar.1 as u32)
            } else {
                (1u32, 1u32)
            };
            let color_trc = frame_color_trc(self.frame);
            let colorspace = frame_colorspace(self.frame);
            let color_range = frame_color_range(self.frame);
            Ok((
                tex_ptr,
                slice,
                width,
                height,
                pts,
                sar,
                color_trc,
                colorspace,
                color_range,
            ))
        }
    }

    /// Baixa um frame HW (D3D11VA, `AV_PIX_FMT_D3D11`) para CPU via
    /// `av_hwframe_transfer_data` e extrai os planos YUV.
    ///
    /// Suporta formatos de saída `AV_PIX_FMT_NV12` (semi-planar, nativo D3D11VA)
    /// e `AV_PIX_FMT_YUV420P` (planar, caso o driver suporte).
    /// Para NV12, o plano UV interleaved é desinterlaceado em U/V separados.
    ///
    /// Retorna os mesmos campos que `to_yuv_planes()`, com `ten_bit = false`
    /// (D3D11VA 8-bit não usa o caminho 10-bit via este método).
    ///
    /// SPEC-AV-HW-DEC-001
    #[allow(clippy::type_complexity)]
    #[allow(dead_code)]
    pub(crate) fn download_to_yuv_planes(
        &self,
    ) -> Result<(u32, u32, i64, [Vec<u8>; 3], (u32, u32), i32, i32, bool), AvError> {
        // Aloca frame SW destino — av_hwframe_transfer_data aloca os buffers de
        // dados automaticamente (não precisamos chamar av_frame_get_buffer).
        // SAFETY: av_frame_alloc aloca com av_malloc e zera o struct.
        let sw_frame = unsafe { (self.lib.av_frame_alloc)() };
        if sw_frame.is_null() {
            return Err(AvError::FfmpegError { code: -12 });
        }

        // Copia dados da surface D3D11 para memória do host.
        // O formato do sw_frame é determinado pelo sw_format configurado no
        // AVHWFramesContext interno do FFmpeg (normalmente NV12 para H.264/HEVC).
        // SAFETY: self.frame é um frame HW válido retornado por avcodec_receive_frame.
        let ret = unsafe { (self.lib.av_hwframe_transfer_data)(sw_frame, self.frame, 0) };
        if ret < 0 {
            let mut p = sw_frame;
            unsafe { (self.lib.av_frame_free)(&mut p) };
            return Err(AvError::FfmpegError { code: ret });
        }

        // Extrai planos YUV do frame SW (NV12 ou YUV420P).
        // SAFETY: av_hwframe_transfer_data preencheu sw_frame com dados válidos.
        let result = unsafe { extract_sw_yuv_planes(sw_frame) };

        // Libera o sw_frame (dados já copiados para Vec<u8>).
        let mut p = sw_frame;
        unsafe { (self.lib.av_frame_free)(&mut p) };

        result
    }
}

impl Drop for FfmpegFrame {
    fn drop(&mut self) {
        // SAFETY: frame foi alocado por av_frame_alloc; é o único dono.
        unsafe { (self.lib.av_frame_free)(&mut self.frame) };
    }
}

// ─── Extração de planos YUV de frames SW ──────────────────────────────────────

/// Extrai planos YUV de um `AVFrame*` SW (YUV420P ou NV12).
///
/// Para `AV_PIX_FMT_NV12`: desinterlaça o plano UV (UVUVUV…) em U e V separados.
/// Para `AV_PIX_FMT_YUV420P`: copia diretamente os três planos planares.
///
/// SAFETY: `frame` deve ser um `AVFrame*` SW válido com buffers preenchidos.
#[allow(clippy::type_complexity)]
#[allow(dead_code)]
unsafe fn extract_sw_yuv_planes(
    frame: *mut c_void,
) -> Result<(u32, u32, i64, [Vec<u8>; 3], (u32, u32), i32, i32, bool), AvError> {
    let fmt = frame_format(frame);
    let width = frame_width(frame);
    let height = frame_height(frame);
    let pts = frame_pts(frame);
    let raw_sar = frame_sar(frame);
    let raw_colorspace = frame_colorspace(frame);
    let raw_color_range = frame_color_range(frame);

    if width <= 0 || height <= 0 {
        return Err(AvError::FfmpegError { code: -22 }); // EINVAL
    }

    let w = width as usize;
    let h = height as usize;
    let sar = if raw_sar.0 > 0 && raw_sar.1 > 0 {
        (raw_sar.0 as u32, raw_sar.1 as u32)
    } else {
        (1u32, 1u32)
    };

    let planes = if fmt == AV_PIX_FMT_YUV420P {
        extract_yuv420p_planes(frame, w, h)?
    } else if fmt == AV_PIX_FMT_NV12 {
        extract_nv12_planes(frame, w, h)?
    } else {
        tracing::warn!(fmt, "download_to_yuv_planes: formato SW inesperado");
        return Err(AvError::FfmpegError { code: -22 });
    };

    Ok((
        width as u32,
        height as u32,
        pts,
        planes,
        sar,
        raw_colorspace,
        raw_color_range,
        false, // Fase B: apenas 8-bit via D3D11VA
    ))
}

/// Extrai planos YUV420P planares de um `AVFrame*`.
///
/// SAFETY: `frame` deve ser um AVFrame* com dados YUV420P válidos.
#[allow(dead_code)]
unsafe fn extract_yuv420p_planes(
    frame: *mut c_void,
    w: usize,
    h: usize,
) -> Result<[Vec<u8>; 3], AvError> {
    let w_uv = w / 2;
    let h_uv = h / 2;
    let ls_y = frame_linesize(frame, 0) as usize;
    let ls_u = frame_linesize(frame, 1) as usize;
    let ls_v = frame_linesize(frame, 2) as usize;

    let mut y = vec![0u8; w * h];
    let mut u = vec![0u8; w_uv * h_uv];
    let mut v = vec![0u8; w_uv * h_uv];

    for row in 0..h {
        let src = frame_data_ptr(frame, 0).add(row * ls_y);
        std::ptr::copy_nonoverlapping(src, y[row * w..].as_mut_ptr(), w);
    }
    for row in 0..h_uv {
        let src_u = frame_data_ptr(frame, 1).add(row * ls_u);
        let src_v = frame_data_ptr(frame, 2).add(row * ls_v);
        std::ptr::copy_nonoverlapping(src_u, u[row * w_uv..].as_mut_ptr(), w_uv);
        std::ptr::copy_nonoverlapping(src_v, v[row * w_uv..].as_mut_ptr(), w_uv);
    }

    Ok([y, u, v])
}

/// Extrai e desinterlaça planos NV12 (semi-planar UV) de um `AVFrame*`.
///
/// NV12: data[0] = Y planar, data[1] = UV interleaved (UVUV…).
/// Converte para YUV420P planar separando U e V.
///
/// SAFETY: `frame` deve ser um AVFrame* com dados NV12 válidos.
#[allow(dead_code)]
unsafe fn extract_nv12_planes(
    frame: *mut c_void,
    w: usize,
    h: usize,
) -> Result<[Vec<u8>; 3], AvError> {
    let w_uv = w / 2;
    let h_uv = h / 2;
    let ls_y = frame_linesize(frame, 0) as usize;
    let ls_uv = frame_linesize(frame, 1) as usize;

    let mut y = vec![0u8; w * h];
    let mut u = vec![0u8; w_uv * h_uv];
    let mut v = vec![0u8; w_uv * h_uv];

    // Plano Y (igual a YUV420P)
    for row in 0..h {
        let src = frame_data_ptr(frame, 0).add(row * ls_y);
        std::ptr::copy_nonoverlapping(src, y[row * w..].as_mut_ptr(), w);
    }

    // Desinterlaça UV: cada linha de UV tem w/2 pares UVUV…
    for row in 0..h_uv {
        let src_uv = frame_data_ptr(frame, 1).add(row * ls_uv);
        let u_row = &mut u[row * w_uv..(row + 1) * w_uv];
        let v_row = &mut v[row * w_uv..(row + 1) * w_uv];
        for col in 0..w_uv {
            u_row[col] = *src_uv.add(col * 2);
            v_row[col] = *src_uv.add(col * 2 + 1);
        }
    }

    Ok([y, u, v])
}

// ─── Utilitários de busca e carregamento ──────────────────────────────────────

/// Retorna o diretório de busca preferencial de DLLs FFmpeg.
///
/// Ordem de precedência:
/// 1. Variável de ambiente `FFMPEG_DLL_DIR`
/// 2. `{exe_dir}/ffmpeg/`
/// 3. `{exe_dir}/`
/// 4. `{cwd}/ffmpeg/` e `{cwd}/` (útil em `cargo run` a partir da raiz do
///    workspace, onde o DLL bundle vive em `ffmpeg/` ao lado de `Cargo.toml`)
/// 5. Ancestrais de `exe_dir` contendo subpasta `ffmpeg/` com `DLL_AVCODEC`
///    (cobre `target/debug` → workspace root)
///
/// SPEC-AV-002b
pub fn find_ffmpeg_dll_dir() -> Option<std::path::PathBuf> {
    // 1. Variável de ambiente explícita (útil em testes CI)
    if let Ok(dir) = std::env::var("FFMPEG_DLL_DIR") {
        let p = std::path::PathBuf::from(dir);
        if p.join(DLL_AVCODEC).exists() {
            return Some(p);
        }
    }

    // 2. Diretório do executável atual
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let sub = exe_dir.join("ffmpeg");
            if sub.join(DLL_AVCODEC).exists() {
                return Some(sub);
            }
            // 3. Diretório do executável diretamente
            if exe_dir.join(DLL_AVCODEC).exists() {
                return Some(exe_dir.to_path_buf());
            }

            // 5. Ancestrais do exe_dir (cobre target/debug/<bin>.exe →
            // <workspace>/ffmpeg/).  Limite de 5 níveis para evitar varredura
            // patológica em layouts inesperados.
            for ancestor in exe_dir.ancestors().take(5) {
                let sub = ancestor.join("ffmpeg");
                if sub.join(DLL_AVCODEC).exists() {
                    return Some(sub);
                }
            }
        }
    }

    // 4. Diretório de trabalho corrente
    if let Ok(cwd) = std::env::current_dir() {
        let sub = cwd.join("ffmpeg");
        if sub.join(DLL_AVCODEC).exists() {
            return Some(sub);
        }
        if cwd.join(DLL_AVCODEC).exists() {
            return Some(cwd);
        }
    }

    None
}

// ─── avfilter: tipos opacos ───────────────────────────────────────────────────

/// Tipo opaco para `AVFilterGraph*`.
#[repr(C)]
pub(crate) struct AvFilterGraph {
    _opaque: [u8; 0],
}

/// Tipo opaco para `AVFilterContext*`.
#[repr(C)]
pub(crate) struct AvFilterContext {
    _opaque: [u8; 0],
}

/// Tipo opaco para `AVFilter*`.
#[repr(C)]
pub(crate) struct AvFilter {
    _opaque: [u8; 0],
}

// ─── avfilter: tipos de ponteiro de função ────────────────────────────────────

type FnAvfilterGraphAlloc = unsafe extern "C" fn() -> *mut AvFilterGraph;
type FnAvfilterGraphFree = unsafe extern "C" fn(graph: *mut *mut AvFilterGraph);
type FnAvfilterGetByName = unsafe extern "C" fn(name: *const i8) -> *const AvFilter;
type FnAvfilterGraphCreateFilter = unsafe extern "C" fn(
    filt_ctx: *mut *mut AvFilterContext,
    filt: *const AvFilter,
    name: *const i8,
    args: *const i8,
    opaque: *mut c_void,
    graph_ctx: *mut AvFilterGraph,
) -> c_int;
type FnAvfilterLink = unsafe extern "C" fn(
    src: *mut AvFilterContext,
    srcpad: u32,
    dst: *mut AvFilterContext,
    dstpad: u32,
) -> c_int;
type FnAvfilterGraphConfig =
    unsafe extern "C" fn(graphctx: *mut AvFilterGraph, log_ctx: *mut c_void) -> c_int;
type FnAvBuffersrcAddFrameFlags =
    unsafe extern "C" fn(ctx: *mut AvFilterContext, frame: *mut c_void, flags: c_int) -> c_int;
type FnAvBuffersinkGetFrame =
    unsafe extern "C" fn(ctx: *mut AvFilterContext, frame: *mut c_void) -> c_int;

// ─── FilterLib ────────────────────────────────────────────────────────────────

/// Biblioteca `avfilter` carregada dinamicamente.
///
/// Separada de `FfmpegLib` para manter o carregamento opcional —
/// se `avfilter-11.dll` não estiver disponível, o deinterlacing é simplesmente
/// ignorado sem degradar o restante do pipeline.
///
/// SPEC-AV-004
#[allow(dead_code)]
pub(crate) struct FilterLib {
    _avfilter: Library,

    pub(crate) avfilter_graph_alloc: FnAvfilterGraphAlloc,
    pub(crate) avfilter_graph_free: FnAvfilterGraphFree,
    pub(crate) avfilter_get_by_name: FnAvfilterGetByName,
    pub(crate) avfilter_graph_create_filter: FnAvfilterGraphCreateFilter,
    pub(crate) avfilter_link: FnAvfilterLink,
    pub(crate) avfilter_graph_config: FnAvfilterGraphConfig,
    pub(crate) av_buffersrc_add_frame_flags: FnAvBuffersrcAddFrameFlags,
    pub(crate) av_buffersink_get_frame: FnAvBuffersinkGetFrame,
}

// SAFETY: Os ponteiros de função são obtidos de DLLs thread-safe do FFmpeg.
unsafe impl Send for FilterLib {}
unsafe impl Sync for FilterLib {}

impl FilterLib {
    /// Carrega `avfilter-11.dll` e resolve os símbolos necessários para bwdif.
    ///
    /// Retorna `Err` se a DLL não existir ou os símbolos não forem encontrados.
    ///
    /// SPEC-AV-004
    pub(crate) fn load(dll_dir: &Path) -> Result<Arc<Self>, AvError> {
        #[cfg(windows)]
        set_dll_search_dir(Some(dll_dir));

        let result = Self::load_inner(dll_dir);

        #[cfg(windows)]
        set_dll_search_dir(None);

        result
    }

    fn load_inner(dll_dir: &Path) -> Result<Arc<Self>, AvError> {
        let avfilter = unsafe { Library::new(dll_dir.join(DLL_AVFILTER)) }.map_err(|e| {
            AvError::FfmpegUnavailable {
                message: format!("falha ao carregar {DLL_AVFILTER}: {e}"),
            }
        })?;

        macro_rules! sym {
            ($lib:expr, $name:literal, $ty:ty) => {{
                let s: libloading::Symbol<$ty> =
                    unsafe { $lib.get($name) }.map_err(|e| AvError::FfmpegUnavailable {
                        message: format!(
                            "símbolo '{}' não encontrado: {e}",
                            std::str::from_utf8(&$name[..$name.len() - 1]).unwrap_or("<invalid>")
                        ),
                    })?;
                *s
            }};
        }

        let avfilter_graph_alloc = sym!(avfilter, b"avfilter_graph_alloc\0", FnAvfilterGraphAlloc);
        let avfilter_graph_free = sym!(avfilter, b"avfilter_graph_free\0", FnAvfilterGraphFree);
        let avfilter_get_by_name = sym!(avfilter, b"avfilter_get_by_name\0", FnAvfilterGetByName);
        let avfilter_graph_create_filter = sym!(
            avfilter,
            b"avfilter_graph_create_filter\0",
            FnAvfilterGraphCreateFilter
        );
        let avfilter_link = sym!(avfilter, b"avfilter_link\0", FnAvfilterLink);
        let avfilter_graph_config =
            sym!(avfilter, b"avfilter_graph_config\0", FnAvfilterGraphConfig);
        let av_buffersrc_add_frame_flags = sym!(
            avfilter,
            b"av_buffersrc_add_frame_flags\0",
            FnAvBuffersrcAddFrameFlags
        );
        let av_buffersink_get_frame = sym!(
            avfilter,
            b"av_buffersink_get_frame\0",
            FnAvBuffersinkGetFrame
        );

        Ok(Arc::new(Self {
            _avfilter: avfilter,
            avfilter_graph_alloc,
            avfilter_graph_free,
            avfilter_get_by_name,
            avfilter_graph_create_filter,
            avfilter_link,
            avfilter_graph_config,
            av_buffersrc_add_frame_flags,
            av_buffersink_get_frame,
        }))
    }
}

// ─── FfmpegFilterGraph ────────────────────────────────────────────────────────

/// Wrapper RAII para um grafo de filtros avfilter com topologia:
/// `buffer → bwdif → buffersink`.
///
/// Usado para deinterlacing de frames 1080i em tempo real.
///
/// SPEC-AV-004
pub(crate) struct FfmpegFilterGraph {
    graph: *mut AvFilterGraph,
    src_ctx: *mut AvFilterContext,
    sink_ctx: *mut AvFilterContext,
    filter_lib: Arc<FilterLib>,
    ffmpeg_lib: Arc<FfmpegLib>,
}

// SAFETY: `AVFilterGraph` e `AVFilterContext` são usados exclusivamente na
// thread av-decode; nunca compartilhados entre threads simultaneamente.
unsafe impl Send for FfmpegFilterGraph {}

impl FfmpegFilterGraph {
    /// Cria um grafo bwdif para deinterlacing de frames YUV planar.
    ///
    /// `pix_fmt`: formato de pixel AVPixelFormat
    /// (`AV_PIX_FMT_YUV420P = 0`, `AV_PIX_FMT_YUV420P10LE = 63`).
    ///
    /// SPEC-AV-004
    pub(crate) fn new_bwdif(
        filter_lib: Arc<FilterLib>,
        ffmpeg_lib: Arc<FfmpegLib>,
        width: u32,
        height: u32,
        pix_fmt: c_int,
    ) -> Result<Self, AvError> {
        // SAFETY: avfilter_graph_alloc usa av_malloc internamente.
        let graph = unsafe { (filter_lib.avfilter_graph_alloc)() };
        if graph.is_null() {
            return Err(AvError::FfmpegError { code: -12 });
        }

        // ── Filtro buffer (source) ─────────────────────────────────────────
        let src_filt = unsafe { (filter_lib.avfilter_get_by_name)(c"buffer".as_ptr()) };
        if src_filt.is_null() {
            unsafe {
                let mut g = graph;
                (filter_lib.avfilter_graph_free)(&mut g);
            }
            return Err(AvError::FfmpegUnavailable {
                message: "filtro 'buffer' não encontrado em avfilter".to_string(),
            });
        }

        // Formato: "video_size=WxH:pix_fmt=N:time_base=1/90000:pixel_aspect=0/1"
        let src_args = CString::new(format!(
            "video_size={}x{}:pix_fmt={}:time_base=1/90000:pixel_aspect=0/1",
            width, height, pix_fmt
        ))
        .map_err(|_| AvError::Other(anyhow::anyhow!("CString inválida para buffer args")))?;

        let mut src_ctx: *mut AvFilterContext = std::ptr::null_mut();
        // SAFETY: todos os ponteiros são válidos e nul-terminados.
        let ret = unsafe {
            (filter_lib.avfilter_graph_create_filter)(
                &mut src_ctx,
                src_filt,
                c"in".as_ptr(),
                src_args.as_ptr(),
                std::ptr::null_mut(),
                graph,
            )
        };
        if ret < 0 {
            unsafe {
                let mut g = graph;
                (filter_lib.avfilter_graph_free)(&mut g);
            }
            return Err(AvError::FfmpegError { code: ret });
        }

        // ── Filtro bwdif ──────────────────────────────────────────────────
        let bwdif_filt = unsafe { (filter_lib.avfilter_get_by_name)(c"bwdif".as_ptr()) };
        if bwdif_filt.is_null() {
            unsafe {
                let mut g = graph;
                (filter_lib.avfilter_graph_free)(&mut g);
            }
            return Err(AvError::FfmpegUnavailable {
                message: "filtro 'bwdif' não encontrado em avfilter".to_string(),
            });
        }

        let mut bwdif_ctx: *mut AvFilterContext = std::ptr::null_mut();
        let ret = unsafe {
            (filter_lib.avfilter_graph_create_filter)(
                &mut bwdif_ctx,
                bwdif_filt,
                c"bwdif".as_ptr(),
                c"mode=send_frame:parity=auto:deint=interlaced".as_ptr(),
                std::ptr::null_mut(),
                graph,
            )
        };
        if ret < 0 {
            unsafe {
                let mut g = graph;
                (filter_lib.avfilter_graph_free)(&mut g);
            }
            return Err(AvError::FfmpegError { code: ret });
        }

        // ── Filtro buffersink ─────────────────────────────────────────────
        let sink_filt = unsafe { (filter_lib.avfilter_get_by_name)(c"buffersink".as_ptr()) };
        if sink_filt.is_null() {
            unsafe {
                let mut g = graph;
                (filter_lib.avfilter_graph_free)(&mut g);
            }
            return Err(AvError::FfmpegUnavailable {
                message: "filtro 'buffersink' não encontrado em avfilter".to_string(),
            });
        }

        let mut sink_ctx: *mut AvFilterContext = std::ptr::null_mut();
        let ret = unsafe {
            (filter_lib.avfilter_graph_create_filter)(
                &mut sink_ctx,
                sink_filt,
                c"out".as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                graph,
            )
        };
        if ret < 0 {
            unsafe {
                let mut g = graph;
                (filter_lib.avfilter_graph_free)(&mut g);
            }
            return Err(AvError::FfmpegError { code: ret });
        }

        // ── Liga: buffer → bwdif → buffersink ─────────────────────────────
        let ret = unsafe { (filter_lib.avfilter_link)(src_ctx, 0, bwdif_ctx, 0) };
        if ret < 0 {
            unsafe {
                let mut g = graph;
                (filter_lib.avfilter_graph_free)(&mut g);
            }
            return Err(AvError::FfmpegError { code: ret });
        }

        let ret = unsafe { (filter_lib.avfilter_link)(bwdif_ctx, 0, sink_ctx, 0) };
        if ret < 0 {
            unsafe {
                let mut g = graph;
                (filter_lib.avfilter_graph_free)(&mut g);
            }
            return Err(AvError::FfmpegError { code: ret });
        }

        // ── Configura o grafo ─────────────────────────────────────────────
        let ret = unsafe { (filter_lib.avfilter_graph_config)(graph, std::ptr::null_mut()) };
        if ret < 0 {
            unsafe {
                let mut g = graph;
                (filter_lib.avfilter_graph_free)(&mut g);
            }
            return Err(AvError::FfmpegError { code: ret });
        }

        tracing::debug!(width, height, pix_fmt, "grafo bwdif criado com sucesso");

        Ok(Self {
            graph,
            src_ctx,
            sink_ctx,
            filter_lib,
            ffmpeg_lib,
        })
    }

    /// Empurra `input` para o buffer source e lê o frame deinterlaced do sink.
    ///
    /// Retorna `Some(frame)` com o frame deinterlaced, ou `None` se o bwdif
    /// ainda estiver acumulando contexto temporal (AVERROR_EAGAIN).
    ///
    /// SPEC-AV-004
    pub(crate) fn process(&mut self, input: &FfmpegFrame) -> Result<Option<FfmpegFrame>, AvError> {
        // AV_BUFFERSRC_FLAG_KEEP_REF = 8 — mantém a referência original no caller.
        // SAFETY: src_ctx é válido; input.as_ptr() aponta para um AVFrame válido.
        let ret = unsafe {
            (self.filter_lib.av_buffersrc_add_frame_flags)(self.src_ctx, input.as_ptr(), 8)
        };
        if ret < 0 {
            return Err(AvError::FfmpegError { code: ret });
        }

        // Aloca frame de saída e lê do buffersink.
        let output = FfmpegFrame::alloc(Arc::clone(&self.ffmpeg_lib))?;
        // SAFETY: sink_ctx é válido; output.frame aponta para AVFrame alocado.
        let ret =
            unsafe { (self.filter_lib.av_buffersink_get_frame)(self.sink_ctx, output.as_ptr()) };

        if ret == 0 {
            Ok(Some(output))
        } else if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
            // bwdif precisa de mais contexto temporal; frame de entrada ignorado.
            Ok(None)
        } else {
            Err(AvError::FfmpegError { code: ret })
        }
    }
}

impl Drop for FfmpegFilterGraph {
    fn drop(&mut self) {
        // SAFETY: graph foi alocado por avfilter_graph_alloc; é o único dono.
        // avfilter_graph_free libera o grafo e todos os AVFilterContexts.
        unsafe { (self.filter_lib.avfilter_graph_free)(&mut self.graph) };
    }
}

// ─── Utilitários de busca e carregamento ──────────────────────────────────────

/// Configura (ou limpa) o diretório adicional de busca de DLLs no Windows.
///
/// SAFETY: `SetDllDirectoryW` é uma syscall de kernel32 com ABI documentada.
#[cfg(windows)]
pub fn set_dll_search_dir(dir: Option<&Path>) {
    use std::os::windows::ffi::OsStrExt as _;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetDllDirectoryW(lpPathName: *const u16) -> i32;
    }

    match dir {
        Some(p) => {
            let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
            // SAFETY: wide é nul-terminado; kernel32 sempre disponível.
            unsafe { SetDllDirectoryW(wide.as_ptr()) };
        }
        None => {
            // SAFETY: NULL restaura o comportamento padrão.
            unsafe { SetDllDirectoryW(std::ptr::null()) };
        }
    }
}

#[cfg(not(windows))]
pub fn set_dll_search_dir(_dir: Option<&Path>) {
    // No-op em plataformas não-Windows.
}

// ─── Testes ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_av_002b_normalize_audio_params_validates_values() {
        assert_eq!(normalize_audio_params(48_000, 2).unwrap(), (48_000, 2));
        assert!(matches!(
            normalize_audio_params(0, 2),
            Err(AvError::FfmpegError { code: -22 })
        ));
        assert!(matches!(
            normalize_audio_params(48_000, 0),
            Err(AvError::FfmpegError { code: -22 })
        ));
    }

    #[test]
    fn spec_av_002b_output_channels_for_input_downmixes_multichannel() {
        assert_eq!(output_channels_for_input(1), 1);
        assert_eq!(output_channels_for_input(2), 2);
        assert_eq!(output_channels_for_input(6), 2);
        assert_eq!(output_channels_for_input(8), 2);
    }

    #[test]
    fn spec_av_002b_audio_frame_metadata_offsets_match_ffmpeg_8_layout() {
        let mut frame = vec![0u8; 416];
        frame[180..184].copy_from_slice(&48_000i32.to_ne_bytes());
        frame[388..392].copy_from_slice(&2i32.to_ne_bytes());

        let frame_ptr = frame.as_mut_ptr().cast::<c_void>();
        let (sample_rate, channels) =
            unsafe { (frame_sample_rate(frame_ptr), frame_channel_count(frame_ptr)) };

        assert_eq!(sample_rate, 48_000);
        assert_eq!(channels, 2);
    }

    /// SPEC-AV-002b: constantes de codec ID devem corresponder aos valores
    /// documentados na ISO 13818 / FFmpeg enum `AVCodecID`.
    #[test]
    fn spec_av_002b_codec_id_constants() {
        assert_eq!(AV_CODEC_ID_MPEG2VIDEO, 2);
        assert_eq!(AV_CODEC_ID_H264, 27);
        assert_eq!(AV_CODEC_ID_HEVC, 173);
        assert_eq!(AV_CODEC_ID_MP2, 0x15000);
        assert_eq!(AV_CODEC_ID_AAC, 0x15002);
        assert_eq!(AV_CODEC_ID_AC3, 0x15003);
        assert_eq!(AV_CODEC_ID_EAC3, 0x15028);
        assert_eq!(AV_CODEC_ID_AAC_LATM, 0x15031);
    }

    /// SPEC-AV-002b: AVERROR_EOF deve ser o valor canônico.
    #[test]
    fn spec_av_002b_averror_eof_value() {
        // AVERROR_EOF = -FFERRTAG(0xF8,'E','O','F') = -541_478_725
        assert_eq!(AVERROR_EOF, -541_478_725_i32);
    }

    /// SPEC-AV-002b: AVERROR_EAGAIN deve ser -EAGAIN = -11.
    #[test]
    fn spec_av_002b_averror_eagain_value() {
        assert_eq!(AVERROR_EAGAIN, -11);
    }

    /// SPEC-AV-002b: `find_ffmpeg_dll_dir` retorna None ou Some com DLL existente.
    #[test]
    fn spec_av_002b_find_ffmpeg_dll_dir_returns_valid_or_none() {
        match find_ffmpeg_dll_dir() {
            Some(dir) => {
                // Se retornou Some, a DLL deve existir no diretório.
                assert!(
                    dir.join(DLL_AVCODEC).exists(),
                    "find_ffmpeg_dll_dir retornou diretório sem {DLL_AVCODEC}"
                );
            }
            None => {
                // FFmpeg não instalado — aceitável em CI sem DLLs.
            }
        }
    }

    /// SPEC-AV-002b: load com diretório inválido deve retornar `AvError::FfmpegUnavailable`.
    #[test]
    fn spec_av_002b_load_invalid_dir_returns_error() {
        let result = FfmpegLib::load(Path::new("/nenhum/diretorio/aqui"));
        let err_msg = result
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            matches!(result, Err(AvError::FfmpegUnavailable { .. })),
            "esperava FfmpegUnavailable, obteve: {err_msg}"
        );
    }

    /// SPEC-AV-002b: se DLLs disponíveis, `FfmpegLib::load` deve ter sucesso.
    #[test]
    fn spec_av_002b_load_succeeds_if_dlls_present() {
        let Some(dir) = find_ffmpeg_dll_dir() else {
            eprintln!("DLLs FFmpeg não encontradas — teste ignorado");
            return;
        };
        let result = FfmpegLib::load(&dir);
        let err_str = result
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            result.is_ok(),
            "esperava Ok após encontrar DLLs em {}: {err_str}",
            dir.display()
        );
    }
}
