//! Detecção de scan type (progressivo vs entrelaçado) para streams de vídeo.
//!
//! Usa parse leve de SPS H.264, `AVCodecContext::field_order` e flags de frame.
//!
//! SPEC-AV-005

use crate::codec::DeinterlaceMode;

/// Tipo de varredura do vídeo detectado por PID.
///
/// SPEC-AV-005
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScanType {
    /// Ainda não determinado (aguardando SPS, field_order ou flag de frame).
    #[default]
    Unknown,
    /// Conteúdo progressivo.
    Progressive,
    /// Conteúdo entrelaçado (1080i, 576i, etc.).
    Interlaced,
}

impl ScanType {
    /// Retorna `true` quando o tipo de varredura já foi fixado para o stream.
    ///
    /// SPEC-AV-005
    pub fn is_resolved(self) -> bool {
        !matches!(self, Self::Unknown)
    }

    /// Retorna `true` quando o stream é entrelaçado.
    ///
    /// SPEC-AV-005
    pub fn is_interlaced(self) -> bool {
        matches!(self, Self::Interlaced)
    }

    /// Rótulo estável para métricas e UI.
    ///
    /// SPEC-AV-005
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::Progressive => "Progressive",
            Self::Interlaced => "Interlaced",
        }
    }
}

/// Motivo do estado atual do deinterlacer (métricas / diagnóstico).
///
/// SPEC-AV-005
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeinterlaceReason {
    #[default]
    NotDetected,
    Off,
    NoAvfilter,
    Active,
    Forced,
}

impl DeinterlaceReason {
    /// Rótulo estável para métricas e UI.
    ///
    /// SPEC-AV-005
    pub fn label(self) -> &'static str {
        match self {
            Self::NotDetected => "NotDetected",
            Self::Off => "Off",
            Self::NoAvfilter => "NoAvfilter",
            Self::Active => "Active",
            Self::Forced => "Forced",
        }
    }
}

/// `AVFieldOrder::AV_FIELD_PROGRESSIVE` — conteúdo progressivo.
#[allow(dead_code)]
const AV_FIELD_PROGRESSIVE: i32 = 1;
/// Primeiro valor de field order que indica entrelaçamento (TT, BB, TB, BT).
const AV_FIELD_INTERLACED_MIN: i32 = 2;

/// Atualiza `scan_type` com sinais disponíveis (modo `Auto` apenas).
///
/// Uma vez fixado como `Interlaced` ou `Progressive`, não regride para `Unknown`.
///
/// SPEC-AV-005
pub fn update_scan_type(
    current: ScanType,
    mode: DeinterlaceMode,
    is_h264: bool,
    pkt_bytes: &[u8],
    frame_interlaced: bool,
    field_order: i32,
) -> ScanType {
    if current.is_resolved() {
        return current;
    }

    match mode {
        DeinterlaceMode::Force => return ScanType::Interlaced,
        DeinterlaceMode::Off => return current,
        DeinterlaceMode::Auto => {}
    }

    if frame_interlaced {
        return ScanType::Interlaced;
    }

    if field_order >= AV_FIELD_INTERLACED_MIN {
        return ScanType::Interlaced;
    }

    // SPS tem prioridade sobre `field_order == PROGRESSIVE`: em hwaccel D3D11VA o
    // FFmpeg frequentemente reporta progressive antes do SPS ser visto no PES.
    if is_h264 {
        if let Some(detected) = detect_h264_scan_type(pkt_bytes) {
            if detected.is_resolved() {
                return detected;
            }
        }
    }

    // Não fixar Progressive só por field_order — evita falso positivo em 1080i
    // MBAFF (Globo/SKY) onde field_order pode ser PROGRESSIVE prematuramente.

    current
}

/// Detecta scan type a partir de NAL units H.264 no payload (PES ou AU).
///
/// Procura o primeiro SPS (NAL type 7) e lê `frame_mbs_only_flag`.
/// `frame_mbs_only_flag == 0` indica sequência com suporte a campos (entrelaçado).
///
/// SPEC-AV-005
pub fn detect_h264_scan_type(data: &[u8]) -> Option<ScanType> {
    for nal in iter_h264_nal_units(data) {
        if nal.is_empty() {
            continue;
        }
        let nal_type = nal[0] & 0x1F;
        if nal_type != 7 {
            continue;
        }
        let rbsp = remove_emulation_prevention_bytes(&nal[1..]);
        return parse_sps_scan_type(&rbsp);
    }
    None
}

/// Itera NAL units H.264 delimitadas por start codes `0x000001` ou `0x00000001`.
fn iter_h264_nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> + '_ {
    H264NalIter { data, pos: 0 }
}

