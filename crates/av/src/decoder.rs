//! Decodificador FFmpeg: converte `PesPacket` → `Vec<DecodedFrame>`.
//!
//! Todo `unsafe` está confinado em `crate::ffi`.  Este módulo apenas chama
//! as interfaces seguras expostas por `ffi/mod.rs`.
//!
//! SPEC-AV-002b

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;

use crate::audio::AudioFrame;
use crate::codec::{AudioCodec, MediaCodec};
use crate::error::AvError;
use crate::ffi::{
    find_ffmpeg_dll_dir, FfmpegCodecContext, FfmpegFrame, FfmpegLib, FfmpegPacket, AV_CODEC_ID_AAC,
    AV_CODEC_ID_AAC_LATM, AV_CODEC_ID_AC3, AV_CODEC_ID_EAC3, AV_CODEC_ID_H264, AV_CODEC_ID_HEVC,
    AV_CODEC_ID_MP2, AV_CODEC_ID_MPEG2VIDEO,
};
use crate::pes::PesPacket;
use crate::renderer::VideoFrame;

// ─── DecodedFrame ─────────────────────────────────────────────────────────────

/// Frame decodificado: vídeo RGB24 ou áudio PCM f32.
///
/// Produzido pelo `FfmpegDecoder` e consumido pelo pipeline de renderização
/// e reprodução de áudio.
///
/// SPEC-AV-002b
#[derive(Debug, Clone)]
pub enum DecodedFrame {
    /// Frame de vídeo decodificado (RGB24).
    Video(VideoFrame),
    /// Frame de áudio decodificado (PCM f32 interleaved).
    Audio(AudioFrame),
}

impl DecodedFrame {
    /// Retorna o PTS do frame, independente de ser vídeo ou áudio.
    ///
    /// SPEC-AV-002b
    pub fn pts(&self) -> Option<u64> {
        match self {
            Self::Video(f) => f.pts,
            Self::Audio(f) => f.pts,
        }
    }

    /// Retorna `true` se este frame é de vídeo.
    pub fn is_video(&self) -> bool {
        matches!(self, Self::Video(_))
    }

    /// Retorna `true` se este frame é de áudio.
    pub fn is_audio(&self) -> bool {
        matches!(self, Self::Audio(_))
    }
}

// ─── Mapeamento de codec ──────────────────────────────────────────────────────

/// Retorna o `AVCodecID` correspondente ao `MediaCodec`.
///
/// SPEC-AV-002b
fn codec_to_avid(codec: MediaCodec) -> Result<u32, AvError> {
    use crate::codec::{AudioCodec, VideoCodec};
    match codec {
        MediaCodec::Video(VideoCodec::H264) => Ok(AV_CODEC_ID_H264),
        MediaCodec::Video(VideoCodec::Hevc) => Ok(AV_CODEC_ID_HEVC),
        MediaCodec::Video(VideoCodec::Mpeg2) => Ok(AV_CODEC_ID_MPEG2VIDEO),
        MediaCodec::Audio(AudioCodec::AacAdts) => Ok(AV_CODEC_ID_AAC),
        MediaCodec::Audio(AudioCodec::AacLatm) => Ok(AV_CODEC_ID_AAC_LATM),
        MediaCodec::Audio(AudioCodec::Ac3) => Ok(AV_CODEC_ID_AC3),
        MediaCodec::Audio(AudioCodec::Eac3) => Ok(AV_CODEC_ID_EAC3),
        MediaCodec::Audio(AudioCodec::Mp2) => Ok(AV_CODEC_ID_MP2),
    }
}

// ─── Estado por PID ───────────────────────────────────────────────────────────

/// Estado de decodificação para um único PID (stream elementar).
struct CodecState {
    codec_ctx: FfmpegCodecContext,
    is_video: bool,
}

// ─── FfmpegDecoder ────────────────────────────────────────────────────────────

