//! Montagem do relatório estilo MediaInfo.
//!
//! SPEC-MI-004 · SPEC-MI-005

use std::collections::HashMap;

use crate::metrics::MetricsSnapshot;
use crate::tables::{Descriptor, KnownDescriptor, Pat, Pmt, PmtStream, Sdt, SdtService};
use crate::Pid;

use super::model::{ElementaryCodecInfo, MediaInfoCodecSnapshot, StreamKind};

/// Campo chave/valor do relatório.
///
/// SPEC-MI-005
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportField {
    pub key: String,
    pub value: String,
}

/// Seção do relatório (General, Video, Audio, Menu).
///
/// SPEC-MI-005
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInfoSection {
    pub title: String,
    pub fields: Vec<ReportField>,
}

/// Relatório completo Media Info.
///
/// SPEC-MI-005
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaInfoReport {
    pub sections: Vec<MediaInfoSection>,
}

impl MediaInfoReport {
    /// Serializa para texto no estilo MediaInfo (clipboard).
    ///
    /// SPEC-MI-006
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for section in &self.sections {
            out.push_str(&section.title);
            out.push('\n');
            for field in &section.fields {
                out.push_str(&format_field_line(&field.key, &field.value));
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }
}

fn format_field_line(key: &str, value: &str) -> String {
    const KEY_WIDTH: usize = 40;
    if key.len() >= KEY_WIDTH {
        format!("{key} : {value}")
    } else {
        format!("{key:<KEY_WIDTH$} : {value}")
    }
}

/// Contexto PSI/SI para montagem do relatório.
///
/// SPEC-MI-005
#[derive(Debug, Clone, Default)]
pub struct MediaInfoTablesCtx {
    pub pat: Option<Pat>,
    pub pmts: HashMap<u16, Pmt>,
    pub sdt: Option<Sdt>,
    pub nit_network_name: Option<String>,
    pub nit_original_network: Option<String>,
    pub nit_frequency_hz: Option<u64>,
    pub nit_orbital_position: Option<String>,
    pub tot_country: Option<String>,
    pub tot_timezone: Option<String>,
}

/// Parâmetros para `build_media_info_report`.
///
/// SPEC-MI-005
pub struct MediaInfoBuildInput<'a> {
    pub source_name: Option<&'a str>,
    pub metrics: &'a MetricsSnapshot,
    pub tables: &'a MediaInfoTablesCtx,
    pub codec: &'a MediaInfoCodecSnapshot,
}

/// Monta relatório Media Info mesclando PSI/SI, métricas e probe de codec.
///
/// SPEC-MI-005
pub fn build_media_info_report(input: &MediaInfoBuildInput<'_>) -> MediaInfoReport {
    let mut sections = Vec::new();
    sections.push(build_general_section(input));
    sections.extend(build_stream_sections(input));
    MediaInfoReport { sections }
}

/// Campos Media Info de um PID elementar (vídeo ou áudio).
///
/// SPEC-MI-005
pub fn build_elementary_stream_fields(
    input: &MediaInfoBuildInput<'_>,
    pid: Pid,
    program_number: u16,
    stream: &PmtStream,
) -> Vec<ReportField> {
    let codec = input.codec.get(pid);
    let kind = codec
        .and_then(|c| c.kind)
        .unwrap_or_else(|| classify_stream(stream));
    match kind {
        StreamKind::Video => {
            let section = build_video_section(0, pid, program_number, stream, codec, input.metrics);
            section.fields
        }
        StreamKind::Audio => {
            let section = build_audio_section(0, pid, program_number, stream, codec, input.metrics);
            section.fields
        }
        StreamKind::Data | StreamKind::Menu => {
            let mut fields = base_stream_fields(pid, program_number, stream, codec, input.metrics);
            merge_codec_fields(&mut fields, codec);
            fields
        }
    }
}

