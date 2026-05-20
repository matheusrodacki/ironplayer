//! Remontagem de PES (Packetized Elementary Stream) a partir de fragmentos TS.
//!
//! `PesAssembler` acumula payloads TS guiado pelo bit PUSI e emite
//! `PesPacket` completos (com PTS/DTS decodificados) via canal bounded.
//!
//! SPEC-AV-001 · SPEC-AV-001a

use std::collections::HashMap;

use bytes::Bytes;
use crossbeam_channel::Sender;
use tracing::warn;
use ts::Pid;

use crate::codec::MediaCodec;
use crate::error::AvError;

// ── PesPacket ─────────────────────────────────────────────────────────────────

/// PES packet completamente remontado, pronto para decodificação.
///
/// Produzido pelo `PesAssembler` e consumido pelo `FfmpegDecoder`.
///
/// SPEC-AV-001
#[derive(Debug, Clone)]
pub struct PesPacket {
    /// PID do stream elementar ao qual este PES pertence.
    pub pid: Pid,

    /// Codec identificado via `stream_type` da PMT.
    pub codec: MediaCodec,

    /// Presentation Timestamp em unidades de 90 kHz (33 bits; `None` se ausente).
    ///
    /// SPEC-AV-001a
    pub pts: Option<u64>,

    /// Decode Timestamp em unidades de 90 kHz (33 bits; `None` se ausente).
    ///
    /// SPEC-AV-001a
    pub dts: Option<u64>,

    /// Payload completo do PES (bytes elementares do codec).
    pub payload: Bytes,
}

impl PesPacket {
    /// Cria um novo `PesPacket`.
    ///
    /// SPEC-AV-001
    pub fn new(
        pid: Pid,
        codec: MediaCodec,
        pts: Option<u64>,
        dts: Option<u64>,
        payload: Bytes,
    ) -> Self {
        Self {
            pid,
            codec,
            pts,
            dts,
            payload,
        }
    }

    /// Retorna a diferença de PTS em ticks de 90 kHz entre este e o próximo
    /// `PesPacket` consecutivo do mesmo stream, com wrap-around em 33 bits.
    ///
    /// Retorna `None` se qualquer um dos dois não tiver PTS.
    ///
    /// SPEC-AV-001a
    pub fn pts_duration(&self, next: &PesPacket) -> Option<u64> {
        let a = self.pts?;
        let b = next.pts?;
        // PTS é 33 bits: wrap-around em 2^33 = 8_589_934_592
        Some(b.wrapping_sub(a) & 0x1_FFFF_FFFF)
    }
}

// ── Buffer interno ────────────────────────────────────────────────────────────

/// Buffer de acumulação para um único PID.
struct PesBuffer {
    codec: MediaCodec,
    pts: Option<u64>,
    dts: Option<u64>,
    /// Payload ES acumulado (sem o cabeçalho PES).
    data: Vec<u8>,
    /// Tamanho esperado do payload ES, derivado de `PES_packet_length`.
    /// `None` significa unbounded (comum em vídeo — emitir no próximo PUSI).
    expected_es_size: Option<usize>,
}

// ── PesAssembler ──────────────────────────────────────────────────────────────

/// Remonta PES packets fragmentados a partir de payloads de pacotes TS.
///
/// Cada chamada a [`PesAssembler::push`] processa um payload de pacote TS.
/// Quando o flag PUSI está setado, o buffer anterior (se houver) é emitido
/// e um novo PES packet começa a ser acumulado.  Quando `PES_packet_length`
/// é não-zero, a emissão acontece assim que todos os bytes ES chegam.
///
/// # Canais bounded
///
/// O canal de saída é bounded. Se estiver cheio, o `PesPacket` é descartado
/// e um aviso é emitido via `tracing`.
///
/// SPEC-AV-001
pub struct PesAssembler {
    /// Buffers de acumulação por PID.
    buffers: HashMap<Pid, PesBuffer>,
    /// Mapeamento de PID → codec, registrado via [`PesAssembler::register_pid`].
    pid_codecs: HashMap<Pid, MediaCodec>,
    /// Canal de saída para `PesPacket` completos.
    tx: Sender<PesPacket>,
}

