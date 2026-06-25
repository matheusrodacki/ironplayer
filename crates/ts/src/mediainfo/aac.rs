//! Parser de frames AAC (ADTS / LATM).
//!
//! SPEC-MI-001

use super::error::MediaInfoError;
use super::model::ElementaryCodecInfo;

const SAMPLE_RATES: [u32; 16] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350, 0, 0,
    0,
];

fn object_type_label(ot: u8) -> &'static str {
    match ot {
        1 => "AAC Main",
        2 => "AAC LC",
        3 => "AAC SSR",
        4 => "AAC LTP",
        5 => "SBR",
        29 => "HE-AAC",
        _ => "AAC",
    }
}

fn channel_layout(ch: u8) -> (u16, String) {
    match ch {
        1 => (1, "C".to_string()),
        2 => (2, "L R".to_string()),
        3 => (3, "L R C".to_string()),
        4 => (4, "L R C Cs".to_string()),
        5 => (5, "L R C Ls Rs".to_string()),
        6 => (6, "L R C LFE Ls Rs".to_string()),
        7 => (8, "L R C LFE Ls Rs Lrs Rrs".to_string()),
        _ => (2, "L R".to_string()),
    }
}

fn parse_audio_specific_config(
    data: &[u8],
    info: &mut ElementaryCodecInfo,
) -> Result<(), MediaInfoError> {
    if data.is_empty() {
        return Err(MediaInfoError::InsufficientData {
            expected: 1,
            found: 0,
        });
    }
    let mut br = super::bitreader::BitReader::new(data);
    let object_type = br.read_bits(5)? as u8;
    let mut ot = object_type;
    if object_type == 31 {
        ot = (br.read_bits(6)? + 32) as u8;
    }
    let freq_idx = br.read_bits(4)? as usize;
    let mut sample_rate = SAMPLE_RATES.get(freq_idx).copied().unwrap_or(0);
    if freq_idx == 0x0F {
        sample_rate = br.read_bits(24)?;
    }
    let channel_config = br.read_bits(4)? as u8;
    let (channels, layout) = channel_layout(channel_config);

    info.format = Some(object_type_label(ot).to_string());
    info.format_info = Some("Advanced Audio Codec Low Complexity".to_string());
    info.sampling_rate_hz = Some(sample_rate);
    info.channels = Some(channels);
    info.channel_layout = Some(layout);
    info.samples_per_frame = Some(1024);
    if sample_rate > 0 {
        let fps = sample_rate as f64 / 1024.0;
        info.frame_rate = Some(format!("{fps:.3} FPS (1024 SPF)"));
    }
    info.compression_mode = Some("Lossy".to_string());
    info.bit_rate_mode = Some("Variable".to_string());
    Ok(())
}

fn probe_adts(data: &[u8], info: &mut ElementaryCodecInfo) -> Result<(), MediaInfoError> {
    for i in 0..data.len().saturating_sub(7) {
        if data[i] != 0xFF || (data[i + 1] & 0xF0) != 0xF0 {
            continue;
        }
        let profile = ((data[i + 2] >> 6) & 0x03) + 1;
        let freq_idx = ((data[i + 2] >> 2) & 0x0F) as usize;
        let channel_config = ((data[i + 2] & 0x01) << 2) | ((data[i + 3] >> 6) & 0x03);
        let sample_rate = SAMPLE_RATES.get(freq_idx).copied().unwrap_or(48000);
        let (channels, layout) = channel_layout(channel_config);

        info.format = Some(object_type_label(profile).to_string());
        info.format_info = Some("Advanced Audio Codec Low Complexity".to_string());
        info.muxing_mode = Some("ADTS".to_string());
        info.sampling_rate_hz = Some(sample_rate);
        info.channels = Some(channels);
        info.channel_layout = Some(layout);
        info.samples_per_frame = Some(1024);
        if sample_rate > 0 {
            let fps = sample_rate as f64 / 1024.0;
            info.frame_rate = Some(format!("{fps:.3} FPS (1024 SPF)"));
        }
        info.compression_mode = Some("Lossy".to_string());
        info.bit_rate_mode = Some("Variable".to_string());
        return Ok(());
    }
    Err(MediaInfoError::SyncNotFound)
}

fn probe_latm(data: &[u8], info: &mut ElementaryCodecInfo) -> Result<(), MediaInfoError> {
    for i in 0..data.len().saturating_sub(4) {
        if data[i] != 0x56 && data[i] != 0x46 {
            continue;
        }
        // LOAS AudioMuxElement — procura AudioSpecificConfig após headers mínimos
        for j in i..data.len().saturating_sub(2) {
            if let Ok(()) = parse_audio_specific_config(&data[j..], info) {
                info.muxing_mode = Some("LATM".to_string());
                return Ok(());
            }
        }
    }
    probe_adts(data, info).map(|()| {
        info.muxing_mode = Some("LATM".to_string());
    })
}

/// Analisa frame AAC (ADTS ou LATM).
///
/// SPEC-MI-001
pub fn probe_aac(
    data: &[u8],
    latm: bool,
    info: &mut ElementaryCodecInfo,
) -> Result<(), MediaInfoError> {
    if latm {
        probe_latm(data, info)
    } else {
        probe_adts(data, info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_mi_001_aac_object_type_label() {
        assert_eq!(object_type_label(2), "AAC LC");
    }
}
