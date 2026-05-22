//! Snapshot tests de PAT/PMT/SDT/NIT contra as 10 fixtures reais.
//!
//! Cada fixture é um arquivo `.ts` em `crates/ts/tests/fixtures/real/`.
//! Os testes varrem os pacotes TS, extraem as seções PSI/SI e verificam
//! campos específicos de cada tabela, funcionando como regressão contra
//! mudanças nos parsers.
//!
//! SPEC-TABLE-001 · SPEC-TABLE-002 · SPEC-TABLE-003 · SPEC-TABLE-004

use std::fs;
use std::path::PathBuf;
use ts::tables::{Nit, Pat, Pmt, Sdt};
use ts::verify_crc32_mpeg2;

// ── Helpers de extração ───────────────────────────────────────────────────────

fn real_fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("real")
}

/// Varre um stream MPEG-TS e retorna a **primeira** seção PSI/SI encontrada
/// no PID especificado, incluindo cabeçalho de 3 bytes e CRC-32 de 4 bytes.
///
/// A função lida apenas com seções que cabem inteiramente em um único pacote
/// TS, que é suficiente para as fixtures geradas pelo gerador sintético.
fn find_first_section(stream: &[u8], target_pid: u16) -> Option<Vec<u8>> {
    for pkt in stream.chunks(188) {
        if pkt.len() < 188 {
            break;
        }
        if pkt[0] != 0x47 {
            continue;
        }

        let pid = (((pkt[1] & 0x1F) as u16) << 8) | pkt[2] as u16;
        let pusi = (pkt[1] & 0x40) != 0;
        let afc = (pkt[3] >> 4) & 0x03;

        if pid != target_pid || !pusi {
            continue;
        }
        if (afc & 0x01) == 0 {
            // payload não presente
            continue;
        }

        // Calcular início do payload após o cabeçalho TS (4 bytes)
        // e, se AFC bit 1 estiver setado, após a adaptation field.
        let payload_start = if (afc & 0x02) != 0 {
            let afl = pkt[4] as usize;
            4 + 1 + afl
        } else {
            4
        };

        if payload_start + 1 >= 188 {
            continue;
        }

        let payload = &pkt[payload_start..];
        // pointer_field indica onde a seção começa dentro do payload
        let pointer = payload[0] as usize;
        let sec_start = 1 + pointer;

        if sec_start + 3 > payload.len() {
            continue;
        }

        let table_id = payload[sec_start];
        if table_id == 0xFF {
            // padding / stuffing
            continue;
        }

        let section_length =
            (((payload[sec_start + 1] & 0x0F) as usize) << 8) | (payload[sec_start + 2] as usize);
        let total = 3 + section_length;

        if sec_start + total > payload.len() {
            // seção ultrapassa este pacote — não esperado nas fixtures sintéticas
            continue;
        }

        return Some(payload[sec_start..sec_start + total].to_vec());
    }
    None
}

/// Lê uma fixture e extrai a primeira seção PAT (PID 0x0000).
fn load_pat(name: &str) -> Pat {
    let stream = fs::read(real_fixtures_dir().join(format!("{name}.ts")))
        .unwrap_or_else(|_| panic!("fixture {name}.ts não encontrada — execute gen_real_* antes"));

    let section = find_first_section(&stream, 0x0000)
        .unwrap_or_else(|| panic!("{name}: seção PAT não encontrada no stream"));

    assert!(
        verify_crc32_mpeg2(&section),
        "{name}: CRC-32 da seção PAT é inválido"
    );

    // body = seção sem 3-byte header e sem 4-byte CRC
    let body = &section[3..section.len() - 4];
    Pat::from_section_body(body).unwrap_or_else(|e| panic!("{name}: falha ao parsear PAT: {e:?}"))
}

