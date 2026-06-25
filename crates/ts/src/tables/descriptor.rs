//! `Descriptor` genérico e `KnownDescriptor` com decode para tipos DVB.
//!
//! SPEC-TABLE-008 · SPEC-TABLE-008b · SPEC-TABLE-008c

use bytes::Bytes;

use super::dvb_string;

// ── Polarization ─────────────────────────────────────────────────────────────

/// Polarização de sinal para entrega via satélite.
///
/// SPEC-TABLE-003
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Polarization {
    LinearHorizontal,
    LinearVertical,
    CircularLeft,
    CircularRight,
}

// ── KnownDescriptor ───────────────────────────────────────────────────────────

/// Descriptor DVB decodificado para tipos conhecidos.
///
/// `decode()` nunca retorna erro — tipos desconhecidos caem em `Unknown`.
///
/// SPEC-TABLE-008 · SPEC-TABLE-008b
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnownDescriptor {
    /// Tag 0x40 — nome da rede de distribuição.
    ///
    /// SPEC-TABLE-003
    NetworkName { name: String },

    /// Tag 0x41 — lista de serviços e seus tipos.
    ///
    /// Cada tupla é `(service_id, service_type)`.
    ///
    /// SPEC-TABLE-008
    ServiceList { services: Vec<(u16, u8)> },

    /// Tag 0x47 — nome do bouquet.
    ///
    /// SPEC-TABLE-007
    BouquetName { name: String },

    /// Tag 0x48 — tipo e nomes do serviço (SDT).
    ///
    /// SPEC-TABLE-004 · SPEC-TABLE-008b
    Service {
        service_type: u8,
        provider: String,
        name: String,
    },

    /// Tag 0x4D — nome curto e descrição de evento (EIT).
    ///
    /// SPEC-TABLE-005 · SPEC-TABLE-008b
    ShortEvent {
        lang: [u8; 3],
        name: String,
        text: String,
    },

    /// Tag 0x43 — entrega via satélite.
    ///
    /// SPEC-TABLE-003 · SPEC-MI-005
    SatelliteDelivery {
        frequency_hz: u64,
        orbital_position_tenths: u16,
        west_east_flag: bool,
        polarization: Polarization,
        symbol_rate: u32,
    },

    /// Tag 0x44 — entrega via cabo.
    ///
    /// SPEC-TABLE-003
    CableDelivery {
        frequency_hz: u64,
        modulation: u8,
        symbol_rate: u32,
    },

    /// Tag 0x5A — entrega terrestre.
    ///
    /// SPEC-TABLE-003
    TerrestrialDelivery {
        centre_frequency_hz: u64,
        bandwidth_hz: u32,
    },

    /// Tag 0x58 — offset de fuso horário local (TOT).
    ///
    /// SPEC-MI-005
    LocalTimeOffset {
        country_code: String,
        country_region_id: u8,
        local_time_offset_polarity: bool,
        local_time_offset_h: u8,
        local_time_offset_m: u8,
    },

    /// Descriptor desconhecido — carrega tag e dados brutos.
    ///
    /// SPEC-TABLE-008b: `decode()` nunca retorna Err; tipos não reconhecidos
    /// caem aqui.
    Unknown { tag: u8, data: Bytes },
}

// ── Descriptor ────────────────────────────────────────────────────────────────

/// Descriptor MPEG-TS/DVB genérico.
///
/// Armazena `tag` e `data` brutos (sem tag e length). Use `decode()` para
/// obter a interpretação de alto nível quando disponível.
///
/// SPEC-TABLE-008
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    /// Identificador do tipo de descriptor (1 byte).
    pub tag: u8,
    /// Payload do descriptor, sem o byte `tag` e sem o byte `length`.
    pub data: Bytes,
}

impl Descriptor {
    /// Cria um `Descriptor` a partir de tag e dados brutos.
    ///
    /// SPEC-TABLE-008
    pub fn new(tag: u8, data: impl Into<Bytes>) -> Self {
        Self {
            tag,
            data: data.into(),
        }
    }

    /// Retorna o `format_identifier` de um registration descriptor (tag `0x05`).
    ///
    /// O identificador usa os 4 primeiros bytes do payload, quando presentes.
    pub fn registration_format_identifier(&self) -> Option<[u8; 4]> {
        if self.tag != 0x05 || self.data.len() < 4 {
            return None;
        }

        Some([self.data[0], self.data[1], self.data[2], self.data[3]])
    }

