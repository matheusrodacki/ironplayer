//! Gerador de fixtures de teste sintéticas para o crate `ts`.
//!
//! Cada `#[test]` cria (ou sobrescreve) um arquivo binário em
//! `crates/ts/tests/fixtures/` com dados MPEG-TS sintéticos bem-formados.
//! Os arquivos gerados são usados pelos demais testes de integração.
//!
//! T08

use std::fs;
use std::path::PathBuf;

use ts::{crc32_mpeg2, verify_crc32_mpeg2};

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Retorna o caminho para `crates/ts/tests/fixtures/`.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Constrói um pacote TS de 188 bytes com AFC=0b01 (payload-only).
///
/// Os primeiros `payload.len().min(184)` bytes são copiados para bytes 4..188.
/// O restante é preenchido com `0xFF`.
fn build_ts_packet(pid: u16, pusi: bool, cc: u8, payload: &[u8]) -> [u8; 188] {
    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47;
    pkt[1] = (if pusi { 0x40u8 } else { 0x00 }) | ((pid >> 8) as u8 & 0x1F);
    pkt[2] = (pid & 0xFF) as u8;
    // AFC=0b01 (payload only)
    pkt[3] = (0b01u8 << 4) | (cc & 0x0F);
    let pl_len = payload.len().min(184);
    pkt[4..4 + pl_len].copy_from_slice(&payload[..pl_len]);
    pkt
}

// ── Geradores ────────────────────────────────────────────────────────────────

/// T08: Gera `ts_packets_cc_error.bin`.
///
/// 5 pacotes no PID 0x0100. Sequência de CC: 0, 1, 2, **4**, 5.
/// O 4º pacote (CC=4) provoca `TsEvent::CcError { expected: 3, got: 4 }`.
#[test]
fn spec_t08_generate_cc_error_fixture() {
    let fixtures = fixtures_dir();
    fs::create_dir_all(&fixtures).expect("criar diretório tests/fixtures");

    let pid = 0x0100u16;
    let payload = [0xABu8; 184];
    // CC=4 pula o 3 → CcError esperado no 4º pacote.
    let cc_seq = [0u8, 1, 2, 4, 5];

    let mut data = Vec::with_capacity(5 * 188);
    for &cc in &cc_seq {
        data.extend_from_slice(&build_ts_packet(pid, false, cc, &payload));
    }

    let path = fixtures.join("ts_packets_cc_error.bin");
    fs::write(&path, &data).expect("escrever ts_packets_cc_error.bin");

    // Verificações de sanidade
    assert_eq!(
        data.len(),
        5 * 188,
        "fixture deve conter exatamente 5 pacotes TS de 188 bytes"
    );
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        (5 * 188) as u64,
        "arquivo gravado deve ter o tamanho correto"
    );
}

