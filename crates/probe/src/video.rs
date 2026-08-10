//! Contratos e análise determinística de vídeo para o modo Probe.
//!
//! O módulo recebe observações compactas do decoder, nunca frames de renderização.
//! Nenhuma ausência de observação é convertida em freeze ou black frame.
//!
//! SPEC-PROBE-VID-001 · SPEC-PROBE-VID-004 · SPEC-PROBE-VID-005

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use ts::Pid;

/// Codec de vídeo que o analisador conhece.
///
/// SPEC-PROBE-VID-002
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    Mpeg2,
    H264,
    Hevc,
    Unsupported(u8),
}

impl VideoCodec {
    /// Constrói o codec a partir do `stream_type` da PMT.
    ///
    /// SPEC-PROBE-VID-003
    pub fn from_stream_type(stream_type: u8) -> Self {
        match stream_type {
            0x02 => Self::Mpeg2,
            0x1b => Self::H264,
            0x24 => Self::Hevc,
            other => Self::Unsupported(other),
        }
    }

    /// Indica se existe parser/decoder previsto nesta versão.
    ///
    /// SPEC-PROBE-VID-003
    pub fn is_supported(self) -> bool {
        !matches!(self, Self::Unsupported(_))
    }
}

/// Aspect ratio sinalizado pelo elementary stream.
///
/// SPEC-PROBE-VID-002
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AspectRatio {
    pub num: u32,
    pub den: u32,
}

/// Taxa racional para frame rate sinalizado ou observado.
///
/// SPEC-PROBE-VID-002
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub num: u32,
    pub den: u32,
}

/// Tipo de varredura do vídeo.
///
/// SPEC-PROBE-VID-002
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanType {
    Progressive,
    Interlaced,
    Unknown,
}

/// Informação Active Format Description quando sinalizada.
///
/// SPEC-PROBE-VID-002
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveFormat(pub u8);

/// Valor HDR que preserva um motivo de invalidez sem descartar a evidência.
///
/// SPEC-PROBE-VID-003
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HdrValue<T> {
    pub value: T,
    pub valid: bool,
    pub invalid_reason: Option<String>,
}

/// Metadados HDR sinalizados no vídeo.
///
/// Campos ausentes são distintos de campos presentes, porém inválidos.
///
/// SPEC-PROBE-VID-003
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HdrMetadata {
    pub transfer: Option<String>,
    pub max_cll: Option<HdrValue<u16>>,
    pub max_fall: Option<HdrValue<u16>>,
    pub display_primaries: Option<HdrValue<[(u16, u16); 3]>>,
    pub white_point: Option<HdrValue<(u16, u16)>>,
    pub max_luminance: Option<HdrValue<u32>>,
    pub min_luminance: Option<HdrValue<u32>>,
}

/// Metadados de vídeo vindos do parser de elementary stream.
///
/// SPEC-PROBE-VID-002 · SPEC-PROBE-VID-003
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoMetadataObservation {
    pub service_id: u16,
    pub pid: Pid,
    pub codec: VideoCodec,
    pub resolution: Option<(u32, u32)>,
    pub aspect_ratio: Option<AspectRatio>,
    pub frame_rate: Option<Rate>,
    pub scan_type: Option<ScanType>,
    pub active_format: Option<ActiveFormat>,
    pub hdr: Option<HdrMetadata>,
}

/// Observação compacta de luma de um quadro decodificado.
///
/// SPEC-PROBE-VID-005
#[derive(Debug, Clone)]
pub struct VideoFrameObservation {
    pub service_id: u16,
    pub pid: Pid,
    pub pts_90khz: Option<u64>,
    pub width: u32,
    pub height: u32,
    pub luma_width: u16,
    pub luma_height: u16,
    pub luma: Box<[u8]>,
    pub received_at: Instant,
}

