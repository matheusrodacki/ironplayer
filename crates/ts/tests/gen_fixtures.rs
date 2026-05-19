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

// ── T04: Fixtures de tabelas PSI ─────────────────────────────────────────────

/// T04: Gera `pat_section.bin`.
///
/// Seção PAT completa (com CRC-32 MPEG-2 válido):
/// - transport_stream_id = 1
/// - version = 1, current_next = true
/// - Programa 0 (NIT) → PID 0x0010
/// - Programa 1 (PMT) → PID 0x0100
///
/// SPEC-TABLE-001
#[test]
fn spec_t04_generate_pat_section_fixture() {
    let fixtures = fixtures_dir();
    fs::create_dir_all(&fixtures).expect("criar diretório tests/fixtures");

    // Construir seção PAT sem CRC
    // section_length = 5 (common header) + 4 (NIT entry) + 4 (PMT entry) + 4 (CRC) = 17
    let section_length: u16 = 17;

    let mut section = Vec::with_capacity(3 + section_length as usize);

    // ── Cabeçalho PSI (3 bytes) ──────────────────────────────────────────────
    section.push(0x00); // table_id = PAT
    // section_syntax_indicator=1(1b), '0'(1b), reserved(2b), section_length[11:8]
    section.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
    section.push((section_length & 0xFF) as u8);

    // ── Cabeçalho comum PSI (5 bytes) ────────────────────────────────────────
    section.push(0x00); // transport_stream_id[15:8]
    section.push(0x01); // transport_stream_id[7:0] = 1
    // reserved(2b)=11, version(5b)=1, current_next(1b)=1 → 0b11_00001_1 = 0xC3
    section.push(0xC3);
    section.push(0x00); // section_number = 0
    section.push(0x00); // last_section_number = 0

    // ── Programa 0: NIT PID 0x0010 ───────────────────────────────────────────
    section.push(0x00); // program_number[15:8] = 0
    section.push(0x00); // program_number[7:0] = 0
    // reserved(3b)=111, pid[12:8]=0 → 0xE0
    section.push(0xE0);
    section.push(0x10); // pid[7:0] = 0x10

    // ── Programa 1: PMT PID 0x0100 ───────────────────────────────────────────
    section.push(0x00); // program_number[15:8] = 0
    section.push(0x01); // program_number[7:0] = 1
    // reserved(3b)=111, pid[12:8]=1 → 0xE1
    section.push(0xE1);
    section.push(0x00); // pid[7:0] = 0x00

    // ── CRC-32 MPEG-2 ────────────────────────────────────────────────────────
    let crc = crc32_mpeg2(&section);
    section.extend_from_slice(&crc.to_be_bytes());

    // Sanidade
    assert!(
        verify_crc32_mpeg2(&section),
        "seção PAT sintética deve ter CRC-32 MPEG-2 válido"
    );
    assert_eq!(section.len(), 3 + section_length as usize);

    let path = fixtures.join("pat_section.bin");
    fs::write(&path, &section).expect("escrever pat_section.bin");

    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        section.len() as u64,
        "arquivo gravado deve ter o tamanho correto"
    );
}

