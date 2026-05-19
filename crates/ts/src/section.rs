//! Remontagem de seções PSI/SI fragmentadas em múltiplos pacotes TS.
//!
//! SPEC-TS-003

use std::collections::HashMap;

use bytes::Bytes;
use crossbeam_channel::Sender;
use tracing::warn;

use crate::crc::verify_crc32_mpeg2;
use crate::demux::SectionData;
use crate::{Pid, TsError, TsEvent};

// ── CompleteSection ───────────────────────────────────────────────────────────

/// Seção PSI/SI completamente montada e com CRC-32 validado.
///
/// SPEC-TS-003
#[derive(Debug, Clone)]
pub struct CompleteSection {
    /// PID do qual a seção foi montada.
    pub pid: Pid,
    /// Identificador da tabela (primeiro byte do header da seção).
    pub table_id: u8,
    /// Bytes da seção completa, **sem** os 4 bytes de CRC finais.
    pub data: Bytes,
}

// ── SectionBuffer ─────────────────────────────────────────────────────────────

/// Estado interno de acumulação de fragmentos de uma seção para um único PID.
struct SectionBuffer {
    pid: Pid,
    table_id: u8,
    /// Número de bytes após os 3 bytes do header (inclui os 4 bytes de CRC).
    ///
    /// Tamanho total da seção = 3 + section_length.
    section_length: u16,
    /// Bytes acumulados (inclui os 3 bytes do header).
    data: Vec<u8>,
}

impl SectionBuffer {
    /// Retorna `true` quando todos os bytes da seção foram recebidos.
    #[inline]
    fn is_complete(&self) -> bool {
        self.data.len() >= 3 + self.section_length as usize
    }
}

// ── SectionAssembler ──────────────────────────────────────────────────────────

/// Remonta seções PSI/SI a partir de payloads de pacotes TS.
///
/// Gerencia buffers por PID, processa `pointer_field`/PUSI e valida CRC-32
/// MPEG-2 nas seções completas antes de emiti-las.
///
/// SPEC-TS-003
pub struct SectionAssembler {
    /// Buffers em preenchimento, indexados por PID.
    buffers: HashMap<Pid, SectionBuffer>,
    /// Canal de saída para seções completamente montadas e validadas.
    tx: Sender<CompleteSection>,
    /// Canal para eventos de diagnóstico (ex: `TsEvent::CrcError`).
    event_tx: Sender<TsEvent>,
}

impl SectionAssembler {
    /// Cria um novo `SectionAssembler`.
    ///
    /// SPEC-TS-003
    pub fn new(tx: Sender<CompleteSection>, event_tx: Sender<TsEvent>) -> Self {
        Self {
            buffers: HashMap::new(),
            tx,
            event_tx,
        }
    }

