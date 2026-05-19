//! Parser da PMT (Program Map Table).
//!
//! SPEC-TABLE-002 · SPEC-TABLE-002b

use super::{Descriptor, SectionParser, TableError};
use crate::Pid;

// ── stream_type_label ─────────────────────────────────────────────────────────

/// Retorna um label legível para o `stream_type` MPEG-TS.
///
/// Cobre os 10+ tipos mais comuns encontrados em streams DVB-C/T/S.
/// Tipos não mapeados retornam `"Unknown"`.
///
/// SPEC-TABLE-002b
pub fn stream_type_label(st: u8) -> &'static str {
    match st {
        0x01 => "MPEG-1 Video",
        0x02 => "MPEG-2 Video",
        0x03 => "MPEG-1 Audio (MP1)",
        0x04 => "MPEG-2 Audio (MP2)",
        0x06 => "Private Data",
        0x0F => "AAC Audio (ADTS)",
        0x11 => "AAC Audio (LATM)",
        0x1B => "H.264 / AVC Video",
        0x24 => "H.265 / HEVC Video",
        0x81 => "AC-3 Audio (ATSC)",
        0x86 => "SCTE-35 Splice",
        _    => "Unknown",
    }
}

// ── PmtStream ─────────────────────────────────────────────────────────────────

/// Stream elementar descrito na PMT.
///
/// SPEC-TABLE-002
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmtStream {
    /// Tipo do stream (ISO 13818-1 Table 2-36).
    pub stream_type: u8,
    /// PID do stream elementar.
    pub elementary_pid: Pid,
    /// Descriptors ES_info associados a este stream.
    pub descriptors: Vec<Descriptor>,
}

impl PmtStream {
    /// Label legível para o `stream_type` desta entrada.
    ///
    /// SPEC-TABLE-002b
    pub fn label(&self) -> &'static str {
        stream_type_label(self.stream_type)
    }
}

// ── Pmt ───────────────────────────────────────────────────────────────────────

/// Program Map Table (PMT).
///
/// Descreve os streams elementares (áudio, vídeo, dados) de um programa,
/// o PID do PCR e os descriptors de programa.
///
/// SPEC-TABLE-002
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pmt {
    /// Número do programa (corresponde ao `program_number` da PAT).
    pub program_number: u16,
    /// Versão da PMT (0–31). Muda quando a tabela é modificada.
    pub version: u8,
    /// `true` quando esta seção está atualmente em vigor.
    pub current_next: bool,
    /// PID do PCR (Program Clock Reference) para este programa.
    pub pcr_pid: Pid,
    /// Descriptors de programa (program_info).
    pub program_descriptors: Vec<Descriptor>,
    /// Lista de streams elementares.
    pub streams: Vec<PmtStream>,
}

impl Pmt {
    /// Parseia o corpo de uma seção PMT a partir do slice `body`.
    ///
    /// `body` deve conter os bytes **após** os 3 bytes de cabeçalho PSI e
    /// **antes** dos 4 bytes de CRC-32 (já validado pelo `SectionAssembler`).
    ///
    /// SPEC-TABLE-002
    pub fn from_section_body(body: &[u8]) -> Result<Self, super::TableError> {
        <Self as super::SectionParser>::parse(body)
    }
}

// ── SectionParser ─────────────────────────────────────────────────────────────