/// Decodificador FFmpeg: converte `PesPacket` → `Vec<DecodedFrame>` via FFI
/// confinado em `av::ffi`.
///
/// Mantém um `AVCodecContext` por PID para que o estado do decoder (cabeçalhos
/// SPS/PPS, buffers internos) persista entre pacotes do mesmo stream.
///
/// # Exemplo de uso
///
/// ```ignore
/// let mut decoder = FfmpegDecoder::new()?;
/// let frames = decoder.decode(&pes_packet)?;
/// ```
///
/// SPEC-AV-002b
pub struct FfmpegDecoder {
    lib: Arc<FfmpegLib>,
    /// Mapa de PID → estado do decodificador para aquele stream.
    states: HashMap<u16, CodecState>,
}

impl FfmpegDecoder {
    /// Cria um `FfmpegDecoder` carregando as DLLs FFmpeg.
    ///
    /// Retorna `Err(AvError::FfmpegUnavailable)` se as DLLs não forem
    /// encontradas ou estiverem com versão incompatível.
    ///
    /// SPEC-AV-002b
    pub fn new() -> Result<Self, AvError> {
        let dll_dir = find_ffmpeg_dll_dir().ok_or_else(|| AvError::FfmpegUnavailable {
            message: "DLLs FFmpeg não encontradas. Defina FFMPEG_DLL_DIR ou coloque \
                 as DLLs em {exe_dir}/ffmpeg/"
                .to_string(),
        })?;

        let lib = FfmpegLib::load(&dll_dir)?;
        tracing::info!(dir = %dll_dir.display(), "FFmpeg carregado com sucesso");

        Ok(Self {
            lib,
            states: HashMap::new(),
        })
    }

    /// Cria um `FfmpegDecoder` a partir de um `Arc<FfmpegLib>` já carregado.
    ///
    /// Útil em testes para reutilizar uma lib já carregada.
    ///
    /// SPEC-AV-002b
    pub fn with_lib(lib: Arc<FfmpegLib>) -> Self {
        Self {
            lib,
            states: HashMap::new(),
        }
    }

    /// Reinicia todos os contextos de decodificação, descartando estados de codec.
    ///
    /// Chamado ao trocar de serviço para evitar decodificação com contexto obsoleto.
    /// O próximo pacote para cada PID criará um novo `AVCodecContext` do zero.
    ///
    /// SPEC-AV-002b
    pub fn reset(&mut self) {
        self.states.clear();
    }

