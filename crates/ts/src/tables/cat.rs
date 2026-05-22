//! Parser da CAT (Conditional Access Table).
//!
//! SPEC-TS-CAT-001

use super::{Descriptor, TableError};

// ── CatCaDescriptor ──────────────────────────────────────────────────────────

/// Descriptor CA (tag 0x09) extraído da CAT.
///
/// Descreve um sistema de acesso condicional presente no multiplex.
///
/// SPEC-TS-CAT-001
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatCaDescriptor {
    /// Identificador do sistema CA (CAS ID).
    pub ca_system_id: u16,
    /// PID do fluxo CATV (EMM PID) associado a este CA system.
    pub ca_pid: u16,
    /// Bytes de dados privados do descriptor (pode estar vazio).
    pub private_data: Vec<u8>,
}

impl CatCaDescriptor {
    /// Tenta decodificar um `Descriptor` como CA descriptor (tag 0x09).
    ///
    /// Retorna `None` se a tag não for 0x09 ou os dados forem insuficientes.
    ///
    /// SPEC-TS-CAT-001
    pub fn from_descriptor(d: &Descriptor) -> Option<Self> {
        if d.tag != 0x09 {
            return None;
        }
        if d.data.len() < 4 {
            return None;
        }
        let ca_system_id = u16::from_be_bytes([d.data[0], d.data[1]]);
        let ca_pid = u16::from_be_bytes([d.data[2], d.data[3]]) & 0x1FFF;
        let private_data = d.data[4..].to_vec();
        Some(Self {
            ca_system_id,
            ca_pid,
            private_data,
        })
    }
}

// ── Cat ───────────────────────────────────────────────────────────────────────

/// Conditional Access Table (CAT).
///
/// Transportada no PID 0x0001, `table_id` 0x01. Contém descriptors CA
/// (tag 0x09) que descrevem os sistemas de acesso condicional presentes
/// no multiplex e os PIDs dos fluxos EMM associados.
///
/// SPEC-TS-CAT-001
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cat {
    /// Versão da tabela (0–31).
    pub version: u8,
    /// `true` quando esta seção está atualmente em vigor.
    pub current_next: bool,
    /// Lista de descriptors CA (tag 0x09) da CAT.
    pub ca_descriptors: Vec<CatCaDescriptor>,
    /// Todos os descriptors brutos da CAT (incluindo desconhecidos).
    pub descriptors: Vec<Descriptor>,
}

