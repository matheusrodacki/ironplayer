//! Enums de codec de vídeo e áudio com mapeamento a partir de `stream_type` MPEG-TS.
//!
//! SPEC-AV-002a · SPEC-AV-002c

use ts::tables::{Descriptor, PmtStream};

// ── VideoCodec ────────────────────────────────────────────────────────────────

/// Codec de vídeo suportado pelo decodificador `av`.
///
/// SPEC-AV-002a
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    /// MPEG-2 Video — stream_type `0x02` (ISO 13818-2).
    Mpeg2,
    /// H.264 / AVC — stream_type `0x1B` (ISO 14496-10).
    H264,
    /// H.265 / HEVC — stream_type `0x24` (ISO 23008-2).
    Hevc,
}

impl VideoCodec {
    /// Retorna o `VideoCodec` correspondente ao `stream_type` MPEG-TS, ou
    /// `None` se o tipo não for um codec de vídeo suportado.
    ///
    /// # Mapeamento suportado
    ///
    /// | `stream_type` | Codec         |
    /// |---------------|---------------|
    /// | `0x02`        | MPEG-2 Video  |
    /// | `0x1B`        | H.264 / AVC   |
    /// | `0x24`        | H.265 / HEVC  |
    ///
    /// SPEC-AV-002a
    pub fn from_stream_type(stream_type: u8) -> Option<Self> {
        match stream_type {
            0x02 => Some(Self::Mpeg2),
            0x1B => Some(Self::H264),
            0x24 => Some(Self::Hevc),
            _ => None,
        }
    }

    /// Retorna o nome legível do codec.
    ///
    /// SPEC-AV-002c
    pub fn name(self) -> &'static str {
        match self {
            Self::Mpeg2 => "MPEG-2 Video",
            Self::H264 => "H.264 / AVC",
            Self::Hevc => "H.265 / HEVC",
        }
    }
}

// ── AudioCodec ────────────────────────────────────────────────────────────────

/// Codec de áudio suportado pelo decodificador `av`.
///
/// SPEC-AV-002a
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    /// MPEG-1/2 Audio Layer I/II (MP2) — stream_type `0x03` ou `0x04`.
    Mp2,
    /// AAC (ADTS) — stream_type `0x0F` (ISO 13818-7).
    AacAdts,
    /// AAC (LATM) — stream_type `0x11` (ISO 14496-3).
    AacLatm,
    /// AC-3 / Dolby Digital — stream_type `0x81` (ATSC A/52).
    Ac3,
    /// E-AC-3 / Dolby Digital Plus — stream_type `0x87` (ATSC A/52B).
    Eac3,
}

impl AudioCodec {
    /// Retorna o `AudioCodec` correspondente ao `stream_type` MPEG-TS, ou
    /// `None` se o tipo não for um codec de áudio suportado.
    ///
    /// # Mapeamento suportado
    ///
    /// | `stream_type` | Codec                     |
    /// |---------------|---------------------------|
    /// | `0x03`        | MP2 (MPEG-1 Audio)        |
    /// | `0x04`        | MP2 (MPEG-2 Audio)        |
    /// | `0x0F`        | AAC ADTS (ISO 13818-7)    |
    /// | `0x11`        | AAC LATM (ISO 14496-3)    |
    /// | `0x81`        | AC-3 (ATSC A/52)          |
    /// | `0x87`        | E-AC-3 (ATSC A/52B)       |
    ///
    /// SPEC-AV-002a
    pub fn from_stream_type(stream_type: u8) -> Option<Self> {
        match stream_type {
            0x03 | 0x04 => Some(Self::Mp2),
            0x0F => Some(Self::AacAdts),
            0x11 => Some(Self::AacLatm),
            0x81 => Some(Self::Ac3),
            0x87 => Some(Self::Eac3),
            _ => None,
        }
    }