    /// Retorna `true` quando este descriptor é um registration descriptor com
    /// o `format_identifier` informado.
    pub fn is_registration_format(&self, expected: &[u8; 4]) -> bool {
        self.registration_format_identifier().as_ref() == Some(expected)
    }

    /// Lê uma lista de descriptors de um slice, retornando quantos foram
    /// consumidos com sucesso.
    ///
    /// Itera até esgotar `buf` ou encontrar um descriptor truncado (ignorado
    /// silenciosamente). Nunca retorna Err.
    ///
    /// SPEC-TABLE-008
    pub fn parse_list(buf: &[u8]) -> Vec<Descriptor> {
        let mut descriptors = Vec::new();
        let mut pos = 0usize;

        while pos + 2 <= buf.len() {
            let tag = buf[pos];
            let length = buf[pos + 1] as usize;
            pos += 2;

            if pos + length > buf.len() {
                // Descriptor truncado — para silenciosamente
                break;
            }

            let data = Bytes::copy_from_slice(&buf[pos..pos + length]);
            descriptors.push(Descriptor { tag, data });
            pos += length;
        }

        descriptors
    }

    /// Decodifica o descriptor para um `KnownDescriptor`.
    ///
    /// **Nunca retorna `Err`** — tipos desconhecidos ou malformados produzem
    /// `KnownDescriptor::Unknown`.
    ///
    /// SPEC-TABLE-008b
    pub fn decode(&self) -> KnownDescriptor {
        match self.tag {
            // ── 0x40: NetworkName ─────────────────────────────────────────
            0x40 => KnownDescriptor::NetworkName {
                name: dvb_string::decode(&self.data),
            },

            // ── 0x41: ServiceList ─────────────────────────────────────────
            0x41 => {
                let mut services = Vec::new();
                let data = &self.data;
                let mut i = 0usize;
                while i + 3 <= data.len() {
                    let service_id = u16::from_be_bytes([data[i], data[i + 1]]);
                    let service_type = data[i + 2];
                    services.push((service_id, service_type));
                    i += 3;
                }
                KnownDescriptor::ServiceList { services }
            }

            // ── 0x43: SatelliteDeliverySystem ────────────────────────────
            0x43 => {
                let data = &self.data;
                if data.len() < 11 {
                    return self.unknown();
                }
                // frequency: 8 BCD digits → 10 kHz units
                let freq_bcd = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                let frequency_hz = bcd32_to_u64(freq_bcd) * 10_000;
                let orbital_position_tenths = ((data[4] as u16) << 8) | data[5] as u16;
                let west_east_flag = (data[6] & 0x80) != 0;
                // polarization: bits 7-6 of byte 6
                let pol_bits = (data[6] >> 5) & 0x03;
                let polarization = match pol_bits {
                    0 => Polarization::LinearHorizontal,
                    1 => Polarization::LinearVertical,
                    2 => Polarization::CircularLeft,
                    _ => Polarization::CircularRight,
                };
                // symbol_rate: 7 BCD digits (28 bits) → 100 sym/s units
                let sr_bcd = u32::from_be_bytes([data[7], data[8], data[9], data[10]]) >> 4;
                let symbol_rate = (bcd32_to_u64(sr_bcd) * 100) as u32;
                KnownDescriptor::SatelliteDelivery {
                    frequency_hz,
                    orbital_position_tenths,
                    west_east_flag,
                    polarization,
                    symbol_rate,
                }
            }

            // ── 0x58: LocalTimeOffsetDescriptor (TOT) ────────────────────
            0x58 => {
                let data = &self.data;
                if data.len() < 6 {
                    return self.unknown();
                }
                let country_code = String::from_utf8_lossy(&data[0..3]).to_string();
                let country_region_id = (data[3] >> 2) & 0x3F;
                let local_time_offset_polarity = data[3] & 0x01 != 0;
                let local_time_offset_h = data[4];
                let local_time_offset_m = data[5];
                KnownDescriptor::LocalTimeOffset {
                    country_code,
                    country_region_id,
                    local_time_offset_polarity,
                    local_time_offset_h,
                    local_time_offset_m,
                }
            }

            // ── 0x44: CableDeliverySystem ────────────────────────────────
            0x44 => {
                let data = &self.data;
                if data.len() < 11 {
                    return self.unknown();
                }
                // frequency: 8 BCD digits → 100 Hz units
                let freq_bcd = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                let frequency_hz = bcd32_to_u64(freq_bcd) * 100;
                let modulation = data[6];
                // symbol_rate: 7 BCD digits (28 bits) → 100 sym/s
                let sr_bcd = u32::from_be_bytes([data[7], data[8], data[9], data[10]]) >> 4;
                let symbol_rate = (bcd32_to_u64(sr_bcd) * 100) as u32;
                KnownDescriptor::CableDelivery {
                    frequency_hz,
                    modulation,
                    symbol_rate,
                }
            }

            // ── 0x47: BouquetName ────────────────────────────────────────
            0x47 => KnownDescriptor::BouquetName {
                name: dvb_string::decode(&self.data),
            },

            // ── 0x48: Service ────────────────────────────────────────────
            0x48 => {
                let data = &self.data;
                if data.is_empty() {
                    return self.unknown();
                }
                let service_type = data[0];
                let mut pos = 1usize;

                // provider_name
                if pos >= data.len() {
                    return KnownDescriptor::Service {
                        service_type,
                        provider: String::new(),
                        name: String::new(),
                    };
                }
                let provider_len = data[pos] as usize;
                pos += 1;
                let provider = if pos + provider_len <= data.len() {
                    let s = dvb_string::decode(&data[pos..pos + provider_len]);
                    pos += provider_len;
                    s
                } else {
                    pos = data.len();
                    String::new()
                };

                // service_name
                if pos >= data.len() {
                    return KnownDescriptor::Service {
                        service_type,
                        provider,
                        name: String::new(),
                    };
                }
                let name_len = data[pos] as usize;
                pos += 1;
                let name = if pos + name_len <= data.len() {
                    dvb_string::decode(&data[pos..pos + name_len])
                } else {
                    String::new()
                };

                KnownDescriptor::Service {
                    service_type,
                    provider,
                    name,
                }
            }

            // ── 0x4D: ShortEvent ─────────────────────────────────────────
            0x4D => {
                let data = &self.data;
                if data.len() < 5 {
                    return self.unknown();
                }
                let lang = [data[0], data[1], data[2]];
                let name_len = data[3] as usize;
                let mut pos = 4usize;

                let name = if pos + name_len <= data.len() {
                    let s = dvb_string::decode(&data[pos..pos + name_len]);
                    pos += name_len;
                    s
                } else {
                    pos = data.len();
                    String::new()
                };

                let text = if pos < data.len() {
                    let text_len = data[pos] as usize;
                    pos += 1;
                    if pos + text_len <= data.len() {
                        dvb_string::decode(&data[pos..pos + text_len])
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };

                KnownDescriptor::ShortEvent { lang, name, text }
            }

            // ── 0x5A: TerrestrialDeliverySystem ──────────────────────────
            0x5A => {
                let data = &self.data;
                if data.len() < 11 {
                    return self.unknown();
                }
                // centre_frequency: 32 bits → 10 Hz units
                let freq_raw = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                let centre_frequency_hz = freq_raw as u64 * 10;
                // bandwidth: bits 7-5 of byte 4
                let bw_code = (data[4] >> 5) & 0x07;
                let bandwidth_hz: u32 = match bw_code {
                    0 => 8_000_000,
                    1 => 7_000_000,
                    2 => 6_000_000,
                    3 => 5_000_000,
                    _ => 0,
                };
                KnownDescriptor::TerrestrialDelivery {
                    centre_frequency_hz,
                    bandwidth_hz,
                }
            }

            // ── Desconhecido ─────────────────────────────────────────────
            _ => self.unknown(),
        }
    }

    /// Cria um `KnownDescriptor::Unknown` a partir deste descriptor.
    fn unknown(&self) -> KnownDescriptor {
        KnownDescriptor::Unknown {
            tag: self.tag,
            data: self.data.clone(),
        }
    }
}

/// Hint de contagem de canais do AC-3 descriptor DVB (tag `0x6A`).
///
/// SPEC-TABLE-009
pub fn ac3_descriptor_channel_hint(data: &[u8]) -> Option<u16> {
    if data.is_empty() {
        return None;
    }

    let component_type = data[0];
    let mode = (component_type >> 6) & 0x03;
    match mode {
        0b00 => Some(1),
        0b01 | 0b10 => Some(2),
        0b11 => {
            let ch_code = component_type & 0x07;
            Some(match ch_code {
                0 => 2,
                1 => 3,
                2 => 4,
                3 => 5,
                4 | 7 => 6,
                5 => 7,
                6 => 8,
                _ => 6,
            })
        }
        _ => None,
    }
}

/// Hint de perfil AAC/HE-AAC do descriptor DVB (tag `0x7C`).
///
/// SPEC-TABLE-009
pub fn aac_descriptor_profile_hint(data: &[u8]) -> Option<&'static str> {
    if data.is_empty() {
        return None;
    }

