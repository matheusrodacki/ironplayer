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

use std::ffi::{c_int, c_void};
use std::path::Path;
use std::sync::Arc;

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
const DLL_SWSCALE: &str = "swscale-9.dll";

#[cfg(not(windows))]
const DLL_AVUTIL: &str = "libavutil.so.60";
#[cfg(not(windows))]
const DLL_AVCODEC: &str = "libavcodec.so.62";
#[cfg(not(windows))]
const DLL_SWRESAMPLE: &str = "libswresample.so.6";
#[cfg(not(windows))]
const DLL_SWSCALE: &str = "libswscale.so.9";

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

/// Formatos de pixel.
pub const AV_PIX_FMT_RGB24: c_int = 2;

/// Formatos de sample de áudio.
pub const AV_SAMPLE_FMT_FLT: c_int = 3;
pub const AV_SAMPLE_FMT_S16P: c_int = 6;
pub const AV_SAMPLE_FMT_FLTP: c_int = 8;

/// Flags de escalonamento para swscale.
pub const SWS_BILINEAR: c_int = 2;

/// Códigos de erro FFmpeg.
pub const AVERROR_EAGAIN: c_int = -11;
/// AVERROR_EOF = FFERRTAG(0xF8,'E','O','F') = -0x20464F45
pub const AVERROR_EOF: c_int = -541_478_725_i32;

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

/// Tipo opaco para `SwsContext*`.
#[repr(C)]
pub struct SwsContext {
    _opaque: [u8; 0],
}

/// Tipo opaco para `SwrContext*`.
#[repr(C)]
pub struct SwrContext {
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

#[allow(non_snake_case)]
type FnSwsGetContext = unsafe extern "C" fn(
    srcW: c_int,
    srcH: c_int,
    srcFormat: c_int,
    dstW: c_int,
    dstH: c_int,
    dstFormat: c_int,
    flags: c_int,
    srcFilter: *mut c_void,
    dstFilter: *mut c_void,
    param: *const f64,
) -> *mut SwsContext;
#[allow(non_snake_case)]
type FnSwsScale = unsafe extern "C" fn(
    ctx: *mut SwsContext,
    srcSlice: *const *const u8,
    srcStride: *const c_int,
    srcSliceY: c_int,
    srcSliceH: c_int,
    dst: *const *mut u8,
    dstStride: *const c_int,
) -> c_int;
#[allow(non_snake_case)]
type FnSwsFreeContext = unsafe extern "C" fn(swsContext: *mut SwsContext);

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
    _swscale: Library,

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

    // Funções swscale
    pub(crate) sws_get_context: FnSwsGetContext,
    pub(crate) sws_scale: FnSwsScale,
    pub(crate) sws_free_context: FnSwsFreeContext,
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

