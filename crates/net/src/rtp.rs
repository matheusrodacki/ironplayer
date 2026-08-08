//! Parse de header RTP e remoção de encapsulamento.
//!
//! [`RtpStripper`] é o caminho do player (SPEC-NET-003): tira o header e segue.
//! [`RtpHeader`] é o caminho da probe (SPEC-PROBE-IP-014): expõe **todos** os
//! campos, porque perda, reordenação, SSRC, bits proibidos e tamanho de payload
//! são exatamente o que a camada 1 mede.
//!
//! SPEC-NET-003 · SPEC-PROBE-IP-014 · SPEC-PROBE-IP-015 · SPEC-PROBE-IP-016 ·
//! SPEC-PROBE-IP-017 · SPEC-PROBE-IP-018

use bytes::Bytes;
use crossbeam_channel::Sender;

use crate::RtpEvent;

/// Sync byte de um pacote MPEG-TS.
pub const TS_SYNC_BYTE: u8 = 0x47;
/// Tamanho de um pacote MPEG-TS.
pub const TS_PACKET_LEN: usize = 188;
/// Header RTP fixo, sem CSRC nem extensão.
pub const RTP_FIXED_HEADER_LEN: usize = 12;
/// Payload type do MPEG-TS sobre RTP (RFC 3551).
pub const RTP_PT_MPEGTS: u8 = 33;

#[cfg(test)]
const RTP_VERSION_2: u8 = 0x80; // V=2 mask

/// Header RTP completo (RFC 3550 §5.1).
///
/// SPEC-PROBE-IP-014 — V, P, X, CC, M, PT, seq, timestamp e SSRC, com o header
/// de extensão devidamente pulado quando `X = 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub version: u8,
    /// Bit P — proibido pelo perfil ST 2022-2 (SPEC-PROBE-IP-016).
    pub padding: bool,
    /// Bit X — proibido pelo perfil ST 2022-2 (SPEC-PROBE-IP-016).
    pub extension: bool,
    pub csrc_count: u8,
    /// Bit M — proibido pelo perfil ST 2022-2 (SPEC-PROBE-IP-016).
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    /// Bytes de header a descartar: fixo + 4·CC + extensão.
    pub header_len: usize,
}

impl RtpHeader {
    /// Faz o parse do header, sem tocar no payload.
    ///
    /// Devolve `None` quando o datagrama é curto demais ou `V ≠ 2` — dado de
    /// rede nunca faz `panic` nem indexa fora do buffer (RNF-PRB-003).
    ///
    /// SPEC-PROBE-IP-014
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < RTP_FIXED_HEADER_LEN {
            return None;
        }
        let b0 = data[0];
        let b1 = data[1];
        let version = (b0 >> 6) & 0x03;
        if version != 2 {
            return None;
        }

        let csrc_count = b0 & 0x0F;
        let extension = (b0 & 0x10) != 0;
        let mut header_len = RTP_FIXED_HEADER_LEN + 4 * csrc_count as usize;
        if data.len() < header_len {
            return None;
        }

        // X = 1 ⇒ o header de extensão vem depois dos CSRC: 2 bytes de perfil,
        // 2 bytes de comprimento em **palavras de 32 bits**, e o corpo.  Pular
        // isso errado desalinharia o payload e transformaria um stream sadio
        // numa enxurrada de `bad_payload_size`.
        if extension {
            let ext_at = header_len;
            if data.len() < ext_at + 4 {
                return None;
            }
            let words = u16::from_be_bytes([data[ext_at + 2], data[ext_at + 3]]) as usize;
            header_len = ext_at + 4 + words * 4;
            if data.len() < header_len {
                return None;
            }
        }

        Some(Self {
            version,
            padding: (b0 & 0x20) != 0,
            extension,
            csrc_count,
            marker: (b1 & 0x80) != 0,
            payload_type: b1 & 0x7F,
            sequence: u16::from_be_bytes([data[2], data[3]]),
            timestamp: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
            ssrc: u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
            header_len,
        })
    }

    /// SSRC formatado como no event log e no CSV.
    pub fn ssrc_hex(&self) -> String {
        format!("0x{:08X}", self.ssrc)
    }
}

/// Conformidade do payload de um datagrama que carrega MPEG-TS.
///
/// SPEC-PROBE-IP-017 · SPEC-PROBE-IP-018
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TsPayloadShape {
    /// Pacotes TS de 188 bytes contidos no datagrama.
    pub ts_packets: usize,
    /// `true` quando o tamanho é múltiplo de 188 **e** começa em `0x47`.
    pub well_formed: bool,
}

/// Classifica o payload de um datagrama.
///
/// SPEC-PROBE-IP-017 — 1315 bytes (múltiplo de 188 menos um) é o caso real que
/// motiva o check: sai como `well_formed = false` e o datagrama não segue para
/// o demux, senão o TS inteiro dessincroniza.
pub fn ts_payload_shape(payload: &[u8]) -> TsPayloadShape {
    let well_formed = !payload.is_empty()
        && payload.len() % TS_PACKET_LEN == 0
        && payload[0] == TS_SYNC_BYTE;
    TsPayloadShape {
        ts_packets: payload.len() / TS_PACKET_LEN,
        well_formed,
    }
}