    let profile = (data[0] >> 5) & 0x03;
    match profile {
        1 => Some("HE-AAC"),
        2 => Some("HE-AAC v2"),
        _ => None,
    }
}

/// Procura hint de canais em descriptors de áudio conhecidos.
///
/// SPEC-TABLE-009
pub fn descriptor_audio_channel_hint(descriptors: &[Descriptor]) -> Option<u16> {
    descriptors
        .iter()
        .find(|descriptor| descriptor.tag == 0x6A)
        .and_then(|descriptor| ac3_descriptor_channel_hint(&descriptor.data))
}

/// Procura hint de perfil AAC em descriptors conhecidos.
///
/// SPEC-TABLE-009
pub fn descriptor_aac_profile_hint(descriptors: &[Descriptor]) -> Option<&'static str> {
    descriptors
        .iter()
        .find(|descriptor| descriptor.tag == 0x7C)
        .and_then(|descriptor| aac_descriptor_profile_hint(&descriptor.data))
}

// ── Helpers BCD ───────────────────────────────────────────────────────────────

/// Converte um valor BCD empacotado em 32 bits para `u64` decimal.
///
/// Cada nibble (4 bits) representa um dígito decimal.
/// Ex: `0x0366_0000` → `3660000`.
fn bcd32_to_u64(bcd: u32) -> u64 {
    let mut result = 0u64;
    let mut multiplier = 1u64;
    let mut val = bcd;
    for _ in 0..8 {
        result += (val & 0x0F) as u64 * multiplier;
        multiplier *= 10;
        val >>= 4;
    }
    result
}

