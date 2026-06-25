//! `StreamProbe` — remonta PES e despacha parsers de codec.
//!
//! SPEC-MI-002

use std::collections::HashMap;

use bytes::Bytes;

use crate::demux::PesData;
use crate::Pid;

use super::aac::probe_aac;
use super::ac3::probe_ac3;
use super::avc::probe_avc;
use super::error::MediaInfoError;
use super::hevc::probe_hevc;
use super::model::{ElementaryCodecInfo, MediaInfoCodecSnapshot, StreamKind};
use super::mpeg2video::probe_mpeg2video;
use super::mpegaudio::probe_mpegaudio;

/// Metadados estáticos de um ES para o probe.
///
/// SPEC-MI-002
#[derive(Debug, Clone)]
pub struct ProbeStreamMeta {
    pub stream_type: u8,
    pub menu_id: u16,
    pub language: Option<String>,
    pub encrypted: bool,
    pub is_latm: bool,
    pub is_private_ac3: bool,
}

struct PidProbeState {
    meta: ProbeStreamMeta,
    buffer: Vec<u8>,
    packet_count: u32,
    complete: bool,
}

/// Analisador de cabeçalhos de codec por PID.
///
/// SPEC-MI-002
pub struct StreamProbe {
    pids: HashMap<Pid, PidProbeState>,
    snapshot: MediaInfoCodecSnapshot,
    /// PIDs concluídos que devem ser desregistrados do demux.
    pub deregister_queue: Vec<Pid>,
}

impl Default for StreamProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamProbe {
    /// Cria um probe vazio.
    ///
    /// SPEC-MI-002
    pub fn new() -> Self {
        Self {
            pids: HashMap::new(),
            snapshot: MediaInfoCodecSnapshot::default(),
            deregister_queue: Vec::new(),
        }
    }

    /// Registra um PID para análise.
    ///
    /// SPEC-MI-002
    pub fn register_pid(&mut self, pid: Pid, meta: ProbeStreamMeta) {
        let kind = stream_kind_from_type(meta.stream_type, meta.is_private_ac3);
        let mut info = ElementaryCodecInfo {
            stream_type: Some(meta.stream_type),
            menu_id: Some(meta.menu_id),
            language: meta.language.clone(),
            encrypted: meta.encrypted,
            kind: Some(kind),
            ..Default::default()
        };
        apply_static_labels(&mut info, &meta);
        self.snapshot.streams.insert(pid, info);
        self.pids.insert(
            pid,
            PidProbeState {
                meta,
                buffer: Vec::new(),
                packet_count: 0,
                complete: false,
            },
        );
    }

    /// Remove estado de um PID.
    ///
    /// SPEC-MI-002
    pub fn deregister_pid(&mut self, pid: Pid) {
        self.pids.remove(&pid);
    }

    /// Limpa todo o estado (troca de fonte).
    ///
    /// SPEC-MI-002
    pub fn reset(&mut self) {
        self.pids.clear();
        self.snapshot = MediaInfoCodecSnapshot::default();
        self.deregister_queue.clear();
    }

    /// Processa fragmento PES.
    ///
    /// SPEC-MI-002
    pub fn push(&mut self, data: PesData) {
        if !self.pids.contains_key(&data.pid) {
            return;
        }

        {
            let Some(state) = self.pids.get_mut(&data.pid) else {
                return;
            };
            if state.complete {
                return;
            }
            state.packet_count += 1;
            if state.packet_count > 512 {
                state.complete = true;
                self.mark_complete(data.pid);
                self.deregister_queue.push(data.pid);
                return;
            }
        }

        let parse_before_clear = if data.pusi {
            let parse = self
                .pids
                .get(&data.pid)
                .is_some_and(|state| !state.buffer.is_empty());
            if let Some(state) = self.pids.get_mut(&data.pid) {
                state.buffer.clear();
                if let Some(payload) = strip_pes_header(&data.data) {
                    state.buffer.extend_from_slice(&payload);
                }
            }
            parse
        } else {
            if let Some(state) = self.pids.get_mut(&data.pid) {
                state.buffer.extend_from_slice(&data.data);
            }
            false
        };

        if parse_before_clear {
            self.try_parse(data.pid);
        }

        let buffer_len = self
            .pids
            .get(&data.pid)
            .map(|state| state.buffer.len())
            .unwrap_or(0);
        if buffer_len >= 4096 {
            self.try_parse(data.pid);
            if self
                .snapshot
                .streams
                .get(&data.pid)
                .is_some_and(|info| info.probe_complete)
            {
                if let Some(state) = self.pids.get_mut(&data.pid) {
                    state.complete = true;
                }
                self.deregister_queue.push(data.pid);
            }
        }
    }