impl VideoFrameObservation {
    /// Valida dimensões antes que detectores indexem a miniatura.
    ///
    /// SPEC-PROBE-VID-005
    pub fn validate(&self) -> Result<(), VideoObservationError> {
        if self.width == 0 || self.height == 0 || self.luma_width == 0 || self.luma_height == 0 {
            return Err(VideoObservationError::InvalidDimensions);
        }
        let needed = usize::from(self.luma_width)
            .checked_mul(usize::from(self.luma_height))
            .ok_or(VideoObservationError::InvalidDimensions)?;
        if self.luma.len() != needed {
            return Err(VideoObservationError::LumaLength {
                expected: needed,
                actual: self.luma.len(),
            });
        }
        Ok(())
    }
}

/// Erro de uma observação recebida de fonte externa.
///
/// SPEC-PROBE-VID-016
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoObservationError {
    InvalidDimensions,
    LumaLength { expected: usize, actual: usize },
    ChannelCapacity,
    Disconnected,
}

/// Sender não bloqueante de observações secundárias.
///
/// SPEC-PROBE-VID-005
#[derive(Clone)]
pub struct VideoObservationSender {
    sender: Sender<VideoFrameObservation>,
    drops: Arc<AtomicU64>,
}

impl VideoObservationSender {
    /// Tenta publicar sem bloquear o decoder; `Ok(false)` indica descarte local.
    ///
    /// SPEC-PROBE-VID-005
    pub fn try_send(&self, frame: VideoFrameObservation) -> Result<bool, VideoObservationError> {
        frame.validate()?;
        match self.sender.try_send(frame) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => {
                self.drops.fetch_add(1, Ordering::Relaxed);
                Ok(false)
            }
            Err(TrySendError::Disconnected(_)) => Err(VideoObservationError::Disconnected),
        }
    }

    /// Número acumulado de observações descartadas localmente.
    ///
    /// SPEC-PROBE-VID-005
    pub fn dropped(&self) -> u64 {
        self.drops.load(Ordering::Relaxed)
    }
}

/// Cria canal bounded para observações secundárias de vídeo.
///
/// SPEC-PROBE-VID-005
pub fn video_observation_channel(
    capacity: usize,
) -> Result<(VideoObservationSender, Receiver<VideoFrameObservation>), VideoObservationError> {
    if capacity == 0 {
        return Err(VideoObservationError::ChannelCapacity);
    }
    let (sender, receiver) = crossbeam_channel::bounded(capacity);
    Ok((
        VideoObservationSender {
            sender,
            drops: Arc::new(AtomicU64::new(0)),
        },
        receiver,
    ))
}

/// Estado que separa entrada, PES, decoder e análise.
///
/// SPEC-PROBE-VID-004
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoAvailability {
    Disabled,
    NotApplicable,
    PesUnavailable,
    DecoderUnavailable,
    Ready,
}

/// Perfil determinístico dos detectores implementados nesta versão.
///
/// Defaults perceptuais permanecem desabilitados até serem aprovados pela operação.
///
/// SPEC-PROBE-VID-001 · SPEC-PROBE-VID-008
#[derive(Debug, Clone)]
pub struct VideoProfile {
    pub enabled: bool,
    pub sample_fps: u16,
    pub freeze_enabled: bool,
    pub freeze_similarity: u8,
    pub freeze_duration: Duration,
    pub black_enabled: bool,
    pub black_luma_threshold: u8,
    pub black_coverage_pct: u8,
    pub black_duration: Duration,
    pub blockiness_enabled: bool,
    pub blockiness_threshold: f64,
}

impl Default for VideoProfile {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_fps: 1,
            freeze_enabled: false,
            freeze_similarity: 2,
            freeze_duration: Duration::from_secs(3),
            black_enabled: false,
            black_luma_threshold: 16,
            black_coverage_pct: 95,
            black_duration: Duration::from_secs(1),
            blockiness_enabled: false,
            blockiness_threshold: 20.0,
        }
    }
}