fn build_general_section(input: &MediaInfoBuildInput<'_>) -> MediaInfoSection {
    let mut fields = Vec::new();
    if let Some(pat) = &input.tables.pat {
        fields.push(field(
            "ID",
            format!(
                "{} (0x{:X})",
                pat.transport_stream_id, pat.transport_stream_id
            ),
        ));
    }
    if let Some(name) = input.source_name {
        fields.push(field("Complete name", name.to_string()));
    }
    fields.push(field("Format", "MPEG-TS".to_string()));
    if input.metrics.total_bitrate_kbps > 0.0 {
        let mbps = input.metrics.total_bitrate_kbps / 1000.0;
        fields.push(field("Overall bit rate", format!("{mbps:.1} Mb/s")));
        fields.push(field("Overall bit rate mode", "Constant".to_string()));
    }
    if let Some(fps) = primary_video_fps(input) {
        fields.push(field("Frame rate", fps));
    }
    if let Some(name) = &input.tables.nit_network_name {
        fields.push(field("Network name", name.clone()));
    }
    if let Some(on) = &input.tables.nit_original_network {
        fields.push(field("Original network name", on.clone()));
    }
    if let Some(c) = &input.tables.tot_country {
        fields.push(field("Country", c.clone()));
    }
    if let Some(tz) = &input.tables.tot_timezone {
        fields.push(field("Timezone", tz.clone()));
    }
    if let Some(freq) = input.tables.nit_frequency_hz {
        fields.push(field("Frequency", freq.to_string()));
    }
    if let Some(orb) = &input.tables.nit_orbital_position {
        fields.push(field("OrbitalPosition", orb.clone()));
    }
    MediaInfoSection {
        title: "General".to_string(),
        fields,
    }
}

fn build_stream_sections(input: &MediaInfoBuildInput<'_>) -> Vec<MediaInfoSection> {
    let mut video_idx = 0usize;
    let mut audio_idx = 0usize;
    let mut sections = Vec::new();

    let mut entries: Vec<(Pid, u16, &PmtStream, Option<&SdtService>)> = Vec::new();
    for (prog, pmt) in &input.tables.pmts {
        let svc = input
            .tables
            .sdt
            .as_ref()
            .and_then(|s| s.services.iter().find(|s| s.service_id == *prog));
        for stream in &pmt.streams {
            entries.push((stream.elementary_pid, *prog, stream, svc));
        }
    }
    entries.sort_by_key(|(pid, _, _, _)| *pid);

    for (pid, menu_id, stream, _svc) in entries {
        let codec = input.codec.get(pid);
        let kind = codec
            .and_then(|c| c.kind)
            .unwrap_or_else(|| classify_stream(stream));
        match kind {
            StreamKind::Video => {
                video_idx += 1;
                sections.push(build_video_section(
                    video_idx,
                    pid,
                    menu_id,
                    stream,
                    codec,
                    input.metrics,
                ));
            }
            StreamKind::Audio => {
                audio_idx += 1;
                sections.push(build_audio_section(
                    audio_idx,
                    pid,
                    menu_id,
                    stream,
                    codec,
                    input.metrics,
                ));
            }
            _ => {}
        }
    }
    sections
}

