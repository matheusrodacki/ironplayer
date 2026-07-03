//! Aba Tabelas — árvore PSI/SI achatada para o `ListView` do Slint.
//!
//! A hierarquia (tabela → programa/serviço → stream → descriptor) é
//! linearizada em `Vec<TreeRow>` conforme o conjunto de nós expandidos
//! (`toggled`, chaves estáveis derivadas de ids). Descriptors conhecidos são
//! decodificados para texto legível; desconhecidos caem em dump hex.

use std::collections::HashSet;
use std::fmt::Write as _;

use ts::tables::descriptor::Polarization;
use ts::tables::{
    aac_descriptor_profile_hint, ac3_descriptor_channel_hint, Descriptor, EitEvent,
    KnownDescriptor,
};

use crate::state::AppState;
use crate::TreeRow;

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

struct Tree<'a> {
    rows: Vec<TreeRow>,
    toggled: &'a HashSet<String>,
}

impl Tree<'_> {
    /// Nó raiz (tabela). Retorna `true` quando os filhos devem ser emitidos.
    fn root(&mut self, key: &str, label: &str, value: String, present: bool, children: bool) -> bool {
        let has_children = present && children;
        let expanded = has_children && self.toggled.contains(key);
        self.rows.push(TreeRow {
            key: key.into(),
            label: label.into(),
            value: value.into(),
            indent: 0,
            has_children,
            expanded,
            is_root: true,
            present,
        });
        expanded
    }

    /// Raiz ausente (tabela não recebida).
    fn absent(&mut self, key: &str, label: &str, detail: &str) {
        self.root(key, label, detail.to_string(), false, false);
    }

    /// Nó interno. Retorna `true` quando os filhos devem ser emitidos.
    fn node(&mut self, key: String, indent: i32, label: String, value: String, children: bool) -> bool {
        let expanded = children && self.toggled.contains(&key);
        self.rows.push(TreeRow {
            key: key.into(),
            label: label.into(),
            value: value.into(),
            indent,
            has_children: children,
            expanded,
            is_root: false,
            present: true,
        });
        expanded
    }

    fn leaf(&mut self, key: String, indent: i32, label: String, value: String) {
        self.node(key, indent, label, value, false);
    }

    fn descriptor(&mut self, key: String, indent: i32, d: &Descriptor) {
        self.leaf(
            key,
            indent,
            format!("0x{:02X} {}", d.tag, descriptor_tag_name(d.tag)),
            descriptor_summary(d),
        );
    }
}

// ---------------------------------------------------------------------------
// Árvore principal
// ---------------------------------------------------------------------------

