//! Parser da EIT (Event Information Table).
//!
//! SPEC-TABLE-005

use chrono::{DateTime, Local, NaiveDateTime};

use super::{Descriptor, KnownDescriptor, RunningStatus, TableError};
use crate::tables::tdt::decode_mjd_bcd;

// ── EitEvent ─────────────────────────────────────────────────────────────────

/// Evento descrito na EIT.
///
/// SPEC-TABLE-005
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EitEvent {
    /// Identificador do evento.
    pub event_id: u16,
    /// Horário de início UTC.
    ///
    /// `None` se os primeiros bytes MJD+BCD forem `0xFF` (horário indefinido).
    ///
    /// SPEC-TABLE-005b
    pub start_time: Option<NaiveDateTime>,
    /// Duração em segundos (decodificada de BCD HH:MM:SS).
    ///
    /// `None` se os bytes de duração forem `0xFF`.
    pub duration_seconds: Option<u32>,
    /// Status de execução do evento.
    pub running_status: RunningStatus,
    /// `true` se o evento é condicionalmente acessado (scrambled).
    pub free_ca_mode: bool,
    /// Nome do evento extraído do `ShortEvent` descriptor (tag 0x4D).
    pub event_name: Option<String>,
    /// Descrição curta do evento extraída do `ShortEvent` descriptor (tag 0x4D).
    pub short_description: Option<String>,
    /// Todos os descriptors deste evento.
    pub descriptors: Vec<Descriptor>,
}

impl EitEvent {
    /// Converte o horário de início UTC para o horário local do sistema.
    ///
    /// Retorna `None` se `start_time` for `None`.
    ///
    /// SPEC-TABLE-005a
    pub fn start_time_local(&self) -> Option<DateTime<Local>> {
        use chrono::TimeZone as _;
        let utc = self.start_time?;
        Some(Local.from_utc_datetime(&utc))
    }
}

// ── Eit ───────────────────────────────────────────────────────────────────────

/// Event Information Table (EIT).
///
/// Descreve os eventos do guia eletrônico de programação (EPG) de um serviço.
///
/// - `table_id == 0x4E` → EIT Present/Following (actual transport stream)
/// - `table_id == 0x4F` → EIT Present/Following (other transport stream)
/// - `table_id in 0x50..=0x5F` → EIT Schedule (actual TS)
/// - `table_id in 0x60..=0x6F` → EIT Schedule (other TS)
///
/// SPEC-TABLE-005
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eit {
    /// Identificador do serviço.
    pub service_id: u16,
    /// Identificador do transport stream.
    pub transport_stream_id: u16,
    /// Identificador da rede original.
    pub original_network_id: u16,
    /// Versão da tabela (0–31).
    pub version: u8,
    /// `table_id` original da seção.
    pub table_id: u8,
    /// Lista de eventos descritos.
    pub events: Vec<EitEvent>,
}