    /// Processa o payload de um pacote TS e tenta montar seções.
    ///
    /// # Erros
    ///
    /// Retorna `Err(TsError::SectionTooLarge)` quando o campo `section_length`
    /// do header de uma seção excede 4093 bytes. Erros de CRC são emitidos como
    /// `TsEvent::CrcError` (não como erro de retorno).
    ///
    /// SPEC-TS-003a · SPEC-TS-003b
    pub fn push(&mut self, data: SectionData) -> Result<(), TsError> {
        let pid = data.pid;
        let payload = &data.payload;

        if data.pusi {
            // ── PUSI=true: início (ou reinício) de seção ──────────────────
            //
            // SPEC-TS-003a: "PUSI=true com buffer pendente → Descarta anterior,
            // inicia nova."
            self.buffers.remove(&pid);

            // Precisamos de pelo menos 1 byte para o pointer_field.
            if payload.is_empty() {
                warn!("PUSI=true mas payload vazio; PID=0x{:04X}", pid);
                return Ok(());
            }

            let pointer_field = payload[0] as usize;
            let section_start = 1 + pointer_field;

            // Precisamos de pelo menos 3 bytes após pointer_field para ler
            // table_id e section_length.
            if section_start + 3 > payload.len() {
                warn!(
                    "payload insuficiente após pointer_field={}; PID=0x{:04X}",
                    pointer_field, pid
                );
                return Ok(());
            }

            let table_id = payload[section_start];
            // Bits de section_length: (byte1 & 0x0F) << 8 | byte2
            let section_length = ((payload[section_start + 1] as u16 & 0x0F) << 8)
                | payload[section_start + 2] as u16;

            // SPEC-TS-003a: section_length > 4093 → Err(TsError::SectionTooLarge)
            if section_length > 4093 {
                return Err(TsError::SectionTooLarge(section_length));
            }

            let total_needed = 3 + section_length as usize;
            let available = payload.len() - section_start;
            let to_copy = available.min(total_needed);

            let mut buf = SectionBuffer {
                pid,
                table_id,
                section_length,
                data: Vec::with_capacity(total_needed),
            };
            buf.data
                .extend_from_slice(&payload[section_start..section_start + to_copy]);

            if buf.is_complete() {
                self.try_emit(buf);
            } else {
                self.buffers.insert(pid, buf);
            }
        } else {
            // ── PUSI=false: continuação de seção pendente ─────────────────
            let Some(buf) = self.buffers.get_mut(&pid) else {
                // Sem buffer pendente — não há seção iniciada para este PID.
                return Ok(());
            };

            let total_needed = 3 + buf.section_length as usize;
            let remaining = total_needed.saturating_sub(buf.data.len());
            let to_append = payload.len().min(remaining);

            buf.data.extend_from_slice(&payload[..to_append]);

            if buf.is_complete() {
                // Remover do mapa antes de chamar try_emit (que consome o buffer).
                let buf = self.buffers.remove(&pid).unwrap();
                self.try_emit(buf);
            }
        }

        Ok(())
    }

    /// Valida o CRC-32 e emite a seção ou um evento de erro CRC.
    ///
    /// SPEC-TS-003b
    fn try_emit(&mut self, buf: SectionBuffer) {
        let pid = buf.pid;
        let total = 3 + buf.section_length as usize;
        let section_bytes = &buf.data[..total];

        if !verify_crc32_mpeg2(section_bytes) {
            // CRC inválido — descartar e notificar via evento.
            if self
                .event_tx
                .try_send(TsEvent::CrcError {
                    pid,
                    table_id: buf.table_id,
                })
                .is_err()
            {
                warn!(
                    "event_tx cheio; CrcError(pid=0x{:04X}, table_id=0x{:02X}) descartado",
                    pid, buf.table_id
                );
            }
            return;
        }

        // CRC válido — emitir seção sem os 4 bytes de CRC finais.
        let data_without_crc = Bytes::copy_from_slice(&section_bytes[..total - 4]);
        let complete = CompleteSection {
            pid,
            table_id: buf.table_id,
            data: data_without_crc,
        };

        if self.tx.try_send(complete).is_err() {
            warn!("tx cheio; CompleteSection(pid=0x{:04X}) descartado", pid);
        }
    }
}

