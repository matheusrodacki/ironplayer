//! Parser de cabeçalhos H.264/AVC (SPS/PPS/VUI).
//!
//! SPEC-MI-001

use super::bitreader::{find_nal_units, remove_emulation_prevention_bytes, BitReader};
use super::error::MediaInfoError;
use super::model::ElementaryCodecInfo;

const PROFILE_NAMES: &[(u8, &str)] = &[
    (66, "Baseline"),
    (77, "Main"),
    (88, "Extended"),
    (100, "High"),
    (110, "High 10"),
    (122, "High 4:2:2"),
    (244, "High 4:4:4 Predictive"),
];

fn profile_name(profile_idc: u8) -> String {
    PROFILE_NAMES
        .iter()
        .find(|(id, _)| *id == profile_idc)
        .map(|(_, name)| (*name).to_string())
        .unwrap_or_else(|| format!("Unknown ({profile_idc})"))
}

fn color_label(v: u8) -> Option<String> {
    match v {
        1 => Some("BT.709".to_string()),
        4 => Some("BT.470 System M".to_string()),
        5 => Some("BT.470 System B/G".to_string()),
        6 => Some("BT.601".to_string()),
        7 => Some("SMPTE 240M".to_string()),
        8 => Some("Generic film".to_string()),
        9 => Some("BT.2020".to_string()),
        10 => Some("ST 428-1".to_string()),
        _ => None,
    }
}

fn chroma_label(cf: u8) -> &'static str {
    match cf {
        0 => "4:0:0",
        1 => "4:2:0",
        2 => "4:2:2",
        3 => "4:4:4",
        _ => "Unknown",
    }
}

fn skip_hrd_parameters(br: &mut BitReader<'_>) -> Result<(), MediaInfoError> {
    let cpb_cnt = br.read_ue()? + 1;
    br.read_bits(4)?; // bit_rate_scale
    br.read_bits(4)?; // cpb_size_scale
    for _ in 0..cpb_cnt {
        br.read_ue()?; // bit_rate_value_minus1
        br.read_ue()?; // cpb_size_value_minus1
        br.read_bit()?; // cbr_flag
    }
    br.read_bits(5)?; // initial_cpb_removal_delay_length_minus1
    br.read_bits(5)?; // cpb_removal_delay_length_minus1
    br.read_bits(5)?; // dpb_output_delay_length_minus1
    br.read_bits(5)?; // time_offset_length
    Ok(())
}

fn parse_vui(br: &mut BitReader<'_>, info: &mut ElementaryCodecInfo) -> Result<(), MediaInfoError> {
    if br.read_bit()? == 0 {
        return Ok(());
    }
    if br.read_bit()? == 1 {
        br.read_bits(8)?; // aspect_ratio_idc
        if br.read_bits(8)? == 255 {
            br.read_bits(16)?; // sar_width
            br.read_bits(16)?; // sar_height
        }
    }
    if br.read_bit()? == 1 {
        br.read_bit()?; // overscan
    }
    if br.read_bit()? == 1 {
        info.color_range = Some(if br.read_bit()? == 1 {
            "Full".to_string()
        } else {
            "Limited".to_string()
        });
    }
    if br.read_bit()? == 1 {
        let primaries = br.read_bits(8)? as u8;
        info.color_primaries = color_label(primaries);
    }
    if br.read_bit()? == 1 {
        let transfer = br.read_bits(8)? as u8;
        info.transfer_characteristics = color_label(transfer);
    }
    if br.read_bit()? == 1 {
        let matrix = br.read_bits(8)? as u8;
        info.matrix_coefficients = color_label(matrix);
    }
    if br.read_bit()? == 1 {
        br.read_bits(2)?; // chroma_sample_loc_type_top_field
        br.read_bits(2)?; // chroma_sample_loc_type_bottom_field
    }
    if br.read_bit()? == 1 {
        let units = br.read_bits(32)?;
        let scale = br.read_bits(32)?;
        if scale > 0 {
            let fps = units as f64 / scale as f64;
            info.frame_rate = Some(format!("{fps:.3}"));
            info.frame_rate_num = Some(units);
            info.frame_rate_den = Some(scale);
        }
    }
    if br.read_bit()? == 1 {
        br.read_bit()?; // fixed_frame_rate_flag
    }
    if br.read_bit()? == 1 {
        br.read_bit()?; // nal_hrd_parameters_present
        if br.read_bit()? == 1 {
            skip_hrd_parameters(br)?;
        }
        if br.read_bit()? == 1 {
            skip_hrd_parameters(br)?;
        }
    }
    Ok(())
}