/// Constrói a árvore PSI/SI achatada a partir do snapshot de tabelas.
pub fn build_psi_tree(st: &AppState, toggled: &HashSet<String>) -> Vec<TreeRow> {
    let mut tree = Tree {
        rows: Vec::with_capacity(64),
        toggled,
    };
    let t = &st.tables;

    // ── PAT ────────────────────────────────────────────────────────────────
    match &t.pat {
        Some(pat) => {
            let value = format!(
                "v{} · TSID {} (0x{:04X}) · {} programa(s)",
                pat.version,
                pat.transport_stream_id,
                pat.transport_stream_id,
                pat.programs.len()
            );
            if tree.root("pat", "PAT", value, true, !pat.programs.is_empty()) {
                for p in &pat.programs {
                    if p.program_number == 0 {
                        tree.leaf(
                            "pat/nit".into(),
                            1,
                            "NIT".into(),
                            format!("PID {} (0x{:04X})", p.pid, p.pid),
                        );
                    } else {
                        tree.leaf(
                            format!("pat/{}", p.program_number),
                            1,
                            format!("Programa {}", p.program_number),
                            format!("PMT PID {} (0x{:04X})", p.pid, p.pid),
                        );
                    }
                }
            }
        }
        None => tree.absent("pat", "PAT", "não recebida"),
    }

    // ── PMT ────────────────────────────────────────────────────────────────
    if t.pmts.is_empty() {
        tree.absent("pmt", "PMT", "não recebida");
    } else {
        let mut pmts: Vec<_> = t.pmts.values().collect();
        pmts.sort_by_key(|p| p.program_number);
        if tree.root("pmt", "PMT", format!("{} programa(s)", pmts.len()), true, true) {
            for pmt in pmts {
                let key = format!("pmt/{}", pmt.program_number);
                let open = tree.node(
                    key.clone(),
                    1,
                    format!("Programa {}", pmt.program_number),
                    format!(
                        "v{} · PCR PID {} (0x{:04X}) · {} ES",
                        pmt.version,
                        pmt.pcr_pid,
                        pmt.pcr_pid,
                        pmt.streams.len()
                    ),
                    true,
                );
                if !open {
                    continue;
                }
                for (i, d) in pmt.program_descriptors.iter().enumerate() {
                    tree.descriptor(format!("{key}/pd/{i}"), 2, d);
                }
                for s in &pmt.streams {
                    let skey = format!("{key}/es/{}", s.elementary_pid);
                    let sopen = tree.node(
                        skey.clone(),
                        2,
                        format!("PID {} (0x{:04X})", s.elementary_pid, s.elementary_pid),
                        format!("{} · type 0x{:02X}", s.label(), s.stream_type),
                        !s.descriptors.is_empty(),
                    );
                    if sopen {
                        for (i, d) in s.descriptors.iter().enumerate() {
                            tree.descriptor(format!("{skey}/d/{i}"), 3, d);
                        }
                    }
                }
            }
        }
    }

    // ── SDT ────────────────────────────────────────────────────────────────
    match &t.sdt {
        Some(sdt) => {
            let value = format!(
                "v{} · ONID {} · {} serviço(s)",
                sdt.version,
                sdt.original_network_id,
                sdt.services.len()
            );
            if tree.root("sdt", "SDT", value, true, !sdt.services.is_empty()) {
                for svc in &sdt.services {
                    let key = format!("sdt/{}", svc.service_id);
                    let name = svc.service_name.clone().unwrap_or_else(|| "—".into());
                    let open = tree.node(
                        key.clone(),
                        1,
                        format!("Serviço {} (0x{:04X})", svc.service_id, svc.service_id),
                        name,
                        true,
                    );
                    if !open {
                        continue;
                    }
                    if let Some(p) = svc.provider_name.as_ref().filter(|p| !p.is_empty()) {
                        tree.leaf(format!("{key}/prov"), 2, "Provedor".into(), p.clone());
                    }
                    if let Some(ty) = svc.service_type {
                        tree.leaf(
                            format!("{key}/type"),
                            2,
                            "Tipo".into(),
                            format!("0x{ty:02X} · {}", service_type_name(ty)),
                        );
                    }
                    tree.leaf(
                        format!("{key}/status"),
                        2,
                        "Status".into(),
                        format!(
                            "{:?}{}",
                            svc.running_status,
                            if svc.free_ca_mode { " · CA (scrambled)" } else { "" }
                        ),
                    );
                    tree.leaf(
                        format!("{key}/eit"),
                        2,
                        "EIT".into(),
                        format!(
                            "p/f {} · schedule {}",
                            sim_nao(svc.eit_present_following),
                            sim_nao(svc.eit_schedule_flag)
                        ),
                    );
                    for (i, d) in svc.descriptors.iter().enumerate() {
                        tree.descriptor(format!("{key}/d/{i}"), 2, d);
                    }
                }
            }
        }
        None => tree.absent("sdt", "SDT", "não recebida"),
    }

    // ── NIT ────────────────────────────────────────────────────────────────
    match &t.nit {
        Some(nit) => {
            let mut value = format!("v{} · rede {}", nit.version, nit.network_id);
            if let Some(name) = nit.network_name.as_ref().filter(|n| !n.is_empty()) {
                let _ = write!(value, " · \"{name}\"");
            }
            let children = !nit.network_descriptors.is_empty() || !nit.transport_streams.is_empty();
            if tree.root("nit", "NIT", value, true, children) {
                for (i, d) in nit.network_descriptors.iter().enumerate() {
                    tree.descriptor(format!("nit/nd/{i}"), 1, d);
                }
                for ts_entry in &nit.transport_streams {
                    let key = format!("nit/ts/{}", ts_entry.transport_stream_id);
                    let open = tree.node(
                        key.clone(),
                        1,
                        format!("TS {}", ts_entry.transport_stream_id),
                        format!("ONID {}", ts_entry.original_network_id),
                        !ts_entry.descriptors.is_empty(),
                    );
                    if open {
                        for (i, d) in ts_entry.descriptors.iter().enumerate() {
                            tree.descriptor(format!("{key}/d/{i}"), 2, d);
                        }
                    }
                }
            }
        }
        None => tree.absent("nit", "NIT", "não recebida"),
    }

    // ── EIT p/f ────────────────────────────────────────────────────────────
    if t.eit_pf.is_empty() {
        tree.absent("eit", "EIT p/f", "não recebida");
    } else {
        let mut services: Vec<_> = t.eit_pf.iter().collect();
        services.sort_by_key(|(id, _)| **id);
        if tree.root("eit", "EIT p/f", format!("{} serviço(s)", services.len()), true, true) {
            for (svc_id, (current, next)) in services {
                let key = format!("eit/{svc_id}");
                let open = tree.node(
                    key.clone(),
                    1,
                    format!("Serviço {svc_id}"),
                    current
                        .as_ref()
                        .and_then(|e| e.event_name.clone())
                        .unwrap_or_else(|| "—".into()),
                    true,
                );
                if open {
                    tree.leaf(format!("{key}/now"), 2, "Agora".into(), event_line(current));
                    tree.leaf(format!("{key}/next"), 2, "A seguir".into(), event_line(next));
                }
            }
        }
    }

    // ── TDT / TOT ──────────────────────────────────────────────────────────
    if t.tdt.is_none() && t.tot.is_none() {
        tree.absent("tdt", "TDT / TOT", "não recebida");
    } else {
        let utc = t
            .tot
            .as_ref()
            .map(|tot| tot.utc_time)
            .or_else(|| t.tdt.as_ref().map(|tdt| tdt.utc_time));
        let value = utc
            .map(|u| format!("UTC {}", u.format("%d/%m/%Y %H:%M:%S")))
            .unwrap_or_default();
        let has_offsets = t.tot.as_ref().is_some_and(|tot| !tot.local_time_offsets.is_empty());
        if tree.root("tdt", "TDT / TOT", value, true, has_offsets) {
            if let Some(tot) = &t.tot {
                for (i, off) in tot.local_time_offsets.iter().enumerate() {
                    let country = String::from_utf8_lossy(&off.country_code).to_string();
                    let (h, m) = off.local_time_offset_hhmm;
                    tree.leaf(
                        format!("tdt/off/{i}"),
                        1,
                        format!("Offset {country}"),
                        format!(
                            "região {} · UTC{}{:02}:{:02}",
                            off.country_region_id,
                            if off.local_offset_polarity { "-" } else { "+" },
                            bcd(h),
                            bcd(m)
                        ),
                    );
                }
            }
        }
    }

    // ── BAT ────────────────────────────────────────────────────────────────
    match &t.bat {
        Some(bat) => {
            let mut value = format!("v{} · bouquet {}", bat.version, bat.bouquet_id);
            if let Some(name) = bat.bouquet_name.as_ref().filter(|n| !n.is_empty()) {
                let _ = write!(value, " · \"{name}\"");
            }
            let children = !bat.bouquet_descriptors.is_empty() || !bat.transport_streams.is_empty();
            if tree.root("bat", "BAT", value, true, children) {
                for (i, d) in bat.bouquet_descriptors.iter().enumerate() {
                    tree.descriptor(format!("bat/bd/{i}"), 1, d);
                }
                for ts_entry in &bat.transport_streams {
                    let key = format!("bat/ts/{}", ts_entry.transport_stream_id);
                    let open = tree.node(
                        key.clone(),
                        1,
                        format!("TS {}", ts_entry.transport_stream_id),
                        format!("ONID {}", ts_entry.original_network_id),
                        !ts_entry.descriptors.is_empty(),
                    );
                    if open {
                        for (i, d) in ts_entry.descriptors.iter().enumerate() {
                            tree.descriptor(format!("{key}/d/{i}"), 2, d);
                        }
                    }
                }
            }
        }
        None => tree.absent("bat", "BAT", "não recebida"),
    }

    // ── CAT ────────────────────────────────────────────────────────────────
    match &t.cat {
        Some(cat) => {
            let value = format!("v{} · {} CA system(s)", cat.version, cat.ca_descriptors.len());
            let children = !cat.descriptors.is_empty();
            if tree.root("cat", "CAT", value, true, children) {
                for (i, ca) in cat.ca_descriptors.iter().enumerate() {
                    tree.leaf(
                        format!("cat/ca/{i}"),
                        1,
                        format!("CA 0x{:04X}", ca.ca_system_id),
                        format!(
                            "{} · EMM PID {} (0x{:04X})",
                            ca_system_name(ca.ca_system_id),
                            ca.ca_pid,
                            ca.ca_pid
                        ),
                    );
                }
                for (i, d) in cat.descriptors.iter().enumerate().filter(|(_, d)| d.tag != 0x09) {
                    tree.descriptor(format!("cat/d/{i}"), 1, d);
                }
            }
        }
        None => tree.absent("cat", "CAT", "não recebida"),
    }

    tree.rows
}