struct H264NalIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for H264NalIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        while self.pos + 3 < self.data.len() {
            let start = find_start_code(self.data, self.pos)?;
            let payload_start = start + start_code_len(&self.data[start..]);
            self.pos = payload_start;
            if self.pos >= self.data.len() {
                return None;
            }
            let end = find_next_start_code(self.data, self.pos).unwrap_or(self.data.len());
            let nal = &self.data[self.pos..end];
            self.pos = end;
            if !nal.is_empty() {
                return Some(nal);
            }
        }
        None
    }
}

fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                return Some(i);
            }
            if i + 3 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn start_code_len(data: &[u8]) -> usize {
    if data.len() >= 4 && data[0] == 0 && data[1] == 0 && data[2] == 0 && data[3] == 1 {
        4
    } else {
        3
    }
}

fn find_next_start_code(data: &[u8], from: usize) -> Option<usize> {
    find_start_code(data, from)
}

fn remove_emulation_prevention_bytes(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len());
    let mut i = 0;
    while i < rbsp.len() {
        if i + 2 < rbsp.len() && rbsp[i] == 0 && rbsp[i + 1] == 0 && rbsp[i + 2] == 3 {
            out.push(0);
            out.push(0);
            i += 3;
        } else {
            out.push(rbsp[i]);
            i += 1;
        }
    }
    out
}

fn parse_sps_scan_type(rbsp: &[u8]) -> Option<ScanType> {
    let mut br = BitReader::new(rbsp);
    // profile_idc (8), constraint flags (8), level_idc (8)
    br.read_bits(8)?;
    br.read_bits(8)?;
    br.read_bits(8)?;
    let profile_idc = rbsp.first().copied().unwrap_or(0);
    let _seq_parameter_set_id = br.read_ue()?;

    // High profile extensions
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma_format_idc = br.read_ue()?;
        if chroma_format_idc == 3 {
            br.read_bit()?; // separate_colour_plane_flag
        }
        br.read_ue()?; // bit_depth_luma_minus8
        br.read_ue()?; // bit_depth_chroma_minus8
        br.read_bit()?; // qpprime_y_zero_transform_bypass_flag
        if br.read_bit()? == 1 {
            // seq_scaling_matrix_present_flag
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                if br.read_bit()? == 1 {
                    skip_scaling_list(&mut br, i < 6)?;
                }
            }
        }
    }

    br.read_ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = br.read_ue()?;
    match pic_order_cnt_type {
        0 => {
            br.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            br.read_bit()?; // delta_pic_order_always_zero_flag
            br.read_se()?; // offset_for_non_ref_pic
            br.read_se()?; // offset_for_top_to_bottom_field
            let cycles = br.read_ue()?;
            for _ in 0..cycles {
                br.read_se()?;
            }
        }
        2 => {}
        _ => return None,
    }

    br.read_ue()?; // max_num_ref_frames
    br.read_bit()?; // gaps_in_frame_num_allowed_flag
    br.read_ue()?; // pic_width_in_mbs_minus1
    br.read_ue()?; // pic_height_in_map_units_minus1
    let frame_mbs_only_flag = br.read_bit()?;
    if frame_mbs_only_flag == 0 {
        let _mb_adaptive = br.read_bit()?;
        return Some(ScanType::Interlaced);
    }
    Some(ScanType::Progressive)
}

fn skip_scaling_list(br: &mut BitReader<'_>, is_16x16: bool) -> Option<()> {
    let size = if is_16x16 { 16 } else { 64 };
    let mut last = 8i32;
    for _ in 0..size {
        if last != 0 {
            let delta = br.read_se()?;
            last = (last + delta + 256) % 256;
        }
    }
    Some(())
}

struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn read_bit(&mut self) -> Option<u8> {
        let byte_idx = self.bit_pos / 8;
        if byte_idx >= self.data.len() {
            return None;
        }
        let shift = 7 - (self.bit_pos % 8);
        self.bit_pos += 1;
        Some((self.data[byte_idx] >> shift) & 1)
    }

    fn read_bits(&mut self, count: u8) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..count {
            value = (value << 1) | u32::from(self.read_bit()?);
        }
        Some(value)
    }

    fn read_ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.read_bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_bits(zeros as u8)?;
        Some((1u32 << zeros) - 1 + suffix)
    }

    fn read_se(&mut self) -> Option<i32> {
        let ue = self.read_ue()?;
        let signed = if ue % 2 == 0 {
            -(ue as i32 / 2)
        } else {
            (ue as i32 + 1) / 2
        };
        Some(signed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPS real de stream H.264 1080i50 (`frame_mbs_only_flag=0`).
    fn interlaced_1080i_sps_au() -> Vec<u8> {
        vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x28, 0xac, 0x2c, 0xa4, 0x0f, 0x80, 0x22,
            0x7e, 0x5b, 0x02, 0x02, 0x02, 0x40, 0x00, 0x00, 0x03, 0x00, 0x40, 0x00, 0x00, 0x0f,
            0x03, 0xc6, 0x0c, 0x65, 0x80,
        ]
    }

    /// SPEC-AV-005: SPS Globo 1080i29.97 (MBAFF broadcast) detecta entrelaçado.
    #[test]
    fn spec_av_005_globo_copa_sps_detects_interlaced() {
        let au: Vec<u8> = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x28, 0xAC, 0x13, 0x31, 0x40, 0x78, 0x04,
            0x47, 0xDE, 0x03, 0xEA, 0x02, 0x02, 0x03, 0xE0, 0x00, 0x00, 0x7D, 0x20, 0x00, 0x1D,
            0x4C, 0x12, 0x80,
        ];
        assert_eq!(
            detect_h264_scan_type(&au),
            Some(ScanType::Interlaced),
            "SPS Globo Copa deve ser entrelaçado (MBAFF)"
        );
    }

    /// SPEC-AV-005: field_order PROGRESSIVE sozinho não fixa Progressive sem SPS.
    #[test]
    fn spec_av_005_field_order_progressive_without_sps_stays_unknown() {
        let updated = update_scan_type(
            ScanType::Unknown,
            DeinterlaceMode::Auto,
            true,
            &[],
            false,
            AV_FIELD_PROGRESSIVE,
        );
        assert_eq!(updated, ScanType::Unknown);
    }

    /// SPEC-AV-005: SPS entrelaçado prevalece sobre field_order PROGRESSIVE prematuro.
    #[test]
    fn spec_av_005_globo_sps_overrides_premature_field_order_progressive() {
        let au: Vec<u8> = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x28, 0xAC, 0x13, 0x31, 0x40, 0x78, 0x04,
            0x47, 0xDE, 0x03, 0xEA, 0x02, 0x02, 0x03, 0xE0, 0x00, 0x00, 0x7D, 0x20, 0x00, 0x1D,
            0x4C, 0x12, 0x80,
        ];
        let updated = update_scan_type(
            ScanType::Unknown,
            DeinterlaceMode::Auto,
            true,
            &au,
            false,
            AV_FIELD_PROGRESSIVE,
        );
        assert_eq!(updated, ScanType::Interlaced);
    }

    /// SPEC-AV-005: SPS com `frame_mbs_only_flag=0` detecta entrelaçado.
    #[test]
    fn spec_av_005_sps_detects_interlaced() {
        let au = interlaced_1080i_sps_au();
        let detected = detect_h264_scan_type(&au);
        assert_eq!(
            detected,
            Some(ScanType::Interlaced),
            "SPS interlaced deve ser detectado"
        );
    }

    /// SPEC-AV-005: payload sem SPS retorna None.
    #[test]
    fn spec_av_005_no_sps_returns_none() {
        let data = [0x00, 0x00, 0x01, 0x65, 0x88, 0x84]; // IDR sem SPS
        assert_eq!(detect_h264_scan_type(&data), None);
    }

    /// SPEC-AV-005: `update_scan_type` fixa Interlaced quando flag de frame está setada.
    #[test]
    fn spec_av_005_frame_flag_latches_interlaced() {
        let updated = update_scan_type(
            ScanType::Unknown,
            DeinterlaceMode::Auto,
            true,
            &[],
            true,
            0,
        );
        assert_eq!(updated, ScanType::Interlaced);
    }

    /// SPEC-AV-005: modo Force sempre retorna Interlaced.
    #[test]
    fn spec_av_005_force_mode_latches_interlaced() {
        let updated = update_scan_type(
            ScanType::Unknown,
            DeinterlaceMode::Force,
            false,
            &[],
            false,
            0,
        );
        assert_eq!(updated, ScanType::Interlaced);
    }

    /// SPEC-AV-005: scan type resolvido não regride.
    #[test]
    fn spec_av_005_resolved_scan_type_is_stable() {
        let updated = update_scan_type(
            ScanType::Interlaced,
            DeinterlaceMode::Auto,
            true,
            &[],
            false,
            AV_FIELD_PROGRESSIVE,
        );
        assert_eq!(updated, ScanType::Interlaced);
    }
}