impl SectionParser for Pmt {
    /// Parseia o corpo de uma seção PMT.
    ///
    /// `section_body` deve ser o conteúdo **sem** os 3 bytes de cabeçalho PSI
    /// e **sem** os 4 bytes de CRC-32 (já validado pelo `SectionAssembler`).
    ///
    /// Layout esperado:
    /// ```text
    /// [program_number 2B]
    /// [reserved(2b) | version(5b) | current_next(1b)]
    /// [section_number 1B]
    /// [last_section_number 1B]
    /// [reserved(3b) | PCR_PID(13b) 2B]
    /// [reserved(4b) | program_info_length(12b) 2B]
    /// [program_descriptors ... (program_info_length bytes)]
    /// [stream_type(1B) | reserved(3b)|elementary_PID(13b)(2B) | reserved(4b)|ES_info_length(12b)(2B) | descriptors...] × N
    /// ```
    ///
    /// SPEC-TABLE-002
    fn parse(section_body: &[u8]) -> Result<Self, TableError> {
        // Cabeçalho comum: program_number(2) + version_byte(1)
        //   + section_number(1) + last_section_number(1) = 5 bytes
        // PMT-específico: pcr_pid(2) + program_info_length(2) = 4 bytes
        // Total mínimo: 9 bytes
        const MIN_LEN: usize = 9;
        if section_body.len() < MIN_LEN {
            return Err(TableError::InsufficientData {
                expected: MIN_LEN,
                found:    section_body.len(),
            });
        }

        let program_number = u16::from_be_bytes([section_body[0], section_body[1]]);
        let version        = (section_body[2] >> 1) & 0x1F;
        let current_next   = section_body[2] & 0x01 != 0;
        // section_body[3] = section_number  (ignorado)
        // section_body[4] = last_section_number (ignorado)

        let pcr_pid =
            u16::from_be_bytes([section_body[5], section_body[6]]) & 0x1FFF;
        let program_info_length =
            (u16::from_be_bytes([section_body[7], section_body[8]]) & 0x0FFF) as usize;

        let prog_info_start = 9usize;
        let prog_info_end   = prog_info_start + program_info_length;

        if section_body.len() < prog_info_end {
            return Err(TableError::InsufficientData {
                expected: prog_info_end,
                found:    section_body.len(),
            });
        }

        let program_descriptors =
            Descriptor::parse_list(&section_body[prog_info_start..prog_info_end]);

        // ── Parsear stream entries ────────────────────────────────────────────
        let mut pos     = prog_info_end;
        let mut streams = Vec::new();

        while pos < section_body.len() {
            // Cada entrada de stream requer pelo menos 5 bytes:
            //   stream_type(1) + reserved|pid(2) + reserved|ES_info_length(2)
            const STREAM_HEADER: usize = 5;
            if pos + STREAM_HEADER > section_body.len() {
                return Err(TableError::InsufficientData {
                    expected: pos + STREAM_HEADER,
                    found:    section_body.len(),
                });
            }

            let stream_type = section_body[pos];
            let elementary_pid =
                u16::from_be_bytes([section_body[pos + 1], section_body[pos + 2]])
                    & 0x1FFF;
            let es_info_length =
                (u16::from_be_bytes([section_body[pos + 3], section_body[pos + 4]])
                    & 0x0FFF) as usize;

            pos += STREAM_HEADER;
            let es_end = pos + es_info_length;

            if section_body.len() < es_end {
                return Err(TableError::InsufficientData {
                    expected: es_end,
                    found:    section_body.len(),
                });
            }

            let descriptors = Descriptor::parse_list(&section_body[pos..es_end]);
            pos = es_end;

            streams.push(PmtStream {
                stream_type,
                elementary_pid,
                descriptors,
            });
        }

        Ok(Pmt {
            program_number,
            version,
            current_next,
            pcr_pid,
            program_descriptors,
            streams,
        })
    }
}

