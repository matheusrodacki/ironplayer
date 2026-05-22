//! Gerador de fixtures MPEG-TS sintéticas "reais" para testes de regressão.
//!
//! Cada `#[test]` gera um arquivo `.ts` em `crates/ts/tests/fixtures/real/`.
//! Os arquivos gerados representam streams MPEG-TS válidos com PAT, PMT, SDT e NIT,
//! simulando cenários reais (cabo, satélite, terrestre, multi-serviço, rádio, etc.).
//!
//! Execute com `cargo test -p ts gen_real` para (re)gerar os arquivos.
//!
//! SPEC-TABLE-001 · SPEC-TABLE-002 · SPEC-TABLE-003 · SPEC-TABLE-004

use std::fs;
use std::path::PathBuf;
use ts::crc32_mpeg2;

// ── Diretório de fixtures ─────────────────────────────────────────────────────

fn real_fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("real")
}

// ── Helpers de seção PSI ──────────────────────────────────────────────────────

/// Constrói uma seção PSI completa:
/// `[table_id(1)][0xB0|(len>>8)(1)][(len&0xFF)(1)][body][CRC32(4)]`
/// onde `len = body.len() + 4` (inclui CRC).
fn build_section(table_id: u8, body: &[u8]) -> Vec<u8> {
    let section_length = body.len() + 4;
    assert!(
        section_length <= 0x0FFF,
        "section_length {section_length} excede o limite de 4093 bytes"
    );
    let mut sec = Vec::with_capacity(3 + body.len() + 4);
    sec.push(table_id);
    sec.push(0xB0 | ((section_length >> 8) & 0x0F) as u8);
    sec.push((section_length & 0xFF) as u8);
    sec.extend_from_slice(body);
    let crc = crc32_mpeg2(&sec);
    sec.push((crc >> 24) as u8);
    sec.push((crc >> 16) as u8);
    sec.push((crc >> 8) as u8);
    sec.push(crc as u8);
    sec
}

/// Empacota uma seção PSI em pacotes TS de 188 bytes.
/// O primeiro pacote tem PUSI=1 e `pointer_field=0x00`.
/// Retorna os bytes brutos de todos os pacotes concatenados.
fn wrap_section_in_ts(pid: u16, section: &[u8], cc: &mut u8) -> Vec<u8> {
    let mut out = Vec::new();

    // Primeiro pacote: PUSI=1, pointer_field=0x00
    let first_copy = 183_usize.min(section.len());
    {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = 0x40 | ((pid >> 8) & 0x1F) as u8; // PUSI=1
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x10 | (*cc & 0x0F); // AFC=0b01 (payload only), CC
        pkt[4] = 0x00; // pointer_field=0
        pkt[5..5 + first_copy].copy_from_slice(&section[..first_copy]);
        *cc = (*cc + 1) & 0x0F;
        out.extend_from_slice(&pkt);
    }
    let mut pos = first_copy;

    // Pacotes de continuação
    while pos < section.len() {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = ((pid >> 8) & 0x1F) as u8; // PUSI=0
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x10 | (*cc & 0x0F);
        let copy_len = 184_usize.min(section.len() - pos);
        pkt[4..4 + copy_len].copy_from_slice(&section[pos..pos + copy_len]);
        pos += copy_len;
        *cc = (*cc + 1) & 0x0F;
        out.extend_from_slice(&pkt);
    }

    out
}

/// Pacote nulo padrão (PID 0x1FFF).
fn null_packet() -> [u8; 188] {
    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x1F;
    pkt[2] = 0xFF;
    pkt[3] = 0x10; // AFC=0b01, CC=0
    pkt
}

/// Pacote com PCR na adaptation field (AFC=0b10 — adaptation only).
/// `pcr_90khz`: valor do PCR base em ticks de 90 kHz (33 bits; PCR ext = 0).
fn pcr_packet(pcr_pid: u16, pcr_90khz: u64, cc: u8) -> [u8; 188] {
    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47;
    pkt[1] = ((pcr_pid >> 8) & 0x1F) as u8;
    pkt[2] = (pcr_pid & 0xFF) as u8;
    pkt[3] = 0x20 | (cc & 0x0F); // AFC=0b10 (adaptation only), CC
    pkt[4] = 183; // adaptation_field_length (preenche os 183 bytes restantes)
    pkt[5] = 0x10; // flags: PCR_flag=1, resto=0
                   // Codificação PCR (48 bits):
                   //   base[32:25] | base[24:17] | base[16:9] | base[8:1]
                   //   (base[0]<<7 | 0x7E | ext[8]) | ext[7:0]
    let base = pcr_90khz & 0x1_FFFF_FFFF; // máscara 33 bits
    pkt[6] = (base >> 25) as u8;
    pkt[7] = (base >> 17) as u8;
    pkt[8] = (base >> 9) as u8;
    pkt[9] = (base >> 1) as u8;
    pkt[10] = (((base & 0x01) << 7) | 0x7E) as u8; // ext=0
    pkt[11] = 0x00; // ext low byte
                    // bytes 12..188 = 0xFF (stuffing)
    pkt
}