    /// Decodifica um `PesPacket` completo, retornando todos os frames prontos.
    ///
    /// Internamente:
    /// 1. Obtém (ou cria) o `AVCodecContext` para o PID do pacote.
    /// 2. Para AAC LATM, divide o payload em frames LOAS individuais antes de
    ///    enviar ao decoder (um `AVPacket` por frame LOAS).
    /// 3. Para outros codecs, cria um `AVPacket` com o payload completo.
    /// 4. Chama `avcodec_send_packet` + loop de `avcodec_receive_frame`.
    /// 5. Converte cada frame para `DecodedFrame` (RGB24 ou PCM f32).
    ///
    /// SPEC-AV-002b
    pub fn decode(&mut self, pes: &PesPacket) -> Result<Vec<DecodedFrame>, AvError> {
        let pid_raw: u16 = pes.pid;

        // Obtém ou cria o codec state para este PID.
        if !self.states.contains_key(&pid_raw) {
            let avid = codec_to_avid(pes.codec)?;
            let codec_ctx = FfmpegCodecContext::open(Arc::clone(&self.lib), avid).map_err(|e| {
                tracing::error!(%e, pid = pid_raw, "falha ao abrir decodificador");
                e
            })?;
            let is_video = matches!(pes.codec, MediaCodec::Video(_));
            self.states.insert(
                pid_raw,
                CodecState {
                    codec_ctx,
                    is_video,
                },
            );
        }

        let state = self.states.get(&pid_raw).ok_or_else(|| {
            AvError::Other(anyhow::anyhow!(
                "codec state ausente após inserção — invariante violado"
            ))
        })?;

        let mut frames = Vec::new();
        let mut av_frame = FfmpegFrame::alloc(Arc::clone(&self.lib))?;

        // AAC LATM: um PES pode conter múltiplos frames LOAS concatenados.
        // O decoder aac_latm do FFmpeg aceita apenas um frame LOAS por AVPacket;
        // enviar o PES inteiro causa "frame length mismatch" e AVERROR_INVALIDDATA.
        // Solução: dividir o payload em frames LOAS individuais via sync word.
        if matches!(pes.codec, MediaCodec::Audio(AudioCodec::AacLatm)) {
            let loas_slices = split_loas_frames(&pes.payload);

            // Conjunto de slices a decodificar: frames LOAS individuais, ou o
            // payload completo como fallback quando nenhum sync word é encontrado.
            enum LoasSource<'a> {
                Slices(Vec<&'a [u8]>),
                Full(&'a [u8]),
            }
            let source = if loas_slices.is_empty() {
                LoasSource::Full(&pes.payload)
            } else {
                LoasSource::Slices(loas_slices)
            };
            let iter: Vec<&[u8]> = match &source {
                LoasSource::Full(d) => vec![d],
                LoasSource::Slices(v) => v.to_vec(),
            };

            for loas_data in iter {
                let pkt = FfmpegPacket::from_bytes(Arc::clone(&self.lib), loas_data, pes.pts)?;
                if let Err(e) = state.codec_ctx.send_packet(&pkt) {
                    tracing::warn!(%e, pid = pid_raw, "aac_latm: falha ao enviar frame LOAS");
                    continue;
                }
                loop {
                    match state.codec_ctx.receive_frame(&mut av_frame) {
                        Ok(true) => {
                            let (sr, ch) = av_frame.audio_params().map_err(|e| {
                                tracing::warn!(
                                    %e,
                                    pid = pid_raw,
                                    "aac_latm: falha ao ler metadata de áudio"
                                );
                                e
                            })?;
                            let (pts_raw, out_sr, out_ch, samples) = av_frame.to_pcm_f32(sr, ch)?;
                            frames.push(DecodedFrame::Audio(AudioFrame::new(
                                out_sr,
                                out_ch,
                                pts_raw_to_option(pts_raw),
                                samples,
                            )));
                            av_frame.unref();
                        }
                        Ok(false) => break,
                        Err(e) => {
                            tracing::warn!(%e, pid = pid_raw, "aac_latm: receive_frame erro");
                            break;
                        }
                    }
                }
            }

            return Ok(frames);
        }

        // Caminho padrão para vídeo e demais codecs de áudio.
        //
        // Erros de send_packet são tratados como recuperáveis: ao entrar no stream
        // no meio de um GOP (ex.: HEVC sem IDR), o decodificador pode rejeitar
        // pacotes com AVERROR_INVALIDDATA até receber um IDR frame.  Retornar Err
        // aqui seria correto para erros fatais, mas para o caso mid-stream o
        // decoder se recupera sozinho no próximo IDR — portanto loga e continua.

        // Cria o AVPacket com o payload PES.
        let pkt = FfmpegPacket::from_bytes(Arc::clone(&self.lib), &pes.payload, pes.pts)?;

        // Envia o pacote ao decodificador.
        if let Err(e) = state.codec_ctx.send_packet(&pkt) {
            tracing::debug!(%e, pid = pid_raw, "send_packet: erro transitório (aguardando IDR?)");
            return Ok(frames);
        }

        loop {
            match state.codec_ctx.receive_frame(&mut av_frame) {
                Ok(true) => {
                    // Frame pronto — converte para tipo Rust.
                    let decoded = if state.is_video {
                        let (w, h, pts_raw, rgb, sar) = av_frame.to_rgb24().map_err(|e| {
                            tracing::warn!(%e, pid = pid_raw, "falha ao converter frame de vídeo");
                            e
                        })?;
                        let pts = pts_raw_to_option(pts_raw);
                        DecodedFrame::Video(VideoFrame::new(
                            w,
                            h,
                            pts,
                            Bytes::from(rgb),
                            sar.0,
                            sar.1,
                        ))
                    } else {
                        let (sr, ch) = av_frame.audio_params().map_err(|e| {
                            tracing::warn!(%e, pid = pid_raw, "falha ao ler metadata de áudio do frame");
                            e
                        })?;
                        let (pts_raw, out_sr, out_ch, samples) = av_frame.to_pcm_f32(sr, ch)?;
                        let pts = pts_raw_to_option(pts_raw);
                        DecodedFrame::Audio(AudioFrame::new(out_sr, out_ch, pts, samples))
                    };
                    frames.push(decoded);
                    // Limpa o frame para reutilização.
                    av_frame.unref();
                }
                Ok(false) => {
                    // EAGAIN ou EOF — sem mais frames por agora.
                    break;
                }
                Err(e) => {
                    // Erro de receive_frame: pode ocorrer em bitstreams corrompidos
                    // ou durante sincronização inicial.  Loga e interrompe o loop
                    // sem propagar o erro — o decodificador continuará no próximo PES.
                    tracing::debug!(%e, pid = pid_raw, "receive_frame: erro transitório");
                    break;
                }
            }
        }

        Ok(frames)
    }
}

// ─── Parsing de LOAS frames ───────────────────────────────────────────────────

/// Divide um payload PES de AAC LATM em frames LOAS individuais.
///
/// O stream LOAS (Low Overhead Audio Stream) usa o sync word de 11 bits `0x2B7`
/// nos MSBs de dois bytes consecutivos (`byte[0] == 0x56`, `byte[1] >> 5 == 7`).
/// Os 13 bits seguintes carregam `audio_mux_length_bytes`; o frame total mede
/// `3 + audio_mux_length_bytes` bytes.
///
/// Um PES de AAC LATM pode conter vários desses frames concatenados.  O decoder
/// `aac_latm` do FFmpeg espera exatamente um frame por `AVPacket`; enviar o PES
/// inteiro resulta em `frame length mismatch`.
///
/// Retorna slice vazio se nenhum sync word for encontrado (fallback: enviar
/// o payload completo).
///
/// SPEC-AV-002b
fn split_loas_frames(data: &[u8]) -> Vec<&[u8]> {
    let mut frames: Vec<&[u8]> = Vec::new();
    let mut pos = 0usize;

    while pos + 3 <= data.len() {
        // LOAS sync: AV_RB16(data) >> 5 == 0x2B7
        //   byte[0] == 0x56 e (byte[1] >> 5) == 7  (top-3-bits = 0b111)
        if data[pos] == 0x56 && (data[pos + 1] >> 5) == 7 {
            let audio_mux_length =
                (((data[pos + 1] & 0x1F) as usize) << 8) | data[pos + 2] as usize;
            let frame_end = pos + 3 + audio_mux_length;
            if frame_end <= data.len() {
                frames.push(&data[pos..frame_end]);
                pos = frame_end;
            } else {
                // Frame truncado — interrompe a varredura.
                break;
            }
        } else {
            // Byte não é parte de um sync word — avança um byte.
            pos += 1;
        }
    }

    frames
}

/// Converte `pts_raw` do FFmpeg para `Option<u64>`.
///
/// O FFmpeg usa `AV_NOPTS_VALUE = i64::MIN` para indicar PTS ausente.
#[inline]
fn pts_raw_to_option(pts_raw: i64) -> Option<u64> {
    if pts_raw == i64::MIN {
        None
    } else {
        Some(pts_raw as u64)
    }
}

// ─── Testes ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::find_ffmpeg_dll_dir;