/// Removes RTP headers from UDP datagrams carrying MPEG-TS (PT=33).
///
/// SPEC-NET-003
pub struct RtpStripper {
    last_seq: Option<u16>,
    event_tx: Sender<RtpEvent>,
}

impl RtpStripper {
    /// Creates a new `RtpStripper`.
    ///
    /// SPEC-NET-003
    pub fn new(event_tx: Sender<RtpEvent>) -> Self {
        Self {
            last_seq: None,
            event_tx,
        }
    }

    /// Strips the RTP header from `data` and returns the MPEG-TS payload.
    ///
    /// - If the first byte is 0x47 (TS sync), the buffer is returned as-is.
    /// - If V=2 and PT=33 are detected the header (12 + 4×CC bytes) is removed.
    /// - Sequence number wrap-around (0xFFFF→0x0001) is treated as in-order.
    ///
    /// SPEC-NET-003
    pub fn strip(&mut self, data: Bytes) -> Bytes {
        // Pass-through: raw MPEG-TS (no RTP wrapper)
        if data.first() == Some(&TS_SYNC_BYTE) {
            return data;
        }

        let Some(header) = RtpHeader::parse(&data) else {
            return data;
        };
        if header.payload_type != RTP_PT_MPEGTS {
            return data;
        }

        self.check_sequence(header.sequence);

        if data.len() <= header.header_len {
            return Bytes::new();
        }
        data.slice(header.header_len..)
    }