impl PesAssembler {
    /// Cria um novo `PesAssembler` com o canal de saída fornecido.
    ///
    /// O canal **deve** ser bounded (criado com `crossbeam_channel::bounded`).
    ///
    /// SPEC-AV-001
    pub fn new(tx: Sender<PesPacket>) -> Self {
        Self {
            buffers: HashMap::new(),
            pid_codecs: HashMap::new(),
            tx,
        }
    }

    /// Registra o codec para um PID de stream elementar.
    ///
    /// Chamado ao parsear a PMT e identificar o `stream_type` de cada PID A/V.
    ///
    /// SPEC-AV-001
    pub fn register_pid(&mut self, pid: Pid, codec: MediaCodec) {
        self.pid_codecs.insert(pid, codec);
    }

    /// Processa o payload de um pacote TS.
    ///
    /// - `pusi = true`: início de novo PES unit. O buffer anterior (se houver)
    ///   é emitido e o novo cabeçalho PES é parseado.
    /// - `pusi = false`: continuação; os bytes são appendados ao buffer do PID.
    ///
    /// SPEC-AV-001
    pub fn push(&mut self, pid: Pid, pusi: bool, data: Bytes) {
        if pusi {
            // Emite o PES anterior incompleto (se houver) antes de iniciar o novo.
            if let Some(buf) = self.buffers.remove(&pid) {
                self.emit(pid, buf);
            }

            let Some(&codec) = self.pid_codecs.get(&pid) else {
                // PID não registrado — ignorar silenciosamente.
                return;
            };

            match parse_pes_header(&data, codec) {
                Ok(buf) => {
                    // Se pes_length != 0 e já temos todos os bytes, emitir imediatamente.
                    let complete = matches!(buf.expected_es_size, Some(n) if buf.data.len() >= n);
                    if complete {
                        self.emit(pid, buf);
                    } else {
                        self.buffers.insert(pid, buf);
                    }
                }
                Err(e) => {
                    warn!("PID 0x{:04X}: erro ao parsear header PES: {}", pid, e);
                }
            }
        } else {
            // Appenda ao buffer existente.
            if let Some(buf) = self.buffers.get_mut(&pid) {
                buf.data.extend_from_slice(&data);

                let complete = matches!(buf.expected_es_size, Some(n) if buf.data.len() >= n);
                if complete {
                    if let Some(buf) = self.buffers.remove(&pid) {
                        self.emit(pid, buf);
                    }
                }
            }
            // Se não há buffer para este PID, o pacote chegou sem PUSI prévio
            // (stream entrou no meio) — descartar silenciosamente.
        }
    }

    /// Emite um `PesPacket` completo pelo canal de saída.
    ///
    /// Se o canal estiver cheio, o packet é descartado e um aviso é emitido.
    fn emit(&self, pid: Pid, buf: PesBuffer) {
        let payload = Bytes::from(buf.data);
        let packet = PesPacket::new(pid, buf.codec, buf.pts, buf.dts, payload);
        if self.tx.try_send(packet).is_err() {
            warn!("PID 0x{:04X}: canal cheio; PES packet descartado", pid);
        }
    }
}

// ── Parsing do cabeçalho PES ──────────────────────────────────────────────────