    fn try_parse(&mut self, pid: Pid) {
        let (buffer, meta) = {
            let Some(state) = self.pids.get(&pid) else {
                return;
            };
            (state.buffer.clone(), state.meta.clone())
        };
        if buffer.is_empty() {
            return;
        }
        let mut info = self.snapshot.streams.get(&pid).cloned().unwrap_or_default();
        let result = dispatch_probe(&buffer, &meta, &mut info);
        if result.is_ok() || info.format.is_some() {
            info.probe_complete = true;
            self.snapshot.streams.insert(pid, info);
            if let Some(state) = self.pids.get_mut(&pid) {
                state.complete = true;
            }
            self.deregister_queue.push(pid);
        }
    }

    fn mark_complete(&mut self, pid: Pid) {
        if let Some(info) = self.snapshot.streams.get_mut(&pid) {
            info.probe_complete = true;
        }
    }

    /// Retorna snapshot atual.
    ///
    /// SPEC-MI-003
    pub fn snapshot(&self) -> MediaInfoCodecSnapshot {
        self.snapshot.clone()
    }

    /// Drena fila de PIDs a desregistrar.
    pub fn take_deregister_queue(&mut self) -> Vec<Pid> {
        std::mem::take(&mut self.deregister_queue)
    }
}

fn strip_pes_header(data: &Bytes) -> Option<Vec<u8>> {
    if data.len() < 9 || data[0] != 0x00 || data[1] != 0x00 || data[2] != 0x01 {
        return Some(data.to_vec());
    }
    let stream_id = data[3];
    if stream_id == 0xBE || stream_id == 0xBF {
        return None;
    }
    let pes_header_len = data[8] as usize;
    let offset = 9 + pes_header_len;
    if offset > data.len() {
        return None;
    }
    Some(data[offset..].to_vec())
}

fn stream_kind_from_type(stream_type: u8, private_ac3: bool) -> StreamKind {
    match stream_type {
        0x01 | 0x02 | 0x1B | 0x24 => StreamKind::Video,
        0x03 | 0x04 | 0x0F | 0x11 | 0x81 | 0x87 => StreamKind::Audio,
        0x06 if private_ac3 => StreamKind::Audio,
        0x06 => StreamKind::Data,
        _ => StreamKind::Data,
    }
}

fn apply_static_labels(info: &mut ElementaryCodecInfo, meta: &ProbeStreamMeta) {
    use crate::tables::pmt::stream_type_label;
    if meta.encrypted {
        info.format = Some(stream_type_label(meta.stream_type).to_string());
        return;
    }
    match meta.stream_type {
        0x1B => {
            info.format = Some("AVC".to_string());
            info.format_info = Some("Advanced Video Codec".to_string());
        }
        0x24 => {
            info.format = Some("HEVC".to_string());
            info.format_info = Some("High Efficiency Video Coding".to_string());
        }
        0x02 => info.format = Some("MPEG-2 Video".to_string()),
        0x11 => {
            info.format = Some("AAC LC".to_string());
            info.format_info = Some("Advanced Audio Codec Low Complexity".to_string());
            info.muxing_mode = Some("LATM".to_string());
        }
        0x0F => {
            info.format = Some("AAC LC".to_string());
            info.muxing_mode = Some("ADTS".to_string());
        }
        0x04 => info.format = Some("MPEG Audio".to_string()),
        0x81 | 0x06 if meta.is_private_ac3 => {
            info.format = Some("AC-3".to_string());
            info.format_info = Some("Audio Coding 3".to_string());
            info.commercial_name = Some("Dolby Digital".to_string());
        }
        _ => {}
    }
}

fn dispatch_probe(
    data: &[u8],
    meta: &ProbeStreamMeta,
    info: &mut ElementaryCodecInfo,
) -> Result<(), MediaInfoError> {
    if meta.encrypted {
        return Ok(());
    }
    match meta.stream_type {
        0x1B => probe_avc(data, info),
        0x24 => probe_hevc(data, info),
        0x02 => probe_mpeg2video(data, info),
        0x03 | 0x04 => probe_mpegaudio(data, info),
        0x0F => probe_aac(data, false, info),
        0x11 => probe_aac(data, true, info),
        0x81 => probe_ac3(data, info),
        0x06 if meta.is_private_ac3 => probe_ac3(data, info),
        _ => Err(MediaInfoError::UnsupportedCodec),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_mi_002_probe_register_and_reset() {
        let mut probe = StreamProbe::new();
        probe.register_pid(
            0x100,
            ProbeStreamMeta {
                stream_type: 0x1B,
                menu_id: 1,
                language: None,
                encrypted: false,
                is_latm: false,
                is_private_ac3: false,
            },
        );
        assert!(probe.snapshot().get(0x100).is_some());
        probe.reset();
        assert!(probe.snapshot().streams.is_empty());
    }
}