// ── Construtores de body PSI ──────────────────────────────────────────────────

/// Body da PAT (sem cabeçalho de 3 bytes e sem CRC de 4 bytes).
/// `programs`: `(program_number, pid)` — `program_number=0` indica NIT PID.
fn pat_body(tsid: u16, version: u8, programs: &[(u16, u16)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.push((tsid >> 8) as u8);
    body.push((tsid & 0xFF) as u8);
    // reserved(2b=11) | version(5b) | current_next(1b=1)
    body.push(0xC0 | ((version & 0x1F) << 1) | 0x01);
    body.push(0x00); // section_number
    body.push(0x00); // last_section_number
    for &(prog_num, pid) in programs {
        body.push((prog_num >> 8) as u8);
        body.push((prog_num & 0xFF) as u8);
        body.push(0xE0 | ((pid >> 8) & 0x1F) as u8); // reserved(3b=111) | pid[12:8]
        body.push((pid & 0xFF) as u8);
    }
    body
}

/// Body da PMT (sem cabeçalho e sem CRC).
/// `streams`: `(stream_type, elementary_pid)` — sem ES descriptors.
fn pmt_body(prog_num: u16, version: u8, pcr_pid: u16, streams: &[(u8, u16)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.push((prog_num >> 8) as u8);
    body.push((prog_num & 0xFF) as u8);
    body.push(0xC0 | ((version & 0x1F) << 1) | 0x01);
    body.push(0x00); // section_number
    body.push(0x00); // last_section_number
    body.push(0xE0 | ((pcr_pid >> 8) & 0x1F) as u8); // reserved(3b=111) | pcr_pid[12:8]
    body.push((pcr_pid & 0xFF) as u8);
    body.push(0xF0); // reserved(4b=1111) | program_info_length[11:8]=0
    body.push(0x00); // program_info_length[7:0]=0
    for &(stream_type, es_pid) in streams {
        body.push(stream_type);
        body.push(0xE0 | ((es_pid >> 8) & 0x1F) as u8); // reserved(3b=111) | pid[12:8]
        body.push((es_pid & 0xFF) as u8);
        body.push(0xF0); // reserved(4b=1111) | ES_info_length[11:8]=0
        body.push(0x00); // ES_info_length[7:0]=0
    }
    body
}

/// Service Descriptor (tag 0x48).
/// `provider` e `name` como bytes (ASCII / ISO 8859-1).
fn service_descriptor(service_type: u8, provider: &[u8], name: &[u8]) -> Vec<u8> {
    let total_len = 3 + provider.len() + name.len();
    assert!(
        total_len <= 255,
        "service_descriptor too large: {total_len} bytes"
    );
    let mut d = Vec::with_capacity(2 + total_len);
    d.push(0x48); // tag
    d.push(total_len as u8); // length
    d.push(service_type);
    d.push(provider.len() as u8);
    d.extend_from_slice(provider);
    d.push(name.len() as u8);
    d.extend_from_slice(name);
    d
}

/// Body da SDT actual (table_id=0x42, sem cabeçalho e sem CRC).
/// `services`: `(service_id, eit_schedule, eit_pf, running_status, free_ca_mode, descriptors_bytes)`.
fn sdt_body(
    tsid: u16,
    version: u8,
    orig_nid: u16,
    services: &[(u16, bool, bool, u8, bool, Vec<u8>)],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.push((tsid >> 8) as u8);
    body.push((tsid & 0xFF) as u8);
    body.push(0xC0 | ((version & 0x1F) << 1) | 0x01);
    body.push(0x00); // section_number
    body.push(0x00); // last_section_number
    body.push((orig_nid >> 8) as u8);
    body.push((orig_nid & 0xFF) as u8);
    body.push(0xFF); // reserved byte
    for (svc_id, eit_sched, eit_pf, running, free_ca, descs) in services {
        body.push((svc_id >> 8) as u8);
        body.push((svc_id & 0xFF) as u8);
        // reserved(6b=111111) | eit_schedule_flag(1b) | eit_pf_flag(1b)
        body.push(0xFC | ((*eit_sched as u8) << 1) | (*eit_pf as u8));
        // running_status(3b) | free_ca_mode(1b) | descriptors_loop_length(12b)
        let dlen = descs.len();
        body.push((running << 5) | ((*free_ca as u8) << 4) | ((dlen >> 8) as u8 & 0x0F));
        body.push((dlen & 0xFF) as u8);
        body.extend_from_slice(descs);
    }
    body
}

/// Network Name Descriptor (tag 0x40).
fn network_name_descriptor(name: &[u8]) -> Vec<u8> {
    let mut d = vec![0x40u8, name.len() as u8];
    d.extend_from_slice(name);
    d
}

/// Body da NIT (table_id=0x40 ou 0x41, sem cabeçalho e sem CRC).
/// `net_descriptors`: bytes brutos dos descriptors de rede (já serializados).
/// `ts_entries`: `(ts_id, orig_nid, ts_descriptors_bytes)`.
fn nit_body(
    network_id: u16,
    version: u8,
    net_descriptors: &[u8],
    ts_entries: &[(u16, u16, Vec<u8>)],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.push((network_id >> 8) as u8);
    body.push((network_id & 0xFF) as u8);
    body.push(0xC0 | ((version & 0x1F) << 1) | 0x01);
    body.push(0x00); // section_number
    body.push(0x00); // last_section_number
    let net_desc_len = net_descriptors.len();
    body.push(0xF0 | ((net_desc_len >> 8) & 0x0F) as u8);
    body.push((net_desc_len & 0xFF) as u8);
    body.extend_from_slice(net_descriptors);

    // TS loop
    let mut ts_loop: Vec<u8> = Vec::new();
    for (ts_id, orig_nid, ts_descs) in ts_entries {
        ts_loop.push((ts_id >> 8) as u8);
        ts_loop.push((ts_id & 0xFF) as u8);
        ts_loop.push((orig_nid >> 8) as u8);
        ts_loop.push((orig_nid & 0xFF) as u8);
        let desc_len = ts_descs.len();
        ts_loop.push(0xF0 | ((desc_len >> 8) & 0x0F) as u8);
        ts_loop.push((desc_len & 0xFF) as u8);
        ts_loop.extend_from_slice(ts_descs);
    }
    let ts_loop_len = ts_loop.len();
    body.push(0xF0 | ((ts_loop_len >> 8) & 0x0F) as u8);
    body.push((ts_loop_len & 0xFF) as u8);
    body.extend_from_slice(&ts_loop);
    body
}

// ── Construtor de stream ──────────────────────────────────────────────────────

/// Configuração de um stream elementar dentro de um programa.
struct EsConfig {
    stream_type: u8,
    pid: u16,
}

/// Configuração de um programa dentro do multiplex.
struct ProgramConfig {
    program_number: u16,
    pmt_pid: u16,
    pcr_pid: u16,
    service_type: u8,
    provider: &'static str,
    service_name: &'static str,
    scrambled: bool,
    streams: Vec<EsConfig>,
}

/// Configuração de uma entrada de TS na NIT.
struct NitTsEntry {
    ts_id: u16,
    orig_nid: u16,
    /// Descriptors serializados (e.g., cable/satellite delivery).
    descriptors: Vec<u8>,
}

/// Parâmetros de uma fixture completa.
struct FixtureConfig {
    name: &'static str,
    tsid: u16,
    network_id: u16,
    network_name: &'static str,
    nit_pid: u16,
    programs: Vec<ProgramConfig>,
    /// Entradas de TS na NIT (inclui o próprio TSID).
    nit_ts_entries: Vec<NitTsEntry>,
    /// Duração simulada em segundos (define os PCR timestamps).
    duration_s: u32,
}

/// Gera o stream MPEG-TS para uma fixture.
///
/// Estrutura do stream:
/// - PCR no início (t = 0)
/// - 5 repetições de cada tabela SI (PAT / PMT / SDT / NIT)
/// - Null packets entre repetições
/// - PCR no final (t = duration_s)
///
/// As tabelas SI são distribuídas uniformemente com null packets como padding.
fn generate_stream(cfg: &FixtureConfig) -> Vec<u8> {
    let mut stream: Vec<u8> = Vec::new();

    // ─ Construir seções SI ────────────────────────────────────────────────────

    // PAT: entry para NIT PID + entries para cada programa
    let mut pat_programs: Vec<(u16, u16)> = vec![(0, cfg.nit_pid)]; // NIT entry
    for prog in &cfg.programs {
        pat_programs.push((prog.program_number, prog.pmt_pid));
    }
    let pat_sec = build_section(0x00, &pat_body(cfg.tsid, 0, &pat_programs));

    // PMTs: uma por programa
    let pmt_secs: Vec<Vec<u8>> = cfg
        .programs
        .iter()
        .map(|prog| {
            let streams: Vec<(u8, u16)> = prog
                .streams
                .iter()
                .map(|es| (es.stream_type, es.pid))
                .collect();
            build_section(
                0x02,
                &pmt_body(prog.program_number, 0, prog.pcr_pid, &streams),
            )
        })
        .collect();

    // SDT
    let sdt_services: Vec<(u16, bool, bool, u8, bool, Vec<u8>)> = cfg
        .programs
        .iter()
        .map(|prog| {
            let desc = service_descriptor(
                prog.service_type,
                prog.provider.as_bytes(),
                prog.service_name.as_bytes(),
            );
            (
                prog.program_number, // service_id == program_number
                false,               // eit_schedule_flag
                false,               // eit_pf_flag
                4u8,                 // running_status = Running
                prog.scrambled,
                desc,
            )
        })
        .collect();
    let sdt_sec = build_section(0x42, &sdt_body(cfg.tsid, 0, cfg.network_id, &sdt_services));

    // NIT
    let net_name_desc = network_name_descriptor(cfg.network_name.as_bytes());
    let ts_entries: Vec<(u16, u16, Vec<u8>)> = cfg
        .nit_ts_entries
        .iter()
        .map(|e| (e.ts_id, e.orig_nid, e.descriptors.clone()))
        .collect();
    let nit_sec = build_section(
        0x40,
        &nit_body(cfg.network_id, 0, &net_name_desc, &ts_entries),
    );

    // ─ PCR PID (do primeiro programa) ─────────────────────────────────────────
    let pcr_pid = cfg.programs.first().map(|p| p.pcr_pid).unwrap_or(0x0100);
    let pcr_end_90khz = cfg.duration_s as u64 * 90_000;

    // ─ Emitir stream ──────────────────────────────────────────────────────────
    // CCs independentes por PID (usamos um CC global simplificado por PID)
    let mut cc_pat = 0u8;
    let mut cc_pmt: Vec<u8> = vec![0u8; cfg.programs.len()];
    let mut cc_sdt = 0u8;
    let mut cc_nit = 0u8;
    let mut cc_pcr = 0u8;

    // PCR inicial (t=0)
    stream.extend_from_slice(&pcr_packet(pcr_pid, 0, cc_pcr));
    cc_pcr = (cc_pcr + 1) & 0x0F;

    // 5 repetições de SI + null packets entre repetições
    for rep in 0..5u64 {
        // PAT
        stream.extend_from_slice(&wrap_section_in_ts(0x0000, &pat_sec, &mut cc_pat));

        // PMTs
        for (i, pmt_sec) in pmt_secs.iter().enumerate() {
            stream.extend_from_slice(&wrap_section_in_ts(
                cfg.programs[i].pmt_pid,
                pmt_sec,
                &mut cc_pmt[i],
            ));
        }

        // SDT (a cada 2 repetições)
        if rep % 2 == 0 {
            stream.extend_from_slice(&wrap_section_in_ts(0x0011, &sdt_sec, &mut cc_sdt));
        }

        // NIT (a cada 3 repetições)
        if rep % 3 == 0 {
            stream.extend_from_slice(&wrap_section_in_ts(cfg.nit_pid, &nit_sec, &mut cc_nit));
        }

        // Null packets de padding (50 por repetição)
        for _ in 0..50 {
            stream.extend_from_slice(&null_packet());
        }
    }

    // PCR final (t=duration_s)
    stream.extend_from_slice(&pcr_packet(pcr_pid, pcr_end_90khz, cc_pcr));

    stream
}

// ── Fixture: cable_sd_1svc (Fixture 01) ──────────────────────────────────────

/// Gera `01_cable_sd_1svc.ts`:
/// - TSID=1001, NID=8442
/// - 1 serviço H.264 + AAC (SD)
/// - Duração: 15 s
#[test]
fn gen_real_01_cable_sd_1svc() {
    let dir = real_fixtures_dir();
    fs::create_dir_all(&dir).expect("criar diretório fixtures/real");

    let cfg = FixtureConfig {
        name: "01_cable_sd_1svc",
        tsid: 1001,
        network_id: 8442,
        network_name: "IronCable",
        nit_pid: 0x0010,
        programs: vec![ProgramConfig {
            program_number: 1,
            pmt_pid: 0x0100,
            pcr_pid: 0x0110,
            service_type: 0x01, // Digital TV
            provider: "IronNet",
            service_name: "IronTV SD",
            scrambled: false,
            streams: vec![
                EsConfig {
                    stream_type: 0x1B,
                    pid: 0x0110,
                }, // H.264 video
                EsConfig {
                    stream_type: 0x0F,
                    pid: 0x0111,
                }, // AAC audio
            ],
        }],
        nit_ts_entries: vec![NitTsEntry {
            ts_id: 1001,
            orig_nid: 8442,
            descriptors: vec![],
        }],
        duration_s: 15,
    };

    let data = generate_stream(&cfg);
    assert!(
        data.len() % 188 == 0,
        "stream deve ser múltiplo de 188 bytes"
    );

    let path = dir.join(format!("{}.ts", cfg.name));
    fs::write(&path, &data).expect("escrever 01_cable_sd_1svc.ts");
}

// ── Fixture: cable_hd_1svc (Fixture 02) ──────────────────────────────────────

/// Gera `02_cable_hd_1svc.ts`:
/// - TSID=1002, NID=8442
/// - 1 serviço HEVC + AAC-LATM (HD)
/// - Duração: 20 s
#[test]
fn gen_real_02_cable_hd_1svc() {
    let dir = real_fixtures_dir();
    fs::create_dir_all(&dir).expect("criar diretório fixtures/real");

    let cfg = FixtureConfig {
        name: "02_cable_hd_1svc",
        tsid: 1002,
        network_id: 8442,
        network_name: "IronCable",
        nit_pid: 0x0010,
        programs: vec![ProgramConfig {
            program_number: 2,
            pmt_pid: 0x0200,
            pcr_pid: 0x0210,
            service_type: 0x01,
            provider: "IronNet",
            service_name: "IronTV HD",
            scrambled: false,
            streams: vec![
                EsConfig {
                    stream_type: 0x24,
                    pid: 0x0210,
                }, // HEVC video
                EsConfig {
                    stream_type: 0x11,
                    pid: 0x0211,
                }, // AAC-LATM audio
            ],
        }],
        nit_ts_entries: vec![NitTsEntry {
            ts_id: 1002,
            orig_nid: 8442,
            descriptors: vec![],
        }],
        duration_s: 20,
    };

    let data = generate_stream(&cfg);
    assert!(data.len() % 188 == 0);

    let path = dir.join(format!("{}.ts", cfg.name));
    fs::write(&path, &data).expect("escrever 02_cable_hd_1svc.ts");
}

// ── Fixture: cable_3svc (Fixture 03) ─────────────────────────────────────────

/// Gera `03_cable_3svc.ts`:
/// - TSID=1003, NID=8442
/// - 3 serviços: Canal3 (H.264+AAC), Canal4 (H.264+AC3), Canal5 (HEVC+AAC)
/// - Duração: 25 s
#[test]
fn gen_real_03_cable_3svc() {
    let dir = real_fixtures_dir();
    fs::create_dir_all(&dir).expect("criar diretório fixtures/real");

    let cfg = FixtureConfig {
        name: "03_cable_3svc",
        tsid: 1003,
        network_id: 8442,
        network_name: "IronCable",
        nit_pid: 0x0010,
        programs: vec![
            ProgramConfig {
                program_number: 3,
                pmt_pid: 0x0300,
                pcr_pid: 0x0310,
                service_type: 0x01,
                provider: "IronNet",
                service_name: "Canal3",
                scrambled: false,
                streams: vec![
                    EsConfig {
                        stream_type: 0x1B,
                        pid: 0x0310,
                    },
                    EsConfig {
                        stream_type: 0x0F,
                        pid: 0x0311,
                    },
                ],
            },
            ProgramConfig {
                program_number: 4,
                pmt_pid: 0x0400,
                pcr_pid: 0x0410,
                service_type: 0x01,
                provider: "IronNet",
                service_name: "Canal4",
                scrambled: false,
                streams: vec![
                    EsConfig {
                        stream_type: 0x1B,
                        pid: 0x0410,
                    },
                    EsConfig {
                        stream_type: 0x81,
                        pid: 0x0411,
                    }, // AC-3
                ],
            },
            ProgramConfig {
                program_number: 5,
                pmt_pid: 0x0500,
                pcr_pid: 0x0510,
                service_type: 0x01,
                provider: "IronNet",
                service_name: "Canal5",
                scrambled: false,
                streams: vec![
                    EsConfig {
                        stream_type: 0x24,
                        pid: 0x0510,
                    },
                    EsConfig {
                        stream_type: 0x0F,
                        pid: 0x0511,
                    },
                ],
            },
        ],
        nit_ts_entries: vec![NitTsEntry {
            ts_id: 1003,
            orig_nid: 8442,
            descriptors: vec![],
        }],
        duration_s: 25,
    };

    let data = generate_stream(&cfg);
    assert!(data.len() % 188 == 0);

    let path = dir.join(format!("{}.ts", cfg.name));
    fs::write(&path, &data).expect("escrever 03_cable_3svc.ts");
}

// ── Fixture: dvbt_hd (Fixture 04) ────────────────────────────────────────────

/// Gera `04_dvbt_hd.ts`:
/// - TSID=2001, NID=8468 (típico DVB-T UK)
/// - 1 serviço H.264 + MP2
/// - Duração: 30 s
#[test]
fn gen_real_04_dvbt_hd() {
    let dir = real_fixtures_dir();
    fs::create_dir_all(&dir).expect("criar diretório fixtures/real");

    let cfg = FixtureConfig {
        name: "04_dvbt_hd",
        tsid: 2001,
        network_id: 8468,
        network_name: "IronTerrestrial",
        nit_pid: 0x0010,
        programs: vec![ProgramConfig {
            program_number: 10,
            pmt_pid: 0x0100,
            pcr_pid: 0x0110,
            service_type: 0x01,
            provider: "IronBroadcast",
            service_name: "IronDTT HD",
            scrambled: false,
            streams: vec![
                EsConfig {
                    stream_type: 0x1B,
                    pid: 0x0110,
                }, // H.264 video
                EsConfig {
                    stream_type: 0x04,
                    pid: 0x0111,
                }, // MPEG-2 Audio (MP2)
            ],
        }],
        nit_ts_entries: vec![NitTsEntry {
            ts_id: 2001,
            orig_nid: 8468,
            descriptors: vec![],
        }],
        duration_s: 30,
    };

    let data = generate_stream(&cfg);
    assert!(data.len() % 188 == 0);

    let path = dir.join(format!("{}.ts", cfg.name));
    fs::write(&path, &data).expect("escrever 04_dvbt_hd.ts");
}

// ── Fixture: dvbs_sd (Fixture 05) ────────────────────────────────────────────

/// Gera `05_dvbs_sd.ts`:
/// - TSID=3001, NID=318 (Eutelsat)
/// - 1 serviço MPEG-2 Video + MP2 (SD clássico)
/// - Duração: 15 s
#[test]
fn gen_real_05_dvbs_sd() {
    let dir = real_fixtures_dir();
    fs::create_dir_all(&dir).expect("criar diretório fixtures/real");

    let cfg = FixtureConfig {
        name: "05_dvbs_sd",
        tsid: 3001,
        network_id: 318,
        network_name: "IronSat",
        nit_pid: 0x0010,
        programs: vec![ProgramConfig {
            program_number: 20,
            pmt_pid: 0x0100,
            pcr_pid: 0x0110,
            service_type: 0x01,
            provider: "IronSat Broadcast",
            service_name: "IronSat SD",
            scrambled: false,
            streams: vec![
                EsConfig {
                    stream_type: 0x02,
                    pid: 0x0110,
                }, // MPEG-2 video
                EsConfig {
                    stream_type: 0x04,
                    pid: 0x0111,
                }, // MPEG-2 audio
            ],
        }],
        nit_ts_entries: vec![NitTsEntry {
            ts_id: 3001,
            orig_nid: 318,
            descriptors: vec![],
        }],
        duration_s: 15,
    };

    let data = generate_stream(&cfg);
    assert!(data.len() % 188 == 0);

    let path = dir.join(format!("{}.ts", cfg.name));
    fs::write(&path, &data).expect("escrever 05_dvbs_sd.ts");
}

// ── Fixture: multi_audio (Fixture 06) ────────────────────────────────────────

/// Gera `06_multi_audio.ts`:
/// - TSID=1004, NID=8442
/// - 1 serviço H.264 + 3 faixas AAC (por, eng, spa)
/// - Duração: 20 s
#[test]
fn gen_real_06_multi_audio() {
    let dir = real_fixtures_dir();
    fs::create_dir_all(&dir).expect("criar diretório fixtures/real");

    let cfg = FixtureConfig {
        name: "06_multi_audio",
        tsid: 1004,
        network_id: 8442,
        network_name: "IronCable",
        nit_pid: 0x0010,
        programs: vec![ProgramConfig {
            program_number: 6,
            pmt_pid: 0x0100,
            pcr_pid: 0x0110,
            service_type: 0x01,
            provider: "IronNet",
            service_name: "IronTV Multi",
            scrambled: false,
            streams: vec![
                EsConfig {
                    stream_type: 0x1B,
                    pid: 0x0110,
                }, // H.264 video
                EsConfig {
                    stream_type: 0x0F,
                    pid: 0x0111,
                }, // AAC POR
                EsConfig {
                    stream_type: 0x0F,
                    pid: 0x0112,
                }, // AAC ENG
                EsConfig {
                    stream_type: 0x0F,
                    pid: 0x0113,
                }, // AAC SPA
            ],
        }],
        nit_ts_entries: vec![NitTsEntry {
            ts_id: 1004,
            orig_nid: 8442,
            descriptors: vec![],
        }],
        duration_s: 20,
    };

    let data = generate_stream(&cfg);
    assert!(data.len() % 188 == 0);

    let path = dir.join(format!("{}.ts", cfg.name));
    fs::write(&path, &data).expect("escrever 06_multi_audio.ts");
}

// ── Fixture: scrambled (Fixture 07) ──────────────────────────────────────────

/// Gera `07_scrambled.ts`:
/// - TSID=1005, NID=8442
/// - 1 serviço H.264 + AAC, `free_ca_mode=true` (encriptado)
/// - Duração: 15 s
#[test]
fn gen_real_07_scrambled() {
    let dir = real_fixtures_dir();
    fs::create_dir_all(&dir).expect("criar diretório fixtures/real");

    let cfg = FixtureConfig {
        name: "07_scrambled",
        tsid: 1005,
        network_id: 8442,
        network_name: "IronCable",
        nit_pid: 0x0010,
        programs: vec![ProgramConfig {
            program_number: 7,
            pmt_pid: 0x0100,
            pcr_pid: 0x0110,
            service_type: 0x01,
            provider: "IronNet Premium",
            service_name: "IronTV Premium",
            scrambled: true,
            streams: vec![
                EsConfig {
                    stream_type: 0x1B,
                    pid: 0x0110,
                },
                EsConfig {
                    stream_type: 0x0F,
                    pid: 0x0111,
                },
            ],
        }],
        nit_ts_entries: vec![NitTsEntry {
            ts_id: 1005,
            orig_nid: 8442,
            descriptors: vec![],
        }],
        duration_s: 15,
    };

    let data = generate_stream(&cfg);
    assert!(data.len() % 188 == 0);

    let path = dir.join(format!("{}.ts", cfg.name));
    fs::write(&path, &data).expect("escrever 07_scrambled.ts");
}

// ── Fixture: fta_dual (Fixture 08) ───────────────────────────────────────────

/// Gera `08_fta_dual.ts`:
/// - TSID=1006, NID=8442
/// - 2 serviços FTA: IronFTA1 (H.264+AAC) e IronFTA2 (H.264+AC3)
/// - Duração: 30 s
#[test]
fn gen_real_08_fta_dual() {
    let dir = real_fixtures_dir();
    fs::create_dir_all(&dir).expect("criar diretório fixtures/real");

    let cfg = FixtureConfig {
        name: "08_fta_dual",
        tsid: 1006,
        network_id: 8442,
        network_name: "IronCable",
        nit_pid: 0x0010,
        programs: vec![
            ProgramConfig {
                program_number: 8,
                pmt_pid: 0x0100,
                pcr_pid: 0x0110,
                service_type: 0x01,
                provider: "IronFTA",
                service_name: "IronFTA1",
                scrambled: false,
                streams: vec![
                    EsConfig {
                        stream_type: 0x1B,
                        pid: 0x0110,
                    },
                    EsConfig {
                        stream_type: 0x0F,
                        pid: 0x0111,
                    },
                ],
            },
            ProgramConfig {
                program_number: 9,
                pmt_pid: 0x0200,
                pcr_pid: 0x0210,
                service_type: 0x01,
                provider: "IronFTA",
                service_name: "IronFTA2",
                scrambled: false,
                streams: vec![
                    EsConfig {
                        stream_type: 0x1B,
                        pid: 0x0210,
                    },
                    EsConfig {
                        stream_type: 0x81,
                        pid: 0x0211,
                    }, // AC-3
                ],
            },
        ],
        nit_ts_entries: vec![NitTsEntry {
            ts_id: 1006,
            orig_nid: 8442,
            descriptors: vec![],
        }],
        duration_s: 30,
    };

    let data = generate_stream(&cfg);
    assert!(data.len() % 188 == 0);

    let path = dir.join(format!("{}.ts", cfg.name));
    fs::write(&path, &data).expect("escrever 08_fta_dual.ts");
}

// ── Fixture: radio_only (Fixture 09) ─────────────────────────────────────────

/// Gera `09_radio_only.ts`:
/// - TSID=1007, NID=8442
/// - 3 serviços de rádio digital (AAC, service_type=0x02), sem vídeo
/// - Duração: 20 s
#[test]
fn gen_real_09_radio_only() {
    let dir = real_fixtures_dir();
    fs::create_dir_all(&dir).expect("criar diretório fixtures/real");

    let cfg = FixtureConfig {
        name: "09_radio_only",
        tsid: 1007,
        network_id: 8442,
        network_name: "IronRadio",
        nit_pid: 0x0010,
        programs: vec![
            ProgramConfig {
                program_number: 30,
                pmt_pid: 0x0300,
                pcr_pid: 0x0311,
                service_type: 0x02, // Digital radio
                provider: "IronRadio",
                service_name: "Radio1",
                scrambled: false,
                streams: vec![EsConfig {
                    stream_type: 0x0F,
                    pid: 0x0311,
                }],
            },
            ProgramConfig {
                program_number: 31,
                pmt_pid: 0x0400,
                pcr_pid: 0x0411,
                service_type: 0x02,
                provider: "IronRadio",
                service_name: "Radio2",
                scrambled: false,
                streams: vec![EsConfig {
                    stream_type: 0x0F,
                    pid: 0x0411,
                }],
            },
            ProgramConfig {
                program_number: 32,
                pmt_pid: 0x0500,
                pcr_pid: 0x0511,
                service_type: 0x02,
                provider: "IronRadio",
                service_name: "Radio3",
                scrambled: false,
                streams: vec![EsConfig {
                    stream_type: 0x0F,
                    pid: 0x0511,
                }],
            },
        ],
        nit_ts_entries: vec![NitTsEntry {
            ts_id: 1007,
            orig_nid: 8442,
            descriptors: vec![],
        }],
        duration_s: 20,
    };

    let data = generate_stream(&cfg);
    assert!(data.len() % 188 == 0);

    let path = dir.join(format!("{}.ts", cfg.name));
    fs::write(&path, &data).expect("escrever 09_radio_only.ts");
}

// ── Fixture: mixed_nit (Fixture 10) ──────────────────────────────────────────

/// Gera `10_mixed_nit.ts`:
/// - TSID=1008, NID=8442
/// - 2 serviços H.264+AAC
/// - NIT referencia 3 TSes diferentes (TSID=1001, 1002, 1003)
/// - Duração: 60 s
#[test]
fn gen_real_10_mixed_nit() {
    let dir = real_fixtures_dir();
    fs::create_dir_all(&dir).expect("criar diretório fixtures/real");

    let cfg = FixtureConfig {
        name: "10_mixed_nit",
        tsid: 1008,
        network_id: 8442,
        network_name: "IronCable",
        nit_pid: 0x0010,
        programs: vec![
            ProgramConfig {
                program_number: 50,
                pmt_pid: 0x0100,
                pcr_pid: 0x0110,
                service_type: 0x01,
                provider: "IronNet",
                service_name: "IronMix1",
                scrambled: false,
                streams: vec![
                    EsConfig {
                        stream_type: 0x1B,
                        pid: 0x0110,
                    },
                    EsConfig {
                        stream_type: 0x0F,
                        pid: 0x0111,
                    },
                ],
            },
            ProgramConfig {
                program_number: 51,
                pmt_pid: 0x0200,
                pcr_pid: 0x0210,
                service_type: 0x01,
                provider: "IronNet",
                service_name: "IronMix2",
                scrambled: false,
                streams: vec![
                    EsConfig {
                        stream_type: 0x24,
                        pid: 0x0210,
                    },
                    EsConfig {
                        stream_type: 0x11,
                        pid: 0x0211,
                    },
                ],
            },
        ],
        nit_ts_entries: vec![
            NitTsEntry {
                ts_id: 1001,
                orig_nid: 8442,
                descriptors: vec![],
            },
            NitTsEntry {
                ts_id: 1002,
                orig_nid: 8442,
                descriptors: vec![],
            },
            NitTsEntry {
                ts_id: 1003,
                orig_nid: 8442,
                descriptors: vec![],
            },
        ],
        duration_s: 60,
    };

    let data = generate_stream(&cfg);
    assert!(data.len() % 188 == 0);

    let path = dir.join(format!("{}.ts", cfg.name));
    fs::write(&path, &data).expect("escrever 10_mixed_nit.ts");
}
