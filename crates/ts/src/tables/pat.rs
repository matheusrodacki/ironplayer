//! Parser da PAT (Program Association Table).
//!
//! SPEC-TABLE-001 · SPEC-TABLE-001b · SPEC-TABLE-001d

use super::{SectionParser, TableError};
use crate::Pid;

// ── PatProgram ────────────────────────────────────────────────────────────────

/// Entrada de programa na PAT.
///
/// SPEC-TABLE-001
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatProgram {
    /// Número do programa.
    ///
    /// Quando `program_number == 0`, `pid` aponta para a NIT (SPEC-TABLE-001b).
    pub program_number: u16,
    /// PID da PMT (ou da NIT quando `program_number == 0`).
    pub pid: Pid,
}

// ── Pat ───────────────────────────────────────────────────────────────────────

/// Program Association Table (PAT).
///
/// Contém a lista de programas presentes no multiplex e os PIDs das
/// respectivas PMTs. Mudança de `version` requer re-parse de todas as PMTs
/// (SPEC-TABLE-001d).
///
/// SPEC-TABLE-001
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pat {
    /// Identificador único do Transport Stream.
    pub transport_stream_id: u16,
    /// Versão da PAT (0–31). Muda quando a tabela é modificada.
    ///
    /// SPEC-TABLE-001d
    pub version: u8,
    /// `true` quando esta seção está atualmente em vigor.
    pub current_next: bool,
    /// Lista de programas e seus PIDs de PMT (ou NIT).
    pub programs: Vec<PatProgram>,
}

impl Pat {
    /// Retorna o PID da NIT, se presente na PAT.
    ///
    /// `program_number == 0` indica o PID da NIT (pode diferir de 0x0010).
    ///
    /// SPEC-TABLE-001b
    pub fn nit_pid(&self) -> Option<Pid> {
        self.programs
            .iter()
            .find(|p| p.program_number == 0)
            .map(|p| p.pid)
    }

    /// Retorna os PIDs das PMTs (programs com `program_number != 0`).
    ///
    /// Estes PIDs devem ser registrados no `TsDemuxer` via `register_pmt_pid`.
    ///
    /// SPEC-TABLE-001c
    pub fn pmt_pids(&self) -> impl Iterator<Item = Pid> + '_ {
        self.programs
            .iter()
            .filter(|p| p.program_number != 0)
            .map(|p| p.pid)
    }

    /// Parseia o corpo de uma seção PAT a partir do slice `body`.
    ///
    /// `body` deve conter os bytes **após** os 3 bytes de cabeçalho PSI e
    /// **antes** dos 4 bytes de CRC-32 (já validado pelo `SectionAssembler`).
    ///
    /// SPEC-TABLE-001
    pub fn from_section_body(body: &[u8]) -> Result<Self, super::TableError> {
        <Self as super::SectionParser>::parse(body)
    }
}

// ── SectionParser ─────────────────────────────────────────────────────────────

impl SectionParser for Pat {
    /// Parseia o corpo de uma seção PAT.
    ///
    /// `section_body` deve ser o conteúdo **sem** os 3 bytes de cabeçalho PSI
    /// e **sem** os 4 bytes de CRC-32 (já validado pelo `SectionAssembler`).
    ///
    /// Layout esperado:
    /// ```text
    /// [transport_stream_id 2B]
    /// [reserved(2b) | version(5b) | current_next(1b)]
    /// [section_number 1B]
    /// [last_section_number 1B]
    /// [program_number(2B) | reserved(3b) | pid(13b)] × N
    /// ```
    ///
    /// SPEC-TABLE-001
    fn parse(section_body: &[u8]) -> Result<Self, TableError> {
        // Cabeçalho comum: tsid(2) + version_byte(1) + sec_num(1) + last_sec_num(1) = 5
        const HEADER_LEN: usize = 5;
        if section_body.len() < HEADER_LEN {
            return Err(TableError::InsufficientData {
                expected: HEADER_LEN,
                found:    section_body.len(),
            });
        }

        let transport_stream_id =
            u16::from_be_bytes([section_body[0], section_body[1]]);
        let version      = (section_body[2] >> 1) & 0x1F;
        let current_next = section_body[2] & 0x01 != 0;
        // section_body[3] = section_number  (ignorado — não necessário para parsing)
        // section_body[4] = last_section_number (ignorado)

        let entries = &section_body[HEADER_LEN..];

        // Cada entrada ocupa exatamente 4 bytes
        if entries.len() % 4 != 0 {
            return Err(TableError::InsufficientData {
                expected: HEADER_LEN + (entries.len() / 4 + 1) * 4,
                found:    section_body.len(),
            });
        }

        let mut programs = Vec::with_capacity(entries.len() / 4);
        for chunk in entries.chunks_exact(4) {
            let program_number = u16::from_be_bytes([chunk[0], chunk[1]]);
            let pid            = u16::from_be_bytes([chunk[2], chunk[3]]) & 0x1FFF;
            programs.push(PatProgram { program_number, pid });
        }

        Ok(Pat {
            transport_stream_id,
            version,
            current_next,
            programs,
        })
    }
}

