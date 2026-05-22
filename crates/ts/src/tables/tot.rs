//! Parser da TOT (Time Offset Table).
//!
//! SPEC-TABLE-TOT-001

use chrono::NaiveDateTime;

use super::{tdt::decode_mjd_bcd, Descriptor, TableError};

// ── LocalTimeOffset ───────────────────────────────────────────────────────────

/// Entrada de offset de fuso horário extraída do `local_time_offset` descriptor
/// (tag 0x58, EN 300 468 §6.2.19).
///
/// SPEC-TABLE-TOT-001
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTimeOffset {
    /// Código do país (ISO 3166-1 alpha-3, 3 bytes ASCII).
    pub country_code: [u8; 3],
    /// Número da região dentro do país (6 bits).
    pub country_region_id: u8,
    /// `true` se o offset local está adiantado em relação ao UTC.
    pub local_offset_polarity: bool,
    /// Offset em relação ao UTC (horas e minutos BCD).
    pub local_time_offset_hhmm: (u8, u8),
    /// Instante UTC a partir do qual o próximo offset entra em vigor.
    pub time_of_change: Option<NaiveDateTime>,
    /// Próximo offset (horas e minutos BCD).
    pub next_time_offset_hhmm: (u8, u8),
}

impl LocalTimeOffset {
    /// Tenta decodificar um `Descriptor` como `local_time_offset` (tag 0x58).
    ///
    /// Cada descriptor pode conter múltiplas entradas de 13 bytes cada.
    ///
    /// SPEC-TABLE-TOT-001
    pub fn from_descriptor(d: &Descriptor) -> Vec<Self> {
        if d.tag != 0x58 {
            return Vec::new();
        }
        let data = &d.data;
        let mut out = Vec::new();
        let mut pos = 0;
        while pos + 13 <= data.len() {
            let country_code = [data[pos], data[pos + 1], data[pos + 2]];
            let country_region_id = (data[pos + 3] >> 2) & 0x3F;
            let local_offset_polarity = data[pos + 3] & 0x01 != 0;
            let lto_hh = data[pos + 4];
            let lto_mm = data[pos + 5];
            // Bytes 6-10: time_of_change (MJD 2B + BCD HH MM SS)
            let mjd = u16::from_be_bytes([data[pos + 6], data[pos + 7]]);
            let time_of_change =
                decode_mjd_bcd(mjd, data[pos + 8], data[pos + 9], data[pos + 10]);
            let next_hh = data[pos + 11];
            let next_mm = data[pos + 12];
            out.push(LocalTimeOffset {
                country_code,
                country_region_id,
                local_offset_polarity,
                local_time_offset_hhmm: (lto_hh, lto_mm),
                time_of_change,
                next_time_offset_hhmm: (next_hh, next_mm),
            });
            pos += 13;
        }
        out
    }
}

// ── Tot ───────────────────────────────────────────────────────────────────────

/// Time Offset Table (TOT).
///
/// Transportada no PID 0x0014, `table_id` 0x73. Superset da TDT: contém a
/// hora UTC atual **e** descriptors de offset de fuso horário local
/// (`local_time_offset`, tag 0x58).
///
/// Ao contrário da TDT, a TOT é uma seção longa (`section_syntax_indicator=0`)
/// com CRC-32.
///
/// SPEC-TABLE-TOT-001
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tot {
    /// Hora UTC atual conforme sinalizada no stream.
    pub utc_time: NaiveDateTime,
    /// Offsets de fuso horário local extraídos dos descriptors (tag 0x58).
    pub local_time_offsets: Vec<LocalTimeOffset>,
    /// Todos os descriptors brutos da TOT.
    pub descriptors: Vec<Descriptor>,
}

impl Tot {
    /// Parseia uma seção TOT completa.
    ///
    /// Layout esperado:
    /// ```text
    /// [table_id=0x73  1B]
    /// [section_syntax_indicator=0 | '1' | reserved(2) | section_length[11:8]  1B]
    /// [section_length[7:0]  1B]
    /// [MJD[15:8]  1B]
    /// [MJD[7:0]   1B]
    /// [BCD HH  1B]
    /// [BCD MM  1B]
    /// [BCD SS  1B]
    /// [reserved(4) | descriptors_loop_length[11:8]  1B]
    /// [descriptors_loop_length[7:0]  1B]
    /// [descriptors…]
    /// [CRC-32  4B]
    /// ```
    ///
    /// SPEC-TABLE-TOT-001
    pub fn parse(section: &[u8]) -> Result<Self, TableError> {
        // Mínimo: 3 (PSI header) + 5 (MJD+BCD) + 2 (desc_loop_len) + 4 (CRC) = 14
        const MIN_LEN: usize = 14;
        if section.len() < MIN_LEN {
            return Err(TableError::InsufficientData {
                expected: MIN_LEN,
                found: section.len(),
            });
        }

        if section[0] != 0x73 {
            return Err(TableError::WrongTableId {
                expected: 0x73,
                found: section[0],
            });
        }

        let section_length = (u16::from_be_bytes([section[1], section[2]]) & 0x0FFF) as usize;
        let total_len = 3 + section_length;
        if section.len() < total_len {
            return Err(TableError::InvalidSectionLength {
                declared: section_length,
                available: section.len().saturating_sub(3),
            });
        }

        // Bytes 3-7: MJD + BCD time
        let mjd = u16::from_be_bytes([section[3], section[4]]);
        let utc_time = decode_mjd_bcd(mjd, section[5], section[6], section[7]).ok_or(
            TableError::InsufficientData {
                expected: MIN_LEN,
                found: 0,
            },
        )?;

        // Bytes 8-9: reserved(4) + descriptors_loop_length(12)
        let desc_loop_len =
            (u16::from_be_bytes([section[8], section[9]]) & 0x0FFF) as usize;
        let desc_start = 10;
        let desc_end = desc_start + desc_loop_len;
        // CRC-32 occupies the last 4 bytes of total_len
        let crc_start = total_len.saturating_sub(4);
        if desc_end > crc_start {
            return Err(TableError::InvalidSectionLength {
                declared: desc_loop_len,
                available: crc_start.saturating_sub(desc_start),
            });
        }

        let descriptors = Descriptor::parse_list(&section[desc_start..desc_end]);
        let local_time_offsets = descriptors
            .iter()
            .flat_map(LocalTimeOffset::from_descriptor)
            .collect();

        Ok(Self {
            utc_time,
            local_time_offsets,
            descriptors,
        })
    }
}