// ── Testes ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crc::crc32_mpeg2;
    use crossbeam_channel::bounded;

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Constrói uma seção PSI/SI mínima (table_id + section_length + body + CRC).
    ///
    /// `section_length` = body.len() + 4 (4 bytes de CRC).
    fn make_section(table_id: u8, body: &[u8]) -> Vec<u8> {
        let section_length = body.len() as u16 + 4;
        let mut section = vec![
            table_id,
            0xB0 | ((section_length >> 8) as u8 & 0x0F),
            (section_length & 0xFF) as u8,
        ];
        section.extend_from_slice(body);
        let crc = crc32_mpeg2(&section);
        section.push((crc >> 24) as u8);
        section.push((crc >> 16) as u8);
        section.push((crc >> 8) as u8);
        section.push(crc as u8);
        section
    }

    /// Envolve uma seção em um payload de pacote TS com PUSI=true e pointer_field=0.
    fn pusi_payload(section: &[u8]) -> Bytes {
        let mut payload = vec![0x00u8]; // pointer_field = 0
        payload.extend_from_slice(section);
        Bytes::from(payload)
    }

    /// Cria um `SectionData` com PUSI=true.
    fn section_data_pusi(pid: Pid, payload: Bytes) -> SectionData {
        SectionData { pid, pusi: true, payload }
    }

    /// Cria um `SectionData` com PUSI=false.
    fn section_data_cont(pid: Pid, payload: Bytes) -> SectionData {
        SectionData { pid, pusi: false, payload }
    }

    // ── SPEC-TS-003 — Cenário 1: seção em pacote único ────────────────────────

    /// Seção que cabe inteiramente em um único pacote (PUSI=true) é emitida
    /// imediatamente após o push.
    ///
    /// SPEC-TS-003
    #[test]
    fn spec_ts_003_single_packet_section() {
        let (tx, rx) = bounded::<CompleteSection>(16);
        let (evt_tx, evt_rx) = bounded::<TsEvent>(16);
        let mut asm = SectionAssembler::new(tx, evt_tx);

        let section = make_section(0x00, &[0xAB, 0xCD]);
        let result = asm.push(section_data_pusi(0x0000, pusi_payload(&section)));

        assert!(result.is_ok());

        // Uma seção deve ter sido emitida.
        let complete = rx.try_recv().expect("CompleteSection esperada");
        assert_eq!(complete.pid, 0x0000);
        assert_eq!(complete.table_id, 0x00);
        // data não inclui CRC (últimos 4 bytes da seção).
        let expected_data = &section[..section.len() - 4];
        assert_eq!(complete.data.as_ref(), expected_data);

        // Nenhum evento de erro.
        assert!(evt_rx.try_recv().is_err(), "nenhum evento esperado");
    }

    // ── SPEC-TS-003 — Cenário 2: seção fragmentada em 3 pacotes ──────────────

    /// Seção fragmentada em 3 pacotes: emitida apenas após o 3º pacote.
    ///
    /// SPEC-TS-003
    #[test]
    fn spec_ts_003_three_packet_section() {
        let (tx, rx) = bounded::<CompleteSection>(16);
        let (evt_tx, evt_rx) = bounded::<TsEvent>(16);
        let mut asm = SectionAssembler::new(tx, evt_tx);

        // Seção com corpo de 20 bytes (para forçar fragmentação).
        let body: Vec<u8> = (0u8..20).collect();
        let section = make_section(0x02, &body);
        // total = 3 + section_length = 3 + 24 = 27 bytes

        // Pacote 1 (PUSI=true): pointer_field + primeiros 9 bytes da seção.
        let pkt1_payload: Vec<u8> = std::iter::once(0x00u8) // pointer_field=0
            .chain(section[..9].iter().copied())
            .collect();
        let r1 = asm.push(section_data_pusi(0x0100, Bytes::from(pkt1_payload)));
        assert!(r1.is_ok());
        assert!(rx.try_recv().is_err(), "não deve emitir no 1º pacote");

        // Pacote 2 (PUSI=false): próximos 9 bytes.
        let pkt2_payload = Bytes::from(section[9..18].to_vec());
        let r2 = asm.push(section_data_cont(0x0100, pkt2_payload));
        assert!(r2.is_ok());
        assert!(rx.try_recv().is_err(), "não deve emitir no 2º pacote");

        // Pacote 3 (PUSI=false): bytes finais.
        let pkt3_payload = Bytes::from(section[18..].to_vec());
        let r3 = asm.push(section_data_cont(0x0100, pkt3_payload));
        assert!(r3.is_ok());

        // Agora deve ter emitido.
        let complete = rx.try_recv().expect("CompleteSection esperada no 3º pacote");
        assert_eq!(complete.pid, 0x0100);
        assert_eq!(complete.table_id, 0x02);
        assert_eq!(complete.data.as_ref(), &section[..section.len() - 4]);

        // Nenhum evento de erro.
        assert!(evt_rx.try_recv().is_err(), "nenhum evento esperado");
    }

    // ── SPEC-TS-003 — Cenário 3: PUSI=true com buffer pendente ───────────────

    /// Quando PUSI=true chega com um buffer pendente para o mesmo PID, o buffer
    /// anterior é descartado e a nova seção é iniciada do zero.
    ///
    /// SPEC-TS-003
    #[test]
    fn spec_ts_003_pusi_with_pending_buffer() {
        let (tx, rx) = bounded::<CompleteSection>(16);
        let (evt_tx, _evt_rx) = bounded::<TsEvent>(16);
        let mut asm = SectionAssembler::new(tx, evt_tx);

        // Seção longa que não cabe em um único pacote.
        let body_old: Vec<u8> = vec![0xFF; 30];
        let section_old = make_section(0x01, &body_old);

        // Primeira seção: PUSI + apenas os primeiros bytes (incompleta).
        let pkt1: Vec<u8> = std::iter::once(0x00u8)
            .chain(section_old[..10].iter().copied())
            .collect();
        asm.push(section_data_pusi(0x0200, Bytes::from(pkt1)))
            .unwrap();
        assert!(rx.try_recv().is_err(), "incompleta — não deve emitir ainda");

        // Nova seção completa chegando com PUSI=true (deve descartar a anterior).
        let body_new = vec![0xAA, 0xBB];
        let section_new = make_section(0x03, &body_new);
        asm.push(section_data_pusi(0x0200, pusi_payload(&section_new)))
            .unwrap();

        // Apenas a nova seção deve ser emitida.
        let complete = rx.try_recv().expect("nova seção esperada");
        assert_eq!(complete.table_id, 0x03, "deve ser a nova seção (table_id=0x03)");
        assert!(rx.try_recv().is_err(), "apenas uma seção deve ter sido emitida");
    }

    // ── SPEC-TS-003 — Cenário 4: CRC inválido ────────────────────────────────

    /// Seção com CRC incorreto é descartada e um `TsEvent::CrcError` é emitido.
    ///
    /// SPEC-TS-003b
    #[test]
    fn spec_ts_003_invalid_crc() {
        let (tx, rx) = bounded::<CompleteSection>(16);
        let (evt_tx, evt_rx) = bounded::<TsEvent>(16);
        let mut asm = SectionAssembler::new(tx, evt_tx);

        // Seção com CRC válido, depois corrompemos o último byte.
        let mut section = make_section(0x00, &[0x01, 0x02, 0x03]);
        let last = section.len() - 1;
        section[last] ^= 0xFF; // corromper CRC

        asm.push(section_data_pusi(0x0000, pusi_payload(&section)))
            .unwrap();

        // Nenhuma seção deve ser emitida.
        assert!(rx.try_recv().is_err(), "seção com CRC inválido não deve ser emitida");

        // Um CrcError deve ter sido emitido.
        let events: Vec<TsEvent> = evt_rx.try_iter().collect();
        let crc_errors: Vec<&TsEvent> = events
            .iter()
            .filter(|e| matches!(e, TsEvent::CrcError { .. }))
            .collect();
        assert_eq!(crc_errors.len(), 1, "exatamente um CrcError esperado");

        match crc_errors[0] {
            TsEvent::CrcError { pid, table_id } => {
                assert_eq!(*pid, 0x0000);
                assert_eq!(*table_id, 0x00);
            }
            _ => panic!("evento inesperado"),
        }
    }

    // ── SPEC-TS-003 — Cenário 5: section_length > 4093 ───────────────────────

    /// Quando `section_length` excede 4093, `push` retorna
    /// `Err(TsError::SectionTooLarge)`.
    ///
    /// SPEC-TS-003
    #[test]
    fn spec_ts_003_section_too_large() {
        let (tx, _rx) = bounded::<CompleteSection>(16);
        let (evt_tx, _evt_rx) = bounded::<TsEvent>(16);
        let mut asm = SectionAssembler::new(tx, evt_tx);

        // Construir payload com section_length = 4094 (acima do limite).
        let section_length: u16 = 4094;
        // pointer_field=0, table_id, section_length_byte1 (com bits reservados),
        // section_length_byte2
        let payload = vec![
            0x00u8,                                         // pointer_field
            0x02,                                           // table_id
            0xF0 | ((section_length >> 8) as u8 & 0x0F),  // reserved(4) | section_length_high
            (section_length & 0xFF) as u8,                 // section_length_low
            0x00, 0x00,                                    // bytes de preenchimento
        ];

        let result = asm.push(section_data_pusi(0x0010, Bytes::from(payload)));
        assert!(
            matches!(result, Err(TsError::SectionTooLarge(4094))),
            "esperado Err(SectionTooLarge(4094)), obtido {:?}",
            result
        );
    }
}
