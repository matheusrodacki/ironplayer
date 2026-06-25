//! Modelo de dados do snapshot Media Info por PID.
//!
//! SPEC-MI-001 · SPEC-MI-003

use std::collections::HashMap;

use crate::Pid;

/// Classificação elementar do stream.
///
/// SPEC-MI-001
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Video,
    Audio,
    Data,
    Menu,
}

/// Informações de codec extraídas do cabeçalho elementar.
///
/// Todos os campos são opcionais até o probe completar o parse.
///
/// SPEC-MI-001
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ElementaryCodecInfo {
    pub kind: Option<StreamKind>,
    pub stream_type: Option<u8>,
    pub menu_id: Option<u16>,
    pub encrypted: bool,
    pub format: Option<String>,
    pub format_info: Option<String>,
    pub format_profile: Option<String>,
    pub format_settings: Option<String>,
    pub format_settings_cabac: Option<String>,
    pub format_settings_ref_frames: Option<String>,
    pub format_settings_gop: Option<String>,
    pub commercial_name: Option<String>,
    pub muxing_mode: Option<String>,
    pub bit_rate_mode: Option<String>,
    pub bit_rate_kbps: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub display_aspect_ratio: Option<String>,
    pub frame_rate: Option<String>,
    pub frame_rate_num: Option<u32>,
    pub frame_rate_den: Option<u32>,
    pub color_space: Option<String>,
    pub chroma_subsampling: Option<String>,
    pub bit_depth: Option<u8>,
    pub scan_type: Option<String>,
    pub scan_store_method: Option<String>,
    pub scan_order: Option<String>,
    pub color_range: Option<String>,
    pub color_primaries: Option<String>,
    pub transfer_characteristics: Option<String>,
    pub matrix_coefficients: Option<String>,
    pub channels: Option<u16>,
    pub channel_layout: Option<String>,
    pub sampling_rate_hz: Option<u32>,
    pub samples_per_frame: Option<u32>,
    pub compression_mode: Option<String>,
    pub language: Option<String>,
    pub delay_relative_to_video_ms: Option<i32>,
    pub service_kind: Option<String>,
    pub dialog_normalization_db: Option<i32>,
    pub compr_db: Option<f32>,
    pub dynrng_db: Option<f32>,
    pub cmixlev_db: Option<f32>,
    pub surmixlev_db: Option<f32>,
    pub mixlevel_db: Option<f32>,
    pub room_type: Option<String>,
    pub probe_complete: bool,
}

/// Snapshot imutável de codec info por PID.
///
/// SPEC-MI-003
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaInfoCodecSnapshot {
    pub streams: HashMap<Pid, ElementaryCodecInfo>,
}

impl MediaInfoCodecSnapshot {
    /// Retorna referência ao info de um PID.
    ///
    /// SPEC-MI-003
    pub fn get(&self, pid: Pid) -> Option<&ElementaryCodecInfo> {
        self.streams.get(&pid)
    }
}