// ---------------------------------------------------------------------------
// Descriptors — nome e resumo legível
// ---------------------------------------------------------------------------

/// Nome do descriptor por tag (MPEG ISO 13818-1 + DVB EN 300 468).
fn descriptor_tag_name(tag: u8) -> &'static str {
    match tag {
        0x02 => "video_stream",
        0x03 => "audio_stream",
        0x05 => "registration",
        0x06 => "data_stream_alignment",
        0x09 => "CA",
        0x0A => "ISO_639_language",
        0x0B => "system_clock",
        0x0E => "maximum_bitrate",
        0x10 => "smoothing_buffer",
        0x11 => "STD",
        0x1C => "MPEG-4_audio",
        0x25 => "metadata_pointer",
        0x26 => "metadata",
        0x28 => "AVC_video",
        0x2A => "AVC_timing_HRD",
        0x38 => "HEVC_video",
        0x40 => "network_name",
        0x41 => "service_list",
        0x42 => "stuffing",
        0x43 => "satellite_delivery",
        0x44 => "cable_delivery",
        0x45 => "VBI_data",
        0x47 => "bouquet_name",
        0x48 => "service",
        0x49 => "country_availability",
        0x4A => "linkage",
        0x4D => "short_event",
        0x4E => "extended_event",
        0x50 => "component",
        0x52 => "stream_identifier",
        0x53 => "CA_identifier",
        0x54 => "content",
        0x55 => "parental_rating",
        0x56 => "teletext",
        0x58 => "local_time_offset",
        0x59 => "subtitling",
        0x5A => "terrestrial_delivery",
        0x5F => "private_data_specifier",
        0x62 => "frequency_list",
        0x66 => "data_broadcast_id",
        0x6A => "AC-3",
        0x73 => "DTS",
        0x7A => "enhanced_AC-3",
        0x7C => "AAC",
        0x7F => "extension",
        0x83 => "logical_channel",
        _ => "descriptor",
    }
}

