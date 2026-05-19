//! Parse de pacotes MPEG-TS de 188 bytes.
//!
//! SPEC-TS-001

use bytes::Bytes;

use crate::adaptation::AdaptationField;
use crate::{Pid, TsError};

/// Pacote MPEG-TS de 188 bytes após parse do header e campos opcionais.
///
/// SPEC-TS-001
#[derive(Debug, Clone)]
pub struct TsPacket {
    /// PID (13 bits; faixa válida 0x0000–0x1FFF).
    pub pid: Pid,
    /// Transport Error Indicator — bit de sinalização de erro de transmissão.
    pub tei: bool,
    /// Payload Unit Start Indicator — início de PES/seção neste pacote.
    pub pusi: bool,
    /// Transport Priority.
    pub priority: bool,
    /// Scrambling control (2 bits; 0 = não embaralhado).
    pub scrambling: u8,
    /// Adaptation Field (presente se AFC = `0b10` ou `0b11`).
    pub adaptation_field: Option<AdaptationField>,
    /// Payload (presente se AFC = `0b01` ou `0b11`).
    pub payload: Option<Bytes>,
    /// Continuity Counter (4 bits; 0x0–0xF).
    pub continuity_counter: u8,
}

impl TsPacket {
    /// Parse de exatamente 188 bytes de um pacote MPEG-TS.
    ///
    /// SPEC-TS-001a
    ///
    /// # Errors
    ///
    /// - [`TsError::InvalidPacketSize`] — `raw.len() != 188`
    /// - [`TsError::InvalidSyncByte`] — `raw[0] != 0x47`
    /// - [`TsError::MalformedAdaptationField`] — adaptation field inválido/truncado
    pub fn parse(raw: &[u8]) -> Result<Self, TsError> {
        if raw.len() != 188 {
            return Err(TsError::InvalidPacketSize(raw.len()));
        }
        if raw[0] != 0x47 {
            return Err(TsError::InvalidSyncByte(raw[0]));
        }

        // ── Header (4 bytes) ────────────────────────────────────────────────
        let tei = (raw[1] & 0x80) != 0;
        let pusi = (raw[1] & 0x40) != 0;
        let priority = (raw[1] & 0x20) != 0;
        let pid = ((raw[1] as u16 & 0x1F) << 8) | (raw[2] as u16);

        let scrambling = (raw[3] >> 6) & 0x03;
        let afc = (raw[3] >> 4) & 0x03;
        let continuity_counter = raw[3] & 0x0F;

        // ── Adaptation Field e Payload (bytes 4–187) ─────────────────────────
        //
        // AFC:  0b00 = reservado (descartamos silenciosamente)
        //       0b01 = payload apenas
        //       0b10 = adaptation field apenas
        //       0b11 = adaptation field + payload
        let (adaptation_field, payload) = match afc {
            0b01 => {
                // Payload apenas — bytes 4..188.
                (None, Some(Bytes::copy_from_slice(&raw[4..])))
            }
            0b10 => {
                // Adaptation field apenas — raw[4] é adaptation_field_length.
                let af = AdaptationField::parse(&raw[4..])?;
                (Some(af), None)
            }
            0b11 => {
                // Adaptation field + payload.
                // raw[4] = adaptation_field_length (N bytes após o byte de tamanho).
                // Payload começa em raw[4 + 1 + adaptation_field_length].
                let af_length = raw[4] as usize;
                let payload_start = 5 + af_length;
                if payload_start > 188 {
                    return Err(TsError::MalformedAdaptationField);
                }
                let af = AdaptationField::parse(&raw[4..])?;
                let payload = Bytes::copy_from_slice(&raw[payload_start..]);
                (Some(af), Some(payload))
            }
            _ => {
                // 0b00: reservado — sem adaptation field nem payload.
                (None, None)
            }
        };

        Ok(TsPacket {
            pid,
            tei,
            pusi,
            priority,
            scrambling,
            adaptation_field,
            payload,
            continuity_counter,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Constrói um pacote TS de 188 bytes com parâmetros básicos.
    fn build_packet(pid: u16, afc: u8, cc: u8) -> [u8; 188] {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = ((pid >> 8) & 0x1F) as u8;
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = (afc << 4) | (cc & 0x0F);
        // Para AFC com adaptation field, setar length = 0 (stuffing).
        if afc == 0b10 || afc == 0b11 {
            pkt[4] = 0x00;
        }
        pkt
    }

    // ── SPEC-TS-001 — 6 cenários obrigatórios ───────────────────────────────

    /// Byte 0 != 0x47 → InvalidSyncByte.
    #[test]
    fn spec_ts_001_invalid_sync_byte() {
        let mut pkt = [0u8; 188];
        pkt[0] = 0x00;
        assert!(matches!(
            TsPacket::parse(&pkt),
            Err(TsError::InvalidSyncByte(0x00))
        ));
    }

    /// Slice com 187 bytes → InvalidPacketSize.
    #[test]
    fn spec_ts_001_invalid_packet_size() {
        let pkt = [0x47u8; 187];
        assert!(matches!(
            TsPacket::parse(&pkt),
            Err(TsError::InvalidPacketSize(187))
        ));
    }

    /// Null packet (PID 0x1FFF) → Ok, pid == 0x1FFF.
    #[test]
    fn spec_ts_001_null_packet() {
        let pkt = build_packet(0x1FFF, 0b01, 0);
        let result = TsPacket::parse(&pkt).unwrap();
        assert_eq!(result.pid, 0x1FFF);
    }

    /// TEI bit setado → tei == true.
    #[test]
    fn spec_ts_001_tei_bit() {
        let mut pkt = build_packet(0x0100, 0b01, 0);
        pkt[1] |= 0x80; // setar TEI
        let result = TsPacket::parse(&pkt).unwrap();
        assert!(result.tei);
    }

    /// AFC = 0b10 (adaptation only) → payload == None, adaptation_field == Some.
    #[test]
    fn spec_ts_001_adaptation_only() {
        let pkt = build_packet(0x0100, 0b10, 5);
        let result = TsPacket::parse(&pkt).unwrap();
        assert!(result.payload.is_none(), "payload deve ser None quando AFC=0b10");
        assert!(
            result.adaptation_field.is_some(),
            "adaptation_field deve ser Some quando AFC=0b10"
        );
        assert_eq!(result.continuity_counter, 5);
    }

    /// AFC = 0b01 (payload only) → adaptation_field == None, payload == Some.
    #[test]
    fn spec_ts_001_payload_only() {
        let pkt = build_packet(0x0100, 0b01, 3);
        let result = TsPacket::parse(&pkt).unwrap();
        assert!(
            result.adaptation_field.is_none(),
            "adaptation_field deve ser None quando AFC=0b01"
        );
        assert!(result.payload.is_some(), "payload deve ser Some quando AFC=0b01");
        // Payload deve ter 184 bytes (188 - 4 header)
        assert_eq!(result.payload.unwrap().len(), 184);
    }

    // ── Testes adicionais ────────────────────────────────────────────────────

    /// AFC = 0b11 (adaptation + payload) → ambos presentes.
    #[test]
    fn spec_ts_001_adaptation_and_payload() {
        // adaptation_field_length = 10, payload = bytes 15..188
        let mut pkt = [0x00u8; 188];
        pkt[0] = 0x47;
        pkt[1] = 0x01;
        pkt[2] = 0x00;
        pkt[3] = (0b11 << 4) | 0x00; // AFC=11, CC=0
        pkt[4] = 10; // adaptation_field_length
        pkt[5] = 0x00; // flags (nenhum flag setado)
        // bytes 6..15 = stuffing (já são 0x00)
        // bytes 15..188 = payload
        let result = TsPacket::parse(&pkt).unwrap();
        assert!(result.adaptation_field.is_some());
        assert!(result.payload.is_some());
        assert_eq!(result.payload.unwrap().len(), 188 - 15);
    }

    /// PUSI bit lido corretamente.
    #[test]
    fn spec_ts_001_pusi_bit() {
        let mut pkt = build_packet(0x0100, 0b01, 0);
        pkt[1] |= 0x40; // setar PUSI
        let result = TsPacket::parse(&pkt).unwrap();
        assert!(result.pusi);
    }

    /// Scrambling bits lidos corretamente.
    #[test]
    fn spec_ts_001_scrambling_bits() {
        let mut pkt = build_packet(0x0100, 0b01, 0);
        pkt[3] |= 0b11 << 6; // scrambling = 0b11
        let result = TsPacket::parse(&pkt).unwrap();
        assert_eq!(result.scrambling, 0b11);
    }
}