    fn check_sequence(&mut self, seq: u16) {
        if let Some(last) = self.last_seq {
            let expected = if last == 0xFFFF {
                0x0001
            } else {
                last.wrapping_add(1)
            };
            if seq != expected {
                let _ = self
                    .event_tx
                    .try_send(RtpEvent::OutOfOrder { expected, got: seq });
            }
        }
        self.last_seq = Some(seq);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    fn make_rtp_packet(seq: u16, cc: u8, payload: &[u8]) -> Bytes {
        let byte0 = RTP_VERSION_2 | (cc & 0x0F);
        let byte1 = RTP_PT_MPEGTS; // marker=0, PT=33
        let mut pkt = vec![
            byte0,
            byte1,
            (seq >> 8) as u8,
            (seq & 0xFF) as u8,
            // timestamp (4 bytes)
            0,
            0,
            0,
            0,
            // SSRC (4 bytes)
            0,
            0,
            0,
            0,
        ];
        // CSRC entries (4 bytes each)
        for _ in 0..cc {
            pkt.extend_from_slice(&[0u8; 4]);
        }
        pkt.extend_from_slice(payload);
        Bytes::from(pkt)
    }

    /// SPEC-NET-003: RTP header válido PT=33, sem CSRC — remove 12 bytes
    #[test]
    fn spec_net_003_rtp_header_stripped() {
        let (tx, _rx) = bounded(8);
        let mut s = RtpStripper::new(tx);
        let payload = vec![0x47u8; 188];
        let pkt = make_rtp_packet(1, 0, &payload);
        let result = s.strip(pkt);
        assert_eq!(result.as_ref(), payload.as_slice());
    }

    /// SPEC-NET-003: CC=2 — remove 12 + 8 = 20 bytes
    #[test]
    fn spec_net_003_csrc_count_2() {
        let (tx, _rx) = bounded(8);
        let mut s = RtpStripper::new(tx);
        let payload = vec![0x47u8; 188];
        let pkt = make_rtp_packet(1, 2, &payload);
        assert_eq!(pkt.len(), 20 + 188);
        let result = s.strip(pkt);
        assert_eq!(result.as_ref(), payload.as_slice());
    }

    /// SPEC-NET-003: sync byte 0x47 no offset 0 — passa integralmente
    #[test]
    fn spec_net_003_passthrough_raw_ts() {
        let (tx, _rx) = bounded(8);
        let mut s = RtpStripper::new(tx);
        let raw = Bytes::from(vec![0x47u8; 188]);
        let result = s.strip(raw.clone());
        assert_eq!(result, raw);
    }

    /// SPEC-NET-003: wrap-around 0xFFFF→0x0001 não emite OutOfOrder
    #[test]
    fn spec_net_003_sequence_wrap_no_out_of_order() {
        let (tx, rx) = bounded(8);
        let mut s = RtpStripper::new(tx);
        let payload = vec![0u8; 188];
        s.strip(make_rtp_packet(0xFFFF, 0, &payload));
        s.strip(make_rtp_packet(0x0001, 0, &payload));
        assert!(
            rx.try_recv().is_err(),
            "wrap-around should not emit OutOfOrder"
        );
    }

    /// SPEC-NET-003: pulo 100→102 emite OutOfOrder { expected: 101, got: 102 }
    #[test]
    fn spec_net_003_sequence_out_of_order() {
        let (tx, rx) = bounded(8);
        let mut s = RtpStripper::new(tx);
        let payload = vec![0u8; 188];
        s.strip(make_rtp_packet(100, 0, &payload));
        s.strip(make_rtp_packet(102, 0, &payload));
        match rx.try_recv().expect("should emit OutOfOrder") {
            RtpEvent::OutOfOrder { expected, got } => {
                assert_eq!(expected, 101);
                assert_eq!(got, 102);
            }
        }
    }

    /// SPEC-PROBE-IP-014 — o header completo é exposto, inclusive os bits que
    /// o perfil ST 2022-2 proíbe.
    #[test]
    fn spec_probe_ip_014_parses_every_header_field() {
        let pkt = [
            0xB1, // V=2, P=1, X=1, CC=1
            0xA1, // M=1, PT=33
            0x12, 0x34, // seq
            0x00, 0x00, 0x10, 0x00, // timestamp
            0x1A, 0x2B, 0x3C, 0x4D, // SSRC
            0x00, 0x00, 0x00, 0x01, // CSRC[0]
            0xBE, 0xDE, 0x00, 0x01, // extensão: perfil + 1 palavra
            0x00, 0x00, 0x00, 0x00, // corpo da extensão
            0x47, // início do payload TS
        ];
        let h = RtpHeader::parse(&pkt).expect("header válido");
        assert_eq!(h.version, 2);
        assert!(h.padding);
        assert!(h.extension);
        assert!(h.marker);
        assert_eq!(h.csrc_count, 1);
        assert_eq!(h.payload_type, RTP_PT_MPEGTS);
        assert_eq!(h.sequence, 0x1234);
        assert_eq!(h.timestamp, 0x0000_1000);
        assert_eq!(h.ssrc, 0x1A2B_3C4D);
        assert_eq!(h.ssrc_hex(), "0x1A2B3C4D");
        // 12 fixos + 4 de CSRC + 4 de cabeçalho de extensão + 4 de corpo.
        assert_eq!(h.header_len, 24);
        assert_eq!(pkt[h.header_len], TS_SYNC_BYTE);
    }

    /// SPEC-PROBE-IP-014 — X=1 com header de extensão de 8 bytes: o payload
    /// começa depois da extensão, não depois dos 12 bytes fixos.
    #[test]
    fn spec_probe_ip_014_extension_header_is_skipped() {
        let mut pkt = vec![
            0x90, // V=2, X=1, CC=0
            RTP_PT_MPEGTS,
            0x00, 0x01, // seq
            0, 0, 0, 0, // timestamp
            0, 0, 0, 0, // SSRC
            0xBE, 0xDE, 0x00, 0x01, // extensão: 1 palavra de corpo
            0xAA, 0xBB, 0xCC, 0xDD,
        ];
        pkt.extend_from_slice(&[TS_SYNC_BYTE; 188]);

        let h = RtpHeader::parse(&pkt).expect("header válido");
        assert_eq!(h.header_len, 20, "12 fixos + 8 de extensão");

        let (tx, _rx) = bounded(8);
        let mut s = RtpStripper::new(tx);
        let payload = s.strip(Bytes::from(pkt));
        assert_eq!(payload.len(), 188);
        assert_eq!(payload[0], TS_SYNC_BYTE);
    }

    /// SPEC-PROBE-IP-014 — dado truncado devolve `None`; nunca `panic`, nunca
    /// leitura fora do buffer.
    #[test]
    fn spec_probe_ip_014_truncated_input_is_rejected() {
        assert!(RtpHeader::parse(&[]).is_none());
        assert!(RtpHeader::parse(&[0x80, 33, 0, 1]).is_none());
        // V=1 não é RTP.
        assert!(RtpHeader::parse(&[0x40; 16]).is_none());
        // X=1 anunciando 4 palavras de extensão que não existem.
        let truncated = [
            0x90, 33, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0xBE, 0xDE, 0x00, 0x04,
        ];
        assert!(RtpHeader::parse(&truncated).is_none());
    }

    /// SPEC-PROBE-IP-017 · SPEC-PROBE-IP-018 — payload múltiplo de 188 começando
    /// em 0x47 é conforme; 1315 bytes não é.
    #[test]
    fn spec_probe_ip_017_payload_shape_detects_bad_size() {
        let good = vec![TS_SYNC_BYTE; 7 * TS_PACKET_LEN];
        let shape = ts_payload_shape(&good);
        assert!(shape.well_formed);
        assert_eq!(shape.ts_packets, 7);

        let bad = vec![TS_SYNC_BYTE; 1315];
        assert!(!ts_payload_shape(&bad).well_formed);

        // Múltiplo de 188 mas sem sync byte: também não é TS.
        let mut no_sync = vec![0u8; TS_PACKET_LEN];
        no_sync[0] = 0x00;
        assert!(!ts_payload_shape(&no_sync).well_formed);

        assert!(!ts_payload_shape(&[]).well_formed);
    }
}