/// T04: Gera `pmt_h264_aac.bin`.
///
/// Seção PMT completa (com CRC-32 MPEG-2 válido):
/// - program_number = 1
/// - version = 1, current_next = true
/// - PCR PID = 0x0110
/// - Nenhum program descriptor
/// - Stream 0: H.264 (0x1B) → PID 0x0110
/// - Stream 1: AAC ADTS (0x0F) → PID 0x0120
///
/// SPEC-TABLE-002
#[test]
fn spec_t04_generate_pmt_h264_aac_fixture() {
    let fixtures = fixtures_dir();
    fs::create_dir_all(&fixtures).expect("criar diretório tests/fixtures");

    // section_length = 5 (common) + 2 (pcr_pid) + 2 (prog_info_len) + 0 (prog_descriptors)
    //                + 5 (stream H.264) + 5 (stream AAC) + 4 (CRC) = 23
    let section_length: u16 = 23;

    let mut section = Vec::with_capacity(3 + section_length as usize);

    // ── Cabeçalho PSI (3 bytes) ──────────────────────────────────────────────
    section.push(0x02); // table_id = PMT
    section.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
    section.push((section_length & 0xFF) as u8);

    // ── Cabeçalho comum PSI (5 bytes) ────────────────────────────────────────
    section.push(0x00); // program_number[15:8]
    section.push(0x01); // program_number[7:0] = 1
    section.push(0xC3); // reserved(2b)|version=1|current_next=1
    section.push(0x00); // section_number = 0
    section.push(0x00); // last_section_number = 0

    // ── PMT específico ────────────────────────────────────────────────────────
    // reserved(3b)=111, PCR_PID[12:8]=1 → 0xE1
    section.push(0xE1);
    section.push(0x10); // PCR_PID[7:0] = 0x10  → PCR_PID = 0x0110

    // reserved(4b)=1111, program_info_length[11:8]=0 → 0xF0
    section.push(0xF0);
    section.push(0x00); // program_info_length = 0

    // ── Stream 0: H.264 Video (0x1B) → PID 0x0110 ───────────────────────────
    section.push(0x1B); // stream_type = H.264
    section.push(0xE1); // reserved(3b)|elementary_PID[12:8]=1 → 0xE1
    section.push(0x10); // elementary_PID[7:0] = 0x10  → PID 0x0110
    section.push(0xF0); // reserved(4b)|ES_info_length[11:8]=0
    section.push(0x00); // ES_info_length = 0

    // ── Stream 1: AAC Audio ADTS (0x0F) → PID 0x0120 ────────────────────────
    section.push(0x0F); // stream_type = AAC ADTS
    section.push(0xE1); // reserved(3b)|elementary_PID[12:8]=1 → 0xE1
    section.push(0x20); // elementary_PID[7:0] = 0x20  → PID 0x0120
    section.push(0xF0); // reserved(4b)|ES_info_length[11:8]=0
    section.push(0x00); // ES_info_length = 0

    // ── CRC-32 MPEG-2 ────────────────────────────────────────────────────────
    let crc = crc32_mpeg2(&section);
    section.extend_from_slice(&crc.to_be_bytes());

    // Sanidade
    assert!(
        verify_crc32_mpeg2(&section),
        "seção PMT sintética deve ter CRC-32 MPEG-2 válido"
    );
    assert_eq!(section.len(), 3 + section_length as usize);

    let path = fixtures.join("pmt_h264_aac.bin");
    fs::write(&path, &section).expect("escrever pmt_h264_aac.bin");

    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        section.len() as u64,
        "arquivo gravado deve ter o tamanho correto"
    );
}

// ── T05–T07: Fixtures NIT, SDT e TDT ─────────────────────────────────────────