/// Lê uma fixture e extrai a primeira seção PMT para o PID especificado.
fn load_pmt(name: &str, pmt_pid: u16) -> Pmt {
    let stream = fs::read(real_fixtures_dir().join(format!("{name}.ts")))
        .unwrap_or_else(|_| panic!("fixture {name}.ts não encontrada"));

    let section = find_first_section(&stream, pmt_pid)
        .unwrap_or_else(|| panic!("{name}: seção PMT no PID 0x{pmt_pid:04X} não encontrada"));

    assert!(
        verify_crc32_mpeg2(&section),
        "{name}: CRC-32 da seção PMT (PID 0x{pmt_pid:04X}) é inválido"
    );

    let body = &section[3..section.len() - 4];
    Pmt::from_section_body(body).unwrap_or_else(|e| panic!("{name}: falha ao parsear PMT: {e:?}"))
}

/// Lê uma fixture e extrai a primeira seção SDT (PID 0x0011).
fn load_sdt(name: &str) -> Sdt {
    let stream = fs::read(real_fixtures_dir().join(format!("{name}.ts")))
        .unwrap_or_else(|_| panic!("fixture {name}.ts não encontrada"));

    let section = find_first_section(&stream, 0x0011)
        .unwrap_or_else(|| panic!("{name}: seção SDT não encontrada"));

    assert!(
        verify_crc32_mpeg2(&section),
        "{name}: CRC-32 da seção SDT é inválido"
    );

    Sdt::parse(&section).unwrap_or_else(|e| panic!("{name}: falha ao parsear SDT: {e:?}"))
}

/// Lê uma fixture e extrai a primeira seção NIT (PID 0x0010).
fn load_nit(name: &str) -> Nit {
    let stream = fs::read(real_fixtures_dir().join(format!("{name}.ts")))
        .unwrap_or_else(|_| panic!("fixture {name}.ts não encontrada"));

    let section = find_first_section(&stream, 0x0010)
        .unwrap_or_else(|| panic!("{name}: seção NIT não encontrada"));

    assert!(
        verify_crc32_mpeg2(&section),
        "{name}: CRC-32 da seção NIT é inválido"
    );

    Nit::parse(&section).unwrap_or_else(|e| panic!("{name}: falha ao parsear NIT: {e:?}"))
}

// ── Fixture 01: cable_sd_1svc ─────────────────────────────────────────────────

/// PAT snapshot — fixture 01: single SD cable service.
///
/// SPEC-TABLE-001
#[test]
fn snap_01_cable_sd_1svc_pat() {
    let pat = load_pat("01_cable_sd_1svc");

    assert_eq!(pat.transport_stream_id, 1001, "TSID deve ser 1001");
    assert_eq!(pat.version, 0, "version deve ser 0");
    assert!(pat.current_next, "current_next deve ser true");
    assert_eq!(pat.nit_pid(), Some(0x0010), "NIT PID deve ser 0x0010");

    let pmt_pids: Vec<u16> = pat.pmt_pids().collect();
    assert_eq!(
        pmt_pids,
        vec![0x0100],
        "deve haver exatamente 1 PMT PID: 0x0100"
    );
}

/// PMT snapshot — fixture 01: H.264 + AAC.
///
/// SPEC-TABLE-002
#[test]
fn snap_01_cable_sd_1svc_pmt() {
    let pmt = load_pmt("01_cable_sd_1svc", 0x0100);

    assert_eq!(pmt.program_number, 1);
    assert_eq!(pmt.pcr_pid, 0x0110);
    assert_eq!(pmt.streams.len(), 2, "PMT deve ter 2 streams");

    assert_eq!(pmt.streams[0].stream_type, 0x1B, "stream 0: H.264");
    assert_eq!(pmt.streams[0].elementary_pid, 0x0110);
    assert_eq!(pmt.streams[1].stream_type, 0x0F, "stream 1: AAC");
    assert_eq!(pmt.streams[1].elementary_pid, 0x0111);
}

