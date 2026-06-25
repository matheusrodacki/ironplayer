//! Parser de sequence header MPEG-2 Video.
//!
//! SPEC-MI-001

use super::error::MediaInfoError;
use super::model::ElementaryCodecInfo;

/// Analisa início de stream MPEG-2 Video.
///
/// SPEC-MI-001
pub fn probe_mpeg2video(data: &[u8], info: &mut ElementaryCodecInfo) -> Result<(), MediaInfoError> {
    let mut i = 0;
    while i + 4 < data.len() {
        if data[i] == 0x00 && data[i + 1] == 0x00 && data[i + 2] == 0x01 {
            let start_code = data[i + 3];
            if start_code == 0xB3 && i + 11 < data.len() {
                let horiz = u16::from_be_bytes([data[i + 4], data[i + 5]]) >> 4;
                let vert = u16::from_be_bytes([data[i + 5], data[i + 6]]) & 0x0FFF;
                let aspect = (data[i + 7] >> 4) & 0x0F;
                let fr_code = data[i + 7] & 0x0F;
                let _bitrate = u16::from_be_bytes([data[i + 8], data[i + 9]]) >> 6;

                info.format = Some("MPEG-2 Video".to_string());
                info.width = Some(horiz as u32);
                info.height = Some(vert as u32);
                info.color_space = Some("YUV".to_string());
                info.chroma_subsampling = Some("4:2:0".to_string());
                info.bit_depth = Some(8);
                info.display_aspect_ratio = Some(aspect_label(aspect));
                if let Some((num, den)) = frame_rate_code(fr_code) {
                    let fps = num as f64 / den as f64;
                    info.frame_rate = Some(format!("{fps:.3} ({num}/{den}) FPS"));
                    info.frame_rate_num = Some(num);
                    info.frame_rate_den = Some(den);
                }
                info.compression_mode = Some("Lossy".to_string());
                return Ok(());
            }
            if start_code == 0xB5 && i + 7 < data.len() {
                let ext = data[i + 4];
                if ext == 0x01 {
                    let profile = (data[i + 5] >> 6) & 0x03;
                    let level = data[i + 5] & 0x0F;
                    info.format_profile = Some(format!("{}@L{}", profile_label(profile), level));
                }
            }
        }
        i += 1;
    }
    if info.format.is_some() {
        Ok(())
    } else {
        Err(MediaInfoError::SyncNotFound)
    }
}

fn aspect_label(code: u8) -> String {
    match code {
        1 => "1:1".to_string(),
        2 => "4:3".to_string(),
        3 => "16:9".to_string(),
        4 => "2.21:1".to_string(),
        _ => "Unknown".to_string(),
    }
}

fn profile_label(p: u8) -> &'static str {
    match p {
        0 => "Simple",
        1 => "Main",
        2 => "SNR",
        3 => "Spatial",
        4 => "High",
        _ => "Unknown",
    }
}

fn frame_rate_code(code: u8) -> Option<(u32, u32)> {
    match code {
        1 => Some((24000, 1001)),
        2 => Some((24, 1)),
        3 => Some((25, 1)),
        4 => Some((30000, 1001)),
        5 => Some((30, 1)),
        6 => Some((50, 1)),
        7 => Some((60000, 1001)),
        8 => Some((60, 1)),
        _ => None,
    }
}
