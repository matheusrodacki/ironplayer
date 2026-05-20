/// SPEC-TABLE — Dispatcher de tabelas PSI/SI para a UI.
///
/// Recebe [`CompleteSection`] do `SectionAssembler`, roteia por `table_id` e
/// emite [`TableEvent`] para o `AppState`.
use std::collections::HashMap;

use av::MediaCodec;
use bytes::Bytes;
use crossbeam_channel::{Receiver, Sender};
use tracing::{trace, warn};
use ts::tables::{Bat, Eit, Nit, Pat, Pmt, Sdt, Tdt};
use ts::{CompleteSection, Pid};
use ui::TableEvent;

use crate::channels::BoundedSender;

/// SPEC-TABLE
/// Comando de controle enviado ao `TsDemuxer` após parse de PAT/PMT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemuxCommand {
    /// Registra um PID de PMT descoberto na PAT.
    RegisterPmtPid(Pid),
    /// Registra um PID A/V descoberto na PMT.
    RegisterAvPid(Pid),
}

/// SPEC-TABLE
/// Comando de controle enviado ao `PesAssembler` após parse de PMT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PesCommand {
    /// Registra o codec de um elementary stream suportado.
    RegisterPid { pid: Pid, codec: MediaCodec },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SectionKey {
    pid: Pid,
    table_id: u8,
    extension: u16,
    section_number: u8,
}

/// SPEC-TABLE
/// Despacha seções PSI/SI completas para o `AppState`.
pub struct TableDispatcher {
    rx: Receiver<CompleteSection>,
    tx: BoundedSender<TableEvent>,
    demux_tx: Sender<DemuxCommand>,
    pes_tx: Sender<PesCommand>,
    last_sections: HashMap<SectionKey, Bytes>,
    /// Versão atual da PAT (SPEC-TABLE-001d).
    pat_version: Option<u8>,
    /// PIDs de PMT conhecidos da PAT atual (SPEC-TABLE-001d).
    pat_pmt_pids: std::collections::HashSet<Pid>,
    /// Versão mais recente de cada PMT por PID de PMT.
    pmt_versions: HashMap<Pid, u8>,
}

impl TableDispatcher {
    /// Cria um novo `TableDispatcher`.
    pub fn new(
        rx: Receiver<CompleteSection>,
        tx: BoundedSender<TableEvent>,
        demux_tx: Sender<DemuxCommand>,
        pes_tx: Sender<PesCommand>,
    ) -> Self {
        Self {
            rx,
            tx,
            demux_tx,
            pes_tx,
            last_sections: HashMap::new(),
            pat_version: None,
            pat_pmt_pids: std::collections::HashSet::new(),
            pmt_versions: HashMap::new(),
        }
    }

    /// Loop principal: drena `complete_sections` e despacha `TableEvent`.
    ///
    /// Termina quando o sender do canal `complete_sections` é fechado.
    pub fn run(mut self) {
        while let Ok(section) = self.rx.recv() {
            trace!(
                pid = section.pid,
                table_id = section.table_id,
                bytes = section.data.len(),
                "seção recebida"
            );
            self.process_section(section);
        }
    }

