//! Parser da BAT (Bouquet Association Table).
//!
//! SPEC-TABLE-007

use super::{Descriptor, KnownDescriptor, TableError};

// ── BatTransportStream ────────────────────────────────────────────────────────

/// Transport stream descrito na BAT.
///
/// SPEC-TABLE-007
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatTransportStream {
    /// Identificador do transport stream.
    pub transport_stream_id: u16,
    /// Identificador da rede original.
    pub original_network_id: u16,
    /// Descriptors associados a este transport stream.
    pub descriptors: Vec<Descriptor>,
}

// ── Bat ───────────────────────────────────────────────────────────────────────

/// Bouquet Association Table (BAT).
///
/// Associa um bouquet (agrupamento de serviços) a um ou mais transport streams,
/// permitindo a navegação entre múltiplos multiplexes de um mesmo pacote comercial.
///
/// `table_id == 0x4A`
///
/// SPEC-TABLE-007
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bat {
    /// Identificador do bouquet.
    pub bouquet_id: u16,
    /// Versão da tabela (0–31).
    pub version: u8,
    /// Nome do bouquet extraído do `BouquetName` descriptor (tag 0x47), se presente.
    pub bouquet_name: Option<String>,
    /// Todos os descriptors de nível de bouquet.
    pub bouquet_descriptors: Vec<Descriptor>,
    /// Lista de transport streams associados ao bouquet.
    pub transport_streams: Vec<BatTransportStream>,
}

impl Bat {
    /// Parseia uma seção BAT completa (cabeçalho PSI + corpo + CRC-32).
    ///
    /// Aceita apenas `table_id` 0x4A.
    ///
    /// SPEC-TABLE-007
    pub fn parse(section: &[u8]) -> Result<Self, TableError> {
        // Mínimo: 3 (header PSI) + 9 (body mínimo) + 4 (CRC) = 16
        const MIN_LEN: usize = 16;
        if section.len() < MIN_LEN {
            return Err(TableError::InsufficientData {
                expected: MIN_LEN,
                found:    section.len(),
            });
        }

        let table_id = section[0];
        if table_id != 0x4A {
            return Err(TableError::WrongTableId {
                expected: 0x4A,
                found:    table_id,
            });
        }

        // section_body = sem os 3 bytes de cabeçalho e sem os 4 bytes de CRC
        let body = &section[3..section.len() - 4];

        // Mínimo do body: bouquet_id(2) + version(1) + sec_num(1) + last_sec(1)
        //               + bouquet_desc_len(2) + ts_loop_len(2) = 9
        const MIN_BODY: usize = 9;
        if body.len() < MIN_BODY {
            return Err(TableError::InsufficientData {
                expected: MIN_BODY,
                found:    body.len(),
            });
        }

        let bouquet_id = u16::from_be_bytes([body[0], body[1]]);
        let version    = (body[2] >> 1) & 0x1F;
        // body[3] = section_number, body[4] = last_section_number (ignorados)

        let bouquet_desc_len = (u16::from_be_bytes([body[5], body[6]]) & 0x0FFF) as usize;
        let bouquet_desc_end = 7 + bouquet_desc_len;

        if body.len() < bouquet_desc_end + 2 {
            return Err(TableError::InsufficientData {
                expected: bouquet_desc_end + 2,
                found:    body.len(),
            });
        }

        let bouquet_descriptors = Descriptor::parse_list(&body[7..bouquet_desc_end]);

        // Extrair bouquet_name do primeiro BouquetName descriptor
        let bouquet_name = bouquet_descriptors.iter().find_map(|d| {
            if let KnownDescriptor::BouquetName { name } = d.decode() {
                Some(name)
            } else {
                None
            }
        });

        // ── Transport stream loop ─────────────────────────────────────────────
        let ts_loop_len = (u16::from_be_bytes([body[bouquet_desc_end], body[bouquet_desc_end + 1]])
            & 0x0FFF) as usize;
        let ts_loop_start = bouquet_desc_end + 2;
        let ts_loop_end   = ts_loop_start + ts_loop_len;

        if body.len() < ts_loop_end {
            return Err(TableError::InsufficientData {
                expected: ts_loop_end,
                found:    body.len(),
            });
        }

        let ts_data = &body[ts_loop_start..ts_loop_end];
        let mut transport_streams = Vec::new();
        let mut pos = 0usize;

        while pos < ts_data.len() {
            // Cada entry requer: ts_id(2) + orig_net_id(2) + desc_len(2) = 6 bytes mínimos
            const TS_HEADER: usize = 6;
            if pos + TS_HEADER > ts_data.len() {
                return Err(TableError::InsufficientData {
                    expected: pos + TS_HEADER,
                    found:    ts_data.len(),
                });
            }

            let transport_stream_id =
                u16::from_be_bytes([ts_data[pos], ts_data[pos + 1]]);
            let original_network_id =
                u16::from_be_bytes([ts_data[pos + 2], ts_data[pos + 3]]);
            let desc_len =
                (u16::from_be_bytes([ts_data[pos + 4], ts_data[pos + 5]]) & 0x0FFF) as usize;
            pos += TS_HEADER;

            if pos + desc_len > ts_data.len() {
                return Err(TableError::InsufficientData {
                    expected: pos + desc_len,
                    found:    ts_data.len(),
                });
            }

            let descriptors = Descriptor::parse_list(&ts_data[pos..pos + desc_len]);
            pos += desc_len;

            transport_streams.push(BatTransportStream {
                transport_stream_id,
                original_network_id,
                descriptors,
            });
        }

        Ok(Bat {
            bouquet_id,
            version,
            bouquet_name,
            bouquet_descriptors,
            transport_streams,
        })
    }
}