/// T05: Gera `nit_cable.bin`.
///
/// Seção NIT actual (table_id=0x40) com CRC-32 MPEG-2 válido:
/// - network_id = 100, version = 1, current_next = true
/// - Network descriptor: NetworkName "IronCable" (tag 0x40)
/// - 1 transport stream: ts_id=1, orig_net_id=100
///   - CableDelivery descriptor: 306 MHz, 64-QAM, 6875 ksym/s
///
/// SPEC-TABLE-003
#[test]
fn spec_t05_generate_nit_cable_fixture() {
    let fixtures = fixtures_dir();
    fs::create_dir_all(&fixtures).expect("criar diretório tests/fixtures");

    // ── Descriptor NetworkName "IronCable" (tag 0x40, len 9) ────────────────
    let name = b"IronCable";
    let mut net_name_desc = Vec::new();
    net_name_desc.push(0x40u8);
    net_name_desc.push(name.len() as u8);
    net_name_desc.extend_from_slice(name);
    // net_name_desc = 11 bytes

    // ── Descriptor CableDelivery (tag 0x44, len 11) ──────────────────────────
    // frequency: BCD 03060000 → 3060000 × 100 Hz = 306 MHz
    // modulation: 0x03 = 64-QAM
    // symbol_rate: BCD 0068750 → 68750 × 100 = 6 875 000 sym/s; fec_inner=0xF
    //   field bytes: [0x00,0x68,0x75,0x0F]
    let cable_payload: [u8; 11] = [
        0x03, 0x06, 0x00, 0x00, // frequency BCD 8-digit × 100 Hz
        0xFF, 0xFF,              // reserved
        0x03,                   // modulation = 64-QAM
        0x00, 0x68, 0x75, 0x0F, // symbol_rate 7-BCD-digit + fec_inner
    ];
    let mut cable_desc = Vec::new();
    cable_desc.push(0x44u8);
    cable_desc.push(cable_payload.len() as u8);
    cable_desc.extend_from_slice(&cable_payload);
    // cable_desc = 13 bytes

    // ── TS loop entry: ts_id=1, orig_net_id=100, cable descriptor ────────────
    let ts_desc_len = cable_desc.len() as u16; // 13
    let mut ts_entry = Vec::new();
    ts_entry.extend_from_slice(&[0x00u8, 0x01]);
    ts_entry.extend_from_slice(&[0x00u8, 0x64]);
    ts_entry.push(0xF0u8 | ((ts_desc_len >> 8) as u8 & 0x0F));
    ts_entry.push((ts_desc_len & 0xFF) as u8);
    ts_entry.extend_from_slice(&cable_desc);
    // ts_entry = 2+2+2+13 = 19 bytes

    // ── Section body ─────────────────────────────────────────────────────────
    let net_desc_len = net_name_desc.len() as u16; // 11
    let ts_loop_len  = ts_entry.len() as u16;      // 19

    let mut body = Vec::new();
    body.extend_from_slice(&[0x00u8, 0x64]);
    body.push(0xC3u8); // reserved|version=1|current_next=1
    body.push(0x00u8); // section_number
    body.push(0x00u8); // last_section_number
    body.push(0xF0u8 | ((net_desc_len >> 8) as u8 & 0x0F));
    body.push((net_desc_len & 0xFF) as u8);
    body.extend_from_slice(&net_name_desc);
    body.push(0xF0u8 | ((ts_loop_len >> 8) as u8 & 0x0F));
    body.push((ts_loop_len & 0xFF) as u8);
    body.extend_from_slice(&ts_entry);
    // body = 2+1+1+1+2+11+2+19 = 39 bytes

    // ── PSI header + CRC ─────────────────────────────────────────────────────
    let section_length = (body.len() + 4) as u16; // 43
    let mut section = Vec::new();
    section.push(0x40u8);
    section.push(0xB0u8 | ((section_length >> 8) as u8 & 0x0F));
    section.push((section_length & 0xFF) as u8);
    section.extend_from_slice(&body);
    let crc = crc32_mpeg2(&section);
    section.extend_from_slice(&crc.to_be_bytes());

    assert!(verify_crc32_mpeg2(&section), "NIT CRC deve ser válido");

    let path = fixtures.join("nit_cable.bin");
    fs::write(&path, &section).expect("escrever nit_cable.bin");
    assert_eq!(fs::metadata(&path).unwrap().len(), section.len() as u64);
}