impl Eit {
    /// Parseia uma seção EIT completa (cabeçalho PSI + corpo + CRC-32).
    ///
    /// Aceita `table_id` no intervalo `0x4E..=0x6F`.
    ///
    /// SPEC-TABLE-005
    pub fn parse(section: &[u8]) -> Result<Self, TableError> {
        // Mínimo: 3 (header) + 11 (body mínimo) + 4 (CRC) = 18
        const MIN_LEN: usize = 18;
        if section.len() < MIN_LEN {
            return Err(TableError::InsufficientData {
                expected: MIN_LEN,
                found:    section.len(),
            });
        }

        let table_id = section[0];
        if !matches!(table_id, 0x4E..=0x6F) {
            return Err(TableError::WrongTableIdMulti { found: table_id });
        }

        // section_body = sem os 3 bytes de cabeçalho e sem os 4 bytes de CRC
        let body = &section[3..section.len() - 4];

        // Mínimo do body: service_id(2) + version(1) + sec_num(1) + last_sec(1)
        //               + ts_id(2) + orig_net_id(2) + seg_last_sec(1) + last_table_id(1) = 11
        const MIN_BODY: usize = 11;
        if body.len() < MIN_BODY {
            return Err(TableError::InsufficientData {
                expected: MIN_BODY,
                found:    body.len(),
            });
        }

        let service_id          = u16::from_be_bytes([body[0], body[1]]);
        let version             = (body[2] >> 1) & 0x1F;
        // body[3] = section_number, body[4] = last_section_number (ignorados)
        let transport_stream_id = u16::from_be_bytes([body[5], body[6]]);
        let original_network_id = u16::from_be_bytes([body[7], body[8]]);
        // body[9] = segment_last_section_number, body[10] = last_table_id (ignorados)

        // ── Events loop ───────────────────────────────────────────────────────
        let mut pos = 11usize;
        let mut events = Vec::new();

        while pos < body.len() {
            // Cada event entry requer: event_id(2) + start(5) + duration(3)
            //                        + running|desc_len(2) = 12 bytes mínimos
            const EVT_HEADER: usize = 12;
            if pos + EVT_HEADER > body.len() {
                return Err(TableError::InsufficientData {
                    expected: pos + EVT_HEADER,
                    found:    body.len(),
                });
            }

            let event_id = u16::from_be_bytes([body[pos], body[pos + 1]]);

            // ── Decodificação MJD + BCD de start_time ─────────────────────────
            // SPEC-TABLE-005b: se byte[2] (HH) == 0xFF → start_time = None
            let start_time = if body[pos + 2] == 0xFF {
                None
            } else {
                let mjd = u16::from_be_bytes([body[pos + 2], body[pos + 3]]);
                let hh  = body[pos + 4];
                let mm  = body[pos + 5];
                let ss  = body[pos + 6];
                decode_mjd_bcd(mjd, hh, mm, ss)
            };

            // ── Duração BCD HH:MM:SS ──────────────────────────────────────────
            let dur_hh = body[pos + 7];
            let dur_mm = body[pos + 8];
            let dur_ss = body[pos + 9];
            let duration_seconds = if dur_hh == 0xFF {
                None
            } else {
                decode_duration_bcd(dur_hh, dur_mm, dur_ss)
            };

            let running_byte   = body[pos + 10];
            let desc_len_lo    = body[pos + 11];
            let running_status = RunningStatus::from_bits(running_byte >> 5);
            let free_ca_mode   = (running_byte >> 4) & 0x01 != 0;
            let desc_len =
                (((running_byte as u16 & 0x0F) << 8) | desc_len_lo as u16) as usize;
            pos += EVT_HEADER;

            if pos + desc_len > body.len() {
                return Err(TableError::InsufficientData {
                    expected: pos + desc_len,
                    found:    body.len(),
                });
            }

            let descriptors = Descriptor::parse_list(&body[pos..pos + desc_len]);
            pos += desc_len;

            // Extrair nome e descrição do ShortEvent descriptor
            let (event_name, short_description) = extract_short_event(&descriptors);

            events.push(EitEvent {
                event_id,
                start_time,
                duration_seconds,
                running_status,
                free_ca_mode,
                event_name,
                short_description,
                descriptors,
            });
        }

        Ok(Eit {
            service_id,
            transport_stream_id,
            original_network_id,
            version,
            table_id,
            events,
        })
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Decodifica duração BCD HH:MM:SS em segundos totais.
///
/// Retorna `None` se qualquer byte BCD for inválido.
fn decode_duration_bcd(hh: u8, mm: u8, ss: u8) -> Option<u32> {
    let h = bcd_to_u32(hh)?;
    let m = bcd_to_u32(mm)?;
    let s = bcd_to_u32(ss)?;
    Some(h * 3600 + m * 60 + s)
}

/// Converte byte BCD para `u32`. Retorna `None` se inválido.
fn bcd_to_u32(b: u8) -> Option<u32> {
    let hi = (b >> 4) as u32;
    let lo = (b & 0x0F) as u32;
    if hi > 9 || lo > 9 {
        return None;
    }
    Some(hi * 10 + lo)
}

/// Extrai `event_name` e `short_description` do primeiro `ShortEvent` descriptor.
fn extract_short_event(descriptors: &[Descriptor]) -> (Option<String>, Option<String>) {
    for d in descriptors {
        if let KnownDescriptor::ShortEvent { name, text, .. } = d.decode() {
            let event_name = if name.is_empty() { None } else { Some(name) };
            let short_desc = if text.is_empty() { None } else { Some(text) };
            return (event_name, short_desc);
        }
    }
    (None, None)
}

// ── Testes ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn build_eit_pf_section() -> Vec<u8> {
        // EIT p/f actual (0x4E):
        //   service_id=1, ts_id=1, orig_net_id=100, version=0
        //   event_id=101, start_time MJD=0xDCAE (2026-05-19), HH=20 MM=00 SS=00
        //   duration: 01:30:00 → 5400s
        //   running_status=4 (Running), free_ca_mode=0
        //   No descriptors

        // section_length = 5 (common_header) + 6 (eit_extra) + 12 (event) + 4 (CRC) = 27
        let section_length: u16 = 27;

        let mut sec = Vec::new();
        sec.push(0x4E); // table_id = EIT p/f actual
        sec.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
        sec.push((section_length & 0xFF) as u8);

        // PSI common header (5 bytes)
        sec.push(0x00); sec.push(0x01); // service_id = 1
        sec.push(0xC1); // reserved(2b)|version=0|current_next=1
        sec.push(0x00); // section_number
        sec.push(0x01); // last_section_number

        // EIT-specific (6 bytes)
        sec.push(0x00); sec.push(0x01); // transport_stream_id = 1
        sec.push(0x00); sec.push(0x64); // original_network_id = 100
        sec.push(0x01); // segment_last_section_number
        sec.push(0x4E); // last_table_id

        // Event entry (12 bytes, no descriptors)
        sec.push(0x00); sec.push(0x65); // event_id = 101
        // start_time: MJD=0xDCAE, HH=0x20, MM=0x00, SS=0x00
        sec.push(0xDC); sec.push(0xAE);
        sec.push(0x20); sec.push(0x00); sec.push(0x00);
        // duration: 01:30:00 BCD
        sec.push(0x01); sec.push(0x30); sec.push(0x00);
        // running_status=4(100b), free_ca_mode=0, desc_loop_len=0
        // byte: (4 << 5) | (0 << 4) | 0x00 = 0x80
        sec.push(0x80);
        sec.push(0x00); // desc_loop_len lo = 0

        let crc = crate::crc32_mpeg2(&sec);
        sec.extend_from_slice(&crc.to_be_bytes());
        sec
    }

