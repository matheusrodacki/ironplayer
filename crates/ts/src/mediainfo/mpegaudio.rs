//! Parser de frame header MPEG-1/2 Audio.
//!
//! SPEC-MI-001

use super::error::MediaInfoError;
use super::model::ElementaryCodecInfo;

const BITRATES_V1_L2: [u16; 16] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
];

const SAMPLE_RATES: [u32; 4] = [44100, 48000, 32000, 0];

/// Analisa syncframe MPEG Audio.
///
/// SPEC-MI-001
pub fn probe_mpegaudio(data: &[u8], info: &mut ElementaryCodecInfo) -> Result<(), MediaInfoError> {
    for i in 0..data.len().saturating_sub(4) {
        if data[i] != 0xFF || (data[i + 1] & 0xE0) != 0xE0 {
            continue;
        }
        let b1 = data[i + 1];
        let b2 = data[i + 2];
        let version = (b1 >> 3) & 0x03;
        let layer = 4 - ((b1 >> 1) & 0x03);
        let bitrate_idx = (b2 >> 4) & 0x0F;
        let sample_idx = (b2 >> 2) & 0x03;
        let channel_mode = b2 & 0x03;

        if layer == 0 || layer > 3 || sample_idx == 3 {
            continue;
        }

        let version_label = match version {
            3 => "Version 1",
            2 => "Version 2",
            0 => "Version 2.5",
            _ => "Unknown",
        };
        let layer_label = match layer {
            1 => "Layer 3",
            2 => "Layer 2",
            3 => "Layer 1",
            _ => "Unknown",
        };

        let bitrate_kbps = if version == 3 && layer == 2 {
            BITRATES_V1_L2[bitrate_idx as usize] as f64
        } else {
            0.0
        };
        let sample_rate = SAMPLE_RATES[sample_idx as usize];
        let (channels, layout) = channel_layout(channel_mode);

        info.format = Some("MPEG Audio".to_string());
        info.format_info = Some(format!("{version_label} {layer_label}"));
        info.format_profile = Some(layer_label.to_string());
        if bitrate_kbps > 0.0 {
            info.bit_rate_kbps = Some(bitrate_kbps);
            info.bit_rate_mode = Some("Constant".to_string());
        }
        info.sampling_rate_hz = Some(sample_rate);
        info.channels = Some(channels);
        info.channel_layout = Some(layout);
        info.samples_per_frame = Some(if layer == 2 { 1152 } else { 576 });
        if sample_rate > 0 {
            let spf = info.samples_per_frame.unwrap_or(1152);
            let fps = sample_rate as f64 / spf as f64;
            info.frame_rate = Some(format!("{fps:.3} FPS ({spf} SPF)"));
        }
        info.compression_mode = Some("Lossy".to_string());
        info.muxing_mode = None;
        return Ok(());
    }
    Err(MediaInfoError::SyncNotFound)
}

fn channel_layout(mode: u8) -> (u16, String) {
    match mode {
        0 => (2, "L R".to_string()),
        1 => (2, "L R".to_string()),
        2 => (2, "L R".to_string()),
        3 => (1, "M".to_string()),
        _ => (2, "L R".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_mi_001_mpegaudio_sync_search_empty() {
        let mut info = ElementaryCodecInfo::default();
        assert!(probe_mpegaudio(&[], &mut info).is_err());
    }
}
