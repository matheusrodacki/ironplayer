//! RTP header stripping and out-of-order detection.
//!
//! SPEC-NET-003

use bytes::Bytes;
use crossbeam_channel::Sender;

use crate::RtpEvent;

const TS_SYNC_BYTE: u8 = 0x47;
#[cfg(test)]
const RTP_VERSION_2: u8 = 0x80; // V=2 mask
const RTP_PT_MPEGTS: u8 = 33;

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

        // Need at least 12 bytes for the fixed RTP header
        if data.len() < 12 {
            return data;
        }

        let byte0 = data[0];
        let byte1 = data[1];

        // Check V=2 (top 2 bits of byte 0) and PT=33 (low 7 bits of byte 1, ignoring marker)
        let version = (byte0 >> 6) & 0x03;
        let payload_type = byte1 & 0x7F;

        if version != 2 || payload_type != RTP_PT_MPEGTS {
            return data;
        }

        // Sequence number is bytes 2–3 (big-endian)
        let seq = u16::from_be_bytes([data[2], data[3]]);
        self.check_sequence(seq);

        // CC field: low 4 bits of byte 0
        let cc = (byte0 & 0x0F) as usize;
        let header_len = 12 + 4 * cc;

        if data.len() <= header_len {
            return Bytes::new();
        }

        data.slice(header_len..)
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
}