impl VideoProfile {
    /// Converte o perfil serializável da configuração em parâmetros de runtime.
    ///
    /// SPEC-PROBE-VID-001
    pub fn from_config(config: &crate::config::VideoProfileConfig) -> Self {
        Self {
            enabled: config.enabled,
            sample_fps: config.sample_fps.max(1),
            freeze_enabled: config.freeze_enabled,
            freeze_similarity: config.freeze_similarity,
            freeze_duration: duration_from_secs(config.freeze_duration_secs),
            black_enabled: config.black_enabled,
            black_luma_threshold: config.black_luma_threshold,
            black_coverage_pct: config.black_coverage_pct.min(100),
            black_duration: duration_from_secs(config.black_duration_secs),
            blockiness_enabled: config.blockiness_enabled,
            blockiness_threshold: config.blockiness_threshold,
        }
    }
}

/// Resultado de um detector: `None` é n/a ou desabilitado, não score zero.
///
/// SPEC-PROBE-VID-006 · SPEC-PROBE-VID-008
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetectorResult {
    pub active: bool,
    pub duration: Duration,
    pub value: f64,
    pub threshold: f64,
}

/// Resultado de análise de uma observação de frame.
///
/// SPEC-PROBE-VID-006 · SPEC-PROBE-VID-008
#[derive(Debug, Clone, PartialEq)]
pub struct VideoAnalysis {
    pub availability: VideoAvailability,
    pub freeze: Option<DetectorResult>,
    pub black: Option<DetectorResult>,
    pub blockiness: Option<DetectorResult>,
}

/// Alteração deduplicada de metadado, adequada para evento e persistência.
///
/// SPEC-PROBE-VID-002 · SPEC-PROBE-VID-014
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataChange {
    pub service_id: u16,
    pub pid: Pid,
    pub field: &'static str,
    pub previous: String,
    pub current: String,
}

/// Estatísticas de GOP por serviço/PID.
///
/// SPEC-PROBE-VID-010
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GopStats {
    pub count: u64,
    pub last_90khz: Option<u64>,
    pub min_90khz: Option<u64>,
    pub max_90khz: Option<u64>,
}

/// Evidência normalizada de erro de elementary stream.
///
/// SPEC-PROBE-VID-011 · SPEC-PROBE-VID-012
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementaryStreamError {
    pub service_id: u16,
    pub pid: Pid,
    pub codec: VideoCodec,
    pub kind: ElementaryStreamErrorKind,
    pub pts_90khz: Option<u64>,
    pub offset: Option<usize>,
    pub caused_by: Option<String>,
}

/// Classe de erro que não depende do decoder de reprodução.
///
/// SPEC-PROBE-VID-011
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementaryStreamErrorKind {
    MissingHeader,
    InvalidSyntax,
    InvalidFrameSize,
    MissingReference,
    IncompletePicture,
}

#[derive(Default)]
struct VideoState {
    metadata: Option<VideoMetadataObservation>,
    previous_luma: Option<Box<[u8]>>,
    previous_pts: Option<u64>,
    freeze_since: Option<Instant>,
    black_since: Option<Instant>,
    last_key_pts: Option<u64>,
    gop: GopStats,
}

/// Analisador por `(service_id, pid)` sem dependência de GPU ou FFmpeg.
///
/// SPEC-PROBE-VID-001 · SPEC-PROBE-VID-016
#[derive(Default)]
pub struct VideoAnalyzer {
    states: HashMap<(u16, Pid), VideoState>,
}