/// SDT snapshot — fixture 01: service name + scramble status.
///
/// SPEC-TABLE-004
#[test]
fn snap_01_cable_sd_1svc_sdt() {
    let sdt = load_sdt("01_cable_sd_1svc");

    assert_eq!(sdt.transport_stream_id, 1001);
    assert_eq!(sdt.original_network_id, 8442);
    assert!(sdt.actual, "SDT deve ser actual (table_id=0x42)");
    assert_eq!(sdt.services.len(), 1);

    let svc = &sdt.services[0];
    assert_eq!(svc.service_id, 1);
    assert!(!svc.free_ca_mode, "serviço deve ser FTA");
    assert_eq!(svc.service_name.as_deref(), Some("IronTV SD"));
    assert_eq!(svc.provider_name.as_deref(), Some("IronNet"));
    assert_eq!(svc.service_type, Some(0x01));
}

/// NIT snapshot — fixture 01: network name + 1 TS entry.
///
/// SPEC-TABLE-003
#[test]
fn snap_01_cable_sd_1svc_nit() {
    let nit = load_nit("01_cable_sd_1svc");

    assert_eq!(nit.network_id, 8442);
    assert!(nit.actual, "NIT deve ser actual (table_id=0x40)");
    assert_eq!(nit.network_name.as_deref(), Some("IronCable"));
    assert_eq!(nit.transport_streams.len(), 1);
    assert_eq!(nit.transport_streams[0].transport_stream_id, 1001);
    assert_eq!(nit.transport_streams[0].original_network_id, 8442);
}

// ── Fixture 02: cable_hd_1svc ─────────────────────────────────────────────────

#[test]
fn snap_02_cable_hd_1svc_pat() {
    let pat = load_pat("02_cable_hd_1svc");
    assert_eq!(pat.transport_stream_id, 1002);
    assert_eq!(pat.nit_pid(), Some(0x0010));
    let pmt_pids: Vec<u16> = pat.pmt_pids().collect();
    assert_eq!(pmt_pids, vec![0x0200]);
}

#[test]
fn snap_02_cable_hd_1svc_pmt() {
    let pmt = load_pmt("02_cable_hd_1svc", 0x0200);
    assert_eq!(pmt.program_number, 2);
    assert_eq!(pmt.pcr_pid, 0x0210);
    assert_eq!(pmt.streams.len(), 2);
    assert_eq!(pmt.streams[0].stream_type, 0x24, "HEVC");
    assert_eq!(pmt.streams[0].elementary_pid, 0x0210);
    assert_eq!(pmt.streams[1].stream_type, 0x11, "AAC-LATM");
    assert_eq!(pmt.streams[1].elementary_pid, 0x0211);
}

#[test]
fn snap_02_cable_hd_1svc_sdt() {
    let sdt = load_sdt("02_cable_hd_1svc");
    assert_eq!(sdt.transport_stream_id, 1002);
    assert_eq!(sdt.original_network_id, 8442);
    assert_eq!(sdt.services.len(), 1);
    let svc = &sdt.services[0];
    assert_eq!(svc.service_name.as_deref(), Some("IronTV HD"));
    assert!(!svc.free_ca_mode);
}

#[test]
fn snap_02_cable_hd_1svc_nit() {
    let nit = load_nit("02_cable_hd_1svc");
    assert_eq!(nit.network_id, 8442);
    assert_eq!(nit.network_name.as_deref(), Some("IronCable"));
    assert_eq!(nit.transport_streams.len(), 1);
    assert_eq!(nit.transport_streams[0].transport_stream_id, 1002);
}

// ── Fixture 03: cable_3svc ────────────────────────────────────────────────────

#[test]
fn snap_03_cable_3svc_pat() {
    let pat = load_pat("03_cable_3svc");
    assert_eq!(pat.transport_stream_id, 1003);
    let pmt_pids: Vec<u16> = pat.pmt_pids().collect();
    assert_eq!(pmt_pids, vec![0x0300, 0x0400, 0x0500], "3 PMT PIDs");
}

#[test]
fn snap_03_cable_3svc_pmt_canal3() {
    let pmt = load_pmt("03_cable_3svc", 0x0300);
    assert_eq!(pmt.program_number, 3);
    assert_eq!(pmt.pcr_pid, 0x0310);
    assert_eq!(pmt.streams.len(), 2);
    assert_eq!(pmt.streams[0].stream_type, 0x1B); // H.264
    assert_eq!(pmt.streams[1].stream_type, 0x0F); // AAC
}