/// Parseia o cabeçalho PES de um payload PUSI e retorna um `PesBuffer`
/// inicializado com PTS/DTS extraídos e o payload ES sem o cabeçalho.
///
/// Estrutura ISO 13818-1:
/// ```text
/// bytes[0..3]  : 0x00 0x00 0x01 (start code prefix)
/// byte[3]      : stream_id
/// bytes[4..6]  : PES_packet_length (0 = unbounded)
/// byte[6]      : flags1
/// byte[7]      : flags2  (bits 7:6 = PTS_DTS_flags)
/// byte[8]      : PES_header_data_length
/// bytes[9..]   : optional fields (PTS, DTS, …)
/// bytes[9+hdl..]: ES payload
/// ```
///
/// SPEC-AV-001
fn parse_pes_header(data: &Bytes, codec: MediaCodec) -> Result<PesBuffer, AvError> {
    // Mínimo 6 bytes: start_code (3) + stream_id (1) + length (2).
    if data.len() < 6 {
        return Err(AvError::InvalidPes {
            reason: "payload PES truncado (< 6 bytes)",
        });
    }

    // Validar magic bytes 0x00 0x00 0x01.
    if data[0] != 0x00 || data[1] != 0x00 || data[2] != 0x01 {
        return Err(AvError::InvalidPes {
            reason: "magic bytes PES inválidos (esperado 0x000001)",
        });
    }

    let stream_id = data[3];
    let pes_packet_length = u16::from_be_bytes([data[4], data[5]]);

    // Stream IDs sem cabeçalho opcional (ISO 13818-1 tabela 2-20).
    let has_optional_header = !matches!(
        stream_id,
        0xBC | 0xBE | 0xBF | 0xF0 | 0xF1 | 0xFF | 0xF2 | 0xF8
    );

    let (pts, dts, es_start) = if has_optional_header {
        // Requer pelo menos 9 bytes: 6 fixed + flags1 + flags2 + hdl.
        if data.len() < 9 {
            return Err(AvError::InvalidPes {
                reason: "cabeçalho opcional PES truncado (< 9 bytes)",
            });
        }
        let flags2 = data[7];
        let header_data_length = data[8] as usize;
        let pts_dts_flags = flags2 >> 6;

        let (pts, dts) = decode_pts_dts(data, pts_dts_flags)?;
        // ES inicia após os 9 bytes fixos + campos opcionais.
        let es_start = 9 + header_data_length;
        (pts, dts, es_start)
    } else {
        // Sem cabeçalho opcional: ES começa logo após os 6 bytes fixos.
        (None, None, 6usize)
    };

    if es_start > data.len() {
        return Err(AvError::InvalidPes {
            reason: "es_start além do fim do payload PUSI",
        });
    }

    let es_payload = data[es_start..].to_vec();

    // `pes_packet_length` cobre os bytes a partir do byte[6].
    // ES size = pes_packet_length - (es_start - 6).
    let expected_es_size = if pes_packet_length == 0 {
        None // unbounded — emitir no próximo PUSI
    } else {
        let header_overhead = es_start - 6;
        Some((pes_packet_length as usize).saturating_sub(header_overhead))
    };

    Ok(PesBuffer {
        codec,
        pts,
        dts,
        data: es_payload,
        expected_es_size,
    })
}

/// Decodifica PTS e/ou DTS a partir dos bytes do cabeçalho PES.
///
/// `pts_dts_flags` são os 2 bits mais significativos do segundo byte de flags:
/// - `0b00` → nem PTS nem DTS
/// - `0b10` → apenas PTS (5 bytes em `data[9..14]`)
/// - `0b11` → PTS e DTS (5 bytes cada, em `data[9..14]` e `data[14..19]`)
///
/// SPEC-AV-001a
fn decode_pts_dts(data: &[u8], pts_dts_flags: u8) -> Result<(Option<u64>, Option<u64>), AvError> {
    match pts_dts_flags {
        0b00 => Ok((None, None)),
        0b10 => {
            if data.len() < 14 {
                return Err(AvError::InvalidPes {
                    reason: "PTS truncado no cabeçalho PES",
                });
            }
            let pts = decode_timestamp(&data[9..14])?;
            Ok((Some(pts), None))
        }
        0b11 => {
            if data.len() < 19 {
                return Err(AvError::InvalidPes {
                    reason: "PTS/DTS truncado no cabeçalho PES",
                });
            }
            let pts = decode_timestamp(&data[9..14])?;
            let dts = decode_timestamp(&data[14..19])?;
            Ok((Some(pts), Some(dts)))
        }
        _ => Err(AvError::InvalidPes {
            reason: "pts_dts_flags 0b01 é reservado pelo padrão ISO 13818-1",
        }),
    }
}