impl VideoAnalyzer {
    /// Resolve a aplicabilidade sem inferir erro a partir da falta de quadros.
    ///
    /// `pes_available` e `decoder_available` devem vir de estágios distintos
    /// do pipeline; isso impede que indisponibilidade do decoder seja tratada
    /// como falha do elementary stream.
    ///
    /// SPEC-PROBE-VID-004
    pub fn availability(
        &mut self,
        profile: &VideoProfile,
        service_id: u16,
        pid: Pid,
        pes_available: bool,
        decoder_available: bool,
    ) -> VideoAvailability {
        let state = self.states.entry((service_id, pid)).or_default();
        if !profile.enabled {
            VideoAvailability::Disabled
        } else if state
            .metadata
            .as_ref()
            .is_some_and(|metadata| !metadata.codec.is_supported())
        {
            VideoAvailability::NotApplicable
        } else if !pes_available {
            VideoAvailability::PesUnavailable
        } else if !decoder_available {
            VideoAvailability::DecoderUnavailable
        } else {
            VideoAvailability::Ready
        }
    }

    /// Registra metadados e devolve apenas mudanças reais.
    ///
    /// SPEC-PROBE-VID-002
    pub fn observe_metadata(
        &mut self,
        observation: VideoMetadataObservation,
    ) -> Vec<MetadataChange> {
        let key = (observation.service_id, observation.pid);
        let state = self.states.entry(key).or_default();
        let changes = state.metadata.as_ref().map_or_else(Vec::new, |previous| {
            metadata_changes(previous, &observation)
        });
        state.metadata = Some(observation);
        changes
    }

    /// Registra que há PES, mas decoder não pôde fornecer quadro.
    ///
    /// SPEC-PROBE-VID-004
    pub fn decoder_unavailable(&mut self, service_id: u16, pid: Pid) -> VideoAvailability {
        self.states.entry((service_id, pid)).or_default();
        VideoAvailability::DecoderUnavailable
    }

    /// Analisa um quadro já validado. Falta de quadro nunca entra neste método.
    ///
    /// SPEC-PROBE-VID-006 · SPEC-PROBE-VID-007 · SPEC-PROBE-VID-008
    pub fn observe_frame(
        &mut self,
        profile: &VideoProfile,
        frame: VideoFrameObservation,
    ) -> Result<VideoAnalysis, VideoObservationError> {
        frame.validate()?;
        let key = (frame.service_id, frame.pid);
        let state = self.states.entry(key).or_default();
        let codec_supported = state
            .metadata
            .as_ref()
            .map(|m| m.codec.is_supported())
            .unwrap_or(true);
        if !profile.enabled {
            return Ok(VideoAnalysis {
                availability: VideoAvailability::Disabled,
                freeze: None,
                black: None,
                blockiness: None,
            });
        }
        if !codec_supported {
            return Ok(VideoAnalysis {
                availability: VideoAvailability::NotApplicable,
                freeze: None,
                black: None,
                blockiness: None,
            });
        }

        let freeze = freeze_result(profile, state, &frame);
        let black = black_result(profile, state, &frame);
        let blockiness = profile.blockiness_enabled.then(|| DetectorResult {
            active: blockiness_score(&frame.luma, frame.luma_width, frame.luma_height)
                > profile.blockiness_threshold,
            duration: Duration::ZERO,
            value: blockiness_score(&frame.luma, frame.luma_width, frame.luma_height),
            threshold: profile.blockiness_threshold,
        });
        state.previous_pts = frame.pts_90khz;
        state.previous_luma = Some(frame.luma);
        Ok(VideoAnalysis {
            availability: VideoAvailability::Ready,
            freeze,
            black,
            blockiness,
        })
    }

    /// Mede um GOP entre dois access units aleatórios sucessivos.
    ///
    /// SPEC-PROBE-VID-010
    pub fn observe_access_unit(
        &mut self,
        service_id: u16,
        pid: Pid,
        pts_90khz: Option<u64>,
        is_random_access: bool,
    ) -> GopStats {
        let state = self.states.entry((service_id, pid)).or_default();
        if is_random_access {
            if let (Some(previous), Some(current)) = (state.last_key_pts, pts_90khz) {
                if let Some(delta) = current.checked_sub(previous) {
                    state.gop.count = state.gop.count.saturating_add(1);
                    state.gop.last_90khz = Some(delta);
                    state.gop.min_90khz = Some(state.gop.min_90khz.map_or(delta, |v| v.min(delta)));
                    state.gop.max_90khz = Some(state.gop.max_90khz.map_or(delta, |v| v.max(delta)));
                }
            }
            state.last_key_pts = pts_90khz;
        }
        state.gop
    }
}

