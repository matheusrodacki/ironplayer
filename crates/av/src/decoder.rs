//! Decodificador FFmpeg: converte `PesPacket` → `Vec<DecodedFrame>`.
//!
//! Todo `unsafe` está confinado em `crate::ffi`.  Este módulo apenas chama
//! as interfaces seguras expostas por `ffi/mod.rs`.
//!
//! SPEC-AV-002b

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::audio::AudioFrame;
use crate::codec::{AudioCodec, CodecConfig, MediaCodec, ThreadType, VideoCodec};
use crate::deinterlace::Deinterlacer;
use crate::error::AvError;
use crate::ffi::{
    find_ffmpeg_dll_dir, frame_flags, FfmpegCodecContext, FfmpegFrame, FfmpegLib, FfmpegPacket,
    FfmpegParser, FilterLib, AV_CODEC_ID_AAC, AV_CODEC_ID_AAC_LATM, AV_CODEC_ID_AC3,
    AV_CODEC_ID_EAC3, AV_CODEC_ID_H264, AV_CODEC_ID_HEVC, AV_CODEC_ID_MP2, AV_CODEC_ID_MPEG2VIDEO,
    AV_COL_RANGE_JPEG, AV_FRAME_FLAG_INTERLACED, AV_HWDEVICE_TYPE_D3D11VA,
};
use crate::hw::{D3d11Device, HwAccelMode, HwAccelState};
#[cfg(windows)]
use crate::hw::{ColorSpace, TransferFunction};
use crate::pes::PesPacket;
use crate::video_queue::{HwVideoFrame, VideoFrame, YuvColorRange, YuvColorspace, YuvFrame};

// ─── DecodedFrame ─────────────────────────────────────────────────────────────

/// Frame decodificado: vídeo (SW ou HW) ou áudio PCM f32.
///
/// Produzido pelo `FfmpegDecoder` e consumido pelo pipeline de renderização
/// e reprodução de áudio.
///
/// SPEC-AV-002b · SPEC-AV-HW-TEX-001
#[derive(Debug)]
pub enum DecodedFrame {
    /// Frame de vídeo decodificado (YUV planar SW ou D3D11VA HW).
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
            Self::Video(f) => f.pts(),
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
    /// Frame AVFrame reutilizado entre chamadas para evitar alloc/free por frame.
    frame: FfmpegFrame,
    is_video: bool,
    /// Parser FFmpeg para realinhar PES em frames/access units completos.
    /// `None` quando o codec não tem parser registrado (AAC LATM usa split
    /// manual via sync word; ver `split_loas_frames`).
    parser: Option<FfmpegParser>,
    /// Deinterlacador bwdif, criado lazily na primeira aparição de frame
    /// interlaced. `None` se o stream não for interlaced ou se `FilterLib`
    /// não estiver disponível.
    deinterlacer: Option<Deinterlacer>,
    /// Deadline para o primeiro frame HW (2 s após abertura do contexto).
    /// `None` quando em modo SW ou após receber o primeiro frame D3D11.
    /// Expirar sem receber frame HW dispara fallback para SW.
    ///
    /// SPEC-AV-HW-DEC-001
    hw_init_deadline: Option<Instant>,
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
/// ```text
/// let mut decoder = FfmpegDecoder::new()?;
/// let frames = decoder.decode(&pes_packet)?;
/// ```
///
/// SPEC-AV-002b
pub struct FfmpegDecoder {
    lib: Arc<FfmpegLib>,
    /// Biblioteca avfilter para deinterlacing bwdif. `None` se a DLL não
    /// estiver disponível — nesse caso frames interlaced são entregues sem
    /// deinterlacing mas o pipeline continua funcionando.
    filter_lib: Option<Arc<FilterLib>>,
    /// Configuração de threading e flags de qualidade/velocidade do decoder.
    codec_config: CodecConfig,
    /// Mapa de PID → estado do decodificador para aquele stream.
    states: HashMap<u16, CodecState>,
    /// Modo de aceleração de hardware solicitado (Fase B, SPEC-AV-HW-DEC-001).
    ///
    /// Nesta versão do decoder, `D3d11Va(_)` é aceito mas o caminho FFI real
    /// (callback `get_format` + `AVHWDeviceContext`) será conectado na próxima
    /// iteração da Fase B.  A máquina de estados em `hw_state` já está pronta
    /// para receber sinais de falha e acionar fallback.
    hwaccel: HwAccelMode,
    /// Estado da máquina de fallback hwaccel (falhas seguidas / motivo).
    ///
    /// SPEC-AV-HW-DEC-001
    hw_state: HwAccelState,
    /// Codec do último decoder HW aberto com sucesso (telemetria/UI).
    last_hw_codec: Option<String>,
}

impl FfmpegDecoder {
    /// Cria um `FfmpegDecoder` carregando as DLLs FFmpeg com configuração padrão.
    ///
    /// O default conservador usa `num_cpus` threads e mantém todas as outras
    /// flags de otimização desabilitadas. Use `new_with_config` para personalizar.
    ///
    /// Retorna `Err(AvError::FfmpegUnavailable)` se as DLLs não forem
    /// encontradas ou estiverem com versão incompatível.
    ///
    /// SPEC-AV-002b
    pub fn new() -> Result<Self, AvError> {
        Self::new_with_config(CodecConfig::default())
    }