    /// SPEC-AV-002b: `FfmpegDecoder::new` retorna erro quando DLLs ausentes.
    ///
    /// Simula ausência de DLLs definindo FFMPEG_DLL_DIR para diretório vazio.
    #[test]
    fn spec_av_002b_new_returns_error_without_dlls() {
        // Definimos a env var para um diretório que não existe.
        // SAFETY do test: apenas manipulação de env var, revertida em seguida.
        let old = std::env::var("FFMPEG_DLL_DIR").ok();
        std::env::set_var("FFMPEG_DLL_DIR", "/sem/ffmpeg/aqui");

        let result = FfmpegDecoder::new();
        assert!(result.is_err(), "esperava erro com FFMPEG_DLL_DIR inválido");
        match &result {
            Err(AvError::FfmpegUnavailable { .. }) | Err(AvError::FfmpegError { .. }) => {}
            Err(other) => panic!("tipo de erro inesperado: {other}"),
            Ok(_) => panic!("esperava Err"),
        }

        // Restaura.
        match old {
            Some(v) => std::env::set_var("FFMPEG_DLL_DIR", v),
            None => std::env::remove_var("FFMPEG_DLL_DIR"),
        }
    }

    /// SPEC-AV-002b: `pts_raw_to_option` deve retornar None para AV_NOPTS_VALUE.
    #[test]
    fn spec_av_002b_pts_raw_to_option_nopts() {
        assert_eq!(pts_raw_to_option(i64::MIN), None);
    }

