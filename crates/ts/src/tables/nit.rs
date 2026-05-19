//! Parser da NIT (Network Information Table).
//!
//! SPEC-TABLE-003

use super::{Descriptor, KnownDescriptor, TableError};

// ── NitTransportStream ────────────────────────────────────────────────────────

/// Transport stream descrito na NIT.
///
/// SPEC-TABLE-003
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NitTransportStream {
    /// Identificador do transport stream.
    pub transport_stream_id: u16,
    /// Identificador da rede original.
    pub original_network_id: u16,
    /// Descriptors associados a este transport stream.
    pub descriptors: Vec<Descriptor>,
}

impl NitTransportStream {
    /// Retorna o delivery descriptor deste transport stream, se presente.
    ///
    /// Retorna o primeiro `KnownDescriptor` que representa entrega física
    /// (Cable, Satellite ou Terrestrial).
    ///
    /// SPEC-TABLE-003
    pub fn delivery(&self) -> Option<KnownDescriptor> {
        self.descriptors.iter().find_map(|d| {
            let kd = d.decode();
            match kd {
                KnownDescriptor::CableDelivery { .. }
                | KnownDescriptor::SatelliteDelivery { .. }
                | KnownDescriptor::TerrestrialDelivery { .. } => Some(kd),
                _ => None,
            }
        })
    }
}

// ── Nit ───────────────────────────────────────────────────────────────────────

/// Network Information Table (NIT).
///
/// Descreve a rede de distribuição e os transport streams presentes no
/// multiplex. `actual == true` quando `table_id == 0x40` (NIT actual);
/// `false` quando `table_id == 0x41` (NIT other).
///
/// SPEC-TABLE-003
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nit {
    /// Identificador da rede.
    pub network_id: u16,
    /// Versão da tabela (0–31).
    pub version: u8,
    /// `true` quando `table_id == 0x40` (NIT actual).
    pub actual: bool,
    /// Nome da rede extraído do `NetworkName` descriptor (tag 0x40), se presente.
    pub network_name: Option<String>,
    /// Todos os descriptors de nível de rede.
    pub network_descriptors: Vec<Descriptor>,
    /// Lista de transport streams descritos na NIT.
    pub transport_streams: Vec<NitTransportStream>,
}

impl Nit {
    /// Parseia uma seção NIT completa (cabeçalho PSI + corpo + CRC-32).
    ///
    /// Aceita `table_id` 0x40 (NIT actual) e 0x41 (NIT other).
    ///
    /// SPEC-TABLE-003
    pub fn parse(section: &[u8]) -> Result<Self, TableError> {
        // Mínimo: 3 (header PSI) + 7 (network_id + version + sec_num×2 + net_desc_len)
        //       + 2 (ts_loop_len) + 4 (CRC) = 16
        const MIN_LEN: usize = 16;
        if section.len() < MIN_LEN {
            return Err(TableError::InsufficientData {
                expected: MIN_LEN,
                found: section.len(),
            });
        }

        let table_id = section[0];
        let actual = match table_id {
            0x40 => true,
            0x41 => false,
            other => return Err(TableError::WrongTableIdMulti { found: other }),
        };

        // section_body = sem os 3 bytes de cabeçalho e sem os 4 bytes de CRC
        let body = &section[3..section.len() - 4];

        // Mínimo do body: network_id(2) + version(1) + sec_num(1) + last_sec(1)
        //                + net_desc_len(2) + ts_loop_len(2) = 9
        const MIN_BODY: usize = 9;
        if body.len() < MIN_BODY {
            return Err(TableError::InsufficientData {
                expected: MIN_BODY,
                found: body.len(),
            });
        }

        let network_id = u16::from_be_bytes([body[0], body[1]]);
        let version = (body[2] >> 1) & 0x1F;
        // body[3] = section_number, body[4] = last_section_number (ignorados)

        let net_desc_len = (u16::from_be_bytes([body[5], body[6]]) & 0x0FFF) as usize;
        let net_desc_end = 7 + net_desc_len;

        if body.len() < net_desc_end + 2 {
            return Err(TableError::InsufficientData {
                expected: net_desc_end + 2,
                found: body.len(),
            });
        }

        let network_descriptors = Descriptor::parse_list(&body[7..net_desc_end]);

        // Extrair network_name do primeiro descriptor NetworkName
        let network_name = network_descriptors.iter().find_map(|d| {
            if let KnownDescriptor::NetworkName { name } = d.decode() {
                Some(name)
            } else {
                None
            }
        });

        // ── Transport stream loop ─────────────────────────────────────────────
        let ts_loop_len =
            (u16::from_be_bytes([body[net_desc_end], body[net_desc_end + 1]]) & 0x0FFF) as usize;
        let ts_loop_start = net_desc_end + 2;
        let ts_loop_end = ts_loop_start + ts_loop_len;

        if body.len() < ts_loop_end {
            return Err(TableError::InsufficientData {
                expected: ts_loop_end,
                found: body.len(),
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
                    found: ts_data.len(),
                });
            }

            let transport_stream_id = u16::from_be_bytes([ts_data[pos], ts_data[pos + 1]]);
            let original_network_id = u16::from_be_bytes([ts_data[pos + 2], ts_data[pos + 3]]);
            let desc_len =
                (u16::from_be_bytes([ts_data[pos + 4], ts_data[pos + 5]]) & 0x0FFF) as usize;
            pos += TS_HEADER;

            if pos + desc_len > ts_data.len() {
                return Err(TableError::InsufficientData {
                    expected: pos + desc_len,
                    found: ts_data.len(),
                });
            }

            let descriptors = Descriptor::parse_list(&ts_data[pos..pos + desc_len]);
            pos += desc_len;

            transport_streams.push(NitTransportStream {
                transport_stream_id,
                original_network_id,
                descriptors,
            });
        }