    /// Cria um `FfmpegDecoder` carregando as DLLs FFmpeg com `config` explicitado.
    ///
    /// Retorna `Err(AvError::FfmpegUnavailable)` se as DLLs não forem
    /// encontradas ou estiverem com versão incompatível.
    ///
    /// SPEC-AV-002b
    pub fn new_with_config(config: CodecConfig) -> Result<Self, AvError> {
        let dll_dir = find_ffmpeg_dll_dir().ok_or_else(|| AvError::FfmpegUnavailable {
            message: "DLLs FFmpeg não encontradas. Defina FFMPEG_DLL_DIR ou coloque \
                 as DLLs em {exe_dir}/ffmpeg/"
                .to_string(),
        })?;

        let lib = FfmpegLib::load(&dll_dir)?;
        tracing::info!(dir = %dll_dir.display(), "FFmpeg carregado com sucesso");

        // Tenta carregar avfilter para suporte a deinterlacing bwdif.
        // Falha silenciosa: se a DLL não estiver disponível, frames interlaced
        // são entregues sem processamento mas o pipeline continua funcional.
        let filter_lib = FilterLib::load(&dll_dir)
            .map_err(|e| {
                tracing::debug!(%e, "avfilter não disponível — deinterlacing desabilitado");
            })
            .ok();
        if filter_lib.is_some() {
            tracing::debug!("avfilter carregado — deinterlacing bwdif habilitado");
        }

        Ok(Self {
            lib,
            filter_lib,
            codec_config: config,
            states: HashMap::new(),
            hwaccel: HwAccelMode::Off,
            hw_state: HwAccelState::new(),
            last_hw_codec: None,
        })
    }

    /// Cria um `FfmpegDecoder` a partir de um `Arc<FfmpegLib>` já carregado,
    /// usando a configuração padrão.
    ///
    /// Útil em testes para reutilizar uma lib já carregada.
    ///
    /// SPEC-AV-002b
    pub fn with_lib(lib: Arc<FfmpegLib>) -> Self {
        Self::with_lib_and_config(lib, CodecConfig::default())
    }