    /// SPEC-TABLE-005: parse básico de seção EIT p/f com fixture em memória.
    #[test]
    fn spec_table_005_eit_pf() {
        let sec = build_eit_pf_section();
        let eit = Eit::parse(&sec).expect("deve parsear EIT p/f corretamente");

        assert_eq!(eit.table_id, 0x4E);
        assert_eq!(eit.service_id, 1);
        assert_eq!(eit.transport_stream_id, 1);
        assert_eq!(eit.original_network_id, 100);
        assert_eq!(eit.version, 0);
        assert_eq!(eit.events.len(), 1);

        let ev = &eit.events[0];
        assert_eq!(ev.event_id, 101);
        assert!(ev.start_time.is_some(), "start_time deve ser Some");
        assert_eq!(ev.duration_seconds, Some(5400)); // 1h30m
        assert_eq!(ev.running_status, RunningStatus::Running);
        assert!(!ev.free_ca_mode);
        assert!(ev.descriptors.is_empty());
    }

    /// SPEC-TABLE-005a: conversão UTC→local via start_time_local().
    #[test]
    fn spec_table_005a_start_time_local() {
        let sec = build_eit_pf_section();
        let eit = Eit::parse(&sec).unwrap();
        let ev = &eit.events[0];
        // Deve retornar Some independente do timezone local
        assert!(ev.start_time_local().is_some());
    }

    /// SPEC-TABLE-005b: start_time = None quando HH=0xFF (horário indefinido).
    #[test]
    fn spec_table_005b_start_time_none_when_ff() {
        let mut sec = build_eit_pf_section();
        // Layout da seção até HH:
        //   3 (PSI header) + 5 (common) + 6 (EIT extra) + 2 (event_id) + 2 (MJD) = offset 18
        let hh_offset = 3 + 5 + 6 + 2 + 2;
        sec[hh_offset] = 0xFF;
        // Recalcular CRC
        let len = sec.len();
        let crc = crate::crc32_mpeg2(&sec[..len - 4]);
        sec[len - 4] = (crc >> 24) as u8;
        sec[len - 3] = (crc >> 16) as u8;
        sec[len - 2] = (crc >> 8) as u8;
        sec[len - 1] = (crc & 0xFF) as u8;

        let eit = Eit::parse(&sec).unwrap();
        assert!(
            eit.events[0].start_time.is_none(),
            "start_time deve ser None quando HH=0xFF"
        );
    }

    /// SPEC-TABLE-005: erros básicos.
    #[test]
    fn spec_table_insufficient_data_eit() {
        assert!(matches!(
            Eit::parse(&[]),
            Err(TableError::InsufficientData { .. })
        ));
    }

    /// SPEC-TABLE-005: table_id incorreto.
    #[test]
    fn spec_table_wrong_table_id_eit() {
        let mut sec = build_eit_pf_section();
        sec[0] = 0x00; // PAT table_id
        // CRC não precisa ser válido para testar o table_id check
        assert!(matches!(
            Eit::parse(&sec),
            Err(TableError::WrongTableIdMulti { .. })
        ));
    }

    /// EIT p/f fixture de arquivo gerado pelo gen_fixtures.
    #[test]
    fn spec_table_005_eit_pf_from_fixture() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("eit_pf.bin");

        if !path.exists() {
            // Fixture ainda não gerada; skipa sem falhar
            return;
        }

        let data = std::fs::read(&path).expect("ler eit_pf.bin");
        let eit = Eit::parse(&data).expect("deve parsear EIT p/f da fixture");
        assert_eq!(eit.table_id, 0x4E);
        assert!(!eit.events.is_empty(), "deve ter ao menos 1 evento");
    }
}