/// Resumo de uma linha para o valor do nó do descriptor.
fn descriptor_summary(d: &Descriptor) -> String {
    match d.decode() {
        KnownDescriptor::NetworkName { name } => format!("\"{name}\""),
        KnownDescriptor::BouquetName { name } => format!("\"{name}\""),
        KnownDescriptor::ServiceList { services } => {
            let mut s = services
                .iter()
                .take(8)
                .map(|(id, ty)| format!("{id}:0x{ty:02X}"))
                .collect::<Vec<_>>()
                .join("  ");
            if services.len() > 8 {
                let _ = write!(s, " … ({} serviços)", services.len());
            }
            s
        }
        KnownDescriptor::Service {
            service_type,
            provider,
            name,
        } => format!("tipo 0x{service_type:02X} · \"{name}\" · \"{provider}\""),
        KnownDescriptor::ShortEvent { lang, name, text } => {
            let lang = String::from_utf8_lossy(&lang).to_string();
            if text.is_empty() {
                format!("[{lang}] \"{name}\"")
            } else {
                format!("[{lang}] \"{name}\" — {text}")
            }
        }
        KnownDescriptor::SatelliteDelivery {
            frequency_hz,
            orbital_position_tenths,
            west_east_flag,
            polarization,
            symbol_rate,
        } => format!(
            "{:.3} GHz · {}.{}°{} · pol {} · {} ksym/s",
            frequency_hz as f64 / 1e9,
            orbital_position_tenths / 10,
            orbital_position_tenths % 10,
            if west_east_flag { "E" } else { "W" },
            polarization_label(&polarization),
            symbol_rate / 1000
        ),
        KnownDescriptor::CableDelivery {
            frequency_hz,
            modulation,
            symbol_rate,
        } => format!(
            "{:.3} MHz · mod 0x{modulation:02X} · {} ksym/s",
            frequency_hz as f64 / 1e6,
            symbol_rate / 1000
        ),
        KnownDescriptor::TerrestrialDelivery {
            centre_frequency_hz,
            bandwidth_hz,
        } => format!(
            "{:.3} MHz · BW {} MHz",
            centre_frequency_hz as f64 / 1e6,
            bandwidth_hz / 1_000_000
        ),
        KnownDescriptor::LocalTimeOffset {
            country_code,
            local_time_offset_polarity,
            local_time_offset_h,
            local_time_offset_m,
            ..
        } => format!(
            "{country_code} UTC{}{:02}:{:02}",
            if local_time_offset_polarity { "-" } else { "+" },
            bcd(local_time_offset_h),
            bcd(local_time_offset_m)
        ),
        KnownDescriptor::Unknown { tag, data } => {
            custom_summary(tag, &data).unwrap_or_else(|| hex_summary(&data))
        }
    }
}