#[test]
fn snap_03_cable_3svc_pmt_canal4() {
    let pmt = load_pmt("03_cable_3svc", 0x0400);
    assert_eq!(pmt.program_number, 4);
    assert_eq!(pmt.pcr_pid, 0x0410);
    assert_eq!(pmt.streams.len(), 2);
    assert_eq!(pmt.streams[0].stream_type, 0x1B); // H.264
    assert_eq!(pmt.streams[1].stream_type, 0x81); // AC-3
}

#[test]
fn snap_03_cable_3svc_pmt_canal5() {
    let pmt = load_pmt("03_cable_3svc", 0x0500);
    assert_eq!(pmt.program_number, 5);
    assert_eq!(pmt.pcr_pid, 0x0510);
    assert_eq!(pmt.streams.len(), 2);
    assert_eq!(pmt.streams[0].stream_type, 0x24); // HEVC
    assert_eq!(pmt.streams[1].stream_type, 0x0F); // AAC
}

#[test]
fn snap_03_cable_3svc_sdt() {
    let sdt = load_sdt("03_cable_3svc");
    assert_eq!(sdt.transport_stream_id, 1003);
    assert_eq!(sdt.services.len(), 3);

    let names: Vec<Option<String>> = sdt
        .services
        .iter()
        .map(|s| s.service_name.clone())
        .collect();
    assert!(names.contains(&Some("Canal3".to_string())));
    assert!(names.contains(&Some("Canal4".to_string())));
    assert!(names.contains(&Some("Canal5".to_string())));
}

#[test]
fn snap_03_cable_3svc_nit() {
    let nit = load_nit("03_cable_3svc");
    assert_eq!(nit.network_id, 8442);
    assert_eq!(nit.transport_streams.len(), 1);
    assert_eq!(nit.transport_streams[0].transport_stream_id, 1003);
}

// ── Fixture 04: dvbt_hd ───────────────────────────────────────────────────────

#[test]
fn snap_04_dvbt_hd_pat() {
    let pat = load_pat("04_dvbt_hd");
    assert_eq!(pat.transport_stream_id, 2001);
    let pmt_pids: Vec<u16> = pat.pmt_pids().collect();
    assert_eq!(pmt_pids, vec![0x0100]);
}

#[test]
fn snap_04_dvbt_hd_pmt() {
    let pmt = load_pmt("04_dvbt_hd", 0x0100);
    assert_eq!(pmt.program_number, 10);
    assert_eq!(pmt.pcr_pid, 0x0110);
    assert_eq!(pmt.streams.len(), 2);
    assert_eq!(pmt.streams[0].stream_type, 0x1B); // H.264
    assert_eq!(pmt.streams[1].stream_type, 0x04); // MPEG-2 Audio / MP2
}

#[test]
fn snap_04_dvbt_hd_sdt() {
    let sdt = load_sdt("04_dvbt_hd");
    assert_eq!(sdt.transport_stream_id, 2001);
    assert_eq!(sdt.original_network_id, 8468);
    assert_eq!(sdt.services.len(), 1);
    assert_eq!(sdt.services[0].service_name.as_deref(), Some("IronDTT HD"));
}

#[test]
fn snap_04_dvbt_hd_nit() {
    let nit = load_nit("04_dvbt_hd");
    assert_eq!(nit.network_id, 8468);
    assert_eq!(nit.network_name.as_deref(), Some("IronTerrestrial"));
    assert_eq!(nit.transport_streams.len(), 1);
    assert_eq!(nit.transport_streams[0].transport_stream_id, 2001);
    assert_eq!(nit.transport_streams[0].original_network_id, 8468);
}

// ── Fixture 05: dvbs_sd ───────────────────────────────────────────────────────