// ── Testes ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify_crc32_mpeg2;

    /// Parseia a fixture `pmt_h264_aac.bin` e verifica os campos extraídos.
    ///
    /// SPEC-TABLE-002
    #[test]
    fn spec_table_002_parse_pmt_fixture() {
        let data = include_bytes!("../../tests/fixtures/pmt_h264_aac.bin");

        assert!(
            verify_crc32_mpeg2(data),
            "CRC-32 da fixture PMT deve ser válido"
        );

        // table_id deve ser 0x02
        assert_eq!(data[0], 0x02, "table_id deve ser 0x02 (PMT)");

        let section_body = &data[3..data.len() - 4];
        let pmt = Pmt::parse(section_body).expect("PMT deve parsear sem erro");

        assert_eq!(pmt.program_number, 1);
        assert_eq!(pmt.version, 1);
        assert!(pmt.current_next);
        assert_eq!(pmt.pcr_pid, 0x0110);
        assert!(pmt.program_descriptors.is_empty());
        assert_eq!(pmt.streams.len(), 2);

        let h264 = &pmt.streams[0];
        assert_eq!(h264.stream_type, 0x1B);
        assert_eq!(h264.elementary_pid, 0x0110);
        assert_eq!(h264.label(), "H.264 / AVC Video");

        let aac = &pmt.streams[1];
        assert_eq!(aac.stream_type, 0x0F);
        assert_eq!(aac.elementary_pid, 0x0120);
        assert_eq!(aac.label(), "AAC Audio (ADTS)");
    }

    /// Verifica que `stream_type_label` retorna strings corretas para os
    /// 10+ tipos obrigatórios definidos na SPEC-TABLE-002b.
    ///
    /// SPEC-TABLE-002b
    #[test]
    fn spec_table_002b_stream_type_labels_all_mapped() {
        let cases: &[(u8, &str)] = &[
            (0x01, "MPEG-1 Video"),
            (0x02, "MPEG-2 Video"),
            (0x03, "MPEG-1 Audio (MP1)"),
            (0x04, "MPEG-2 Audio (MP2)"),
            (0x06, "Private Data"),
            (0x0F, "AAC Audio (ADTS)"),
            (0x11, "AAC Audio (LATM)"),
            (0x1B, "H.264 / AVC Video"),
            (0x24, "H.265 / HEVC Video"),
            (0x81, "AC-3 Audio (ATSC)"),
            (0x86, "SCTE-35 Splice"),
        ];

        for &(st, expected) in cases {
            assert_eq!(
                stream_type_label(st),
                expected,
                "stream_type 0x{st:02X} deve ter label '{expected}'"
            );
        }

        // Tipo desconhecido não deve entrar em panic
        assert_eq!(stream_type_label(0xFF), "Unknown");
    }

    /// PMT sem streams deve parsear como lista vazia.
    ///
    /// SPEC-TABLE-002
    #[test]
    fn spec_table_002_empty_streams_parses_ok() {
        let section_body: &[u8] = &[
            0x00, 0x01, // program_number = 1
            0xC3,       // reserved|version=1|current_next=1
            0x00,       // section_number
            0x00,       // last_section_number
            0xE1, 0x10, // reserved|PCR_PID = 0x0110
            0xF0, 0x00, // reserved|program_info_length = 0
        ];
        let pmt = Pmt::parse(section_body).expect("PMT sem streams deve parsear");
        assert_eq!(pmt.program_number, 1);
        assert_eq!(pmt.pcr_pid, 0x0110);
        assert!(pmt.streams.is_empty());
    }

    /// Dados insuficientes devem retornar `TableError::InsufficientData`.
    ///
    /// SPEC-TABLE-002
    #[test]
    fn spec_table_002_insufficient_data_returns_error() {
        let short = [0x00, 0x01, 0xC3]; // apenas 3 bytes
        let err = Pmt::parse(&short).unwrap_err();
        assert!(
            matches!(err, TableError::InsufficientData { .. }),
            "deve retornar InsufficientData, obteve: {err:?}"
        );
    }

    /// PMT com descriptors de programa deve parsear corretamente.
    ///
    /// SPEC-TABLE-002
    #[test]
    fn spec_table_002_with_program_descriptors() {
        // Descriptor tag=0x09 (CA), length=4, dados=[0x00, 0x26, 0xE1, 0x00]
        let section_body: &[u8] = &[
            0x00, 0x01, // program_number = 1
            0xC3,       // reserved|version=1|current_next=1
            0x00,       // section_number
            0x00,       // last_section_number
            0xE1, 0x10, // reserved|PCR_PID = 0x0110
            0xF0, 0x06, // reserved|program_info_length = 6
            // program descriptor: tag=0x09, length=4
            0x09, 0x04, 0x00, 0x26, 0xE1, 0x00,
            // stream: H.264 at PID 0x0110, no ES descriptors
            0x1B, 0xE1, 0x10, 0xF0, 0x00,
        ];
        let pmt = Pmt::parse(section_body).expect("PMT com descriptors deve parsear");
        assert_eq!(pmt.program_descriptors.len(), 1);
        assert_eq!(pmt.program_descriptors[0].tag, 0x09);
        assert_eq!(pmt.streams.len(), 1);
        assert_eq!(pmt.streams[0].stream_type, 0x1B);
    }
}