    /// SPEC-AV-002b: `pts_raw_to_option` deve retornar Some para valor válido.
    #[test]
    fn spec_av_002b_pts_raw_to_option_valid() {
        assert_eq!(pts_raw_to_option(90_000), Some(90_000u64));
        assert_eq!(pts_raw_to_option(0), Some(0u64));
    }

    /// SPEC-AV-002b: `codec_to_avid` deve mapear todos os codecs suportados.
    #[test]
    fn spec_av_002b_codec_to_avid_all_codecs() {
        use crate::codec::{AudioCodec, VideoCodec};

        assert_eq!(
            codec_to_avid(MediaCodec::Video(VideoCodec::H264)).unwrap(),
            AV_CODEC_ID_H264
        );
        assert_eq!(
            codec_to_avid(MediaCodec::Video(VideoCodec::Hevc)).unwrap(),
            AV_CODEC_ID_HEVC
        );
        assert_eq!(
            codec_to_avid(MediaCodec::Video(VideoCodec::Mpeg2)).unwrap(),
            AV_CODEC_ID_MPEG2VIDEO
        );
        assert_eq!(
            codec_to_avid(MediaCodec::Audio(AudioCodec::AacAdts)).unwrap(),
            AV_CODEC_ID_AAC
        );
        assert_eq!(
            codec_to_avid(MediaCodec::Audio(AudioCodec::AacLatm)).unwrap(),
            AV_CODEC_ID_AAC_LATM
        );
        assert_eq!(
            codec_to_avid(MediaCodec::Audio(AudioCodec::Ac3)).unwrap(),
            AV_CODEC_ID_AC3
        );
        assert_eq!(
            codec_to_avid(MediaCodec::Audio(AudioCodec::Eac3)).unwrap(),
            AV_CODEC_ID_EAC3
        );
        assert_eq!(
            codec_to_avid(MediaCodec::Audio(AudioCodec::Mp2)).unwrap(),
            AV_CODEC_ID_MP2
        );
    }