#[test]
fn snap_05_dvbs_sd_pat() {
    let pat = load_pat("05_dvbs_sd");
    assert_eq!(pat.transport_stream_id, 3001);
    let pmt_pids: Vec<u16> = pat.pmt_pids().collect();
    assert_eq!(pmt_pids, vec![0x0100]);
}

#[test]
fn snap_05_dvbs_sd_pmt() {
    let pmt = load_pmt("05_dvbs_sd", 0x0100);
    assert_eq!(pmt.program_number, 20);
    assert_eq!(pmt.pcr_pid, 0x0110);
    assert_eq!(pmt.streams.len(), 2);
    assert_eq!(pmt.streams[0].stream_type, 0x02); // MPEG-2 Video
    assert_eq!(pmt.streams[1].stream_type, 0x04); // MPEG-2 Audio
}

#[test]
fn snap_05_dvbs_sd_sdt() {
    let sdt = load_sdt("05_dvbs_sd");
    assert_eq!(sdt.transport_stream_id, 3001);
    assert_eq!(sdt.original_network_id, 318);
    assert_eq!(sdt.services[0].service_name.as_deref(), Some("IronSat SD"));
    assert!(!sdt.services[0].free_ca_mode);
}

#[test]
fn snap_05_dvbs_sd_nit() {
    let nit = load_nit("05_dvbs_sd");
    assert_eq!(nit.network_id, 318);
    assert_eq!(nit.network_name.as_deref(), Some("IronSat"));
    assert_eq!(nit.transport_streams[0].transport_stream_id, 3001);
    assert_eq!(nit.transport_streams[0].original_network_id, 318);
}

// ── Fixture 06: multi_audio ───────────────────────────────────────────────────

#[test]
fn snap_06_multi_audio_pat() {
    let pat = load_pat("06_multi_audio");
    assert_eq!(pat.transport_stream_id, 1004);
    assert_eq!(pat.pmt_pids().count(), 1);
}

#[test]
fn snap_06_multi_audio_pmt() {
    let pmt = load_pmt("06_multi_audio", 0x0100);
    assert_eq!(pmt.program_number, 6);
    assert_eq!(pmt.streams.len(), 4, "1 vídeo + 3 áudios");
    assert_eq!(pmt.streams[0].stream_type, 0x1B); // H.264
    assert_eq!(pmt.streams[0].elementary_pid, 0x0110);
    // Três faixas AAC
    assert!(pmt.streams[1..].iter().all(|s| s.stream_type == 0x0F));
    let audio_pids: Vec<u16> = pmt.streams[1..].iter().map(|s| s.elementary_pid).collect();
    assert_eq!(audio_pids, vec![0x0111, 0x0112, 0x0113]);
}

#[test]
fn snap_06_multi_audio_sdt() {
    let sdt = load_sdt("06_multi_audio");
    assert_eq!(sdt.transport_stream_id, 1004);
    assert_eq!(
        sdt.services[0].service_name.as_deref(),
        Some("IronTV Multi")
    );
}

#[test]
fn snap_06_multi_audio_nit() {
    let nit = load_nit("06_multi_audio");
    assert_eq!(nit.network_id, 8442);
    assert_eq!(nit.transport_streams[0].transport_stream_id, 1004);
}

// ── Fixture 07: scrambled ─────────────────────────────────────────────────────

#[test]
fn snap_07_scrambled_pat() {
    let pat = load_pat("07_scrambled");
    assert_eq!(pat.transport_stream_id, 1005);
    assert_eq!(pat.pmt_pids().count(), 1);
}

#[test]
fn snap_07_scrambled_pmt() {
    let pmt = load_pmt("07_scrambled", 0x0100);
    assert_eq!(pmt.program_number, 7);
    assert_eq!(pmt.streams.len(), 2);
    assert_eq!(pmt.streams[0].stream_type, 0x1B);
    assert_eq!(pmt.streams[1].stream_type, 0x0F);
}