// ── Testes ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Constrói uma seção BAT minimal em memória.
    ///
    /// - bouquet_id=42, version=1
    /// - BouquetName descriptor "IronTV" (tag 0x47)
    /// - 1 transport stream: ts_id=1, orig_net_id=100, sem descriptors
    fn build_bat_section() -> Vec<u8> {
        // ── Descriptor BouquetName "IronTV" (tag 0x47, len 6) ────────────────
        let name = b"IronTV";
        let mut bname_desc = Vec::new();
        bname_desc.push(0x47u8);
        bname_desc.push(name.len() as u8);
        bname_desc.extend_from_slice(name);
        // bname_desc = 8 bytes

        // ── TS loop entry: ts_id=1, orig_net_id=100, no descriptors ───────────
        let mut ts_entry = Vec::new();
        ts_entry.extend_from_slice(&[0x00u8, 0x01]); // ts_id = 1
        ts_entry.extend_from_slice(&[0x00u8, 0x64]); // orig_net_id = 100
        // desc_loop_len = 0: reserved(4b)=1111, len=0 → 0xF000 & 0x0000
        ts_entry.push(0xF0u8);
        ts_entry.push(0x00u8);
        // ts_entry = 6 bytes

        let bouquet_desc_len = bname_desc.len() as u16; // 8
        let ts_loop_len      = ts_entry.len() as u16;   // 6

        // section_length = 5 (PSI common) + 2 (bouquet_desc_len) + bouquet_desc_len
        //                + 2 (ts_loop_len) + ts_loop_len + 4 (CRC)
        let section_length: u16 = 5 + 2 + bouquet_desc_len + 2 + ts_loop_len + 4;

        let mut sec = Vec::new();
        sec.push(0x4A); // table_id = BAT
        sec.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
        sec.push((section_length & 0xFF) as u8);

        // PSI common header (5 bytes): bouquet_id + version/flags + sec_num + last_sec_num
        sec.push(0x00); sec.push(0x2A); // bouquet_id = 42
        sec.push(0xC3); // reserved(2b)|version=1|current_next=1
        sec.push(0x00); // section_number = 0
        sec.push(0x00); // last_section_number = 0

        // bouquet_desc_length (2 bytes, 12-bit)
        sec.push(0xF0u8 | ((bouquet_desc_len >> 8) as u8 & 0x0F));
        sec.push((bouquet_desc_len & 0xFF) as u8);

        sec.extend_from_slice(&bname_desc);

        // ts_loop_length (2 bytes, 12-bit)
        sec.push(0xF0u8 | ((ts_loop_len >> 8) as u8 & 0x0F));
        sec.push((ts_loop_len & 0xFF) as u8);

        sec.extend_from_slice(&ts_entry);

        let crc = crate::crc32_mpeg2(&sec);
        sec.extend_from_slice(&crc.to_be_bytes());
        sec
    }

    /// SPEC-TABLE-007: parse básico de seção BAT.
    #[test]
    fn spec_table_007_bat() {
        let sec = build_bat_section();
        let bat = Bat::parse(&sec).expect("deve parsear BAT corretamente");

        assert_eq!(bat.bouquet_id, 42);
        assert_eq!(bat.version, 1);
        assert_eq!(bat.bouquet_name.as_deref(), Some("IronTV"));
        assert_eq!(bat.transport_streams.len(), 1);

        let ts = &bat.transport_streams[0];
        assert_eq!(ts.transport_stream_id, 1);
        assert_eq!(ts.original_network_id, 100);
        assert!(ts.descriptors.is_empty());
    }

    /// SPEC-TABLE-007: BAT sem descriptors de bouquet.
    #[test]
    fn spec_table_007_bat_no_bouquet_name() {
        // Construir BAT mínima sem nenhum descriptor
        // section_length = 5 (common) + 2 (bouquet_desc_len=0) + 2 (ts_loop_len=0) + 4 (CRC) = 13
        let section_length: u16 = 13;

        let mut sec = Vec::new();
        sec.push(0x4A);
        sec.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
        sec.push((section_length & 0xFF) as u8);

        sec.push(0x00); sec.push(0x01); // bouquet_id = 1
        sec.push(0xC1); // version=0, current_next=1
        sec.push(0x00);
        sec.push(0x00);

        sec.push(0xF0); sec.push(0x00); // bouquet_desc_len = 0
        sec.push(0xF0); sec.push(0x00); // ts_loop_len = 0

        let crc = crate::crc32_mpeg2(&sec);
        sec.extend_from_slice(&crc.to_be_bytes());

        let bat = Bat::parse(&sec).expect("deve parsear BAT sem descriptors");
        assert_eq!(bat.bouquet_id, 1);
        assert!(bat.bouquet_name.is_none());
        assert!(bat.transport_streams.is_empty());
    }

    /// SPEC-TABLE-007: dados insuficientes retorna erro.
    #[test]
    fn spec_table_insufficient_data_bat() {
        assert!(matches!(
            Bat::parse(&[]),
            Err(TableError::InsufficientData { .. })
        ));
        assert!(matches!(
            Bat::parse(&[0x4Au8; 10]),
            Err(TableError::InsufficientData { .. })
        ));
    }

    /// SPEC-TABLE-007: table_id inválido retorna WrongTableId.
    #[test]
    fn spec_table_wrong_table_id_bat() {
        let mut sec = build_bat_section();
        sec[0] = 0x00;
        assert!(matches!(
            Bat::parse(&sec),
            Err(TableError::WrongTableId { expected: 0x4A, .. })
        ));
    }

    /// BAT fixture gerada pelo gen_fixtures (carrega se existir).
    #[test]
    fn spec_table_007_bat_from_fixture() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("bat.bin");

        if !path.exists() {
            // Fixture ainda não gerada; skipa sem falhar
            return;
        }

        let data = std::fs::read(&path).expect("ler bat.bin");
        let bat = Bat::parse(&data).expect("deve parsear BAT da fixture");
        assert_eq!(bat.bouquet_id, 42);
        assert!(!bat.transport_streams.is_empty(), "deve ter ao menos 1 TS");
    }
}