/// T06: Gera `sdt_actual.bin`.
///
/// Seção SDT actual (table_id=0x42) com CRC-32 MPEG-2 válido:
/// - transport_stream_id = 1, original_network_id = 100
/// - version = 3, current_next = true
/// - 1 serviço: service_id=1, running=4(Running), EIT flags set
///   - Service descriptor: type=1(Digital TV), provider="IronTV", name="Channel 1"
///
/// SPEC-TABLE-004
#[test]
fn spec_t06_generate_sdt_actual_fixture() {
    let fixtures = fixtures_dir();
    fs::create_dir_all(&fixtures).expect("criar diretório tests/fixtures");

    // ── Descriptor Service (tag 0x48) ─────────────────────────────────────────
    let provider = b"IronTV";
    let svc_name = b"Channel 1";
    let mut svc_payload = Vec::new();
    svc_payload.push(0x01u8);                  // service_type = Digital Television
    svc_payload.push(provider.len() as u8);    // provider_name_length
    svc_payload.extend_from_slice(provider);
    svc_payload.push(svc_name.len() as u8);    // service_name_length
    svc_payload.extend_from_slice(svc_name);
    // svc_payload = 18 bytes

    let mut svc_desc = Vec::new();
    svc_desc.push(0x48u8);
    svc_desc.push(svc_payload.len() as u8);
    svc_desc.extend_from_slice(&svc_payload);
    // svc_desc = 20 bytes

    // ── Service loop entry ───────────────────────────────────────────────────
    let desc_loop_len = svc_desc.len() as u16; // 20
    // running_status=4, free_CA_mode=0, descriptors_loop_length=20
    let rf_hi = (4u8 << 5) | (0u8 << 4) | ((desc_loop_len >> 8) as u8 & 0x0F);
    let rf_lo = (desc_loop_len & 0xFF) as u8;

    let mut svc_entry = Vec::new();
    svc_entry.extend_from_slice(&[0x00u8, 0x01]); // service_id = 1
    svc_entry.push(0xFFu8);                        // reserved(6b)|EIT_sched=1|EIT_pf=1
    svc_entry.push(rf_hi);
    svc_entry.push(rf_lo);
    svc_entry.extend_from_slice(&svc_desc);
    // svc_entry = 2+1+2+20 = 25 bytes

    // ── Section body ─────────────────────────────────────────────────────────
    let mut body = Vec::new();
    body.extend_from_slice(&[0x00u8, 0x01]);  // transport_stream_id = 1
    body.push(0xC7u8);                         // reserved|version=3|current_next=1
    body.push(0x00u8);                         // section_number
    body.push(0x00u8);                         // last_section_number
    body.extend_from_slice(&[0x00u8, 0x64]);  // original_network_id = 100
    body.push(0xFFu8);                         // reserved
    body.extend_from_slice(&svc_entry);
    // body = 2+1+1+1+2+1+25 = 33 bytes

    // ── PSI header + CRC ─────────────────────────────────────────────────────
    let section_length = (body.len() + 4) as u16; // 37
    let mut section = Vec::new();
    section.push(0x42u8);
    section.push(0xB0u8 | ((section_length >> 8) as u8 & 0x0F));
    section.push((section_length & 0xFF) as u8);
    section.extend_from_slice(&body);
    let crc = crc32_mpeg2(&section);
    section.extend_from_slice(&crc.to_be_bytes());

    assert!(verify_crc32_mpeg2(&section), "SDT CRC deve ser válido");

    let path = fixtures.join("sdt_actual.bin");
    fs::write(&path, &section).expect("escrever sdt_actual.bin");
    assert_eq!(fs::metadata(&path).unwrap().len(), section.len() as u64);
}

/// T07: Gera `tdt.bin`.
///
/// Seção TDT sintética (short section, sem CRC-32):
/// - table_id = 0x70
/// - UTC time: 2019-05-23 14:30:45
///   - MJD = 58626 = 0xE502
///   - BCD: HH=0x14, MM=0x30, SS=0x45
/// Total: 8 bytes.
///
/// SPEC-TABLE-006
#[test]
fn spec_t07_generate_tdt_fixture() {
    let fixtures = fixtures_dir();
    fs::create_dir_all(&fixtures).expect("criar diretório tests/fixtures");

    // TDT short section (section_syntax_indicator=0, sem CRC):
    // [table_id][0b0111_0000][0x05][MJD_hi][MJD_lo][HH][MM][SS]
    let section: [u8; 8] = [
        0x70, // table_id = TDT
        0x70, // section_syntax_indicator=0, dvb_reserved=1, res=11, len[11:8]=0
        0x05, // section_length = 5
        0xE5, // MJD[15:8] → MJD = 0xE502 = 58626 → 2019-05-23
        0x02, // MJD[7:0]
        0x14, // BCD HH = 14
        0x30, // BCD MM = 30
        0x45, // BCD SS = 45
    ];

    let path = fixtures.join("tdt.bin");
    fs::write(&path, &section).expect("escrever tdt.bin");
    assert_eq!(fs::metadata(&path).unwrap().len(), 8, "tdt.bin deve ter 8 bytes");
}

// ── T08–T09: Fixtures EIT e BAT ──────────────────────────────────────────────