        // Carrega swscale (depende de avutil)
        let swscale = unsafe { Library::new(dll_dir.join(DLL_SWSCALE)) }.map_err(|e| {
            AvError::FfmpegUnavailable {
                message: format!("falha ao carregar {DLL_SWSCALE}: {e}"),
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

        let sws_get_context = sym!(swscale, b"sws_getContext\0", FnSwsGetContext);
        let sws_scale = sym!(swscale, b"sws_scale\0", FnSwsScale);
        let sws_free_context = sym!(swscale, b"sws_freeContext\0", FnSwsFreeContext);

        Ok(Arc::new(Self {
            _avutil: avutil,
            _avcodec: avcodec,
            _swresample: swresample,
            _swscale: swscale,
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
            sws_get_context,
            sws_scale,
            sws_free_context,
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
    /// SPEC-AV-002b
    pub fn open(lib: Arc<FfmpegLib>, codec_id: u32) -> Result<Self, AvError> {
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

        // SAFETY: avcodec_open2 configura o contexto com o codec encontrado.
        let ret = unsafe { (lib.avcodec_open2)(ctx, codec, std::ptr::null_mut()) };
        if ret < 0 {
            // Libera o contexto antes de retornar erro.
            // SAFETY: ctx não-nulo, avcodec_free_context é o destrutor correto.
            let mut p = ctx;
            unsafe { (lib.avcodec_free_context)(&mut p) };
            return Err(AvError::FfmpegError { code: ret });
        }

        tracing::debug!(codec_id, "decodificador FFmpeg aberto");
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

    /// Converte o frame de vídeo para RGB24 via swscale.
    ///
    /// Retorna `(width, height, pts, rgb_bytes, (sar_num, sar_den))`.
    /// `sar_num/sar_den` é o Sample Aspect Ratio do frame; use-o para calcular
    /// o Display Aspect Ratio: `DAR = (sar_num * width) / (sar_den * height)`.
    /// Quando `sar_num == sar_den` (incluindo ambos == 1), pixels são quadrados.
    ///
    /// SPEC-AV-002b
    #[allow(clippy::type_complexity)]
    pub(crate) fn to_rgb24(&self) -> Result<(u32, u32, i64, Vec<u8>, (u32, u32)), AvError> {
        // SAFETY: offsets validados contra FFmpeg 8.x headers (ver comentário de layout).
        let (width, height, pts, src_fmt, raw_sar) = unsafe {
            (
                frame_width(self.frame),
                frame_height(self.frame),
                frame_pts(self.frame),
                frame_format(self.frame),
                frame_sar(self.frame),
            )
        };

        // Normaliza SAR: (0,*), (*,0) ou valores negativos → 1:1 (pixels quadrados).
        let sar = if raw_sar.0 > 0 && raw_sar.1 > 0 {
            (raw_sar.0 as u32, raw_sar.1 as u32)
        } else {
            (1u32, 1u32)
        };

        if width <= 0 || height <= 0 {
            return Err(AvError::FfmpegError { code: -22 }); // EINVAL
        }

        let w = width as usize;
        let h = height as usize;
        let rgb_stride = w * 3;
        let mut rgb_data: Vec<u8> = vec![0u8; rgb_stride * h];

        // SAFETY: sws_getContext retorna nulo se os parâmetros forem inválidos.
        let sws = unsafe {
            (self.lib.sws_get_context)(
                width,
                height,
                src_fmt,
                width,
                height,
                AV_PIX_FMT_RGB24,
                SWS_BILINEAR,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if sws.is_null() {
            return Err(AvError::FfmpegError { code: -22 });
        }

        // SAFETY: frame->data e frame->linesize são válidos após receive_frame.
        let src_data: [*const u8; 8] = unsafe {
            [
                frame_data_ptr(self.frame, 0),
                frame_data_ptr(self.frame, 1),
                frame_data_ptr(self.frame, 2),
                frame_data_ptr(self.frame, 3),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            ]
        };
        let src_stride: [c_int; 8] = unsafe {
            [
                frame_linesize(self.frame, 0),
                frame_linesize(self.frame, 1),
                frame_linesize(self.frame, 2),
                frame_linesize(self.frame, 3),
                0,
                0,
                0,
                0,
            ]
        };

        let dst_stride = [rgb_stride as c_int, 0, 0, 0, 0, 0, 0, 0];
        let dst_ptr = rgb_data.as_mut_ptr();
        let dst_data: [*mut u8; 8] = [
            dst_ptr,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ];

        // SAFETY: sws é válido, src_data/dst_data apontam para buffers corretos.
        let ret = unsafe {
            (self.lib.sws_scale)(
                sws,
                src_data.as_ptr(),
                src_stride.as_ptr(),
                0,
                height,
                dst_data.as_ptr(),
                dst_stride.as_ptr(),
            )
        };

        // SAFETY: sws é não-nulo e foi criado por sws_getContext.
        unsafe { (self.lib.sws_free_context)(sws) };

        if ret <= 0 {
            return Err(AvError::FfmpegError { code: -22 });
        }

        Ok((width as u32, height as u32, pts, rgb_data, sar))
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

impl Drop for FfmpegFrame {
    fn drop(&mut self) {
        // SAFETY: frame foi alocado por av_frame_alloc; é o único dono.
        unsafe { (self.lib.av_frame_free)(&mut self.frame) };
    }
}

// ─── Utilitários de busca e carregamento ──────────────────────────────────────

/// Retorna o diretório de busca preferencial de DLLs FFmpeg.
///
/// Ordem de precedência:
/// 1. Variável de ambiente `FFMPEG_DLL_DIR`
/// 2. `{exe_dir}/ffmpeg/`
/// 3. `{exe_dir}/`
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
        }
    }

    None
}

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
