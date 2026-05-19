//! Teste de integração: fluxo PAT → `register_pmt_pid` → PMT roteada.
//!
//! Simula o fluxo completo de bootstrap do demultiplexador:
//! 1. Seção PAT parseada → PIDs de PMT extraídos.
//! 2. PIDs registrados no `TsDemuxer` via `register_pmt_pid`.
//! 3. Pacote TS com o PID da PMT roteado para o canal de seções.
//! 4. PMT parseada a partir dos dados de seção recebidos.
//!
//! T11 — SPEC-TABLE-001/002

use std::fs;
use std::path::PathBuf;

use crossbeam_channel::bounded;
use ts::tables::{Pat, Pmt};
use ts::TsDemuxer;

// ── helpers ──────────────────────────────────────────────────────────────────

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Constrói um pacote TS de 188 bytes (AFC=0b01, payload-only).
///
/// O primeiro byte do payload é o `pointer_field` (0x00), indicando que
/// a seção começa imediatamente. Os bytes seguintes contêm `section[..183]`.
fn build_ts_packet_with_section(pid: u16, section: &[u8]) -> [u8; 188] {
    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47;
    // PUSI = 1, PID[12:8]
    pkt[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
    pkt[2] = (pid & 0xFF) as u8;
    // AFC = 0b01 (payload only), CC = 0
    pkt[3] = 0b0001_0000;
    // pointer_field = 0: a seção começa no próximo byte
    pkt[4] = 0x00;
    // copiar até 183 bytes da seção no payload
    let copy_len = section.len().min(183);
    pkt[5..5 + copy_len].copy_from_slice(&section[..copy_len]);
    pkt
}

// ── teste T11 ─────────────────────────────────────────────────────────────────

/// Integração PAT → `register_pmt_pid` → PMT roteada e parseada.
///
/// Fluxo testado:
/// 1. `pat_section.bin` lida e parseada via `Pat::from_section_body`.
/// 2. PIDs de PMT extraídos e registrados no `TsDemuxer`.
/// 3. Pacote TS com PID=0x0100 (PMT) processado pelo demuxer.
/// 4. `SectionData` recebido no canal de seções e PMT parseada.
///
/// T11 — SPEC-TABLE-001/002
#[test]
fn spec_tables_t11_pat_pmt_register_pmt_pid_flow() {
    let dir = fixtures_dir();

    // ── 1. Parsear PAT ────────────────────────────────────────────────────────
    let pat_bytes = fs::read(dir.join("pat_section.bin"))
        .expect("pat_section.bin deve estar presente em tests/fixtures/");

    // section_body = sem os 3 bytes de cabeçalho PSI e sem os 4 bytes de CRC-32
    let pat_body = &pat_bytes[3..pat_bytes.len() - 4];
    let pat = Pat::from_section_body(pat_body)
        .expect("PAT deve parsear sem erro a partir da fixture");

    assert_eq!(pat.transport_stream_id, 1, "transport_stream_id deve ser 1");
    assert_eq!(pat.version, 1, "versão deve ser 1");
    assert!(pat.current_next, "current_next deve ser true");

    let pmt_pids: Vec<u16> = pat.pmt_pids().collect();
    assert_eq!(pmt_pids, vec![0x0100], "deve haver exatamente um PMT PID: 0x0100");

    // NIT PID identificado corretamente
    assert_eq!(pat.nit_pid(), Some(0x0010), "NIT PID deve ser 0x0010");

    // ── 2. Criar TsDemuxer e registrar PMT PIDs ───────────────────────────────
    let (sec_tx, sec_rx) = bounded(64);
    let (pes_tx, _pes_rx) = bounded(64);
    let (evt_tx, _evt_rx) = bounded(64);
    let mut demuxer = TsDemuxer::new(sec_tx, pes_tx, evt_tx);

    for pid in pat.pmt_pids() {
        demuxer.register_pmt_pid(pid);
    }

    // ── 3. Construir pacote TS com PID=0x0100 contendo a PMT ──────────────────
    let pmt_bytes = fs::read(dir.join("pmt_h264_aac.bin"))
        .expect("pmt_h264_aac.bin deve estar presente em tests/fixtures/");

    let pkt = build_ts_packet_with_section(0x0100, &pmt_bytes);
    demuxer.process_chunk(&pkt);

    // ── 4. Verificar que SectionData chegou no canal de seções ────────────────
    let sec = sec_rx
        .try_recv()
        .expect("SectionData deve estar disponível no canal de seções após process_chunk");

    assert_eq!(sec.pid, 0x0100, "SectionData deve ter pid=0x0100");
    assert!(sec.pusi, "PUSI deve estar setado no SectionData");

    // ── 5. Parsear PMT a partir do payload recebido ───────────────────────────
    // payload[0] = pointer_field = 0 → seção começa em payload[1]
    let payload = sec.payload.as_ref();
    assert!(
        !payload.is_empty(),
        "payload do SectionData não deve estar vazio"
    );
    let pointer_field = payload[0] as usize;
    let section_start = 1 + pointer_field;

    // seção: [table_id(1)] [ssi|res|section_length(2)] [body] [CRC(4)]
    let section_slice = &payload[section_start..];
    assert!(
        section_slice.len() >= 3,
        "seção deve ter pelo menos 3 bytes de cabeçalho PSI"
    );

    let declared_len =
        (((section_slice[1] & 0x0F) as usize) << 8) | section_slice[2] as usize;
    let body_end = 3 + declared_len - 4; // −4 para excluir CRC-32

    assert!(
        section_slice.len() >= 3 + declared_len,
        "payload deve conter a seção completa (declarado: {} bytes)",
        declared_len
    );

    let pmt_body = &section_slice[3..body_end];
    let pmt = Pmt::from_section_body(pmt_body)
        .expect("PMT deve parsear sem erro a partir do SectionData");

    // ── 6. Verificar campos da PMT ────────────────────────────────────────────
    assert_eq!(pmt.program_number, 1, "program_number deve ser 1");
    assert_eq!(pmt.pcr_pid, 0x0110, "PCR PID deve ser 0x0110");
    assert_eq!(pmt.streams.len(), 2, "PMT deve conter 2 streams elementares");

    let video = &pmt.streams[0];
    assert_eq!(video.stream_type, 0x1B, "stream 0 deve ser H.264 (0x1B)");
    assert_eq!(video.elementary_pid, 0x0110, "PID de vídeo deve ser 0x0110");
    assert_eq!(video.label(), "H.264 / AVC Video");

    let audio = &pmt.streams[1];
    assert_eq!(audio.stream_type, 0x0F, "stream 1 deve ser AAC ADTS (0x0F)");
    assert_eq!(audio.elementary_pid, 0x0120, "PID de áudio deve ser 0x0120");
    assert_eq!(audio.label(), "AAC Audio (ADTS)");
}