/// Decodificação rasa de descriptors frequentes que o `KnownDescriptor` não cobre.
fn custom_summary(tag: u8, data: &[u8]) -> Option<String> {
    match tag {
        // registration — format_identifier fourcc
        0x05 if data.len() >= 4 => Some(format!(
            "\"{}\"",
            String::from_utf8_lossy(&data[..4]).replace(['\u{0}', '\u{fffd}'], "?")
        )),
        // CA — sistema + PID ECM/EMM
        0x09 if data.len() >= 4 => {
            let sys = u16::from_be_bytes([data[0], data[1]]);
            let pid = u16::from_be_bytes([data[2], data[3]]) & 0x1FFF;
            Some(format!(
                "{} (0x{sys:04X}) · PID {pid} (0x{pid:04X})",
                ca_system_name(sys)
            ))
        }
        // ISO 639 — idioma(s) + audio_type
        0x0A if data.len() >= 4 => {
            let mut parts = Vec::new();
            let mut i = 0usize;
            while i + 4 <= data.len() {
                let lang = String::from_utf8_lossy(&data[i..i + 3]).to_string();
                parts.push(match data[i + 3] {
                    0x01 => format!("{lang} (clean effects)"),
                    0x02 => format!("{lang} (hearing impaired)"),
                    0x03 => format!("{lang} (audio description)"),
                    _ => lang,
                });
                i += 4;
            }
            Some(parts.join(" · "))
        }
        // maximum_bitrate — unidades de 50 bytes/s
        0x0E if data.len() >= 3 => {
            let raw = ((data[0] as u32 & 0x3F) << 16) | ((data[1] as u32) << 8) | data[2] as u32;
            Some(format!("{:.1} Mbps", raw as f64 * 50.0 * 8.0 / 1e6))
        }
        // stream_identifier — component_tag
        0x52 if !data.is_empty() => Some(format!("component_tag 0x{:02X}", data[0])),
        // teletext — idioma + tipo + página
        0x56 if data.len() >= 5 => {
            let lang = String::from_utf8_lossy(&data[..3]).to_string();
            let ttype = data[3] >> 3;
            let magazine = data[3] & 0x07;
            Some(format!(
                "{lang} · tipo {ttype} · página {}{:02X}",
                if magazine == 0 { 8 } else { magazine },
                data[4]
            ))
        }
        // subtitling — idioma + tipo + páginas
        0x59 if data.len() >= 8 => {
            let lang = String::from_utf8_lossy(&data[..3]).to_string();
            let comp = u16::from_be_bytes([data[4], data[5]]);
            let anc = u16::from_be_bytes([data[6], data[7]]);
            Some(format!(
                "{lang} · tipo 0x{:02X} · comp {comp} · anc {anc}",
                data[3]
            ))
        }
        // AC-3 / E-AC-3 — hint de canais
        0x6A | 0x7A => Some(match ac3_descriptor_channel_hint(data) {
            Some(ch) => format!("{ch} canais"),
            None => hex_summary(data),
        }),
        // AAC — perfil
        0x7C => aac_descriptor_profile_hint(data).map(|p| p.to_string()),
        // private_data_specifier
        0x5F if data.len() >= 4 => Some(format!(
            "0x{:08X}",
            u32::from_be_bytes([data[0], data[1], data[2], data[3]])
        )),
        _ => None,
    }
}