/// Valida de forma conservadora a moldura mínima de um access unit.
///
/// Não substitui os parsers detalhados de `ts::mediainfo`: é uma barreira de
/// Probe para normalizar evidência por codec sem invocar FFmpeg e sem rejeitar
/// payload truncado com panic.
///
/// SPEC-PROBE-VID-011 · SPEC-PROBE-VID-016
pub fn validate_elementary_stream(
    service_id: u16,
    pid: Pid,
    codec: VideoCodec,
    payload: &[u8],
    pts_90khz: Option<u64>,
    caused_by: Option<String>,
) -> Vec<ElementaryStreamError> {
    let error = |kind| ElementaryStreamError {
        service_id,
        pid,
        codec,
        kind,
        pts_90khz,
        offset: None,
        caused_by: caused_by.clone(),
    };
    if !codec.is_supported() {
        return Vec::new();
    }
    if payload.is_empty() {
        return vec![error(ElementaryStreamErrorKind::IncompletePicture)];
    }
    let start_code = payload.windows(3).position(|bytes| bytes == [0, 0, 1]);
    let Some(offset) = start_code else {
        return vec![error(ElementaryStreamErrorKind::MissingHeader)];
    };
    let header_offset = offset.saturating_add(3);
    if header_offset >= payload.len() {
        return vec![error(ElementaryStreamErrorKind::IncompletePicture)];
    }
    match codec {
        VideoCodec::Mpeg2 if payload[header_offset] == 0x00 => vec![ElementaryStreamError {
            offset: Some(offset),
            ..error(ElementaryStreamErrorKind::InvalidSyntax)
        }],
        VideoCodec::H264 | VideoCodec::Hevc if payload.len() < header_offset.saturating_add(2) => {
            vec![ElementaryStreamError {
                offset: Some(offset),
                ..error(ElementaryStreamErrorKind::IncompletePicture)
            }]
        }
        _ => Vec::new(),
    }
}

fn freeze_result(
    profile: &VideoProfile,
    state: &mut VideoState,
    frame: &VideoFrameObservation,
) -> Option<DetectorResult> {
    if !profile.freeze_enabled {
        return None;
    }
    let similar = state.previous_luma.as_ref().is_some_and(|previous| {
        mean_abs_difference(previous, &frame.luma) <= f64::from(profile.freeze_similarity)
    });
    if similar && state.previous_pts != frame.pts_90khz {
        state.freeze_since.get_or_insert(frame.received_at);
    } else {
        state.freeze_since = None;
    }
    let duration = state.freeze_since.map_or(Duration::ZERO, |since| {
        frame.received_at.saturating_duration_since(since)
    });
    Some(DetectorResult {
        active: duration >= profile.freeze_duration,
        duration,
        value: if similar { 1.0 } else { 0.0 },
        threshold: profile.freeze_duration.as_secs_f64(),
    })
}

fn black_result(
    profile: &VideoProfile,
    state: &mut VideoState,
    frame: &VideoFrameObservation,
) -> Option<DetectorResult> {
    if !profile.black_enabled {
        return None;
    }
    let covered = frame
        .luma
        .iter()
        .filter(|v| **v <= profile.black_luma_threshold)
        .count()
        * 100
        / frame.luma.len();
    if covered >= usize::from(profile.black_coverage_pct) {
        state.black_since.get_or_insert(frame.received_at);
    } else {
        state.black_since = None;
    }
    let duration = state.black_since.map_or(Duration::ZERO, |since| {
        frame.received_at.saturating_duration_since(since)
    });
    Some(DetectorResult {
        active: duration >= profile.black_duration,
        duration,
        value: covered as f64,
        threshold: f64::from(profile.black_coverage_pct),
    })
}