        Ok(Nit {
            network_id,
            version,
            actual,
            network_name,
            network_descriptors,
            transport_streams,
        })
    }
}

// ── Testes ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{tables::KnownDescriptor, verify_crc32_mpeg2};

    /// Parseia a fixture `nit_cable.bin` e verifica os campos extraídos.
    ///
    /// SPEC-TABLE-003
    #[test]
    fn spec_table_003_parse_nit_fixture() {
        let data = include_bytes!("../../tests/fixtures/nit_cable.bin");

        assert!(
            verify_crc32_mpeg2(data),
            "CRC-32 da fixture NIT deve ser válido"
        );
        assert_eq!(data[0], 0x40, "table_id deve ser 0x40 (NIT actual)");

        let nit = Nit::parse(data).expect("NIT deve parsear sem erro");

        assert_eq!(nit.network_id, 100);
        assert_eq!(nit.version, 1);
        assert!(nit.actual);
        assert_eq!(nit.network_name.as_deref(), Some("IronCable"));
        assert_eq!(nit.transport_streams.len(), 1);

        let ts = &nit.transport_streams[0];
        assert_eq!(ts.transport_stream_id, 1);
        assert_eq!(ts.original_network_id, 100);

        let delivery = ts.delivery().expect("deve ter delivery descriptor");
        match delivery {
            KnownDescriptor::CableDelivery {
                frequency_hz,
                modulation,
                symbol_rate,
            } => {
                assert_eq!(frequency_hz, 306_000_000, "frequency deve ser 306 MHz");
                assert_eq!(modulation, 0x03, "modulation deve ser 64-QAM");
                assert_eq!(symbol_rate, 6_875_000, "symbol_rate deve ser 6875 ksym/s");
            }
            other => panic!("delivery deve ser CableDelivery, obteve: {other:?}"),
        }
    }

    /// NIT other (table_id=0x41) deve parsear com actual=false.
    ///
    /// SPEC-TABLE-003
    #[test]
    fn spec_table_003_nit_other_actual_false() {
        // Reutiliza a lógica da fixture mas com table_id=0x41
        let data = include_bytes!("../../tests/fixtures/nit_cable.bin");
        let mut patched = data.to_vec();
        patched[0] = 0x41; // mudar para NIT other
                           // Recalcular CRC
        let crc = crate::crc32_mpeg2(&patched[..patched.len() - 4]);
        let len = patched.len();
        patched[len - 4] = (crc >> 24) as u8;
        patched[len - 3] = (crc >> 16) as u8;
        patched[len - 2] = (crc >> 8) as u8;
        patched[len - 1] = (crc & 0xFF) as u8;

        let nit = Nit::parse(&patched).expect("NIT other deve parsear");
        assert!(!nit.actual);
    }

    /// table_id inválido deve retornar WrongTableIdMulti.
    ///
    /// SPEC-TABLE-003
    #[test]
    fn spec_table_003_wrong_table_id_returns_error() {
        let data = include_bytes!("../../tests/fixtures/nit_cable.bin");
        let mut bad = data.to_vec();
        bad[0] = 0x00; // PAT table_id
        let err = Nit::parse(&bad).unwrap_err();
        assert!(
            matches!(err, TableError::WrongTableIdMulti { found: 0x00 }),
            "deve retornar WrongTableIdMulti"
        );
    }

    /// Dados insuficientes devem retornar InsufficientData.
    ///
    /// SPEC-TABLE-003
    #[test]
    fn spec_table_003_insufficient_data_returns_error() {
        let short = [0x40u8, 0xB0, 0x0A]; // só 3 bytes
        let err = Nit::parse(&short).unwrap_err();
        assert!(matches!(err, TableError::InsufficientData { .. }));
    }
}