impl Cat {
    /// Parseia uma seção CAT completa (cabeçalho PSI + corpo + CRC-32).
    ///
    /// Layout esperado (a partir do byte 0 da seção):
    /// ```text
    /// [table_id=0x01  1B]
    /// [section_syntax_indicator=1 | '0' | reserved(2) | section_length[11:8]  1B]
    /// [section_length[7:0]  1B]
    /// [reserved(18b)  = 0xFFFF  2B]   ← table_id_extension (ignorado)
    /// [reserved(2b) | version(5b) | current_next(1b)  1B]
    /// [section_number  1B]
    /// [last_section_number  1B]
    /// [descriptors…]
    /// [CRC-32  4B]
    /// ```
    ///
    /// SPEC-TS-CAT-001
    pub fn parse(section: &[u8]) -> Result<Self, TableError> {
        // Mínimo: 3 (header PSI) + 2 (reserved) + 3 (version+sec_num×2) + 4 (CRC) = 12
        const MIN_LEN: usize = 12;
        if section.len() < MIN_LEN {
            return Err(TableError::InsufficientData {
                expected: MIN_LEN,
                found: section.len(),
            });
        }

        if section[0] != 0x01 {
            return Err(TableError::WrongTableId {
                expected: 0x01,
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

        // Bytes 3–4: reserved (table_id_extension, ignorado)
        // Byte 5: reserved(2) | version(5) | current_next(1)
        let version = (section[5] >> 1) & 0x1F;
        let current_next = section[5] & 0x01 != 0;
        // section[6] = section_number (ignorado)
        // section[7] = last_section_number (ignorado)

        // Descriptors: do byte 8 até total_len - 4 (excluindo CRC-32)
        let desc_start = 8;
        let desc_end = total_len.saturating_sub(4);
        if desc_end < desc_start {
            return Err(TableError::InsufficientData {
                expected: desc_start,
                found: total_len,
            });
        }
        let desc_bytes = &section[desc_start..desc_end];
        let descriptors = Descriptor::parse_list(desc_bytes);
        let ca_descriptors = descriptors
            .iter()
            .filter_map(CatCaDescriptor::from_descriptor)
            .collect();

        Ok(Self {
            version,
            current_next,
            ca_descriptors,
            descriptors,
        })
    }
}

// ── Testes ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crc::crc32_mpeg2;

    /// Constrói uma seção CAT mínima com os descriptors fornecidos.
    fn build_cat_section(version: u8, descriptors: &[u8]) -> Vec<u8> {
        let desc_len = descriptors.len();
        // section_length = 2 (reserved) + 3 (version+sec_nums) + desc_len + 4 (CRC)
        let section_length = 2 + 3 + desc_len + 4;
        let mut sec = vec![
            0x01u8,                                    // table_id
            0x80 | ((section_length >> 8) as u8),      // section_syntax_indicator=1 + hi
            (section_length & 0xFF) as u8,             // lo
            0xFF, 0xFF,                                // reserved (table_id_extension)
            0xC0 | ((version & 0x1F) << 1) | 0x01,    // reserved + version + current_next
            0x00,                                      // section_number
            0x00,                                      // last_section_number
        ];
        sec.extend_from_slice(descriptors);
        // CRC-32 placeholder
        let crc_pos = sec.len();
        sec.extend_from_slice(&[0, 0, 0, 0]);
        let crc = crc32_mpeg2(&sec[..crc_pos]);
        sec[crc_pos..].copy_from_slice(&crc.to_be_bytes());
        sec
    }

    /// Constrói um CA descriptor (tag 0x09) bruto.
    fn build_ca_descriptor(ca_system_id: u16, ca_pid: u16, private: &[u8]) -> Vec<u8> {
        let data_len = 4 + private.len();
        let mut d = vec![
            0x09,               // tag
            data_len as u8,     // length
            (ca_system_id >> 8) as u8,
            ca_system_id as u8,
            0xE0 | ((ca_pid >> 8) as u8 & 0x1F),
            ca_pid as u8,
        ];
        d.extend_from_slice(private);
        d
    }

    /// SPEC-TS-CAT-001: parse de CAT vazia (sem descriptors).
    #[test]
    fn spec_ts_cat_001_empty_cat() {
        let section = build_cat_section(3, &[]);
        let cat = Cat::parse(&section).expect("parse falhou");
        assert_eq!(cat.version, 3);
        assert!(cat.current_next);
        assert!(cat.ca_descriptors.is_empty());
        assert!(cat.descriptors.is_empty());
    }

    /// SPEC-TS-CAT-001: parse de CAT com um CA descriptor.
    #[test]
    fn spec_ts_cat_001_single_ca_descriptor() {
        let ca_desc = build_ca_descriptor(0x0100, 0x0200, &[]);
        let section = build_cat_section(1, &ca_desc);
        let cat = Cat::parse(&section).expect("parse falhou");
        assert_eq!(cat.version, 1);
        assert_eq!(cat.ca_descriptors.len(), 1);
        assert_eq!(cat.ca_descriptors[0].ca_system_id, 0x0100);
        assert_eq!(cat.ca_descriptors[0].ca_pid, 0x0200);
        assert!(cat.ca_descriptors[0].private_data.is_empty());
    }

    /// SPEC-TS-CAT-001: parse de CAT com dois CA descriptors e dados privados.
    #[test]
    fn spec_ts_cat_001_two_ca_descriptors_with_private() {
        let mut descs = build_ca_descriptor(0x0500, 0x0100, &[0xAB, 0xCD]);
        descs.extend(build_ca_descriptor(0x0900, 0x0200, &[]));
        let section = build_cat_section(0, &descs);
        let cat = Cat::parse(&section).expect("parse falhou");
        assert_eq!(cat.ca_descriptors.len(), 2);
        assert_eq!(cat.ca_descriptors[0].ca_system_id, 0x0500);
        assert_eq!(cat.ca_descriptors[0].private_data, vec![0xAB, 0xCD]);
        assert_eq!(cat.ca_descriptors[1].ca_system_id, 0x0900);
        assert_eq!(cat.ca_descriptors[1].ca_pid, 0x0200);
    }

    /// SPEC-TS-CAT-001: table_id errado retorna WrongTableId.
    #[test]
    fn spec_ts_cat_001_wrong_table_id() {
        let mut section = build_cat_section(0, &[]);
        section[0] = 0x00; // PAT table_id
        assert_eq!(
            Cat::parse(&section),
            Err(TableError::WrongTableId {
                expected: 0x01,
                found: 0x00,
            })
        );
    }

    /// SPEC-TS-CAT-001: dados insuficientes retornam InsufficientData.
    #[test]
    fn spec_ts_cat_001_insufficient_data() {
        let section = &[0x01u8, 0x80, 0x09]; // apenas 3 bytes
        assert!(matches!(
            Cat::parse(section),
            Err(TableError::InsufficientData { .. })
        ));
    }
}