fn mean_abs_difference(a: &[u8], b: &[u8]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return f64::INFINITY;
    }
    a.iter()
        .zip(b)
        .map(|(x, y)| u8::abs_diff(*x, *y) as u64)
        .sum::<u64>() as f64
        / a.len() as f64
}

fn duration_from_secs(secs: f64) -> Duration {
    if secs.is_finite() && secs >= 0.0 {
        Duration::from_secs_f64(secs)
    } else {
        Duration::ZERO
    }
}

fn blockiness_score(luma: &[u8], width: u16, height: u16) -> f64 {
    let w = usize::from(width);
    let h = usize::from(height);
    if w < 17 || h < 17 {
        return 0.0;
    }
    let mut sum = 0_u64;
    let mut count = 0_u64;
    for y in 0..h {
        for x in 1..w {
            if x % 16 == 0 {
                sum += u8::abs_diff(luma[y * w + x], luma[y * w + x - 1]) as u64;
                count += 1;
            }
        }
    }
    for y in 1..h {
        if y % 16 == 0 {
            for x in 0..w {
                sum += u8::abs_diff(luma[y * w + x], luma[(y - 1) * w + x]) as u64;
                count += 1;
            }
        }
    }
    if count == 0 {
        0.0
    } else {
        sum as f64 / count as f64
    }
}

