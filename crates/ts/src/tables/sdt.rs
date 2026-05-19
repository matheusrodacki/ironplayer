//! Parser da SDT (Service Description Table).
//!
//! SPEC-TABLE-004

use super::{Descriptor, KnownDescriptor, TableError};

// ── RunningStatus ─────────────────────────────────────────────────────────────

/// Status de execução de um serviço DVB.
///
/// SPEC-TABLE-004
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RunningStatus {
    /// Status não definido.
    Undefined = 0,
    /// Serviço não está em execução.
    NotRunning = 1,
    /// Serviço inicia em poucos segundos.
    StartsInFewSeconds = 2,
    /// Serviço pausado.
    Pausing = 3,
    /// Serviço em execução.
    Running = 4,
    /// Serviço fora do ar.
    ServiceOffAir = 5,
    /// Valor reservado (6–7).
    Reserved = 6,
}

impl RunningStatus {
    /// Cria `RunningStatus` a partir de um valor de 3 bits.
    pub fn from_bits(v: u8) -> Self {
        match v & 0x07 {
            0 => Self::Undefined,
            1 => Self::NotRunning,
            2 => Self::StartsInFewSeconds,
            3 => Self::Pausing,
            4 => Self::Running,
            5 => Self::ServiceOffAir,
            _ => Self::Reserved,
        }
    }
}

// ── SdtService ────────────────────────────────────────────────────────────────

/// Serviço descrito na SDT.
///
/// SPEC-TABLE-004
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdtService {
    /// Identificador do serviço.
    pub service_id: u16,
    /// `true` se houver EIT Schedule disponível para este serviço.
    pub eit_schedule_flag: bool,
    /// `true` se houver EIT Present/Following disponível.
    pub eit_present_following: bool,
    /// Status de execução do serviço.
    pub running_status: RunningStatus,
    /// `true` se o serviço é condicionalmente acessado (scrambled).
    pub free_ca_mode: bool,
    /// Nome do serviço extraído do `Service` descriptor (tag 0x48).
    pub service_name: Option<String>,
    /// Nome do provedor extraído do `Service` descriptor.
    pub provider_name: Option<String>,
    /// Tipo do serviço extraído do `Service` descriptor.
    pub service_type: Option<u8>,
    /// Todos os descriptors deste serviço.
    pub descriptors: Vec<Descriptor>,
}

// ── Sdt ───────────────────────────────────────────────────────────────────────

/// Service Description Table (SDT).
///
/// Descreve os serviços presentes no multiplex, incluindo nomes e status de
/// execução. `actual == true` quando `table_id == 0x42` (SDT actual);
/// `false` quando `table_id == 0x46` (SDT other).
///
/// SPEC-TABLE-004
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sdt {
    /// Identificador do transport stream.
    pub transport_stream_id: u16,
    /// Identificador da rede original.
    pub original_network_id: u16,
    /// Versão da tabela (0–31).
    pub version: u8,
    /// `true` quando `table_id == 0x42` (SDT actual).
    pub actual: bool,
    /// Lista de serviços descritos.
    pub services: Vec<SdtService>,
}

impl Sdt {
    /// Parseia uma seção SDT completa (cabeçalho PSI + corpo + CRC-32).
    ///
    /// Aceita `table_id` 0x42 (SDT actual) e 0x46 (SDT other).
    ///
    /// SPEC-TABLE-004
    pub fn parse(section: &[u8]) -> Result<Self, TableError> {
        // Mínimo: 3 (header) + 8 (ts_id + version + sec×2 + orig_net_id + reserved) + 4 (CRC) = 15
        const MIN_LEN: usize = 15;
        if section.len() < MIN_LEN {
            return Err(TableError::InsufficientData {
                expected: MIN_LEN,
                found: section.len(),
            });
        }

        let table_id = section[0];
        let actual = match table_id {
            0x42 => true,
            0x46 => false,
            other => return Err(TableError::WrongTableIdMulti { found: other }),
        };

        // section_body = sem os 3 bytes de cabeçalho e sem os 4 bytes de CRC
        let body = &section[3..section.len() - 4];

        // Mínimo do body: ts_id(2) + version(1) + sec_num(1) + last_sec(1)
        //               + orig_net_id(2) + reserved(1) = 8
        const MIN_BODY: usize = 8;
        if body.len() < MIN_BODY {
            return Err(TableError::InsufficientData {
                expected: MIN_BODY,
                found: body.len(),
            });
        }

        let transport_stream_id = u16::from_be_bytes([body[0], body[1]]);
        let version = (body[2] >> 1) & 0x1F;
        // body[3] = section_number, body[4] = last_section_number (ignorados)
        let original_network_id = u16::from_be_bytes([body[5], body[6]]);
        // body[7] = reserved byte

        // ── Services loop ─────────────────────────────────────────────────────
        let mut pos = 8usize;
        let mut services = Vec::new();

        while pos < body.len() {
            // Mínimo de cada entry: service_id(2) + eit_flags(1) + running|len(2) = 5
            const SVC_HEADER: usize = 5;
            if pos + SVC_HEADER > body.len() {
                return Err(TableError::InsufficientData {
                    expected: pos + SVC_HEADER,
                    found: body.len(),
                });
            }

            let service_id = u16::from_be_bytes([body[pos], body[pos + 1]]);
            let eit_flags = body[pos + 2];
            let eit_schedule_flag = eit_flags & 0x02 != 0;
            let eit_present_following = eit_flags & 0x01 != 0;
            let running_hi = body[pos + 3];
            let desc_loop_len_lo = body[pos + 4];
            let running_status = RunningStatus::from_bits(running_hi >> 5);
            let free_ca_mode = (running_hi >> 4) & 0x01 != 0;
            let desc_loop_len =
                (((running_hi as u16 & 0x0F) << 8) | desc_loop_len_lo as u16) as usize;
            pos += SVC_HEADER;

            if pos + desc_loop_len > body.len() {
                return Err(TableError::InsufficientData {
                    expected: pos + desc_loop_len,
                    found: body.len(),
                });
            }

            let descriptors = Descriptor::parse_list(&body[pos..pos + desc_loop_len]);
            pos += desc_loop_len;

            // Extrair service_name, provider_name, service_type do Service descriptor
            let (service_name, provider_name, service_type) = extract_service_info(&descriptors);

            services.push(SdtService {
                service_id,
                eit_schedule_flag,
                eit_present_following,
                running_status,
                free_ca_mode,
                service_name,
                provider_name,
                service_type,
                descriptors,
            });
        }

        Ok(Sdt {
            transport_stream_id,
            original_network_id,
            version,
            actual,
            services,
        })
    }
}

