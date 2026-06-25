//! Parser de syncframe AC-3 / E-AC-3.
//!
//! SPEC-MI-001

use super::error::MediaInfoError;
use super::model::ElementaryCodecInfo;

const BITRATE_TABLE: [[u16; 19]; 3] = [
    [
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
    ],
    [
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
    ],
    [
        32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640, 768,
    ],
];

const SAMPLE_RATES: [u32; 4] = [48000, 44100, 32000, 0];

fn acmod_layout(acmod: u8, lfe: bool) -> (u16, String) {
    let layout = match acmod {
        0 => "1+1".to_string(),
        1 => "C".to_string(),
        2 => "L R".to_string(),
        3 => "L R C".to_string(),
        4 => "L R Ls Rs".to_string(),
        5 => "L R C Ls Rs".to_string(),
        6 => {
            if lfe {
                "L R C LFE Ls Rs".to_string()
            } else {
                "L R C Ls Rs".to_string()
            }
        }
        7 => "L R C LFE Ls Rs".to_string(),
        _ => "L R".to_string(),
    };
    let channels = match acmod {
        0 => 2,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 5,
        6 | 7 => {
            if lfe {
                6
            } else {
                5
            }
        }
        _ => 2,
    };
    (channels, layout)
}

/// Analisa syncframe AC-3 ou E-AC-3.
///
/// SPEC-MI-001
pub fn probe_ac3(data: &[u8], info: &mut ElementaryCodecInfo) -> Result<(), MediaInfoError> {
    for i in 0..data.len().saturating_sub(8) {
        if data[i] != 0x0B || data[i + 1] != 0x77 {
            continue;
        }
        let b5 = data[i + 5];
        let fscod = (b5 >> 6) & 0x03;
        let frmsizecod = b5 & 0x3F;
        if fscod == 3 || frmsizecod >= 38 {
            continue;
        }

        let b6 = data[i + 6];
        let bsid = (b6 >> 3) & 0x1F;
        let bsmod = b6 & 0x07;
        let b7 = data[i + 7];
        let acmod = (b7 >> 5) & 0x07;
        let lfeon = (b7 >> 1) & 0x01 == 1;

        let bitrate_kbps = BITRATE_TABLE[fscod as usize][frmsizecod as usize] as f64;
        let sample_rate = SAMPLE_RATES[fscod as usize];
        let (channels, layout) = acmod_layout(acmod, lfeon);

        let is_eac3 = bsid > 10;
        info.format = Some(if is_eac3 {
            "E-AC-3".to_string()
        } else {
            "AC-3".to_string()
        });
        info.format_info = Some("Audio Coding 3".to_string());
        info.commercial_name = Some("Dolby Digital".to_string());
        info.bit_rate_kbps = Some(bitrate_kbps);
        info.bit_rate_mode = Some("Constant".to_string());
        info.sampling_rate_hz = Some(sample_rate);
        info.channels = Some(channels);
        info.channel_layout = Some(layout);
        info.samples_per_frame = Some(1536);
        if sample_rate > 0 {
            let fps = sample_rate as f64 / 1536.0;
            info.frame_rate = Some(format!("{fps:.3} FPS (1536 SPF)"));
        }
        info.compression_mode = Some("Lossy".to_string());
        info.service_kind = Some("Complete Main".to_string());

        if i + 11 < data.len() {
            let bsn = data[i + 8] as i32;
            if bsn <= 31 {
                info.dialog_normalization_db = Some(-bsn);
            }
        }

        let _ = bsmod;
        return Ok(());
    }
    Err(MediaInfoError::SyncNotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_mi_001_ac3_layout_stereo() {
        let (ch, layout) = acmod_layout(2, false);
        assert_eq!(ch, 2);
        assert_eq!(layout, "L R");
    }
}