    /// Resolve um codec de áudio a partir do `stream_type` e dos descriptors
    /// do ES na PMT.
    pub fn from_pmt(stream_type: u8, descriptors: &[Descriptor]) -> Option<Self> {
        Self::from_stream_type(stream_type).or_else(|| {
            if stream_type != 0x06 {
                return None;
            }

            if has_descriptor_tag(descriptors, 0x7A) || has_registration(descriptors, b"EAC3") {
                return Some(Self::Eac3);
            }
            if has_descriptor_tag(descriptors, 0x6A) || has_registration(descriptors, b"AC-3") {
                return Some(Self::Ac3);
            }
            if has_descriptor_tag(descriptors, 0x7C) {
                return Some(Self::AacLatm);
            }

            None
        })
    }

    /// Retorna o nome legível do codec.
    ///
    /// SPEC-AV-002c
    pub fn name(self) -> &'static str {
        match self {
            Self::Mp2 => "MPEG-1/2 Audio (MP2)",
            Self::AacAdts => "AAC (ADTS)",
            Self::AacLatm => "AAC (LATM)",
            Self::Ac3 => "AC-3 / Dolby Digital",
            Self::Eac3 => "E-AC-3 / Dolby Digital Plus",
        }
    }
}

// ── MediaCodec ────────────────────────────────────────────────────────────────

/// União de codec de vídeo ou áudio, derivada de um `stream_type` MPEG-TS.
///
/// SPEC-AV-002a
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaCodec {
    /// Codec de vídeo.
    Video(VideoCodec),
    /// Codec de áudio.
    Audio(AudioCodec),
}

impl MediaCodec {
    /// Tenta derivar um `MediaCodec` a partir do `stream_type` MPEG-TS.
    ///
    /// Retorna `None` se o `stream_type` não for suportado.
    ///
    /// SPEC-AV-002a
    pub fn from_stream_type(stream_type: u8) -> Option<Self> {
        if let Some(v) = VideoCodec::from_stream_type(stream_type) {
            return Some(Self::Video(v));
        }
        if let Some(a) = AudioCodec::from_stream_type(stream_type) {
            return Some(Self::Audio(a));
        }
        None
    }

    /// Tenta derivar um `MediaCodec` a partir de uma entrada de PMT, incluindo
    /// descriptors DVB/ATSC para streams privados (`stream_type=0x06`).
    pub fn from_pmt(stream_type: u8, descriptors: &[Descriptor]) -> Option<Self> {
        if let Some(v) = VideoCodec::from_stream_type(stream_type) {
            return Some(Self::Video(v));
        }
        if let Some(a) = AudioCodec::from_pmt(stream_type, descriptors) {
            return Some(Self::Audio(a));
        }
        None
    }

    /// Variante conveniente para entradas já parseadas da PMT.
    pub fn from_pmt_stream(stream: &PmtStream) -> Option<Self> {
        Self::from_pmt(stream.stream_type, &stream.descriptors)
    }
}

fn has_descriptor_tag(descriptors: &[Descriptor], tag: u8) -> bool {
    descriptors.iter().any(|descriptor| descriptor.tag == tag)
}