// ── Testes ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{crc32_mpeg2, verify_crc32_mpeg2};

    /// Parseia a fixture `pat_section.bin` e verifica os campos extraídos.
    ///
    /// SPEC-TABLE-001
    #[test]
    fn spec_table_001_parse_pat_fixture() {
        let data = include_bytes!("../../tests/fixtures/pat_section.bin");

        // Verificar CRC da seção completa
        assert!(
            verify_crc32_mpeg2(data),
            "CRC-32 da fixture PAT deve ser válido"
        );

        // table_id deve ser 0x00
        assert_eq!(data[0], 0x00, "table_id deve ser 0x00 (PAT)");

        // section_body = sem os 3 bytes de cabeçalho e sem os 4 bytes de CRC
        let section_body = &data[3..data.len() - 4];
        let pat = Pat::parse(section_body).expect("PAT deve parsear sem erro");

        assert_eq!(pat.transport_stream_id, 1);
        assert_eq!(pat.version, 1);
        assert!(pat.current_next);
        assert_eq!(pat.programs.len(), 2);

        // Programa 0 → NIT PID
        let nit = &pat.programs[0];
        assert_eq!(nit.program_number, 0);
        assert_eq!(nit.pid, 0x0010);

        // Programa 1 → PMT PID
        let pmt_entry = &pat.programs[1];
        assert_eq!(pmt_entry.program_number, 1);
        assert_eq!(pmt_entry.pid, 0x0100);
    }

    /// `nit_pid()` retorna o PID correto quando `program_number == 0`.
    ///
    /// SPEC-TABLE-001b
    #[test]
    fn spec_table_001b_nit_pid_identified() {
        let pat = Pat {
            transport_stream_id: 1,
            version: 0,
            current_next: true,
            programs: vec![
                PatProgram { program_number: 0,    pid: 0x0010 },
                PatProgram { program_number: 1,    pid: 0x0100 },
            ],
        };
        assert_eq!(pat.nit_pid(), Some(0x0010));
    }

    /// `pmt_pids()` exclui o entry NIT e retorna apenas PIDs de PMT.
    ///
    /// SPEC-TABLE-001c
    #[test]
    fn spec_table_001c_pmt_pids_excludes_nit() {
        let pat = Pat {
            transport_stream_id: 1,
            version: 0,
            current_next: true,
            programs: vec![
                PatProgram { program_number: 0, pid: 0x0010 },
                PatProgram { program_number: 1, pid: 0x0100 },
                PatProgram { program_number: 2, pid: 0x0200 },
            ],
        };
        let pmt_pids: Vec<Pid> = pat.pmt_pids().collect();
        assert_eq!(pmt_pids, vec![0x0100, 0x0200]);
    }

    /// PAT sem programas (seção de "clear stream") deve parsear vazio.
    ///
    /// SPEC-TABLE-001
    #[test]
    fn spec_table_001_empty_pat_parses_ok() {
        // section_body apenas com cabeçalho comum, sem entradas
        let section_body = [
            0x00, 0x01, // transport_stream_id = 1
            0xC1,       // reserved|version=0|current_next=1
            0x00,       // section_number
            0x00,       // last_section_number
        ];
        let pat = Pat::parse(&section_body).expect("PAT vazia deve parsear sem erro");
        assert_eq!(pat.programs.len(), 0);
    }

    /// Dados insuficientes devem retornar `TableError::InsufficientData`.
    ///
    /// SPEC-TABLE-001
    #[test]
    fn spec_table_001_insufficient_data_returns_error() {
        let short = [0x00, 0x01, 0xC1]; // apenas 3 bytes
        let err = Pat::parse(&short).unwrap_err();
        assert!(
            matches!(err, TableError::InsufficientData { .. }),
            "deve retornar InsufficientData, obteve: {err:?}"
        );
    }

    /// Verifica que o CRC da seção da fixture é calculado corretamente
    /// com a função `crc32_mpeg2` do crate.
    ///
    /// SPEC-TABLE-001
    #[test]
    fn spec_table_001_crc_roundtrip() {
        let data = include_bytes!("../../tests/fixtures/pat_section.bin");
        let without_crc = &data[..data.len() - 4];
        let computed = crc32_mpeg2(without_crc);
        let stored = u32::from_be_bytes([
            data[data.len() - 4],
            data[data.len() - 3],
            data[data.len() - 2],
            data[data.len() - 1],
        ]);
        assert_eq!(
            computed, stored,
            "CRC computado deve coincidir com o CRC armazenado na fixture"
        );
    }
}