/// T08: Gera `eit_pf.bin`.
///
/// Seção EIT Present/Following actual (table_id=0x4E) com CRC-32 MPEG-2 válido:
/// - service_id=1, transport_stream_id=1, original_network_id=100, version=0
/// - 2 eventos:
///   - event_id=101: start MJD=0xDCAE HH=0x20 MM=0x00 SS=0x00,
///                   duration 01:30:00, running=Running(4), ShortEvent "Film A"/"Action"
///   - event_id=102: start_time indefinido (HH=0xFF), duration indefinida (HH=0xFF)
///
/// SPEC-TABLE-005
#[test]
fn spec_t08_generate_eit_pf_fixture() {
    let fixtures = fixtures_dir();
    fs::create_dir_all(&fixtures).expect("criar diretório tests/fixtures");

    // ── ShortEvent descriptor (tag 0x4D) para o evento 101 ───────────────────
    // lang(3) + name_len(1) + name + text_len(1) + text
    let lang = b"eng";
    let ev_name = b"Film A";
    let ev_text = b"Action";
    let mut short_event_payload = Vec::new();
    short_event_payload.extend_from_slice(lang);
    short_event_payload.push(ev_name.len() as u8);
    short_event_payload.extend_from_slice(ev_name);
    short_event_payload.push(ev_text.len() as u8);
    short_event_payload.extend_from_slice(ev_text);
    // short_event_payload = 3+1+6+1+6 = 17 bytes

    let mut short_event_desc = Vec::new();
    short_event_desc.push(0x4Du8);
    short_event_desc.push(short_event_payload.len() as u8);
    short_event_desc.extend_from_slice(&short_event_payload);
    // short_event_desc = 19 bytes

    // ── Evento 101 (12 + desc bytes) ─────────────────────────────────────────
    let desc_len_101 = short_event_desc.len() as u16; // 19
    let mut evt101 = Vec::new();
    evt101.push(0x00); evt101.push(0x65); // event_id = 101
    // start_time: MJD=0xDCAE, HH=0x20 MM=0x00 SS=0x00
    evt101.push(0xDC); evt101.push(0xAE);
    evt101.push(0x20); evt101.push(0x00); evt101.push(0x00);
    // duration: BCD 01:30:00
    evt101.push(0x01); evt101.push(0x30); evt101.push(0x00);
    // running_status=4 (Running), free_ca=0, desc_loop_len=19
    // byte10: (4<<5)|(0<<4)|(19>>8) = 0x80 | 0 = 0x80
    evt101.push(0x80u8 | ((desc_len_101 >> 8) as u8 & 0x0F));
    evt101.push((desc_len_101 & 0xFF) as u8);
    evt101.extend_from_slice(&short_event_desc);
    // evt101 = 12 + 19 = 31 bytes

    // ── Evento 102 (start_time indefinido, sem descriptors) ──────────────────
    let mut evt102 = Vec::new();
    evt102.push(0x00); evt102.push(0x66); // event_id = 102
    // start_time indefinido: MJD=0xFFFF, HH=0xFF MM=0xFF SS=0xFF
    evt102.push(0xFF); evt102.push(0xFF);
    evt102.push(0xFF); evt102.push(0xFF); evt102.push(0xFF);
    // duration indefinida: 0xFF:0xFF:0xFF
    evt102.push(0xFF); evt102.push(0xFF); evt102.push(0xFF);
    // running_status=0 (Undefined), free_ca=0, desc_loop_len=0
    evt102.push(0x00); evt102.push(0x00);
    // evt102 = 12 bytes

    // ── Section body ─────────────────────────────────────────────────────────
    let events_data_len = evt101.len() + evt102.len();
    // body = service_id(2) + version(1) + sec×2(2) + ts_id(2) + orig_net_id(2)
    //      + seg_last_sec(1) + last_table_id(1) + events = 11 + events
    let body_len = 11 + events_data_len;

    // section_length = body_len + 4 (CRC)
    let section_length = (body_len + 4) as u16;

    let mut section = Vec::new();
    section.push(0x4Eu8); // table_id = EIT p/f actual
    section.push(0xB0u8 | ((section_length >> 8) as u8 & 0x0F));
    section.push((section_length & 0xFF) as u8);

    // PSI common header (5 bytes)
    section.push(0x00); section.push(0x01); // service_id = 1
    section.push(0xC1u8);                   // reserved|version=0|current_next=1
    section.push(0x00);                     // section_number = 0
    section.push(0x01);                     // last_section_number = 1

    // EIT-specific header (6 bytes)
    section.push(0x00); section.push(0x01); // transport_stream_id = 1
    section.push(0x00); section.push(0x64); // original_network_id = 100
    section.push(0x01);                     // segment_last_section_number
    section.push(0x4E);                     // last_table_id

    // Events
    section.extend_from_slice(&evt101);
    section.extend_from_slice(&evt102);

    let crc = crc32_mpeg2(&section);
    section.extend_from_slice(&crc.to_be_bytes());

    assert!(verify_crc32_mpeg2(&section), "EIT p/f CRC deve ser válido");

    let path = fixtures.join("eit_pf.bin");
    fs::write(&path, &section).expect("escrever eit_pf.bin");
    assert_eq!(fs::metadata(&path).unwrap().len(), section.len() as u64);
}