    /// Processa uma seção com deduplicação e despacho.
    ///
    /// Combina a verificação de seção repetida com o despacho para permitir
    /// testes unitários sem a dependência do canal `rx`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn process_section(&mut self, section: CompleteSection) {
        if self.is_repeated_section(&section) {
            return;
        }
        self.dispatch(section);
    }

    fn dispatch(&mut self, section: CompleteSection) {
        match section.table_id {
            0x00 => self.dispatch_pat(&section),
            0x02 => self.dispatch_pmt(&section),
            0x40 | 0x41 => self.dispatch_full_section("NIT", &section, Nit::parse, TableEvent::Nit),
            0x42 | 0x46 => self.dispatch_full_section("SDT", &section, Sdt::parse, TableEvent::Sdt),
            0x4A => self.dispatch_full_section("BAT", &section, Bat::parse, TableEvent::Bat),
            0x4E | 0x4F => self.dispatch_eit_pf(&section),
            0x70 => self.dispatch_tdt(&section),
            other => {
                trace!(
                    pid = section.pid,
                    table_id = other,
                    "table_id sem parser no dispatcher"
                );
            }
        }
    }

    fn dispatch_pat(&mut self, section: &CompleteSection) {
        let Some(body) = section_body(section) else {
            return;
        };

        match Pat::from_section_body(body) {
            Ok(pat) => {
                // SPEC-TABLE-001d: quando a versão da PAT muda, invalida o cache
                // de deduplicação para todos os PIDs de PMT conhecidos, forçando
                // o re-parse das PMTs quando chegarem novamente.
                let version_changed = self.pat_version != Some(pat.version);
                if version_changed {
                    if self.pat_version.is_some() {
                        tracing::info!(
                            old_version = self.pat_version,
                            new_version = pat.version,
                            "PAT version mudou — invalidando cache de PMTs"
                        );
                    }
                    // Remove entradas de dedup para todos os PIDs de PMT antigos.
                    let old_pmt_pids = std::mem::take(&mut self.pat_pmt_pids);
                    self.last_sections
                        .retain(|key, _| !old_pmt_pids.contains(&key.pid));
                    // Limpa versões de PMT para forçar re-registro dos streams A/V.
                    for pid in &old_pmt_pids {
                        self.pmt_versions.remove(pid);
                    }
                    self.pat_version = Some(pat.version);
                    self.pat_pmt_pids = pat.pmt_pids().collect();
                }
                for pid in pat.pmt_pids() {
                    self.send_demux_command(DemuxCommand::RegisterPmtPid(pid));
                }
                self.tx.try_send(TableEvent::Pat(pat));
            }
            Err(error) => warn!(
                pid = section.pid,
                error = %error,
                "falha ao parsear PAT"
            ),
        }
    }

    fn dispatch_pmt(&mut self, section: &CompleteSection) {
        let Some(body) = section_body(section) else {
            return;
        };

        match Pmt::from_section_body(body) {
            Ok(pmt) => {
                let pmt_pid = section.pid;
                let version_changed = self.pmt_versions.get(&pmt_pid).copied() != Some(pmt.version);
                if version_changed {
                    if self.pmt_versions.contains_key(&pmt_pid) {
                        tracing::info!(
                            pid = pmt_pid,
                            program = pmt.program_number,
                            new_version = pmt.version,
                            "PMT version mudou — re-registrando streams A/V"
                        );
                    }
                    self.pmt_versions.insert(pmt_pid, pmt.version);
                    for stream in &pmt.streams {
                        if let Some(codec) = MediaCodec::from_stream_type(stream.stream_type) {
                            self.send_demux_command(DemuxCommand::RegisterAvPid(
                                stream.elementary_pid,
                            ));
                            self.send_pes_command(PesCommand::RegisterPid {
                                pid: stream.elementary_pid,
                                codec,
                            });
                        }
                    }
                }
                self.tx.try_send(TableEvent::Pmt(pmt));
            }
            Err(error) => warn!(
                pid = section.pid,
                error = %error,
                "falha ao parsear PMT"
            ),
        }
    }

    fn dispatch_full_section<T, F, E>(
        &self,
        label: &'static str,
        section: &CompleteSection,
        parse: F,
        event: E,
    ) where
        F: FnOnce(&[u8]) -> Result<T, ts::tables::TableError>,
        E: FnOnce(T) -> TableEvent,
    {
        let bytes = section_with_crc_padding(section);
        match parse(&bytes) {
            Ok(table) => {
                self.tx.try_send(event(table));
            }
            Err(error) => warn!(
                pid = section.pid,
                table = label,
                error = %error,
                "falha ao parsear tabela"
            ),
        }
    }

    fn dispatch_eit_pf(&self, section: &CompleteSection) {
        let bytes = section_with_crc_padding(section);
        match Eit::parse(&bytes) {
            Ok(eit) => {
                if !matches!(eit.table_id, 0x4E | 0x4F) {
                    return;
                }
                let current = eit.events.first().cloned();
                let next = eit.events.get(1).cloned();
                self.tx.try_send(TableEvent::EitPf {
                    service_id: eit.service_id,
                    current,
                    next,
                });
            }
            Err(error) => warn!(
                pid = section.pid,
                error = %error,
                "falha ao parsear EIT p/f"
            ),
        }
    }

    fn dispatch_tdt(&self, section: &CompleteSection) {
        match Tdt::parse(&section.data) {
            Ok(tdt) => {
                self.tx.try_send(TableEvent::Tdt(tdt));
            }
            Err(error) => warn!(
                pid = section.pid,
                error = %error,
                "falha ao parsear TDT"
            ),
        }
    }

    fn send_demux_command(&self, command: DemuxCommand) {
        if self.demux_tx.try_send(command).is_err() {
            warn!(?command, "canal demux-control cheio — comando descartado");
        }
    }

    fn send_pes_command(&self, command: PesCommand) {
        if self.pes_tx.try_send(command).is_err() {
            warn!(?command, "canal pes-control cheio — comando descartado");
        }
    }

    fn is_repeated_section(&mut self, section: &CompleteSection) -> bool {
        let key = section_key(section);
        if let Some(previous) = self.last_sections.get(&key) {
            if previous.as_ref() == section.data.as_ref() {
                return true;
            }
        }
        self.last_sections.insert(key, section.data.clone());
        false
    }
}