// ── Testes ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_list ───────────────────────────────────────────────────────────

    /// Descriptor list vazia retorna Vec vazia.
    #[test]
    fn spec_table_008_parse_list_empty() {
        assert!(Descriptor::parse_list(&[]).is_empty());
    }

    /// Lista com um descriptor completo é parseada corretamente.
    #[test]
    fn spec_table_008_parse_list_single() {
        let buf = &[0x48u8, 0x03, 0x01, 0x02, 0x03];
        let list = Descriptor::parse_list(buf);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].tag, 0x48);
        assert_eq!(list[0].data.as_ref(), &[0x01u8, 0x02, 0x03]);
    }

    /// Descriptor truncado é ignorado silenciosamente.
    #[test]
    fn spec_table_008_parse_list_truncated_ignored() {
        // tag=0x40, length=5, mas só há 2 bytes de data
        let buf = &[0x40u8, 0x05, 0x41, 0x42];
        let list = Descriptor::parse_list(buf);
        assert!(list.is_empty());
    }

    /// Dois descriptors consecutivos são parseados.
    #[test]
    fn spec_table_008_parse_list_two_descriptors() {
        let buf = &[0x40u8, 0x01, 0xAA, 0x47, 0x02, 0xBB, 0xCC];
        let list = Descriptor::parse_list(buf);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].tag, 0x40);
        assert_eq!(list[1].tag, 0x47);
    }

    #[test]
    fn spec_table_008_registration_descriptor_identifier() {
        let desc = Descriptor::new(0x05, b"AC-3".to_vec());

        assert_eq!(desc.registration_format_identifier(), Some(*b"AC-3"));
        assert!(desc.is_registration_format(b"AC-3"));
        assert!(!desc.is_registration_format(b"EAC3"));
    }

    #[test]
    fn spec_table_008_registration_descriptor_requires_tag_and_length() {
        let wrong_tag = Descriptor::new(0x06, b"AC-3".to_vec());
        let too_short = Descriptor::new(0x05, vec![0x41, 0x42, 0x43]);

        assert_eq!(wrong_tag.registration_format_identifier(), None);
        assert_eq!(too_short.registration_format_identifier(), None);
    }

    // ── KnownDescriptor::NetworkName (0x40) ──────────────────────────────────

    /// SPEC-TABLE-008b: tag 0x40 decodifica para NetworkName.
    #[test]
    fn spec_table_008b_descriptor_decode_network_name() {
        let data = b"Net1".to_vec();
        let desc = Descriptor::new(0x40, data.clone());
        match desc.decode() {
            KnownDescriptor::NetworkName { name } => assert_eq!(name, "Net1"),
            other => panic!("esperado NetworkName, obtido {other:?}"),
        }
    }

    // ── KnownDescriptor::BouquetName (0x47) ──────────────────────────────────

    /// SPEC-TABLE-008b: tag 0x47 decodifica para BouquetName.
    #[test]
    fn spec_table_008b_descriptor_decode_bouquet_name() {
        let data = b"Bouquet1".to_vec();
        let desc = Descriptor::new(0x47, data);
        match desc.decode() {
            KnownDescriptor::BouquetName { name } => assert_eq!(name, "Bouquet1"),
            other => panic!("esperado BouquetName, obtido {other:?}"),
        }
    }

    // ── KnownDescriptor::ServiceList (0x41) ──────────────────────────────────

    /// SPEC-TABLE-008b: tag 0x41 com dois serviços.
    #[test]
    fn spec_table_008b_descriptor_decode_service_list() {
        // Dois serviços: (0x0101, 0x01) e (0x0202, 0x02)
        let data = vec![0x01u8, 0x01, 0x01, 0x02, 0x02, 0x02];
        let desc = Descriptor::new(0x41, data);
        match desc.decode() {
            KnownDescriptor::ServiceList { services } => {
                assert_eq!(services.len(), 2);
                assert_eq!(services[0], (0x0101, 0x01));
                assert_eq!(services[1], (0x0202, 0x02));
            }
            other => panic!("esperado ServiceList, obtido {other:?}"),
        }
    }

    /// SPEC-TABLE-008b: ServiceList vazia (data vazia).
    #[test]
    fn spec_table_008b_descriptor_decode_service_list_empty() {
        let desc = Descriptor::new(0x41, vec![]);
        match desc.decode() {
            KnownDescriptor::ServiceList { services } => assert!(services.is_empty()),
            other => panic!("esperado ServiceList, obtido {other:?}"),
        }
    }

    // ── KnownDescriptor::Service (0x48) ──────────────────────────────────────

    /// SPEC-TABLE-008b: tag 0x48 decodifica provider e nome.
    #[test]
    fn spec_table_008b_descriptor_decode_service() {
        // service_type=0x01, provider="ABC" (3 bytes), name="Canal1" (6 bytes)
        let mut data = vec![0x01u8]; // service_type
        data.push(3u8); // provider_name_length
        data.extend_from_slice(b"ABC");
        data.push(6u8); // service_name_length
        data.extend_from_slice(b"Canal1");

        let desc = Descriptor::new(0x48, data);
        match desc.decode() {
            KnownDescriptor::Service {
                service_type,
                provider,
                name,
            } => {
                assert_eq!(service_type, 0x01);
                assert_eq!(provider, "ABC");
                assert_eq!(name, "Canal1");
            }
            other => panic!("esperado Service, obtido {other:?}"),
        }
    }

    /// SPEC-TABLE-008b: Service com data vazia → Unknown.
    #[test]
    fn spec_table_008b_descriptor_decode_service_empty_data() {
        let desc = Descriptor::new(0x48, vec![]);
        assert!(matches!(desc.decode(), KnownDescriptor::Unknown { .. }));
    }

    // ── KnownDescriptor::ShortEvent (0x4D) ───────────────────────────────────

    /// SPEC-TABLE-008b: tag 0x4D decodifica idioma, nome e texto.
    #[test]
    fn spec_table_008b_descriptor_decode_short_event() {
        let mut data = vec![b'p', b'o', b'r']; // lang = "por"
        data.push(5u8); // event_name_length
        data.extend_from_slice(b"Filme");
        data.push(4u8); // text_length
        data.extend_from_slice(b"Acao");

        let desc = Descriptor::new(0x4D, data);
        match desc.decode() {
            KnownDescriptor::ShortEvent { lang, name, text } => {
                assert_eq!(&lang, b"por");
                assert_eq!(name, "Filme");
                assert_eq!(text, "Acao");
            }
            other => panic!("esperado ShortEvent, obtido {other:?}"),
        }
    }

    /// SPEC-TABLE-008b: ShortEvent com data curta demais → Unknown.
    #[test]
    fn spec_table_008b_descriptor_decode_short_event_too_short() {
        let desc = Descriptor::new(0x4D, vec![0x01, 0x02]);
        assert!(matches!(desc.decode(), KnownDescriptor::Unknown { .. }));
    }

    // ── Unknown fallback ─────────────────────────────────────────────────────

    /// SPEC-TABLE-008b: descriptor desconhecido → Unknown.
    #[test]
    fn spec_table_008_unknown_descriptor_fallback() {
        let desc = Descriptor::new(0xFE, vec![0xDE, 0xAD]);
        match desc.decode() {
            KnownDescriptor::Unknown { tag, data } => {
                assert_eq!(tag, 0xFE);
                assert_eq!(data.as_ref(), &[0xDE, 0xAD]);
            }
            other => panic!("esperado Unknown, obtido {other:?}"),
        }
    }

    // ── BCD helper ───────────────────────────────────────────────────────────

    #[test]
    fn bcd32_zero() {
        assert_eq!(super::bcd32_to_u64(0x00000000), 0);
    }

    #[test]
    fn bcd32_simple() {
        assert_eq!(super::bcd32_to_u64(0x00000001), 1);
        assert_eq!(super::bcd32_to_u64(0x00000099), 99);
        assert_eq!(super::bcd32_to_u64(0x00000123), 123);
    }

    #[test]
    fn bcd32_known_value() {
        // 0x00000123 em BCD: nibbles 3,2,1,0,0,0,0,0 → 3*1 + 2*10 + 1*100 = 123
        assert_eq!(super::bcd32_to_u64(0x00000123), 123);
    }

    // ── AC-3 / AAC descriptors (SPEC-TABLE-009) ────────────────────────────

    #[test]
    fn spec_table_009_ac3_descriptor_channel_hint_multichannel() {
        // component_type: mode=11 (multichannel), ch_code=100 (6 ch)
        assert_eq!(ac3_descriptor_channel_hint(&[0xC4]), Some(6));
    }

    #[test]
    fn spec_table_009_ac3_descriptor_channel_hint_stereo() {
        assert_eq!(ac3_descriptor_channel_hint(&[0x40]), Some(2));
    }

    #[test]
    fn spec_table_009_aac_descriptor_profile_hint_he_aac() {
        // profile bits 01 at positions 6-5 => 0x20
        assert_eq!(aac_descriptor_profile_hint(&[0x20]), Some("HE-AAC"));
    }

    // ── CableDelivery (0x44) ─────────────────────────────────────────────────

    /// Descriptor CableDelivery com 11 bytes de tamanho mínimo.
    #[test]
    fn spec_table_008b_descriptor_decode_cable_delivery() {
        // frequency: BCD 0x03660000 → 3660000 * 100 Hz = 366000000 Hz (366 MHz)
        // modulation: 0x03 = QAM-64
        // symbol_rate (28 bits BCD): 0x06875000 >> 4 = 0x0687500
        let data = vec![
            0x03, 0x66, 0x00, 0x00, // frequency BCD
            0x00, 0x00, // reserved + FEC_outer
            0x03, // modulation
            0x06, 0x87, 0x50, 0x00, // symbol_rate BCD (28 bits) + FEC_inner
        ];
        let desc = Descriptor::new(0x44, data);
        match desc.decode() {
            KnownDescriptor::CableDelivery {
                frequency_hz,
                modulation,
                symbol_rate: _,
            } => {
                assert_eq!(modulation, 0x03);
                // frequency_hz = bcd32(0x03660000) * 100
                // bcd32(0x03660000): nibbles 0,0,0,0,6,6,3,0 → 0+0+0+0+60000+600000+3000000+0 = 3660000
                assert_eq!(frequency_hz, 3_660_000 * 100);
            }
            other => panic!("esperado CableDelivery, obtido {other:?}"),
        }
    }
}