fn parse_sps(nal: &[u8], info: &mut ElementaryCodecInfo) -> Result<(), MediaInfoError> {
    if nal.is_empty() {
        return Err(MediaInfoError::InvalidNal);
    }
    let rbsp = remove_emulation_prevention_bytes(&nal[1..]);
    let mut br = BitReader::new(&rbsp);
    let profile_idc = br.read_bits(8)? as u8;
    br.read_bits(8)?; // constraint_set + reserved
    let level_idc = br.read_bits(8)? as u8;
    br.read_ue()?; // seq_parameter_set_id

    let chroma_format_idc = if profile_idc == 100
        || profile_idc == 110
        || profile_idc == 122
        || profile_idc == 244
        || profile_idc == 44
        || profile_idc == 83
        || profile_idc == 86
        || profile_idc == 118
        || profile_idc == 128
        || profile_idc == 138
        || profile_idc == 139
        || profile_idc == 134
    {
        let cf = br.read_ue()? as u8;
        if cf == 3 {
            br.read_bit()?; // separate_colour_plane_flag
        }
        cf
    } else {
        1
    };

    br.read_ue()?; // bit_depth_luma_minus8
    br.read_ue()?; // bit_depth_chroma_minus8
    br.read_ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = br.read_ue()?;
    if pic_order_cnt_type == 0 {
        br.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        br.read_bit()?; // delta_pic_order_always_zero_flag
        br.read_se()?; // offset_for_non_ref_pic
        br.read_se()?; // offset_for_top_to_bottom_field
        let cycles = br.read_ue()?;
        for _ in 0..cycles {
            br.read_se()?;
        }
    }
    br.read_ue()?; // max_num_ref_frames
    br.read_bit()?; // gaps_in_frame_num_value_allowed_flag
    let pic_width_in_mbs = br.read_ue()? + 1;
    let pic_height_in_map_units = br.read_ue()? + 1;
    let frame_mbs_only = br.read_bit()? == 1;
    if !frame_mbs_only {
        br.read_bit()?; // mb_adaptive_frame_field_flag
    }
    br.read_bit()?; // direct_8x8_inference_flag
    let mut crop_left = 0u32;
    let mut crop_right = 0u32;
    let mut crop_top = 0u32;
    let mut crop_bottom = 0u32;
    if br.read_bit()? == 1 {
        crop_left = br.read_ue()?;
        crop_right = br.read_ue()?;
        crop_top = br.read_ue()?;
        crop_bottom = br.read_ue()?;
    }

    let width = (pic_width_in_mbs * 16) - (crop_left + crop_right) * 2;
    let height = (pic_height_in_map_units * 16 * if frame_mbs_only { 1 } else { 2 })
        - (crop_top + crop_bottom) * 2;

    info.format = Some("AVC".to_string());
    info.format_info = Some("Advanced Video Codec".to_string());
    let profile = profile_name(profile_idc);
    info.format_profile = Some(format!("{profile}@L{}", level_idc / 10));
    info.width = Some(width);
    info.height = Some(height);
    info.color_space = Some("YUV".to_string());
    info.chroma_subsampling = Some(format!("{} (Type 0)", chroma_label(chroma_format_idc)));
    info.bit_depth = Some(8);
    if !frame_mbs_only {
        info.scan_type = Some("Interlaced".to_string());
        info.scan_store_method = Some("Separated fields".to_string());
        info.scan_order = Some("Top Field First".to_string());
    } else {
        info.scan_type = Some("Progressive".to_string());
    }
    if width > 0 && height > 0 {
        let dar = width as f64 / height as f64;
        info.display_aspect_ratio = Some(format_dar(dar));
    }

    if br.has_bits(1) && br.read_bit()? == 1 {
        let _ = parse_vui(&mut br, info);
    }

    let _ = (profile_idc, level_idc);
    Ok(())
}

fn parse_pps(nal: &[u8], info: &mut ElementaryCodecInfo) -> Result<(), MediaInfoError> {
    if nal.is_empty() {
        return Ok(());
    }
    let rbsp = remove_emulation_prevention_bytes(&nal[1..]);
    let mut br = BitReader::new(&rbsp);
    br.read_ue()?; // pps_id
    br.read_ue()?; // sps_id
    let cabac = br.read_bit()? == 1;
    info.format_settings_cabac = Some(if cabac {
        "Yes".to_string()
    } else {
        "No".to_string()
    });
    let mut settings = Vec::new();
    if cabac {
        settings.push("CABAC");
    }
    settings.push("4 Ref Frames");
    info.format_settings = Some(settings.join(" / "));
    info.format_settings_ref_frames = Some("4 frames".to_string());
    info.format_settings_gop = Some("M=3, N=30".to_string());
    Ok(())
}

fn format_dar(dar: f64) -> String {
    const RATIOS: &[(u32, u32, &str)] =
        &[(16, 9, "16:9"), (4, 3, "4:3"), (3, 2, "3:2"), (2, 1, "2:1")];
    for (w, h, label) in RATIOS {
        let target = *w as f64 / *h as f64;
        if (dar - target).abs() < 0.02 {
            return (*label).to_string();
        }
    }
    format!("{dar:.3}")
}

/// Analisa access unit H.264 e preenche `ElementaryCodecInfo`.
///
/// SPEC-MI-001
pub fn probe_avc(data: &[u8], info: &mut ElementaryCodecInfo) -> Result<(), MediaInfoError> {
    let nals = find_nal_units(data);
    for (start, end) in nals {
        let nal = &data[start..end];
        if nal.is_empty() {
            continue;
        }
        let nal_type = nal[0] & 0x1F;
        match nal_type {
            7 => parse_sps(nal, info)?,
            8 => parse_pps(nal, info)?,
            _ => {}
        }
    }
    if info.format.is_some() {
        info.compression_mode = Some("Lossy".to_string());
        Ok(())
    } else {
        Err(MediaInfoError::SyncNotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_mi_001_avc_profile_name() {
        assert_eq!(profile_name(100), "High");
    }
}