/// T08: Gera `ts_fragmented_section.bin`.
///
/// Seção PSI sintética de 400 bytes (`table_id=0x42`, `section_length=397`)
/// com CRC-32 MPEG-2 válido, distribuída em **3 pacotes** no PID 0x0200:
///
/// - Pacote 1 (PUSI=1, CC=0): `pointer_field=0x00` + seção\[0..183\]
/// - Pacote 2 (PUSI=0, CC=1): seção\[183..367\]
/// - Pacote 3 (PUSI=0, CC=2): seção\[367..400\] + stuffing 0xFF
#[test]
fn spec_t08_generate_fragmented_section_fixture() {
    let fixtures = fixtures_dir();
    fs::create_dir_all(&fixtures).expect("criar diretório tests/fixtures");

    let pid = 0x0200u16;

    // Construir seção de 400 bytes: header(3) + body(393) + CRC(4).
    let section_length: u16 = 397; // body(393) + CRC(4)
    let total_len = 3 + section_length as usize; // = 400
    let mut section = vec![0u8; total_len];

    // Header PSI
    section[0] = 0x42; // table_id (DVB: Network Information Table - actual)
    // Byte 1: si=1(1), private=0(0), reserved=11, section_length[11:8]
    section[1] = 0xB0 | (((section_length >> 8) & 0x0F) as u8);
    section[2] = (section_length & 0xFF) as u8;

    // Body: padrão incremental × 3 para distinguir visualmente
    for i in 3..(total_len - 4) {
        section[i] = (i as u8).wrapping_mul(3);
    }

    // CRC-32 MPEG-2 sobre bytes 0..(total_len-4), armazenado big-endian
    let crc = crc32_mpeg2(&section[..total_len - 4]);
    section[total_len - 4] = (crc >> 24) as u8;
    section[total_len - 3] = (crc >> 16) as u8;
    section[total_len - 2] = (crc >> 8) as u8;
    section[total_len - 1] = (crc & 0xFF) as u8;

    // Sanidade: a seção completa deve verificar como CRC válido
    assert!(
        verify_crc32_mpeg2(&section),
        "seção sintética deve ter CRC-32 MPEG-2 válido (residual == 0)"
    );

    // Distribuir a seção em 3 pacotes TS
    // ── Pacote 1 (PUSI=1, CC=0): pointer_field=0 + seção[0..183]
    let mut p1_payload = vec![0u8; 184];
    p1_payload[0] = 0x00; // pointer_field = 0 (seção começa imediatamente)
    p1_payload[1..184].copy_from_slice(&section[0..183]);
    let pkt1 = build_ts_packet(pid, true, 0, &p1_payload);

    // ── Pacote 2 (PUSI=0, CC=1): seção[183..367]
    let pkt2 = build_ts_packet(pid, false, 1, &section[183..367]);

    // ── Pacote 3 (PUSI=0, CC=2): seção[367..400] + stuffing 0xFF
    let mut p3_payload = vec![0xFFu8; 184];
    p3_payload[..33].copy_from_slice(&section[367..400]);
    let pkt3 = build_ts_packet(pid, false, 2, &p3_payload);

    // Sanidade: bytes de seção distribuídos = 183 + 184 + 33 = 400
    assert_eq!(183 + 184 + 33, total_len);

    let mut data = Vec::with_capacity(3 * 188);
    data.extend_from_slice(&pkt1);
    data.extend_from_slice(&pkt2);
    data.extend_from_slice(&pkt3);

    let path = fixtures.join("ts_fragmented_section.bin");
    fs::write(&path, &data).expect("escrever ts_fragmented_section.bin");

    assert_eq!(
        data.len(),
        3 * 188,
        "fixture deve conter exatamente 3 pacotes TS de 188 bytes"
    );
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        (3 * 188) as u64,
        "arquivo gravado deve ter o tamanho correto"
    );
}

/// T08: Gera `ts_rtp_wrapped.bin`.
///
/// Cabeçalho RTP mínimo de **12 bytes** (V=2, PT=33, Seq=1, TS=0,
/// SSRC=0x12345678) seguido de **7 pacotes TS** válidos no PID 0x0300.
/// Total: 12 + 7 × 188 = 1328 bytes.
#[test]
fn spec_t08_generate_rtp_wrapped_fixture() {
    let fixtures = fixtures_dir();
    fs::create_dir_all(&fixtures).expect("criar diretório tests/fixtures");

    // Cabeçalho RTP (RFC 3550) — 12 bytes fixos
    let rtp_header: [u8; 12] = [
        0x80, // V=2, P=0, X=0, CC=0
        0x21, // M=0, PT=33 (MPEG-TS, RFC 2250)
        0x00, 0x01, // Sequence number = 1
        0x00, 0x00, 0x00, 0x00, // Timestamp = 0
        0x12, 0x34, 0x56, 0x78, // SSRC = 0x12345678
    ];

    // 7 pacotes TS válidos no PID 0x0300, CC=0..6
    let ts_payload = [0xCDu8; 184];
    let mut ts_data = Vec::with_capacity(7 * 188);
    for cc in 0u8..7 {
        ts_data.extend_from_slice(&build_ts_packet(0x0300, false, cc, &ts_payload));
    }

    let mut data = Vec::with_capacity(12 + 7 * 188);
    data.extend_from_slice(&rtp_header);
    data.extend_from_slice(&ts_data);

    let path = fixtures.join("ts_rtp_wrapped.bin");
    fs::write(&path, &data).expect("escrever ts_rtp_wrapped.bin");

    assert_eq!(
        data.len(),
        12 + 7 * 188,
        "fixture deve ter header RTP (12 bytes) + 7 pacotes TS (1316 bytes)"
    );
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        (12 + 7 * 188) as u64,
        "arquivo gravado deve ter o tamanho correto"
    );
}