#[test]
fn snap_07_scrambled_sdt() {
    let sdt = load_sdt("07_scrambled");
    assert_eq!(sdt.transport_stream_id, 1005);
    assert_eq!(sdt.services.len(), 1);
    let svc = &sdt.services[0];
    assert!(
        svc.free_ca_mode,
        "serviço deve ter free_ca_mode=true (scrambled)"
    );
    assert_eq!(svc.service_name.as_deref(), Some("IronTV Premium"));
    assert_eq!(svc.provider_name.as_deref(), Some("IronNet Premium"));
}

#[test]
fn snap_07_scrambled_nit() {
    let nit = load_nit("07_scrambled");
    assert_eq!(nit.network_id, 8442);
    assert_eq!(nit.transport_streams[0].transport_stream_id, 1005);
}

// ── Fixture 08: fta_dual ──────────────────────────────────────────────────────

#[test]
fn snap_08_fta_dual_pat() {
    let pat = load_pat("08_fta_dual");
    assert_eq!(pat.transport_stream_id, 1006);
    let pmt_pids: Vec<u16> = pat.pmt_pids().collect();
    assert_eq!(pmt_pids, vec![0x0100, 0x0200], "2 programas FTA");
}

#[test]
fn snap_08_fta_dual_pmt_svc8() {
    let pmt = load_pmt("08_fta_dual", 0x0100);
    assert_eq!(pmt.program_number, 8);
    assert_eq!(pmt.pcr_pid, 0x0110);
    assert_eq!(pmt.streams.len(), 2);
    assert_eq!(pmt.streams[0].stream_type, 0x1B); // H.264
    assert_eq!(pmt.streams[1].stream_type, 0x0F); // AAC
}

#[test]
fn snap_08_fta_dual_pmt_svc9() {
    let pmt = load_pmt("08_fta_dual", 0x0200);
    assert_eq!(pmt.program_number, 9);
    assert_eq!(pmt.pcr_pid, 0x0210);
    assert_eq!(pmt.streams.len(), 2);
    assert_eq!(pmt.streams[0].stream_type, 0x1B); // H.264
    assert_eq!(pmt.streams[1].stream_type, 0x81); // AC-3
}

#[test]
fn snap_08_fta_dual_sdt() {
    let sdt = load_sdt("08_fta_dual");
    assert_eq!(sdt.transport_stream_id, 1006);
    assert_eq!(sdt.services.len(), 2);
    assert!(sdt.services.iter().all(|s| !s.free_ca_mode), "ambos FTA");

    let names: Vec<Option<String>> = sdt
        .services
        .iter()
        .map(|s| s.service_name.clone())
        .collect();
    assert!(names.contains(&Some("IronFTA1".to_string())));
    assert!(names.contains(&Some("IronFTA2".to_string())));
}

#[test]
fn snap_08_fta_dual_nit() {
    let nit = load_nit("08_fta_dual");
    assert_eq!(nit.network_id, 8442);
    assert_eq!(nit.transport_streams.len(), 1);
    assert_eq!(nit.transport_streams[0].transport_stream_id, 1006);
}

// ── Fixture 09: radio_only ────────────────────────────────────────────────────

#[test]
fn snap_09_radio_only_pat() {
    let pat = load_pat("09_radio_only");
    assert_eq!(pat.transport_stream_id, 1007);
    let pmt_pids: Vec<u16> = pat.pmt_pids().collect();
    assert_eq!(pmt_pids.len(), 3, "3 programas de rádio");
    assert!(pmt_pids.contains(&0x0300));
    assert!(pmt_pids.contains(&0x0400));
    assert!(pmt_pids.contains(&0x0500));
}

#[test]
fn snap_09_radio_only_pmt_radio1() {
    let pmt = load_pmt("09_radio_only", 0x0300);
    assert_eq!(pmt.program_number, 30);
    assert_eq!(pmt.streams.len(), 1, "rádio: apenas 1 stream de áudio");
    assert_eq!(pmt.streams[0].stream_type, 0x0F); // AAC
}