fn hex_summary(data: &[u8]) -> String {
    if data.is_empty() {
        return "(vazio)".into();
    }
    let mut s = String::with_capacity(data.len().min(16) * 3 + 16);
    for (i, b) in data.iter().take(16).enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{b:02X}");
    }
    if data.len() > 16 {
        let _ = write!(s, " … ({} bytes)", data.len());
    }
    s
}

// ---------------------------------------------------------------------------
// Helpers de rótulo
// ---------------------------------------------------------------------------

fn polarization_label(p: &Polarization) -> &'static str {
    match p {
        Polarization::LinearHorizontal => "H",
        Polarization::LinearVertical => "V",
        Polarization::CircularLeft => "L",
        Polarization::CircularRight => "R",
    }
}

/// Nome do tipo de serviço DVB (EN 300 468 tabela 87).
fn service_type_name(ty: u8) -> &'static str {
    match ty {
        0x01 => "TV SD (MPEG-2)",
        0x02 => "Rádio",
        0x03 => "Teletexto",
        0x0C => "Dados",
        0x10 => "MHP",
        0x11 => "TV HD (MPEG-2)",
        0x16 => "TV SD (AVC)",
        0x19 => "TV HD (AVC)",
        0x1F => "TV (HEVC)",
        0x20 => "TV UHD (HEVC)",
        _ => "reservado/privado",
    }
}

/// Nome comercial do CA system por faixa de `ca_system_id` (DVB/ETSI).
fn ca_system_name(sys: u16) -> &'static str {
    match sys {
        0x0100..=0x01FF => "Seca/Mediaguard",
        0x0500..=0x05FF => "Viaccess",
        0x0600..=0x06FF => "Irdeto",
        0x0900..=0x09FF => "NDS/VideoGuard",
        0x0B00..=0x0BFF => "Conax",
        0x0D00..=0x0DFF => "Cryptoworks",
        0x0E00..=0x0EFF => "PowerVu",
        0x1200..=0x12FF => "NagraVision (BellVu)",
        0x1800..=0x18FF => "NagraVision",
        0x4AE0..=0x4AE1 => "DRE-Crypt",
        0x5601..=0x5604 => "Verimatrix",
        _ => "CA system",
    }
}

fn sim_nao(v: bool) -> &'static str {
    if v {
        "sim"
    } else {
        "não"
    }
}

/// Decodifica um byte BCD (`0x30` → 30).
fn bcd(b: u8) -> u8 {
    (b >> 4) * 10 + (b & 0x0F)
}

/// Linha compacta de evento EIT (`nome · HH:MM · NN min`).
fn event_line(ev: &Option<EitEvent>) -> String {
    let Some(e) = ev else {
        return "—".into();
    };
    let mut s = e
        .event_name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("Evento {}", e.event_id));
    if let Some(start) = e.start_time {
        let _ = write!(s, " · {}", start.format("%H:%M"));
    }
    if let Some(dur) = e.duration_seconds {
        let _ = write!(s, " · {} min", dur / 60);
    }
    if let Some(desc) = e.short_description.as_ref().filter(|d| !d.is_empty()) {
        let _ = write!(s, " — {desc}");
    }
    s
}