/// T09: Gera `bat.bin`.
///
/// Seção BAT (table_id=0x4A) com CRC-32 MPEG-2 válido:
/// - bouquet_id=42, version=1, current_next=true
/// - BouquetName descriptor: "IronBundle" (tag 0x47)
/// - 1 transport stream: ts_id=1, orig_net_id=100, sem descriptors
///
/// SPEC-TABLE-007
#[test]
fn spec_t09_generate_bat_fixture() {
    let fixtures = fixtures_dir();
    fs::create_dir_all(&fixtures).expect("criar diretório tests/fixtures");

    // ── Descriptor BouquetName "IronBundle" (tag 0x47, len 10) ──────────────
    let bname = b"IronBundle";
    let mut bname_desc = Vec::new();
    bname_desc.push(0x47u8);
    bname_desc.push(bname.len() as u8);
    bname_desc.extend_from_slice(bname);
    // bname_desc = 12 bytes

    // ── TS loop entry: ts_id=1, orig_net_id=100, no descriptors ─────────────
    let mut ts_entry = Vec::new();
    ts_entry.extend_from_slice(&[0x00u8, 0x01]); // ts_id = 1
    ts_entry.extend_from_slice(&[0x00u8, 0x64]); // orig_net_id = 100
    // desc_loop_len = 0
    ts_entry.push(0xF0u8);
    ts_entry.push(0x00u8);
    // ts_entry = 6 bytes

    let bouquet_desc_len = bname_desc.len() as u16; // 12
    let ts_loop_len      = ts_entry.len() as u16;   // 6

    // section_length = 5 (common) + 2 + bouquet_desc_len + 2 + ts_loop_len + 4 (CRC)
    let section_length: u16 = 5 + 2 + bouquet_desc_len + 2 + ts_loop_len + 4;

    let mut section = Vec::new();
    section.push(0x4Au8); // table_id = BAT
    section.push(0xB0u8 | ((section_length >> 8) as u8 & 0x0F));
    section.push((section_length & 0xFF) as u8);

    // PSI common header (5 bytes)
    section.push(0x00); section.push(0x2A); // bouquet_id = 42
    section.push(0xC3u8);                   // reserved|version=1|current_next=1
    section.push(0x00);                     // section_number = 0
    section.push(0x00);                     // last_section_number = 0

    // bouquet_desc_length
    section.push(0xF0u8 | ((bouquet_desc_len >> 8) as u8 & 0x0F));
    section.push((bouquet_desc_len & 0xFF) as u8);
    section.extend_from_slice(&bname_desc);

    // ts_loop_length
    section.push(0xF0u8 | ((ts_loop_len >> 8) as u8 & 0x0F));
    section.push((ts_loop_len & 0xFF) as u8);
    section.extend_from_slice(&ts_entry);

    let crc = crc32_mpeg2(&section);
    section.extend_from_slice(&crc.to_be_bytes());

    assert!(verify_crc32_mpeg2(&section), "BAT CRC deve ser válido");

    let path = fixtures.join("bat.bin");
    fs::write(&path, &section).expect("escrever bat.bin");
    assert_eq!(fs::metadata(&path).unwrap().len(), section.len() as u64);
}