/// Extrai `(service_name, provider_name, service_type)` do primeiro
/// `Service` descriptor (tag 0x48) encontrado na lista.
fn extract_service_info(
    descriptors: &[Descriptor],
) -> (Option<String>, Option<String>, Option<u8>) {
    for d in descriptors {
        if let KnownDescriptor::Service {
            service_type,
            provider,
            name,
        } = d.decode()
        {
            return (
                if name.is_empty() { None } else { Some(name) },
                if provider.is_empty() {
                    None
                } else {
                    Some(provider)
                },
                Some(service_type),
            );
        }
    }
    (None, None, None)
}

// ── Testes ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify_crc32_mpeg2;

    /// Parseia a fixture `sdt_actual.bin` e verifica os campos extraídos.
    ///
    /// SPEC-TABLE-004
    #[test]
    fn spec_table_004_parse_sdt_fixture() {
        let data = include_bytes!("../../tests/fixtures/sdt_actual.bin");

        assert!(
            verify_crc32_mpeg2(data),
            "CRC-32 da fixture SDT deve ser válido"
        );
        assert_eq!(data[0], 0x42, "table_id deve ser 0x42 (SDT actual)");

        let sdt = Sdt::parse(data).expect("SDT deve parsear sem erro");

        assert_eq!(sdt.transport_stream_id, 1);
        assert_eq!(sdt.original_network_id, 100);
        assert_eq!(sdt.version, 3);
        assert!(sdt.actual);
        assert_eq!(sdt.services.len(), 1);

        let svc = &sdt.services[0];
        assert_eq!(svc.service_id, 1);
        assert!(svc.eit_schedule_flag);
        assert!(svc.eit_present_following);
        assert_eq!(svc.running_status, RunningStatus::Running);
        assert!(!svc.free_ca_mode);
        assert_eq!(svc.service_name.as_deref(), Some("Channel 1"));
        assert_eq!(svc.provider_name.as_deref(), Some("IronTV"));
        assert_eq!(svc.service_type, Some(0x01));
    }

    /// SDT other (table_id=0x46) deve parsear com actual=false.
    ///
    /// SPEC-TABLE-004
    #[test]
    fn spec_table_004_sdt_other_actual_false() {
        let data = include_bytes!("../../tests/fixtures/sdt_actual.bin");
        let mut patched = data.to_vec();
        patched[0] = 0x46;
        let crc = crate::crc32_mpeg2(&patched[..patched.len() - 4]);
        let len = patched.len();
        patched[len - 4] = (crc >> 24) as u8;
        patched[len - 3] = (crc >> 16) as u8;
        patched[len - 2] = (crc >> 8) as u8;
        patched[len - 1] = (crc & 0xFF) as u8;

        let sdt = Sdt::parse(&patched).expect("SDT other deve parsear");
        assert!(!sdt.actual);
    }

    /// table_id inválido deve retornar WrongTableIdMulti.
    ///
    /// SPEC-TABLE-004
    #[test]
    fn spec_table_004_wrong_table_id_returns_error() {
        let data = include_bytes!("../../tests/fixtures/sdt_actual.bin");
        let mut bad = data.to_vec();
        bad[0] = 0x00;
        let err = Sdt::parse(&bad).unwrap_err();
        assert!(matches!(err, TableError::WrongTableIdMulti { found: 0x00 }));
    }

    /// Dados insuficientes devem retornar InsufficientData.
    ///
    /// SPEC-TABLE-004
    #[test]
    fn spec_table_004_insufficient_data_returns_error() {
        let short = [0x42u8, 0xB0, 0x25]; // só 3 bytes
        let err = Sdt::parse(&short).unwrap_err();
        assert!(matches!(err, TableError::InsufficientData { .. }));
    }

    /// `RunningStatus::from_bits` cobre todos os valores 0–7.
    ///
    /// SPEC-TABLE-004
    #[test]
    fn spec_table_004_running_status_all_values() {
        assert_eq!(RunningStatus::from_bits(0), RunningStatus::Undefined);
        assert_eq!(RunningStatus::from_bits(1), RunningStatus::NotRunning);
        assert_eq!(
            RunningStatus::from_bits(2),
            RunningStatus::StartsInFewSeconds
        );
        assert_eq!(RunningStatus::from_bits(3), RunningStatus::Pausing);
        assert_eq!(RunningStatus::from_bits(4), RunningStatus::Running);
        assert_eq!(RunningStatus::from_bits(5), RunningStatus::ServiceOffAir);
        assert_eq!(RunningStatus::from_bits(6), RunningStatus::Reserved);
        assert_eq!(RunningStatus::from_bits(7), RunningStatus::Reserved);
    }
}