// ── Testes ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use chrono::Timelike as _;

    use super::*;
    use crate::crc::crc32_mpeg2;

    /// Constrói uma seção TOT com os descriptors fornecidos.
    fn build_tot_section(mjd: u16, hh: u8, mm: u8, ss: u8, descriptors: &[u8]) -> Vec<u8> {
        let desc_len = descriptors.len();
        // section_length = 5 (MJD+BCD) + 2 (desc_loop_len) + desc_len + 4 (CRC)
        let section_length = 5 + 2 + desc_len + 4;
        let mut sec = vec![
            0x73u8,                               // table_id
            0x70 | ((section_length >> 8) as u8), // section_syntax_indicator=0 + '1' + reserved + hi
            (section_length & 0xFF) as u8,        // lo
            (mjd >> 8) as u8,                     // MJD hi
            mjd as u8,                            // MJD lo
            hh,
            mm,
            ss,
            0xF0 | ((desc_len >> 8) as u8),       // reserved + desc_loop_len hi
            (desc_len & 0xFF) as u8,              // desc_loop_len lo
        ];
        sec.extend_from_slice(descriptors);
        // CRC-32
        let crc_pos = sec.len();
        sec.extend_from_slice(&[0, 0, 0, 0]);
        let crc = crc32_mpeg2(&sec[..crc_pos]);
        sec[crc_pos..].copy_from_slice(&crc.to_be_bytes());
        sec
    }

    /// MJD para 2024-01-15 (calculado de acordo com EN 300 468 Annex C).
    /// MJD = 14956 + (year-1900)*365.25 + month*30.6001 + day (approx)
    /// Para 2024-01-15: MJD ≈ 60324 (valor calculado)
    const MJD_2024_01_15: u16 = 60324;

    /// SPEC-TABLE-TOT-001: parse de TOT sem descriptors.
    #[test]
    fn spec_table_tot_001_empty_tot() {
        let section = build_tot_section(MJD_2024_01_15, 0x12, 0x30, 0x00, &[]);
        let tot = Tot::parse(&section).expect("parse falhou");
        assert_eq!(tot.utc_time.hour(), 12);
        assert_eq!(tot.utc_time.minute(), 30);
        assert_eq!(tot.utc_time.second(), 0);
        assert!(tot.descriptors.is_empty());
        assert!(tot.local_time_offsets.is_empty());
    }

    /// SPEC-TABLE-TOT-001: parse de TOT com local_time_offset descriptor.
    #[test]
    fn spec_table_tot_001_with_local_time_offset() {
        // Constrói um local_time_offset descriptor (tag 0x58) com uma entrada.
        let mut lto_data: Vec<u8> = vec![
            b'B', b'R', b'A',   // country_code = "BRA"
            0x01,               // country_region_id=0, lto_polarity=1 (behind UTC)
            0x03, 0x00,         // local_time_offset = BCD 03:00
            // time_of_change: MJD_2024_01_15 + 00:00:00
            (MJD_2024_01_15 >> 8) as u8, MJD_2024_01_15 as u8,
            0x00, 0x00, 0x00,   // 00:00:00
            0x03, 0x00,         // next_time_offset = BCD 03:00
        ];
        let mut desc = vec![0x58u8, lto_data.len() as u8];
        desc.append(&mut lto_data);

        let section = build_tot_section(MJD_2024_01_15, 0x15, 0x45, 0x30, &desc);
        let tot = Tot::parse(&section).expect("parse falhou");
        assert_eq!(tot.utc_time.hour(), 15);
        assert_eq!(tot.utc_time.minute(), 45);
        assert_eq!(tot.local_time_offsets.len(), 1);
        assert_eq!(&tot.local_time_offsets[0].country_code, b"BRA");
        assert!(tot.local_time_offsets[0].local_offset_polarity);
        assert_eq!(tot.local_time_offsets[0].local_time_offset_hhmm, (0x03, 0x00));
    }

    /// SPEC-TABLE-TOT-001: table_id errado retorna WrongTableId.
    #[test]
    fn spec_table_tot_001_wrong_table_id() {
        let mut section = build_tot_section(MJD_2024_01_15, 0x12, 0x00, 0x00, &[]);
        section[0] = 0x70; // TDT table_id
        assert_eq!(
            Tot::parse(&section),
            Err(TableError::WrongTableId {
                expected: 0x73,
                found: 0x70,
            })
        );
    }

    /// SPEC-TABLE-TOT-001: dados insuficientes retornam InsufficientData.
    #[test]
    fn spec_table_tot_001_insufficient_data() {
        let section = &[0x73u8, 0x70, 0x0B]; // apenas 3 bytes
        assert!(matches!(
            Tot::parse(section),
            Err(TableError::InsufficientData { .. })
        ));
    }
}