fn section_body(section: &CompleteSection) -> Option<&[u8]> {
    if section.data.len() < 3 {
        warn!(
            pid = section.pid,
            table_id = section.table_id,
            bytes = section.data.len(),
            "seção curta demais para extrair corpo"
        );
        return None;
    }
    Some(&section.data[3..])
}

fn section_with_crc_padding(section: &CompleteSection) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(section.data.len() + 4);
    bytes.extend_from_slice(&section.data);
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes
}

fn section_key(section: &CompleteSection) -> SectionKey {
    let data = section.data.as_ref();
    if data.len() >= 8 {
        let extension = u16::from_be_bytes([data[3], data[4]]);
        let section_number = data[6];
        SectionKey {
            pid: section.pid,
            table_id: section.table_id,
            extension,
            section_number,
        }
    } else {
        SectionKey {
            pid: section.pid,
            table_id: section.table_id,
            extension: 0,
            section_number: 0,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use crossbeam_channel::bounded;
    use ts::CompleteSection;

    /// Constrói bytes de seção PAT para testes.
    ///
    /// `section.data` layout: [table_id, hi, lo, body...]
    /// body = [ts_id_hi, ts_id_lo, version_byte, sec_num, last_sec_num, programs...]
    fn make_pat_section(ts_id: u16, version: u8, pmt_pids: &[(u16, u16)]) -> CompleteSection {
        let version_byte = ((version & 0x1F) << 1) | 0x01; // current_next = 1
        let mut body: Vec<u8> = vec![
            (ts_id >> 8) as u8,
            ts_id as u8,
            version_byte,
            0x00, // section_number
            0x00, // last_section_number
        ];
        for (prog_num, pmt_pid) in pmt_pids {
            body.push((*prog_num >> 8) as u8);
            body.push(*prog_num as u8);
            body.push(0xE0 | ((*pmt_pid >> 8) as u8 & 0x1F));
            body.push(*pmt_pid as u8);
        }
        // 3-byte PSI header prefix (table_id + section_length placeholder)
        let mut data = vec![0x00u8, 0x80, (body.len() + 4) as u8];
        data.extend_from_slice(&body);
        CompleteSection {
            pid: 0x0000,
            table_id: 0x00,
            data: Bytes::from(data),
        }
    }

    /// Constrói bytes de seção PMT para testes.
    ///
    /// Cria uma PMT com um stream H.264 (stream_type 0x1B).
    fn make_pmt_section(
        pmt_pid: u16,
        program_number: u16,
        version: u8,
        video_pid: u16,
    ) -> CompleteSection {
        let version_byte = ((version & 0x1F) << 1) | 0x01; // current_next = 1
        let body: Vec<u8> = vec![
            (program_number >> 8) as u8,
            program_number as u8,
            version_byte,
            0x00,                                   // section_number
            0x00,                                   // last_section_number
            0xE0 | ((video_pid >> 8) as u8 & 0x1F), // PCR PID high
            video_pid as u8,                        // PCR PID low
            0xF0, // reserved(4b) | program_info_length(12b) high = 0
            0x00, // program_info_length low = 0
            // Stream entry: type(1) + e_pid(2) + es_info_len(2)
            0x1B, // H.264
            0xE0 | ((video_pid >> 8) as u8 & 0x1F),
            video_pid as u8,
            0xF0, // reserved | ES_info_length high = 0
            0x00, // ES_info_length low = 0
        ];
        let mut data = vec![0x02u8, 0x80, (body.len() + 4) as u8];
        data.extend_from_slice(&body);
        CompleteSection {
            pid: pmt_pid,
            table_id: 0x02,
            data: Bytes::from(data),
        }
    }

    fn make_dispatcher() -> (
        TableDispatcher,
        crossbeam_channel::Sender<CompleteSection>,
        crossbeam_channel::Receiver<TableEvent>,
        crossbeam_channel::Receiver<DemuxCommand>,
        crossbeam_channel::Receiver<PesCommand>,
    ) {
        let (sections_tx, sections_rx) = bounded(64);
        let (table_events_tx, table_events_rx) = bounded(64);
        let (demux_cmd_tx, demux_cmd_rx) = bounded(64);
        let (pes_cmd_tx, pes_cmd_rx) = bounded(64);
        let bounded_tx = BoundedSender::new(table_events_tx, "test_table_events");
        let dispatcher = TableDispatcher::new(sections_rx, bounded_tx, demux_cmd_tx, pes_cmd_tx);
        (
            dispatcher,
            sections_tx,
            table_events_rx,
            demux_cmd_rx,
            pes_cmd_rx,
        )
    }

    /// SPEC-TABLE-001d: primeira PAT registra PMT PID e armazena versão.
    #[test]
    fn spec_table_001d_first_pat_registers_pmt_pid() {
        let (mut dispatcher, _tx, _events_rx, demux_rx, _pes_rx) = make_dispatcher();
        let pat = make_pat_section(0x0001, 1, &[(1, 0x100)]);
        dispatcher.process_section(pat);
        let cmd = demux_rx.try_recv().expect("deve ter RegisterPmtPid");
        assert_eq!(cmd, DemuxCommand::RegisterPmtPid(0x100));
        assert_eq!(dispatcher.pat_version, Some(1));
        assert!(dispatcher.pat_pmt_pids.contains(&0x100));
    }

    /// SPEC-TABLE-001d: mesma versão PAT não re-registra PIDs (dedup ativo).
    #[test]
    fn spec_table_001d_same_pat_version_is_deduped() {
        let (mut dispatcher, _tx, _events_rx, demux_rx, _pes_rx) = make_dispatcher();
        let pat = make_pat_section(0x0001, 1, &[(1, 0x100)]);
        dispatcher.process_section(pat.clone());
        let _ = demux_rx.try_recv(); // consome o primeiro RegisterPmtPid
                                     // Envia a mesma seção novamente — deve ser ignorada pelo dedup
        dispatcher.process_section(pat);
        assert!(
            demux_rx.try_recv().is_err(),
            "mesma seção PAT não deve gerar novos comandos"
        );
    }

    /// SPEC-TABLE-001d: mudança de versão PAT invalida cache de PMTs e re-registra PIDs.
    #[test]
    fn spec_table_001d_pat_version_change_invalidates_pmt_cache() {
        let (mut dispatcher, _tx, _events_rx, demux_rx, _pes_rx) = make_dispatcher();

        // Processa PAT versão 1 com PMT PID 0x100
        let pat_v1 = make_pat_section(0x0001, 1, &[(1, 0x100)]);
        dispatcher.process_section(pat_v1);
        // Consome o RegisterPmtPid da v1
        let _ = demux_rx.try_recv();

        // Simula PMT chegando e sendo armazenada no cache de dedup
        let pmt = make_pmt_section(0x100, 1, 0, 0x200);
        dispatcher.process_section(pmt.clone());
        // A mesma PMT não deve ser processada novamente (dedup)
        dispatcher.process_section(pmt.clone());
        // Consome os RegisterAvPid da primeira vez (e PesCommand)
        while demux_rx.try_recv().is_ok() {}

        // Processa PAT versão 2 com o mesmo PMT PID 0x100
        let pat_v2 = make_pat_section(0x0001, 2, &[(1, 0x100)]);
        dispatcher.process_section(pat_v2);
        // Deve ter re-emitido RegisterPmtPid
        let cmd = demux_rx
            .try_recv()
            .expect("deve ter RegisterPmtPid após versão PAT mudar");
        assert_eq!(cmd, DemuxCommand::RegisterPmtPid(0x100));
        assert_eq!(dispatcher.pat_version, Some(2));

        // Agora a mesma PMT deve ser re-processada (cache invalidado)
        dispatcher.process_section(pmt);
        let cmd = demux_rx
            .try_recv()
            .expect("PMT deve ser re-processada após invalidade de cache");
        assert_eq!(cmd, DemuxCommand::RegisterAvPid(0x200));
    }

    /// SPEC-TABLE-001d: mudança de versão PMT re-registra streams A/V.
    #[test]
    fn spec_table_001d_pmt_version_change_reregisters_av_pids() {
        let (mut dispatcher, _tx, _events_rx, demux_rx, pes_rx) = make_dispatcher();

        // PAT
        let pat = make_pat_section(0x0001, 1, &[(1, 0x100)]);
        dispatcher.process_section(pat);
        let _ = demux_rx.try_recv(); // RegisterPmtPid

        // PMT versão 0 com vídeo PID 0x200
        let pmt_v0 = make_pmt_section(0x100, 1, 0, 0x200);
        dispatcher.process_section(pmt_v0);
        assert_eq!(
            demux_rx.try_recv().unwrap(),
            DemuxCommand::RegisterAvPid(0x200)
        );
        let _ = pes_rx.try_recv(); // RegisterPid

        // PMT versão 1 com vídeo PID 0x200 (versão mudou → deve re-registrar)
        let pmt_v1 = make_pmt_section(0x100, 1, 1, 0x200);
        dispatcher.process_section(pmt_v1);
        assert_eq!(
            demux_rx.try_recv().unwrap(),
            DemuxCommand::RegisterAvPid(0x200),
            "versão PMT mudou — deve re-registrar stream A/V"
        );
    }
}