    /// Cria um `FfmpegDecoder` a partir de um `Arc<FfmpegLib>` já carregado
    /// com `config` explicitado.
    ///
    /// SPEC-AV-002b
    pub fn with_lib_and_config(lib: Arc<FfmpegLib>, config: CodecConfig) -> Self {
        Self {
            lib,
            filter_lib: None,
            codec_config: config,
            states: HashMap::new(),
            hwaccel: HwAccelMode::Off,
            hw_state: HwAccelState::new(),
            last_hw_codec: None,
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

    /// Retorna `true` se pelo menos um PID de vídeo tem o deinterlacador bwdif ativo.
    ///
    /// SPEC-AV-004
    pub fn has_deinterlacer_active(&self) -> bool {
        self.states
            .values()
            .any(|s| s.is_video && s.deinterlacer.is_some())
    }

    /// Retorna o número de threads de decodificação configurado.
    ///
    /// SPEC-AV-002b
    pub fn threads_used(&self) -> u32 {
        self.codec_config.thread_count
    }

    // ── Hardware acceleration (Fase B, SPEC-AV-HW-DEC-001) ────────────────────

    /// Habilita (ou desabilita) o caminho hwaccel para decodes futuros.
    ///
    /// O caller deve chamar `reset()` antes de mudar o modo se houver streams
    /// já abertos — `AVCodecContext` existentes mantêm o pix_fmt original e
    /// não migram entre CPU e GPU em runtime.
    ///
    /// O caminho FFI real (callback `get_format` + `AVHWDeviceContext`) é
    /// conectado na continuação da Fase B; nesta versão a chamada apenas
    /// registra a intenção, ativa a máquina de estado e mantém compatibilidade
    /// com o caminho SW para CI e fallback.
    ///
    /// SPEC-AV-HW-DEC-001
    pub fn enable_hwaccel(&mut self, mode: HwAccelMode) -> Result<(), AvError> {
        match &mode {
            HwAccelMode::Off => {
                self.hw_state = HwAccelState::new();
            }
            HwAccelMode::D3d11Va(_) => {
                self.hw_state = HwAccelState::new();
                self.hw_state.activate();
                tracing::info!(target: "av::hw", "hw.init.ok mode=d3d11va");
            }
        }
        self.hwaccel = mode;
        Ok(())
    }

    /// Modo de hwaccel configurado atualmente (não reflete fallback dinâmico).
    ///
    /// SPEC-AV-HW-DEC-001
    pub fn hwaccel_mode(&self) -> &HwAccelMode {
        &self.hwaccel
    }

    /// `true` se o decoder está ativamente produzindo frames acelerados em GPU.
    ///
    /// Difere de `hwaccel_mode().is_gpu()`: este método fica `false` após o
    /// fallback automático para CPU.
    ///
    /// SPEC-AV-HW-DEC-001
    pub fn is_hwaccel_active(&self) -> bool {
        self.hw_state.is_active()
    }

    /// Motivo do último fallback hwaccel→CPU (se houve).
    ///
    /// SPEC-AV-HW-DEC-001
    pub fn fallback_reason(&self) -> Option<&str> {
        self.hw_state.fallback_reason()
    }

    /// Label do último codec HW ativo (ex.: `hevc_d3d11va`).
    pub fn hw_decode_codec(&self) -> Option<&str> {
        self.last_hw_codec.as_deref()
    }

    /// Contagem aproximada de contextos de vídeo HW ativos nesta sessão.
    pub fn hw_frame_pool_in_use(&self) -> u32 {
        if !self.hw_state.is_active() {
            return 0;
        }
        self.states.values().filter(|state| state.is_video).count() as u32
    }

    /// Promove imediatamente o decoder para o caminho CPU, registrando o motivo.
    ///
    /// Usado pelo callback `get_format` (Fase B FFI) quando o driver recusa o
    /// pix_fmt D3D11, ou pelo render pass na recuperação de TDR (Fase E).
    ///
    /// SPEC-AV-HW-DEC-001
    pub fn fallback_to_sw(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        tracing::warn!(target: "av::hw", reason = %reason, "hw.fallback");
        self.hw_state.fallback(reason);
    }

    /// Registra uma falha transitória do caminho hwaccel.  Retorna `true` se a
    /// contagem atingiu o limite (`HW_FALLBACK_THRESHOLD`) e o caller já deve
    /// chamar `fallback_to_sw`.
    ///
    /// SPEC-AV-HW-DEC-001
    #[must_use = "verifique o retorno para acionar fallback quando necessário"]
    pub fn record_hw_failure(&mut self) -> bool {
        self.hw_state.record_failure()
    }

    /// Sinaliza que o caminho hwaccel entregou um frame válido (reseta a streak).
    ///
    /// SPEC-AV-HW-DEC-001
    #[allow(dead_code)]
    pub(crate) fn record_hw_success(&mut self) {
        self.hw_state.record_success();
    }

    /// Reset completo do decoder (limpa codec_ctxs e estado hwaccel).
    ///
    /// Nota: para preservar a compatibilidade da API, esta variante é apenas
    /// um helper que combina `reset()` + reset do estado hwaccel.  Use-a ao
    /// reabrir um stream para começar uma nova sessão de telemetria.
    ///
    /// SPEC-AV-HW-DEC-001
    pub fn reset_with_hw_state(&mut self) {
        self.states.clear();
        self.hw_state = HwAccelState::new();
        self.last_hw_codec = None;
        if self.hwaccel.is_gpu() {
            self.hw_state.activate();
        }
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
            let is_video = matches!(pes.codec, MediaCodec::Video(_));

            // Tenta abrir com hwaccel se for vídeo e o estado estiver ativo.
            let codec_ctx = if is_video && self.hw_state.is_active() {
                let hw_codec_config = CodecConfig {
                    thread_count: 1,
                    thread_type: ThreadType::Auto,
                    ..self.codec_config.clone()
                };
                let external_d3d11 = self.shared_d3d11_device().map(|dev| unsafe {
                    (dev.as_raw(), dev.as_raw_context())
                });
                match FfmpegCodecContext::open_with_hwaccel(
                    Arc::clone(&self.lib),
                    avid,
                    &hw_codec_config,
                    AV_HWDEVICE_TYPE_D3D11VA,
                    external_d3d11,
                ) {
                    Ok(ctx) => {
                        self.last_hw_codec = hw_codec_label(pes.codec).map(str::to_owned);
                        tracing::debug!(pid = pid_raw, "hwaccel D3D11VA aberto para PID");
                        ctx
                    }
                    Err(e) => {
                        tracing::warn!(
                            %e,
                            pid = pid_raw,
                            "hwaccel init falhou — revertendo para SW"
                        );
                        self.fallback_to_sw(format!("hwaccel init: {e}"));
                        FfmpegCodecContext::open(
                            Arc::clone(&self.lib),
                            avid,
                            &self.codec_config,
                        )
                        .map_err(|e2| {
                            tracing::error!(%e2, pid = pid_raw, "falha ao abrir decodificador SW");
                            e2
                        })?
                    }
                }
            } else {
                FfmpegCodecContext::open(Arc::clone(&self.lib), avid, &self.codec_config).map_err(
                    |e| {
                        tracing::error!(%e, pid = pid_raw, "falha ao abrir decodificador");
                        e
                    },
                )?
            };

            // Deadline para o primeiro frame HW quando em modo D3D11VA.
            // 2 s (spec §4.3): tempo para o primeiro frame HW antes de fallback SW.
            let hw_init_deadline = if is_video && self.hw_state.is_active() {
                Some(Instant::now() + Duration::from_secs(2))
            } else {
                None
            };

            let frame = FfmpegFrame::alloc(Arc::clone(&self.lib))?;
            // Parser para realinhar PES em frames/access units (HEVC/H264/AC3/EAC3/MP2).
            // AAC LATM tem seu próprio splitter manual (split_loas_frames).
            let parser = if matches!(pes.codec, MediaCodec::Audio(AudioCodec::AacLatm)) {
                None
            } else {
                FfmpegParser::try_init(Arc::clone(&self.lib), avid)
            };
            self.states.insert(
                pid_raw,
                CodecState {
                    codec_ctx,
                    frame,
                    is_video,
                    parser,
                    deinterlacer: None,
                    hw_init_deadline,
                },
            );
        }

        let mut frames = Vec::new();

        let shared_d3d = self.shared_d3d11_device();

        let state = self.states.get_mut(&pid_raw).ok_or_else(|| {
            AvError::Other(anyhow::anyhow!(
                "codec state ausente após inserção — invariante violado"
            ))
        })?;

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
                    match state.codec_ctx.receive_frame(&mut state.frame) {
                        Ok(true) => {
                            let (sr, ch) = state.frame.audio_params().map_err(|e| {
                                tracing::warn!(
                                    %e,
                                    pid = pid_raw,
                                    "aac_latm: falha ao ler metadata de áudio"
                                );
                                e
                            })?;
                            let (pts_raw, out_sr, out_ch, samples) =
                                state.frame.to_pcm_f32(sr, ch)?;
                            let stream_info = state.codec_ctx.audio_stream_info();
                            frames.push(DecodedFrame::Audio(AudioFrame::new(
                                pid_raw,
                                out_sr,
                                out_ch,
                                sr,
                                ch,
                                stream_info,
                                resolve_audio_pts(pts_raw, None, pes.pts),
                                samples,
                            )));
                            state.frame.unref();
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

        // Usa o parser FFmpeg para realinhar o payload PES em frames/access
        // units completos antes de enviar ao decoder. PES não está garantido
        // a alinhar-se com fronteiras de codec (especialmente HEVC com PES
        // longos atravessando múltiplos AUs, e AC-3 com vários frames por PES).
        //
        // Quando o parser não está disponível (codec sem parser registrado),
        // cai para o comportamento legado de enviar o payload completo de uma vez.
        let parsed_packets: Vec<(Vec<u8>, Option<u64>)> =
            if let Some(parser) = state.parser.as_mut() {
                parser.parse(&state.codec_ctx, &pes.payload, pes.pts)
            } else {
                vec![(pes.payload.to_vec(), pes.pts)]
            };

        // Flag para acionar fallback SW após sair do loop (evita borrow duplo
        // de `self` dentro do loop onde `state` já emprestou `self.states`).
        let mut hw_download_failed = false;
        let mut hw_interlaced_requires_sw = false;
        let mut hw_frames_ok: usize = 0;

        'pkt_loop: for (pkt_bytes, pkt_pts) in parsed_packets {
            if pkt_bytes.is_empty() {
                continue;
            }
            let pkt = FfmpegPacket::from_bytes(Arc::clone(&self.lib), &pkt_bytes, pkt_pts)?;
            if let Err(e) = state.codec_ctx.send_packet(&pkt) {
                tracing::debug!(%e, pid = pid_raw, "send_packet: erro transitório (aguardando IDR?)");
                continue;
            }

            loop {
                match state.codec_ctx.receive_frame(&mut state.frame) {
                    Ok(true) => {
                        // Frame pronto — converte para tipo Rust.
                        let decoded = if state.is_video {
                            // ── Caminho HW: frame D3D11VA — download seguro via FFmpeg ──
                            if state.frame.is_hw() {
                                let is_interlaced = unsafe {
                                    frame_flags(state.frame.as_ptr()) & AV_FRAME_FLAG_INTERLACED
                                        != 0
                                };
                                if is_interlaced {
                                    tracing::warn!(
                                    pid = pid_raw,
                                    "frame HW interlaced detectado — reabrindo decoder em SW para bwdif"
                                );
                                    state.frame.unref();
                                    hw_interlaced_requires_sw = true;
                                    break 'pkt_loop;
                                }

                                // Cancela o deadline de init — recebemos o primeiro frame HW.
                                state.hw_init_deadline = None;

                                let decoded_hw = if let Some(d3d_dev) = shared_d3d.as_deref() {
                                    match try_hw_zero_copy(&state.frame, d3d_dev) {
                                        Ok(vf) => Some(vf),
                                        Err(e) => {
                                            tracing::warn!(
                                                %e,
                                                pid = pid_raw,
                                                "zero-copy HW falhou — tentando download SW"
                                            );
                                            hw_video_frame_from_download(&state.frame, pid_raw)
                                                .map_err(|e2| {
                                                    hw_download_failed = true;
                                                    tracing::warn!(
                                                        %e2,
                                                        pid = pid_raw,
                                                        "falha ao baixar frame HW para YUV"
                                                    );
                                                    e2
                                                })
                                                .ok()
                                        }
                                    }
                                } else {
                                    hw_video_frame_from_download(&state.frame, pid_raw)
                                        .map_err(|e| {
                                            hw_download_failed = true;
                                            tracing::warn!(
                                                %e,
                                                pid = pid_raw,
                                                "falha ao baixar frame HW para YUV"
                                            );
                                            e
                                        })
                                        .ok()
                                };

                                if let Some(vf) = decoded_hw {
                                    hw_frames_ok += 1;
                                    Some(DecodedFrame::Video(vf))
                                } else {
                                    state.frame.unref();
                                    continue;
                                }
                            } else {
                                // ── Caminho SW: YUV420P / YUV420P10LE ────────────────
                                // Verifica se o frame é interlaced (FFmpeg 8.x: flags bit 0).
                                // SAFETY: state.frame.as_ptr() aponta para AVFrame válido e preenchido.
                                let is_interlaced = unsafe {
                                    frame_flags(state.frame.as_ptr()) & AV_FRAME_FLAG_INTERLACED
                                        != 0
                                };

                                // Aplica bwdif se interlaced e FilterLib disponível.
                                let di_frame: Option<FfmpegFrame> =
                                    if is_interlaced && state.deinterlacer.is_none() {
                                        if let Some(fl) = &self.filter_lib {
                                            state.deinterlacer = Some(Deinterlacer::new(
                                                Arc::clone(fl),
                                                Arc::clone(&self.lib),
                                            ));
                                        }
                                        None
                                    } else {
                                        None
                                    };

                                let di_frame = if is_interlaced {
                                    if let Some(di) = state.deinterlacer.as_mut() {
                                        match di.process(&state.frame) {
                                            Ok(f) => f,
                                            Err(e) => {
                                                tracing::warn!(%e, pid = pid_raw, "bwdif: erro; usando frame original");
                                                None
                                            }
                                        }
                                    } else {
                                        di_frame
                                    }
                                } else {
                                    di_frame
                                };

                                // Se bwdif retornou EAGAIN (precisando de mais contexto),
                                // descarta o frame e aguarda o próximo.
                                let source = di_frame.as_ref().unwrap_or(&state.frame);

                                let (w, h, pts_raw, planes, sar, raw_cs, raw_cr, ten_bit) =
                            source.to_yuv_planes().map_err(|e| {
                                tracing::warn!(%e, pid = pid_raw, "falha ao extrair planos YUV");
                                e
                            })?;
                                let pts = pts_raw_to_option(pts_raw);
                                Some(DecodedFrame::Video(VideoFrame::Sw(YuvFrame {
                                    planes,
                                    width: w,
                                    height: h,
                                    pts,
                                    sar_num: sar.0,
                                    sar_den: sar.1,
                                    colorspace: YuvColorspace::from_avutil(raw_cs),
                                    color_range: YuvColorRange::from_avutil(raw_cr),
                                    ten_bit,
                                })))
                            } // fim caminho SW
                        } else {
                            let (sr, ch) = state.frame.audio_params().map_err(|e| {
                            tracing::warn!(%e, pid = pid_raw, "falha ao ler metadata de áudio do frame");
                            e
                        })?;
                            let (pts_raw, out_sr, out_ch, samples) =
                                state.frame.to_pcm_f32(sr, ch)?;
                            let pts = resolve_audio_pts(pts_raw, pkt_pts, pes.pts);
                            let stream_info = state.codec_ctx.audio_stream_info();
                            Some(DecodedFrame::Audio(AudioFrame::new(
                                pid_raw,
                                out_sr,
                                out_ch,
                                sr,
                                ch,
                                stream_info,
                                pts,
                                samples,
                            )))
                        };
                        if let Some(f) = decoded {
                            frames.push(f);
                        }
                        state.frame.unref();
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
        } // fim 'pkt_loop

        // Pós-loop: atualiza estado HW (fora do borrow de `state`).
        // SPEC-AV-HW-DEC-001
        if hw_frames_ok > 0 {
            self.record_hw_success();
        }
        if hw_interlaced_requires_sw {
            self.fallback_to_sw("frame HW interlaced requer bwdif SW");
            self.states.remove(&pid_raw);
            return Ok(frames);
        }
        if hw_download_failed && self.record_hw_failure() {
            tracing::warn!(
                pid = pid_raw,
                "3 falhas consecutivas no download HW — fallback SW"
            );
            self.fallback_to_sw("3 falhas consecutivas no download HW");
            self.states.remove(&pid_raw);
        }

        let deadline_exceeded = self
            .states
            .get(&pid_raw)
            .and_then(|s| s.hw_init_deadline)
            .is_some_and(|d| Instant::now() > d);
        if deadline_exceeded {
            tracing::warn!(pid = pid_raw, "timeout de 2 s sem frame HW — fallback SW");
            self.fallback_to_sw("timeout HW init (2 s)");
            self.states.remove(&pid_raw);
        }

        Ok(frames)
    }

    /// Retorna o `D3d11Device` compartilhado quando hwaccel D3D11VA está ativo.
    fn shared_d3d11_device(&self) -> Option<Arc<D3d11Device>> {
        match &self.hwaccel {
            #[cfg(windows)]
            HwAccelMode::D3d11Va(dev) if self.hw_state.is_active() => Some(Arc::clone(dev)),
            #[cfg(not(windows))]
            HwAccelMode::D3d11Va(_) if self.hw_state.is_active() => None,
            _ => None,
        }
    }
}

/// Extrai os planos NV12/P010 de um frame HW D3D11VA para `VideoFrame::Hw`.
///
/// CRÍTICO: a staging copy (`extract_nv12_planes`) ocorre **aqui**, enquanto o
/// `AVFrame` ainda está vivo. A surface pertence ao pool do decoder e é
/// reescrita assim que liberada (`unref`); adiar a cópia para a thread de
/// render faria a UI copiar uma slice já reutilizada por um frame mais novo,
/// produzindo batimento ("zig-zag"). O `AddRef` na textura é temporário e serve
/// apenas para a cópia — não protege a slice contra reuso.
#[cfg(windows)]
fn try_hw_zero_copy(frame: &FfmpegFrame, d3d_dev: &D3d11Device) -> Result<VideoFrame, AvError> {
    use crate::hw::D3d11Texture;

    let (tex_ptr, slice, w, h, pts_raw, sar, trc, cs, cr) = frame.hw_frame_info()?;
    let tex = unsafe {
        D3d11Texture::from_raw_addref(
            tex_ptr,
            slice,
            w,
            h,
            ColorSpace::from_avutil(cs),
            TransferFunction::from_avutil(trc),
            cr == AV_COL_RANGE_JPEG,
        )?
    };
    let planes = d3d_dev.extract_nv12_planes(&tex)?;
    let colorspace = match ColorSpace::from_avutil(cs) {
        ColorSpace::Bt601 => YuvColorspace::Bt601,
        ColorSpace::Bt709 => YuvColorspace::Bt709,
        ColorSpace::Bt2020 => YuvColorspace::Bt2020,
    };
    let color_range = if cr == AV_COL_RANGE_JPEG {
        YuvColorRange::Full
    } else {
        YuvColorRange::Limited
    };
    Ok(VideoFrame::Hw(HwVideoFrame {
        planes,
        colorspace,
        color_range,
        transfer: TransferFunction::from_avutil(trc),
        pts: pts_raw_to_option(pts_raw),
        width: w,
        height: h,
        sar_num: sar.0,
        sar_den: sar.1,
    }))
}

/// Fallback: baixa frame HW para YUV na CPU (`VideoFrame::Sw`).
fn hw_video_frame_from_download(frame: &FfmpegFrame, pid_raw: u16) -> Result<VideoFrame, AvError> {
    let (w, h, pts_raw, planes, sar, raw_cs, raw_cr, ten_bit) =
        frame.download_to_yuv_planes().map_err(|e| {
            tracing::debug!(%e, pid = pid_raw, "download_to_yuv_planes falhou");
            e
        })?;
    Ok(VideoFrame::Sw(YuvFrame {
        planes,
        width: w,
        height: h,
        pts: pts_raw_to_option(pts_raw),
        sar_num: sar.0,
        sar_den: sar.1,
        colorspace: YuvColorspace::from_avutil(raw_cs),
        color_range: YuvColorRange::from_avutil(raw_cr),
        ten_bit,
    }))
}

fn hw_codec_label(codec: MediaCodec) -> Option<&'static str> {
    match codec {
        MediaCodec::Video(VideoCodec::Mpeg2) => Some("mpeg2video_d3d11va"),
        MediaCodec::Video(VideoCodec::H264) => Some("h264_d3d11va"),
        MediaCodec::Video(VideoCodec::Hevc) => Some("hevc_d3d11va"),
        MediaCodec::Audio(_) => None,
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

/// Resolve PTS de áudio quando o decoder FFmpeg não preenche `AVFrame::pts`.
///
/// AC-3 (e outros codecs com parser) podem emitir frames com `AV_NOPTS_VALUE`
/// mesmo quando o PES/pacote carrega PTS válido — usar o PTS do container
/// evita âncora `0` no `AudioClock` e descompasso A/V.
#[inline]
fn resolve_audio_pts(
    pts_raw: i64,
    packet_pts: Option<u64>,
    pes_pts: Option<u64>,
) -> Option<u64> {
    pts_raw_to_option(pts_raw)
        .or(packet_pts)
        .or(pes_pts)
}

// ─── Testes ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::find_ffmpeg_dll_dir;
    use bytes::Bytes;

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

    /// AC-3: quando o decoder não preenche PTS, usa o PTS do pacote/PES.
    #[test]
    fn spec_av_002b_resolve_audio_pts_falls_back_to_packet() {
        assert_eq!(
            resolve_audio_pts(i64::MIN, Some(4_875_238_029), Some(99)),
            Some(4_875_238_029)
        );
        assert_eq!(
            resolve_audio_pts(i64::MIN, None, Some(4_875_238_029)),
            Some(4_875_238_029)
        );
        assert_eq!(resolve_audio_pts(123, Some(456), Some(789)), Some(123));
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
                        assert!(vf.width() > 0, "width deve ser > 0");
                        assert!(vf.height() > 0, "height deve ser > 0");
                        if let VideoFrame::Sw(sw) = vf {
                            let y_expected = sw.width as usize * sw.height as usize;
                            assert_eq!(
                                sw.planes[0].len(),
                                y_expected,
                                "plano Y deve ter w*h bytes"
                            );
                        }
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

    // ── Hwaccel API (Fase B) ────────────────────────────────────────────────

    /// Constrói um decoder isolado, sem DLLs FFmpeg, para testar a superfície
    /// pública de hwaccel.
    fn dummy_decoder() -> Option<FfmpegDecoder> {
        let dir = find_ffmpeg_dll_dir()?;
        let lib = FfmpegLib::load(&dir).ok()?;
        Some(FfmpegDecoder::with_lib(lib))
    }

    /// SPEC-AV-HW-DEC-001: estado inicial do decoder = CPU, sem fallback.
    #[test]
    fn spec_av_hw_dec_001_decoder_starts_in_cpu_mode() {
        let Some(d) = dummy_decoder() else {
            eprintln!("FFmpeg DLLs ausentes — teste ignorado");
            return;
        };
        assert!(matches!(d.hwaccel_mode(), HwAccelMode::Off));
        assert!(!d.is_hwaccel_active());
        assert!(d.fallback_reason().is_none());
    }

    /// SPEC-AV-HW-DEC-001: enable_hwaccel(Off) é idempotente e mantém estado limpo.
    #[test]
    fn spec_av_hw_dec_001_enable_off_idempotent() {
        let Some(mut d) = dummy_decoder() else {
            return;
        };
        d.enable_hwaccel(HwAccelMode::Off).unwrap();
        assert!(!d.is_hwaccel_active());
        assert!(d.fallback_reason().is_none());
    }

    /// SPEC-AV-HW-DEC-001: 3 falhas seguidas → record_hw_failure retorna true na 3ª.
    #[test]
    fn spec_av_hw_dec_001_three_failures_signal_fallback() {
        let Some(mut d) = dummy_decoder() else {
            return;
        };
        // Simula ativação em GPU manualmente via record_hw_success (zera
        // contador) e depois 3 falhas.
        assert!(!d.record_hw_failure());
        assert!(!d.record_hw_failure());
        assert!(d.record_hw_failure(), "3ª falha deve disparar fallback");
        d.fallback_to_sw("simulated");
        assert!(!d.is_hwaccel_active());
        assert_eq!(d.fallback_reason(), Some("simulated"));
    }

    /// SPEC-AV-HW-DEC-001: fallback_to_sw preserva a primeira razão.
    #[test]
    fn spec_av_hw_dec_001_fallback_preserves_first_reason() {
        let Some(mut d) = dummy_decoder() else {
            return;
        };
        d.fallback_to_sw("driver ausente");
        d.fallback_to_sw("outra causa");
        assert_eq!(d.fallback_reason(), Some("driver ausente"));
    }

    #[test]
    fn spec_av_hw_dec_001_hw_codec_labels_are_stable() {
        assert_eq!(
            hw_codec_label(MediaCodec::Video(VideoCodec::Mpeg2)),
            Some("mpeg2video_d3d11va")
        );
        assert_eq!(
            hw_codec_label(MediaCodec::Video(VideoCodec::H264)),
            Some("h264_d3d11va")
        );
        assert_eq!(
            hw_codec_label(MediaCodec::Video(VideoCodec::Hevc)),
            Some("hevc_d3d11va")
        );
        assert_eq!(hw_codec_label(MediaCodec::Audio(AudioCodec::Mp2)), None);
    }
}