fn metadata_changes(
    previous: &VideoMetadataObservation,
    current: &VideoMetadataObservation,
) -> Vec<MetadataChange> {
    let mut out = Vec::new();
    macro_rules! change {
        ($field:literal, $a:expr, $b:expr) => {
            if $a != $b {
                out.push(MetadataChange {
                    service_id: current.service_id,
                    pid: current.pid,
                    field: $field,
                    previous: format!("{:?}", $a),
                    current: format!("{:?}", $b),
                });
            }
        };
    }
    change!("codec", previous.codec, current.codec);
    change!("resolution", previous.resolution, current.resolution);
    change!("aspect_ratio", previous.aspect_ratio, current.aspect_ratio);
    change!("frame_rate", previous.frame_rate, current.frame_rate);
    change!("scan_type", previous.scan_type, current.scan_type);
    change!(
        "active_format",
        previous.active_format,
        current.active_format
    );
    change!("hdr", previous.hdr, current.hdr);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(at: Instant, luma: u8) -> VideoFrameObservation {
        VideoFrameObservation {
            service_id: 1,
            pid: 100,
            pts_90khz: Some(1),
            width: 16,
            height: 16,
            luma_width: 16,
            luma_height: 16,
            luma: vec![luma; 256].into_boxed_slice(),
            received_at: at,
        }
    }

    #[test]
    fn spec_probe_vid_005_saturated_channel_drops_without_blocking() {
        let (sender, _receiver) = video_observation_channel(1).expect("channel");
        let now = Instant::now();
        assert!(sender.try_send(frame(now, 10)).expect("first"));
        assert!(!sender.try_send(frame(now, 10)).expect("drop"));
        assert_eq!(sender.dropped(), 1);
    }

    #[test]
    fn spec_probe_vid_006_static_frames_open_only_after_duration() {
        let mut analyzer = VideoAnalyzer::default();
        let start = Instant::now();
        let mut p = VideoProfile {
            freeze_enabled: true,
            freeze_duration: Duration::from_secs(2),
            ..Default::default()
        };
        let mut a = frame(start, 50);
        a.pts_90khz = Some(1);
        assert!(
            !analyzer
                .observe_frame(&p, a)
                .expect("frame")
                .freeze
                .expect("enabled")
                .active
        );
        let mut b = frame(start + Duration::from_secs(1), 50);
        b.pts_90khz = Some(2);
        assert!(
            !analyzer
                .observe_frame(&p, b)
                .expect("frame")
                .freeze
                .expect("enabled")
                .active
        );
        let mut c = frame(start + Duration::from_secs(3), 50);
        c.pts_90khz = Some(3);
        assert!(
            analyzer
                .observe_frame(&p, c)
                .expect("frame")
                .freeze
                .expect("enabled")
                .active
        );
        p.freeze_enabled = false;
        assert!(analyzer
            .observe_frame(&p, frame(start + Duration::from_secs(4), 50))
            .expect("frame")
            .freeze
            .is_none());
    }

    #[test]
    fn spec_probe_vid_007_black_requires_coverage_and_duration() {
        let mut analyzer = VideoAnalyzer::default();
        let start = Instant::now();
        let p = VideoProfile {
            black_enabled: true,
            black_duration: Duration::from_secs(2),
            ..Default::default()
        };
        assert!(
            !analyzer
                .observe_frame(&p, frame(start, 0))
                .expect("frame")
                .black
                .expect("enabled")
                .active
        );
        assert!(
            analyzer
                .observe_frame(&p, frame(start + Duration::from_secs(2), 0))
                .expect("frame")
                .black
                .expect("enabled")
                .active
        );
        assert!(
            !analyzer
                .observe_frame(&p, frame(start + Duration::from_secs(3), 100))
                .expect("frame")
                .black
                .expect("enabled")
                .active
        );
    }

    #[test]
    fn spec_probe_vid_016_invalid_frame_returns_error_not_panic() {
        let mut bad = frame(Instant::now(), 0);
        bad.luma = vec![0; 3].into_boxed_slice();
        assert!(matches!(
            bad.validate(),
            Err(VideoObservationError::LumaLength { .. })
        ));
    }

    #[test]
    fn spec_probe_vid_010_gop_tracks_valid_random_access_points() {
        let mut analyzer = VideoAnalyzer::default();
        analyzer.observe_access_unit(1, 100, Some(90_000), true);
        let stats = analyzer.observe_access_unit(1, 100, Some(270_000), true);
        assert_eq!(stats.count, 1);
        assert_eq!(stats.last_90khz, Some(180_000));
    }

    #[test]
    fn spec_probe_vid_004_pes_and_decoder_unavailability_are_distinct() {
        let mut analyzer = VideoAnalyzer::default();
        assert_eq!(
            analyzer.availability(&VideoProfile::default(), 1, 100, false, false),
            VideoAvailability::PesUnavailable
        );
        assert_eq!(
            analyzer.availability(&VideoProfile::default(), 1, 100, true, false),
            VideoAvailability::DecoderUnavailable
        );
    }

    #[test]
    fn spec_probe_vid_011_truncated_access_unit_is_evidence_not_panic() {
        let errors = validate_elementary_stream(1, 100, VideoCodec::H264, &[0, 0, 1], None, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ElementaryStreamErrorKind::IncompletePicture);
        assert_eq!(errors[0].codec, VideoCodec::H264);
    }

    #[test]
    fn spec_probe_vid_002_metadata_change_is_deduplicated() {
        let mut analyzer = VideoAnalyzer::default();
        let base = VideoMetadataObservation {
            service_id: 1,
            pid: 100,
            codec: VideoCodec::H264,
            resolution: Some((1920, 1080)),
            aspect_ratio: None,
            frame_rate: None,
            scan_type: None,
            active_format: None,
            hdr: None,
        };
        assert!(analyzer.observe_metadata(base.clone()).is_empty());
        assert!(analyzer.observe_metadata(base.clone()).is_empty());
        let mut changed = base;
        changed.resolution = Some((1280, 720));
        let updates = analyzer.observe_metadata(changed);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].field, "resolution");
    }
}