#[test]
fn snap_09_radio_only_sdt() {
    let sdt = load_sdt("09_radio_only");
    assert_eq!(sdt.transport_stream_id, 1007);
    assert_eq!(sdt.services.len(), 3);
    // Todos devem ter service_type=0x02 (rádio digital)
    assert!(
        sdt.services.iter().all(|s| s.service_type == Some(0x02)),
        "todos os serviços devem ser do tipo 0x02 (rádio digital)"
    );

    let names: Vec<Option<String>> = sdt
        .services
        .iter()
        .map(|s| s.service_name.clone())
        .collect();
    assert!(names.contains(&Some("Radio1".to_string())));
    assert!(names.contains(&Some("Radio2".to_string())));
    assert!(names.contains(&Some("Radio3".to_string())));
}

#[test]
fn snap_09_radio_only_nit() {
    let nit = load_nit("09_radio_only");
    assert_eq!(nit.network_id, 8442);
    assert_eq!(nit.network_name.as_deref(), Some("IronRadio"));
    assert_eq!(nit.transport_streams.len(), 1);
    assert_eq!(nit.transport_streams[0].transport_stream_id, 1007);
}

// ── Fixture 10: mixed_nit ─────────────────────────────────────────────────────

#[test]
fn snap_10_mixed_nit_pat() {
    let pat = load_pat("10_mixed_nit");
    assert_eq!(pat.transport_stream_id, 1008);
    let pmt_pids: Vec<u16> = pat.pmt_pids().collect();
    assert_eq!(pmt_pids, vec![0x0100, 0x0200], "2 programas");
}

#[test]
fn snap_10_mixed_nit_pmt_svc50() {
    let pmt = load_pmt("10_mixed_nit", 0x0100);
    assert_eq!(pmt.program_number, 50);
    assert_eq!(pmt.pcr_pid, 0x0110);
    assert_eq!(pmt.streams.len(), 2);
    assert_eq!(pmt.streams[0].stream_type, 0x1B); // H.264
    assert_eq!(pmt.streams[1].stream_type, 0x0F); // AAC
}

#[test]
fn snap_10_mixed_nit_pmt_svc51() {
    let pmt = load_pmt("10_mixed_nit", 0x0200);
    assert_eq!(pmt.program_number, 51);
    assert_eq!(pmt.pcr_pid, 0x0210);
    assert_eq!(pmt.streams.len(), 2);
    assert_eq!(pmt.streams[0].stream_type, 0x24); // HEVC
    assert_eq!(pmt.streams[1].stream_type, 0x11); // AAC-LATM
}

#[test]
fn snap_10_mixed_nit_sdt() {
    let sdt = load_sdt("10_mixed_nit");
    assert_eq!(sdt.transport_stream_id, 1008);
    assert_eq!(sdt.services.len(), 2);

    let names: Vec<Option<String>> = sdt
        .services
        .iter()
        .map(|s| s.service_name.clone())
        .collect();
    assert!(names.contains(&Some("IronMix1".to_string())));
    assert!(names.contains(&Some("IronMix2".to_string())));
}

/// NIT com 3 transport streams — snapshot crítico para regressão.
///
/// SPEC-TABLE-003
#[test]
fn snap_10_mixed_nit_nit() {
    let nit = load_nit("10_mixed_nit");
    assert_eq!(nit.network_id, 8442);
    assert_eq!(nit.network_name.as_deref(), Some("IronCable"));

    assert_eq!(
        nit.transport_streams.len(),
        3,
        "NIT deve referenciar 3 transport streams"
    );

    let ts_ids: Vec<u16> = nit
        .transport_streams
        .iter()
        .map(|ts| ts.transport_stream_id)
        .collect();
    assert!(ts_ids.contains(&1001), "NIT deve conter TSID=1001");
    assert!(ts_ids.contains(&1002), "NIT deve conter TSID=1002");
    assert!(ts_ids.contains(&1003), "NIT deve conter TSID=1003");

    // Todos os TSes pertencem à mesma rede
    assert!(
        nit.transport_streams
            .iter()
            .all(|ts| ts.original_network_id == 8442),
        "todos os TSes devem ter original_network_id=8442"
    );
}