fn has_registration(descriptors: &[Descriptor], format_identifier: &[u8; 4]) -> bool {
    descriptors
        .iter()
        .any(|descriptor| descriptor.is_registration_format(format_identifier))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ts::tables::Descriptor;

    #[test]
    fn spec_av_002a_video_codec_from_stream_type() {
        assert_eq!(VideoCodec::from_stream_type(0x02), Some(VideoCodec::Mpeg2));
        assert_eq!(VideoCodec::from_stream_type(0x1B), Some(VideoCodec::H264));
        assert_eq!(VideoCodec::from_stream_type(0x24), Some(VideoCodec::Hevc));
    }

    #[test]
    fn spec_av_002a_video_codec_unknown_returns_none() {
        assert_eq!(VideoCodec::from_stream_type(0x00), None);
        assert_eq!(VideoCodec::from_stream_type(0x0F), None);
        assert_eq!(VideoCodec::from_stream_type(0x81), None);
        assert_eq!(VideoCodec::from_stream_type(0xFF), None);
    }

    #[test]
    fn spec_av_002a_audio_codec_from_stream_type() {
        assert_eq!(AudioCodec::from_stream_type(0x03), Some(AudioCodec::Mp2));
        assert_eq!(AudioCodec::from_stream_type(0x04), Some(AudioCodec::Mp2));
        assert_eq!(
            AudioCodec::from_stream_type(0x0F),
            Some(AudioCodec::AacAdts)
        );
        assert_eq!(
            AudioCodec::from_stream_type(0x11),
            Some(AudioCodec::AacLatm)
        );
        assert_eq!(AudioCodec::from_stream_type(0x81), Some(AudioCodec::Ac3));
        assert_eq!(AudioCodec::from_stream_type(0x87), Some(AudioCodec::Eac3));
    }

    #[test]
    fn spec_av_002a_audio_codec_unknown_returns_none() {
        assert_eq!(AudioCodec::from_stream_type(0x00), None);
        assert_eq!(AudioCodec::from_stream_type(0x02), None);
        assert_eq!(AudioCodec::from_stream_type(0x1B), None);
        assert_eq!(AudioCodec::from_stream_type(0xFF), None);
    }

    #[test]
    fn spec_av_002a_audio_codec_private_stream_uses_descriptors() {
        assert_eq!(
            AudioCodec::from_pmt(0x06, &[Descriptor::new(0x6A, vec![])]),
            Some(AudioCodec::Ac3)
        );
        assert_eq!(
            AudioCodec::from_pmt(0x06, &[Descriptor::new(0x7A, vec![])]),
            Some(AudioCodec::Eac3)
        );
        assert_eq!(
            AudioCodec::from_pmt(0x06, &[Descriptor::new(0x7C, vec![0x11, 0x90, 0x00])]),
            Some(AudioCodec::AacLatm)
        );
    }

    #[test]
    fn spec_av_002a_audio_codec_private_stream_uses_registration_descriptor() {
        assert_eq!(
            AudioCodec::from_pmt(0x06, &[Descriptor::new(0x05, b"AC-3".to_vec())]),
            Some(AudioCodec::Ac3)
        );
        assert_eq!(
            AudioCodec::from_pmt(0x06, &[Descriptor::new(0x05, b"EAC3".to_vec())]),
            Some(AudioCodec::Eac3)
        );
    }

    #[test]
    fn spec_av_002c_media_codec_video_routes() {
        assert_eq!(
            MediaCodec::from_stream_type(0x1B),
            Some(MediaCodec::Video(VideoCodec::H264))
        );
        assert_eq!(
            MediaCodec::from_stream_type(0x24),
            Some(MediaCodec::Video(VideoCodec::Hevc))
        );
    }

    #[test]
    fn spec_av_002c_media_codec_audio_routes() {
        assert_eq!(
            MediaCodec::from_stream_type(0x81),
            Some(MediaCodec::Audio(AudioCodec::Ac3))
        );
        assert_eq!(
            MediaCodec::from_stream_type(0x87),
            Some(MediaCodec::Audio(AudioCodec::Eac3))
        );
    }

    #[test]
    fn spec_av_002c_media_codec_unsupported_returns_none() {
        assert_eq!(MediaCodec::from_stream_type(0x06), None);
        assert_eq!(MediaCodec::from_stream_type(0xFF), None);
    }

    #[test]
    fn spec_av_002c_media_codec_from_pmt_stream_supports_private_audio() {
        let stream = PmtStream {
            stream_type: 0x06,
            elementary_pid: 0x0120,
            descriptors: vec![Descriptor::new(0x6A, vec![])],
        };

        assert_eq!(
            MediaCodec::from_pmt_stream(&stream),
            Some(MediaCodec::Audio(AudioCodec::Ac3))
        );
    }
}