fn build_video_section(
    idx: usize,
    pid: Pid,
    menu_id: u16,
    stream: &PmtStream,
    codec: Option<&ElementaryCodecInfo>,
    metrics: &MetricsSnapshot,
) -> MediaInfoSection {
    let title = if idx == 1 {
        "Video".to_string()
    } else {
        format!("Video #{idx}")
    };
    let mut fields = base_stream_fields(pid, menu_id, stream, codec, metrics);
    merge_codec_fields(&mut fields, codec);
    if let Some(c) = codec {
        push_opt(&mut fields, "Width", c.width.map(|w| format!("{w} pixels")));
        push_opt(
            &mut fields,
            "Height",
            c.height.map(|h| format!("{h} pixels")),
        );
        push_opt(
            &mut fields,
            "Display aspect ratio",
            c.display_aspect_ratio.clone(),
        );
        push_opt(&mut fields, "Frame rate", c.frame_rate.clone());
        push_opt(&mut fields, "Color space", c.color_space.clone());
        push_opt(
            &mut fields,
            "Chroma subsampling",
            c.chroma_subsampling.clone(),
        );
        push_opt(
            &mut fields,
            "Bit depth",
            c.bit_depth.map(|b| format!("{b} bits")),
        );
        push_opt(&mut fields, "Scan type", c.scan_type.clone());
        push_opt(
            &mut fields,
            "Scan type, store method",
            c.scan_store_method.clone(),
        );
        push_opt(&mut fields, "Scan order", c.scan_order.clone());
        push_opt(&mut fields, "Color range", c.color_range.clone());
        push_opt(&mut fields, "Color primaries", c.color_primaries.clone());
        push_opt(
            &mut fields,
            "Transfer characteristics",
            c.transfer_characteristics.clone(),
        );
        push_opt(
            &mut fields,
            "Matrix coefficients",
            c.matrix_coefficients.clone(),
        );
    }
    MediaInfoSection { title, fields }
}

fn build_audio_section(
    idx: usize,
    pid: Pid,
    menu_id: u16,
    stream: &PmtStream,
    codec: Option<&ElementaryCodecInfo>,
    metrics: &MetricsSnapshot,
) -> MediaInfoSection {
    let title = if idx == 1 {
        "Audio".to_string()
    } else {
        format!("Audio #{idx}")
    };
    let mut fields = base_stream_fields(pid, menu_id, stream, codec, metrics);
    merge_codec_fields(&mut fields, codec);
    if let Some(c) = codec {
        push_opt(&mut fields, "Bit rate mode", c.bit_rate_mode.clone());
        push_opt(
            &mut fields,
            "Bit rate",
            c.bit_rate_kbps.map(|b| format!("{b:.0} kb/s")),
        );
        push_opt(
            &mut fields,
            "Channel(s)",
            c.channels.map(|ch| format!("{ch} channels")),
        );
        push_opt(&mut fields, "Channel layout", c.channel_layout.clone());
        push_opt(
            &mut fields,
            "Sampling rate",
            c.sampling_rate_hz
                .map(|hz| format!("{:.1} kHz", hz as f64 / 1000.0)),
        );
        push_opt(&mut fields, "Frame rate", c.frame_rate.clone());
        push_opt(&mut fields, "Compression mode", c.compression_mode.clone());
        push_opt(&mut fields, "Language", c.language.clone());
        push_opt(
            &mut fields,
            "Delay relative to video",
            c.delay_relative_to_video_ms.map(|ms| format!("{ms} ms")),
        );
        push_opt(&mut fields, "Service kind", c.service_kind.clone());
        push_opt(
            &mut fields,
            "Dialog Normalization",
            c.dialog_normalization_db.map(|db| format!("{db} dB")),
        );
        if c.encrypted {
            fields.push(field("Encryption", "Encrypted".to_string()));
        }
    }
    MediaInfoSection { title, fields }
}

fn base_stream_fields(
    pid: Pid,
    menu_id: u16,
    stream: &PmtStream,
    codec: Option<&ElementaryCodecInfo>,
    metrics: &MetricsSnapshot,
) -> Vec<ReportField> {
    let mut fields = vec![
        field("ID", format!("{pid} (0x{pid:X})")),
        field("Menu ID", format!("{menu_id} (0x{menu_id:X})")),
        field("Codec ID", stream.stream_type.to_string()),
    ];
    if let Some(br) = metrics
        .pid_table
        .iter()
        .find(|e| e.pid == pid)
        .map(|e| e.bitrate_kbps)
    {
        if br > 0.0 {
            fields.push(field("Bit rate", format!("{br:.0} kb/s")));
        }
    }
    let _ = (stream, codec);
    fields
}