/// Decodifica um timestamp de 33 bits (PTS ou DTS) a partir de 5 bytes.
///
/// Formato ISO 13818-1:
/// ```text
/// byte[0]: [marker(4)] [ts[32:30]] [marker_bit]
/// byte[1]: [ts[29:22]]
/// byte[2]: [ts[21:15]] [marker_bit]
/// byte[3]: [ts[14:7]]
/// byte[4]: [ts[6:0]]  [marker_bit]
/// ```
///
/// SPEC-AV-001a
fn decode_timestamp(b: &[u8]) -> Result<u64, AvError> {
    if b.len() < 5 {
        return Err(AvError::InvalidPes {
            reason: "timestamp truncado (< 5 bytes)",
        });
    }
    let val = ((b[0] & 0x0E) as u64) << 29
        | (b[1] as u64) << 22
        | ((b[2] & 0xFE) as u64) << 14
        | (b[3] as u64) << 7
        | ((b[4] >> 1) as u64);
    Ok(val)
}

// ── Testes ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::VideoCodec;
    use crossbeam_channel::bounded;

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Codifica um timestamp de 33 bits em 5 bytes PES (formato ISO 13818-1).
    ///
    /// `marker` indica os 4 bits de marcação superiores do byte[0]:
    /// - `0b0010` para PTS-only
    /// - `0b0011` para PTS quando pts+dts presentes
    /// - `0b0001` para DTS quando pts+dts presentes
    fn encode_timestamp(ts: u64, marker: u8) -> [u8; 5] {
        let ts = ts & 0x1_FFFF_FFFF; // garantir 33 bits
        [
            ((marker & 0x0F) << 4) | (((ts >> 30) & 0x07) as u8) << 1 | 0x01,
            ((ts >> 22) & 0xFF) as u8,
            (((ts >> 15) & 0x7F) as u8) << 1 | 0x01,
            ((ts >> 7) & 0xFF) as u8,
            ((ts & 0x7F) as u8) << 1 | 0x01,
        ]
    }

    /// Constrói um PES packet H.264 mínimo com PTS e payload ES fornecido.
    ///
    /// Estrutura: start_code(3) + stream_id(1) + length(2) + flags(2) + hdl(1)
    ///            + PTS(5) + es_payload
    fn build_pes_packet(pts: u64, es_payload: &[u8]) -> Vec<u8> {
        // Cabeçalho opcional: flags1(1) + flags2(1) + hdl(1) + PTS(5) = 8 bytes
        // pes_packet_length cobre bytes após byte[6]: 8 + es_payload.len()
        let opt_header_len: u16 = 8; // 3 bytes de flags/hdl + 5 bytes PTS
        let pes_packet_length: u16 = opt_header_len + es_payload.len() as u16;

        let pts_bytes = encode_timestamp(pts, 0b0010);

        let mut buf = Vec::new();
        // Start code prefix + stream_id (0xE0 = vídeo)
        buf.extend_from_slice(&[0x00, 0x00, 0x01, 0xE0]);
        // PES_packet_length
        buf.extend_from_slice(&pes_packet_length.to_be_bytes());
        // flags1: '10' marker bits
        buf.push(0x80);
        // flags2: PTS_DTS_flags = 0b10 (apenas PTS)
        buf.push(0x80);
        // PES_header_data_length = 5 (apenas PTS)
        buf.push(0x05);
        // PTS (5 bytes)
        buf.extend_from_slice(&pts_bytes);
        // ES payload
        buf.extend_from_slice(es_payload);
        buf
    }

    /// Constrói um PES packet com PTS e DTS.
    fn build_pes_packet_with_dts(pts: u64, dts: u64, es_payload: &[u8]) -> Vec<u8> {
        // Cabeçalho opcional: flags1 + flags2 + hdl + PTS(5) + DTS(5) = 13 bytes
        let opt_header_len: u16 = 13;
        let pes_packet_length: u16 = opt_header_len + es_payload.len() as u16;

        let pts_bytes = encode_timestamp(pts, 0b0011);
        let dts_bytes = encode_timestamp(dts, 0b0001);

        let mut buf = Vec::new();
        buf.extend_from_slice(&[0x00, 0x00, 0x01, 0xE0]);
        buf.extend_from_slice(&pes_packet_length.to_be_bytes());
        buf.push(0x80); // flags1
        buf.push(0xC0); // flags2: PTS_DTS_flags = 0b11 (PTS+DTS)
        buf.push(0x0A); // PES_header_data_length = 10 (PTS + DTS)
        buf.extend_from_slice(&pts_bytes);
        buf.extend_from_slice(&dts_bytes);
        buf.extend_from_slice(es_payload);
        buf
    }

    // ── Testes de decodificação de timestamp ─────────────────────────────────

    /// Timestamp zero deve ser decodificado como zero.
    ///
    /// SPEC-AV-001a
    #[test]
    fn spec_av_001_timestamp_decode_zero() {
        let encoded = encode_timestamp(0, 0b0010);
        let ts = decode_timestamp(&encoded).unwrap();
        assert_eq!(ts, 0);
    }

    /// Timestamp máximo de 33 bits (2^33 - 1) deve ser decodificado corretamente.
    ///
    /// SPEC-AV-001a
    #[test]
    fn spec_av_001_timestamp_decode_max_33bit() {
        let max = 0x1_FFFF_FFFF_u64;
        let encoded = encode_timestamp(max, 0b0010);
        let ts = decode_timestamp(&encoded).unwrap();
        assert_eq!(ts, max);
    }

    /// Timestamp de valor arbitrário (90_000 ticks = 1 segundo em 90 kHz).
    ///
    /// SPEC-AV-001a
    #[test]
    fn spec_av_001_timestamp_decode_one_second() {
        let one_sec = 90_000_u64;
        let encoded = encode_timestamp(one_sec, 0b0010);
        let ts = decode_timestamp(&encoded).unwrap();
        assert_eq!(ts, one_sec);
    }

    // ── Testes de pts_duration ────────────────────────────────────────────────

    /// `pts_duration` calcula a diferença correta entre dois PTS consecutivos.
    ///
    /// SPEC-AV-001a
    #[test]
    fn spec_av_001_pts_duration_simple() {
        let codec = MediaCodec::Video(VideoCodec::H264);
        let p1 = PesPacket::new(0x100, codec, Some(90_000), None, Bytes::new());
        let p2 = PesPacket::new(0x100, codec, Some(93_600), None, Bytes::new()); // 1/25 fps
        assert_eq!(p1.pts_duration(&p2), Some(3_600));
    }

    /// `pts_duration` retorna `None` se qualquer PTS for `None`.
    ///
    /// SPEC-AV-001a
    #[test]
    fn spec_av_001_pts_duration_none_when_missing() {
        let codec = MediaCodec::Video(VideoCodec::H264);
        let p1 = PesPacket::new(0x100, codec, None, None, Bytes::new());
        let p2 = PesPacket::new(0x100, codec, Some(90_000), None, Bytes::new());
        assert_eq!(p1.pts_duration(&p2), None);
        assert_eq!(p2.pts_duration(&p1), None);
    }

    /// `pts_duration` lida com wrap-around de 33 bits sem underflow.
    ///
    /// SPEC-AV-001a
    #[test]
    fn spec_av_001_pts_duration_wraparound() {
        let codec = MediaCodec::Video(VideoCodec::H264);
        let max_33 = 0x1_FFFF_FFFF_u64;
        let p1 = PesPacket::new(0x100, codec, Some(max_33 - 100), None, Bytes::new());
        let p2 = PesPacket::new(0x100, codec, Some(100), None, Bytes::new());
        // Diferença deve ser 201 com wrap-around
        assert_eq!(p1.pts_duration(&p2), Some(201));
    }

    // ── Testes do PesAssembler ────────────────────────────────────────────────

    /// PES H.264 fragmentado em 4 pacotes TS é remontado corretamente.
    ///
    /// SPEC-AV-001
    #[test]
    fn spec_av_001_reassemble_h264_four_ts_packets() {
        let (tx, rx) = bounded::<PesPacket>(16);
        let mut asm = PesAssembler::new(tx);
        let pid: Pid = 0x0100;
        asm.register_pid(pid, MediaCodec::Video(VideoCodec::H264));

        // ES payload de 300 bytes distribuídos em 4 fragmentos TS.
        let es_data: Vec<u8> = (0u8..=255).chain(0u8..44).collect();
        assert_eq!(es_data.len(), 300);

        let pes = build_pes_packet(90_000, &es_data);

        // Fragmentar em 4 partes com tamanhos variados.
        let (part1, rest) = pes.split_at(60);
        let (part2, rest) = rest.split_at(80);
        let (part3, part4) = rest.split_at(80);

        // Primeiro pacote: PUSI=true.
        asm.push(pid, true, Bytes::copy_from_slice(part1));
        // Pacotes de continuação: PUSI=false.
        asm.push(pid, false, Bytes::copy_from_slice(part2));
        asm.push(pid, false, Bytes::copy_from_slice(part3));
        asm.push(pid, false, Bytes::copy_from_slice(part4));

        // O canal deve conter exatamente 1 PesPacket (emitido por pes_length).
        let packets: Vec<PesPacket> = rx.try_iter().collect();
        assert_eq!(packets.len(), 1, "deve emitir exatamente um PesPacket");

        let pkt = &packets[0];
        assert_eq!(pkt.pid, pid);
        assert_eq!(pkt.pts, Some(90_000));
        assert_eq!(pkt.dts, None);
        assert_eq!(
            pkt.payload.as_ref(),
            es_data.as_slice(),
            "payload ES remontado deve ser idêntico ao original"
        );
        assert_eq!(pkt.codec, MediaCodec::Video(VideoCodec::H264));
    }

    /// PES unbounded (pes_length=0) é emitido quando o próximo PUSI chega.
    ///
    /// SPEC-AV-001
    #[test]
    fn spec_av_001_unbounded_pes_emitted_on_next_pusi() {
        let (tx, rx) = bounded::<PesPacket>(16);
        let mut asm = PesAssembler::new(tx);
        let pid: Pid = 0x0200;
        asm.register_pid(pid, MediaCodec::Video(VideoCodec::H264));

        let es_data_1 = vec![0xAAu8; 50];
        let es_data_2 = vec![0xBBu8; 30];

        // PES unbounded (pes_length = 0).
        let mut pes1 = vec![0x00u8, 0x00, 0x01, 0xE0, 0x00, 0x00]; // length=0
        pes1.push(0x80); // flags1
        pes1.push(0x00); // flags2: no PTS/DTS
        pes1.push(0x00); // header_data_length = 0
        pes1.extend_from_slice(&es_data_1);

        let mut pes2 = vec![0x00u8, 0x00, 0x01, 0xE0, 0x00, 0x00];
        pes2.push(0x80);
        pes2.push(0x00);
        pes2.push(0x00);
        pes2.extend_from_slice(&es_data_2);

        asm.push(pid, true, Bytes::from(pes1));
        // Neste ponto não deve ter emitido ainda (unbounded).
        assert!(
            rx.try_recv().is_err(),
            "ainda não deve emitir antes do próximo PUSI"
        );

        // Segundo PUSI força emissão do primeiro.
        asm.push(pid, true, Bytes::from(pes2));

        let pkts: Vec<PesPacket> = rx.try_iter().collect();
        assert_eq!(pkts.len(), 1, "deve emitir o PES anterior ao segundo PUSI");
        assert_eq!(pkts[0].payload.as_ref(), es_data_1.as_slice());
    }

    /// Pacote sem PUSI prévio é descartado silenciosamente.
    ///
    /// SPEC-AV-001
    #[test]
    fn spec_av_001_data_without_prior_pusi_discarded() {
        let (tx, rx) = bounded::<PesPacket>(16);
        let mut asm = PesAssembler::new(tx);
        let pid: Pid = 0x0300;
        asm.register_pid(pid, MediaCodec::Video(VideoCodec::H264));

        // Primeiro pacote sem PUSI (stream entrou no meio).
        asm.push(pid, false, Bytes::from_static(b"\xDE\xAD\xBE\xEF"));

        assert!(
            rx.try_recv().is_err(),
            "pacote sem PUSI prévio não deve ser emitido"
        );
    }

    /// PID desconhecido (não registrado) é ignorado.
    ///
    /// SPEC-AV-001
    #[test]
    fn spec_av_001_unknown_pid_ignored() {
        let (tx, rx) = bounded::<PesPacket>(16);
        let mut asm = PesAssembler::new(tx);
        // Não registrar nenhum PID.

        let es_data = vec![0xCCu8; 10];
        let pes = build_pes_packet(1000, &es_data);
        asm.push(0x0400, true, Bytes::from(pes));

        assert!(rx.try_recv().is_err(), "PID desconhecido deve ser ignorado");
    }

    /// PES com PTS e DTS tem ambos os timestamps decodificados.
    ///
    /// SPEC-AV-001a
    #[test]
    fn spec_av_001_pts_and_dts_decoded() {
        let (tx, rx) = bounded::<PesPacket>(16);
        let mut asm = PesAssembler::new(tx);
        let pid: Pid = 0x0150;
        asm.register_pid(pid, MediaCodec::Video(VideoCodec::H264));

        let pts_val = 900_000_u64;
        let dts_val = 897_000_u64; // DTS ligeiramente antes do PTS
        let es_data = vec![0x11u8; 10];

        let pes = build_pes_packet_with_dts(pts_val, dts_val, &es_data);

        // Enviar o PES (bounded — será emitido automaticamente após pes_length).
        asm.push(pid, true, Bytes::from(pes));

        // Forçar emissão do PES anterior com um segundo PUSI (unbounded dummy).
        let dummy_header = vec![0x00u8, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x00, 0x00];
        asm.push(pid, true, Bytes::from(dummy_header));

        let pkts: Vec<PesPacket> = rx.try_iter().collect();
        assert!(!pkts.is_empty(), "deve ter ao menos o primeiro PesPacket");
        let first = &pkts[0];
        assert_eq!(first.pts, Some(pts_val));
        assert_eq!(first.dts, Some(dts_val));
        assert_eq!(first.payload.as_ref(), es_data.as_slice());
    }

    /// Magic bytes PES inválidos geram aviso mas não causam panic.
    ///
    /// SPEC-AV-001
    #[test]
    fn spec_av_001_invalid_magic_bytes_no_panic() {
        let (tx, rx) = bounded::<PesPacket>(16);
        let mut asm = PesAssembler::new(tx);
        let pid: Pid = 0x0500;
        asm.register_pid(pid, MediaCodec::Video(VideoCodec::H264));

        // Dados com magic bytes errados.
        asm.push(
            pid,
            true,
            Bytes::from_static(b"\xFF\xFF\xFF\xE0\x00\x10garbage"),
        );

        assert!(rx.try_recv().is_err(), "PES inválido não deve emitir nada");
    }

    /// `pts_duration` com PTS de 25 fps em 90 kHz deve retornar 3600 ticks.
    ///
    /// SPEC-AV-001a
    #[test]
    fn spec_av_001a_pts_duration_25fps_in_90khz() {
        let codec = MediaCodec::Video(VideoCodec::H264);
        // 90_000 / 25 = 3_600 ticks por frame
        let p1 = PesPacket::new(0x100, codec, Some(0), None, Bytes::new());
        let p2 = PesPacket::new(0x100, codec, Some(3_600), None, Bytes::new());
        assert_eq!(p1.pts_duration(&p2), Some(3_600));
    }
}