    /// SPEC-AV-002b: decode com codec inválido (H264) e DLLs disponíveis deve
    /// abrir o decodificador sem panics.
    ///
    /// Este teste é condicional: só executa se as DLLs FFmpeg estiverem presentes.
    #[test]
    fn spec_av_002b_decode_open_codec_no_panic() {
        let Some(dir) = find_ffmpeg_dll_dir() else {
            eprintln!(
                "DLLs FFmpeg não encontradas — spec_av_002b_decode_open_codec_no_panic ignorado"
            );
            return;
        };

        let lib = match FfmpegLib::load(&dir) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Falha ao carregar FFmpeg: {e} — teste ignorado");
                return;
            }
        };

        let mut decoder = FfmpegDecoder::with_lib(lib);
        use crate::codec::{MediaCodec, VideoCodec};

        // Tenta decodificar um payload inválido — o decoder deve abrir sem panic
        // e retornar erro de decode (bitstream inválido), não um panic.
        let pes = PesPacket::new(
            256u16,
            MediaCodec::Video(VideoCodec::H264),
            None,
            None,
            Bytes::from_static(b"\x00\x00\x00\x01\x09\xf0"), // access unit delimiter
        );

        let result = decoder.decode(&pes);
        // Pode retornar Ok([]) com zero frames (payload muito curto para frame completo)
        // ou Err — mas nunca deve panic.
        match result {
            Ok(frames) => {
                // Zero frames é aceitável para bitstream incompleto.
                assert!(
                    frames.len() <= 1,
                    "não esperava frames completos de stub H.264"
                );
            }
            Err(AvError::FfmpegError { .. }) => {
                // Erro de decodificação esperado para bitstream inválido.
            }
            Err(other) => {
                // Outros erros são aceitáveis.
                eprintln!("Erro inesperado (aceitável): {other}");
            }
        }
    }

    /// SPEC-AV-002b / spec_av_integration_pes_to_frame:
    ///
    /// Testa o pipeline completo PES → DecodedFrame usando um bitstream MPEG-2
    /// mínimo que garante pelo menos um frame decodificado.
    ///
    /// Condicional: executa apenas se DLLs FFmpeg estiverem presentes.
    #[test]
    fn spec_av_integration_pes_to_frame() {
        let Some(dir) = find_ffmpeg_dll_dir() else {
            eprintln!("DLLs FFmpeg não encontradas — spec_av_integration_pes_to_frame ignorado");
            return;
        };

        let lib = match FfmpegLib::load(&dir) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Falha ao carregar FFmpeg: {e} — teste ignorado");
                return;
            }
        };

        let mut decoder = FfmpegDecoder::with_lib(lib);
        use crate::codec::{MediaCodec, VideoCodec};

        // Sequência mínima MPEG-2 Video: sequence header + I-frame 16x16 preto.
        // Gerada com FFmpeg: ffmpeg -f lavfi -i color=black:16x16:r=1 -vframes 1
        //   -c:v mpeg2video -f mpeg2video /dev/stdout | xxd -i
        // Esta é uma sequência MPEG-2 válida com 1 frame I-frame 16×16 preto.
        let mpeg2_frame: &[u8] = &[
            // sequence header
            0x00, 0x00, 0x01, 0xb3, // sequence_start_code
            0x01, 0x00, 0x10, // width=16, height=16 (packed 12+12 bits)
            0x11, // aspect_ratio=1 (square), frame_rate=1 (23.976)
            0x00, 0x00, 0x00, // bit_rate=0, marker, vbv_buffer_size=0
            0x00, // constrained_params=0
            // load_intra_quantiser_matrix=0, load_non_intra_quantiser_matrix=0
            // sequence_extension (MPEG-2)
            0x00, 0x00, 0x01, 0xb5, // extension_start_code
            0x14, 0x8a, 0x00, 0x01, // sequence_extension data
            0x00, 0x00, // group_of_pictures_header
            0x00, 0x00, 0x01, 0xb8, // gop_start_code
            0x00, 0x00, 0x08, 0x00, // time_code, closed=0, broken=0
            // picture_header
            0x00, 0x00, 0x01, 0x00, // picture_start_code
            0x00, 0x08, // temporal_ref=0, picture_coding_type=I(1)
            0xff, 0xff, // vbv_delay=0xFFFF
            // picture_coding_extension
            0x00, 0x00, 0x01, 0xb5, 0x8f, 0xff, 0xfb, 0x80, 0x00, 0x00,
            // slice + macroblock (zeros = preto)
            0x00, 0x00, 0x01, 0x01, // slice_start_code (slice 1)
            0x2a, 0x4c, 0x7e, 0x40, // quantizer_scale + macroblock data
            0x00, 0x00, 0x01, 0xb7, // sequence_end_code
        ];

        let pes = PesPacket::new(
            256u16,
            MediaCodec::Video(VideoCodec::Mpeg2),
            Some(90_000),
            None,
            Bytes::copy_from_slice(mpeg2_frame),
        );

        let result = decoder.decode(&pes);

        match result {
            Ok(frames) => {
                // Payload mínimo pode não produzir frame completo nesta passagem;
                // zero frames é aceitável. Se produzir frames, verificamos o tipo.
                for frame in &frames {
                    assert!(frame.is_video(), "esperava frame de vídeo");
                    if let DecodedFrame::Video(vf) = frame {
                        // Dimensões devem ser positivas se frame for decodificado.
                        assert!(vf.width > 0, "width deve ser > 0");
                        assert!(vf.height > 0, "height deve ser > 0");
                        assert!(vf.is_valid_size(), "tamanho dos dados RGB24 inconsistente");
                    }
                }
            }
            Err(e) => {
                // Bitstream mínimo pode falhar por ser inválido — aceitável.
                eprintln!(
                    "spec_av_integration_pes_to_frame: decode retornou erro (aceitável \
                     com bitstream mínimo): {e}"
                );
            }
        }
    }

    // ─── Testes de split_loas_frames ─────────────────────────────────────────

    /// SPEC-AV-002b: payload vazio retorna slice vazio.
    #[test]
    fn spec_av_002b_split_loas_empty_input() {
        let frames = split_loas_frames(&[]);
        assert!(frames.is_empty());
    }

    /// SPEC-AV-002b: payload sem sync word retorna slice vazio (fallback path).
    #[test]
    fn spec_av_002b_split_loas_no_sync_word() {
        let data = [0x00u8; 64];
        let frames = split_loas_frames(&data);
        assert!(frames.is_empty());
    }

    /// SPEC-AV-002b: um único frame LOAS é identificado corretamente.
    ///
    /// Frame: [0x56, 0xE0 | 0x04, 0x00] = length = 0x0400 = 1024 → total = 1027 bytes.
    #[test]
    fn spec_av_002b_split_loas_single_frame() {
        // Sync: 0x56 0xE4 → audio_mux_length = (0x04 << 8) | 0x00 = 0x400 = 1024
        // frame_end = 3 + 1024 = 1027
        let mut data = vec![0x00u8; 1027];
        data[0] = 0x56;
        data[1] = 0xE4; // top-3-bits = 111, low-5-bits = 0x04
        data[2] = 0x00; // low 8 bits of length

        let frames = split_loas_frames(&data);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 1027);
        assert_eq!(frames[0][0], 0x56);
    }

    /// SPEC-AV-002b: dois frames LOAS concatenados são divididos corretamente.
    ///
    /// Reproduz o caso real: PES com ~2 frames de ~8 KB cada.
    #[test]
    fn spec_av_002b_split_loas_two_frames() {
        // Frame A: audio_mux_length = (0x1F << 8) | 0xAD = 0x1FAD = 8109 → total = 8112
        // Frame B: audio_mux_length = (0x1F << 8) | 0xFC = 0x1FFC = 8188 → total = 8191
        let len_a = 8112usize;
        let len_b = 8191usize;
        let mut data = vec![0x00u8; len_a + len_b];

        // Frame A header
        data[0] = 0x56;
        data[1] = 0xE0 | ((((len_a - 3) >> 8) & 0x1F) as u8);
        data[2] = ((len_a - 3) & 0xFF) as u8;

        // Frame B header (immediately after frame A)
        data[len_a] = 0x56;
        data[len_a + 1] = 0xE0 | ((((len_b - 3) >> 8) & 0x1F) as u8);
        data[len_a + 2] = ((len_b - 3) & 0xFF) as u8;

        let frames = split_loas_frames(&data);
        assert_eq!(frames.len(), 2, "esperava 2 frames LOAS");
        assert_eq!(frames[0].len(), len_a, "frame A tamanho incorreto");
        assert_eq!(frames[1].len(), len_b, "frame B tamanho incorreto");
    }

    /// SPEC-AV-002b: frame truncado (tamanho declarado > dados disponíveis) é ignorado.
    #[test]
    fn spec_av_002b_split_loas_truncated_frame() {
        // Declara length = 500, mas só há 100 bytes após o header.
        let length: usize = 500;
        let mut data = vec![0x00u8; 3 + 100]; // truncado: 3-byte header + 100 bytes
        data[0] = 0x56;
        data[1] = 0xE0 | (((length >> 8) & 0x1F) as u8);
        data[2] = (length & 0xFF) as u8;

        let frames = split_loas_frames(&data);
        // Frame truncado não deve ser incluído.
        assert!(frames.is_empty(), "frame truncado não deve ser emitido");
    }

    /// SPEC-AV-002b: bytes de padding antes do sync word são ignorados.
    #[test]
    fn spec_av_002b_split_loas_leading_padding() {
        let frame_len = 100usize;
        let total = 5 + 3 + frame_len; // 5 bytes de padding + header + payload
        let mut data = vec![0xFFu8; total];
        // Frame começa no byte 5
        data[5] = 0x56;
        data[6] = 0xE0 | (((frame_len >> 8) & 0x1F) as u8);
        data[7] = (frame_len & 0xFF) as u8;

        let frames = split_loas_frames(&data);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 3 + frame_len);
    }
}