fn merge_codec_fields(fields: &mut Vec<ReportField>, codec: Option<&ElementaryCodecInfo>) {
    let Some(c) = codec else { return };
    push_opt(fields, "Format", c.format.clone());
    push_opt(fields, "Format/Info", c.format_info.clone());
    push_opt(fields, "Format profile", c.format_profile.clone());
    push_opt(fields, "Format settings", c.format_settings.clone());
    push_opt(
        fields,
        "Format settings, CABAC",
        c.format_settings_cabac.clone(),
    );
    push_opt(
        fields,
        "Format settings, Reference frames",
        c.format_settings_ref_frames.clone(),
    );
    push_opt(
        fields,
        "Format settings, GOP",
        c.format_settings_gop.clone(),
    );
    push_opt(fields, "Commercial name", c.commercial_name.clone());
    push_opt(fields, "Muxing mode", c.muxing_mode.clone());
}

fn classify_stream(stream: &PmtStream) -> StreamKind {
    if stream.is_audio() {
        StreamKind::Audio
    } else if matches!(stream.stream_type, 0x01 | 0x02 | 0x1B | 0x24) {
        StreamKind::Video
    } else {
        StreamKind::Data
    }
}

fn primary_video_fps(input: &MediaInfoBuildInput<'_>) -> Option<String> {
    for info in input.codec.streams.values() {
        if info.kind == Some(StreamKind::Video) {
            return info.frame_rate.clone();
        }
    }
    None
}

fn field(key: &str, value: String) -> ReportField {
    ReportField {
        key: key.to_string(),
        value,
    }
}

fn push_opt(fields: &mut Vec<ReportField>, key: &str, value: Option<String>) {
    if let Some(v) = value.filter(|s| !s.is_empty()) {
        if !fields.iter().any(|f| f.key == key) {
            fields.push(field(key, v));
        }
    }
}

/// Extrai contexto NIT/TOT de descriptors para o bloco General.
///
/// SPEC-MI-005
pub fn enrich_tables_ctx_from_descriptors(
    ctx: &mut MediaInfoTablesCtx,
    nit_descriptors: &[Descriptor],
    tot_descriptors: &[Descriptor],
) {
    for desc in nit_descriptors {
        match desc.decode() {
            KnownDescriptor::NetworkName { name } => {
                ctx.nit_network_name = Some(name);
            }
            KnownDescriptor::SatelliteDelivery {
                frequency_hz,
                orbital_position_tenths,
                west_east_flag,
                ..
            } => {
                ctx.nit_frequency_hz = Some(frequency_hz);
                let deg = orbital_position_tenths as f64 / 10.0;
                ctx.nit_orbital_position = Some(format!(
                    "{deg:.1}{}",
                    if west_east_flag { "W" } else { "E" }
                ));
            }
            _ => {}
        }
    }
    for desc in tot_descriptors {
        if let KnownDescriptor::LocalTimeOffset {
            country_code,
            local_time_offset_polarity,
            local_time_offset_h,
            local_time_offset_m,
            ..
        } = desc.decode()
        {
            let sign = if local_time_offset_polarity { "+" } else { "-" };
            ctx.tot_country = Some(country_code);
            ctx.tot_timezone = Some(format!(
                "{sign}{local_time_offset_h:02}:{local_time_offset_m:02}"
            ));
        } else if desc.tag == 0x58 && desc.data.len() >= 6 {
            let country = String::from_utf8_lossy(&desc.data[0..3]).to_string();
            let polarity = desc.data[3] & 0x01 != 0;
            let h = desc.data[4];
            let m = desc.data[5];
            let sign = if polarity { "+" } else { "-" };
            ctx.tot_country = Some(country);
            ctx.tot_timezone = Some(format!("{sign}{h:02}:{m:02}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_mi_005_report_text_format() {
        let report = MediaInfoReport {
            sections: vec![MediaInfoSection {
                title: "General".to_string(),
                fields: vec![field("Format", "MPEG-TS".to_string())],
            }],
        };
        assert!(report.to_text().contains("Format"));
        assert!(report.to_text().contains("MPEG-TS"));
    }
}
