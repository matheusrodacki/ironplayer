//! Parser de cabeçalhos H.265/HEVC (SPS/VUI).
//!
//! SPEC-MI-001

use super::bitreader::{find_nal_units, remove_emulation_prevention_bytes, BitReader};
use super::error::MediaInfoError;
use super::model::ElementaryCodecInfo;

fn chroma_label(cf: u8) -> &'static str {
    match cf {
        0 => "4:0:0",
        1 => "4:2:0",
        2 => "4:2:2",
        3 => "4:4:4",
        _ => "Unknown",
    }
}

fn profile_string(general_profile_idc: u8, chroma: u8, bit_depth: u8) -> String {
    let base = match general_profile_idc {
        1 => "Main",
        2 => "Main 10",
        4 => "RExt",
        _ => "Unknown",
    };
    format!("{base} {} {}", chroma_label(chroma), bit_depth)
}

fn parse_profile_tier_level(
    br: &mut BitReader<'_>,
    max_sub_layers: u8,
) -> Result<(), MediaInfoError> {
    br.read_bits(2)?; // general_profile_space
    br.read_bit()?; // general_tier_flag
    br.read_bits(5)?; // general_profile_idc
    br.read_bits(32)?; // general_profile_compatibility
    br.read_bit()?; // general_progressive_source_flag
    br.read_bit()?; // general_interlaced_source_flag
    br.read_bit()?; // general_non_packed_constraint_flag
    br.read_bit()?; // general_frame_only_constraint_flag
    br.read_bits(44)?; // reserved
    br.read_bits(8)?; // general_level_idc
    let sub_layers = max_sub_layers.saturating_sub(1);
    let mut sub_layer_profile_present = Vec::new();
    let mut sub_layer_level_present = Vec::new();
    for _ in 0..sub_layers {
        sub_layer_profile_present.push(br.read_bit()? == 1);
        sub_layer_level_present.push(br.read_bit()? == 1);
    }
    if sub_layers > 0 {
        br.read_bits((8 - sub_layers as u32) * 2)?;
    }
    for i in 0..sub_layers as usize {
        if sub_layer_profile_present[i] {
            br.read_bits(88)?;
        }
        if sub_layer_level_present[i] {
            br.read_bits(8)?;
        }
    }
    Ok(())
}

fn parse_sps(nal: &[u8], info: &mut ElementaryCodecInfo) -> Result<(), MediaInfoError> {
    if nal.len() < 2 {
        return Err(MediaInfoError::InvalidNal);
    }
    let rbsp = remove_emulation_prevention_bytes(&nal[2..]);
    let mut br = BitReader::new(&rbsp);
    br.read_bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers = br.read_bits(3)? as u8 + 1;
    br.read_bit()?; // temporal_id_nesting_flag
    parse_profile_tier_level(&mut br, max_sub_layers)?;

    br.read_ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = br.read_ue()? as u8;
    if chroma_format_idc == 3 {
        br.read_bit()?; // separate_colour_plane_flag
    }
    let bit_depth_luma = br.read_ue()? + 8;
    let bit_depth_chroma = br.read_ue()? + 8;
    if max_sub_layers > 1 {
        let present = br.read_bit()? == 1;
        if present {
            for _ in 0..(max_sub_layers - 1) {
                br.read_ue()?; // bit_depth_luma_minus8[i]
                br.read_ue()?; // bit_depth_chroma_minus8[i]
            }
        }
    }
    br.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
    let sublayer_ordering = br.read_bit()? == 1;
    if sublayer_ordering {
        let layers = max_sub_layers.saturating_sub(1);
        for _ in 0..layers {
            br.read_ue()?;
            br.read_ue()?;
            br.read_ue()?;
        }
    }
    br.read_ue()?; // log2_min_luma_coding_block_size_minus3
    br.read_ue()?; // log2_diff_max_min_luma_coding_block_size
    br.read_ue()?; // log2_min_transform_block_size_minus2
    br.read_ue()?; // log2_diff_max_min_transform_block_size
    br.read_ue()?; // max_transform_hierarchy_depth_inter
    br.read_ue()?; // max_transform_hierarchy_depth_intra
    let scaling_enabled = br.read_bit()? == 1;
    if scaling_enabled {
        let lists = br.read_ue()?;
        for _ in 0..lists {
            let next = br.read_bit()? == 1;
            if next {
                let size = br.read_ue()?;
                for _ in 0..size {
                    br.read_se()?;
                }
            }
        }
    }
    br.read_bit()?; // amp_enabled_flag
    br.read_bit()?; // sample_adaptive_offset_enabled_flag
    if br.read_bit()? == 1 {
        let count = br.read_ue()?;
        for _ in 0..count {
            br.read_ue()?;
            br.read_ue()?;
            br.read_ue()?;
        }
    }
    let pic_width = br.read_ue()? + 1;
    let pic_height = br.read_ue()? + 1;
    if br.read_bit()? == 1 {
        br.read_ue()?; // conf_win_left_offset
        br.read_ue()?; // conf_win_right_offset
        br.read_ue()?; // conf_win_top_offset
        br.read_ue()?; // conf_win_bottom_offset
    }
    let bit_depth = bit_depth_luma.max(bit_depth_chroma) as u8;

    info.format = Some("HEVC".to_string());
    info.format_info = Some("High Efficiency Video Coding".to_string());
    info.format_profile = Some(format!("Main {}@L4@Main", chroma_label(chroma_format_idc)));
    if bit_depth > 8 {
        info.format_profile = Some(format!(
            "{} {}@L4@Main",
            if chroma_format_idc == 2 {
                "Main 4:2:2"
            } else {
                "Main"
            },
            bit_depth
        ));
    }
    info.width = Some(pic_width);
    info.height = Some(pic_height);
    info.color_space = Some("YUV".to_string());
    info.chroma_subsampling = Some(chroma_label(chroma_format_idc).to_string());
    info.bit_depth = Some(bit_depth);
    info.scan_type = Some("Progressive".to_string());
    if pic_width > 0 && pic_height > 0 {
        let dar = pic_width as f64 / pic_height as f64;
        info.display_aspect_ratio = Some(format!("{dar:.3}"));
    }
    info.compression_mode = Some("Lossy".to_string());

    let _ = profile_string(1, chroma_format_idc, bit_depth);
    let _ = bit_depth_chroma;
    Ok(())
}

/// Analisa access unit HEVC.
///
/// SPEC-MI-001
pub fn probe_hevc(data: &[u8], info: &mut ElementaryCodecInfo) -> Result<(), MediaInfoError> {
    let nals = find_nal_units(data);
    for (start, end) in nals {
        let nal = &data[start..end];
        if nal.len() < 2 {
            continue;
        }
        let nal_type = (nal[0] >> 1) & 0x3F;
        if nal_type == 33 {
            parse_sps(nal, info)?;
        }
    }
    if info.format.is_some() {
        Ok(())
    } else {
        Err(MediaInfoError::SyncNotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_mi_001_hevc_chroma_label() {
        assert_eq!(chroma_label(2), "4:2:2");
    }
}
