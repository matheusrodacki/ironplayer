/// SPEC-TABLE — Dispatcher de tabelas PSI/SI para a UI.
///
/// Recebe [`CompleteSection`] do `SectionAssembler`, roteia por `table_id` e
/// emite [`TableEvent`] para o `AppState`.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

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
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemuxCommand {
    /// Registra um PID de PMT descoberto na PAT.
    RegisterPmtPid(Pid),
    /// Registra um PID A/V descoberto na PMT.
    RegisterAvPid(Pid),
    /// Remove um PID A/V do roteamento (ao trocar de serviço).
    DeregisterAvPid(Pid),
}

/// SPEC-TABLE
/// Comando de controle enviado ao `PesAssembler` após parse de PMT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PesCommand {
    /// Registra o codec de um elementary stream suportado.
    RegisterPid { pid: Pid, codec: MediaCodec },
    /// Remove o registro de um PID (ao trocar de serviço).
    DeregisterPid { pid: Pid },
}

/// SPEC-TABLE
/// Comando de controle enviado ao thread `av-decode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeCommand {
    /// Reinicia todos os contextos de decodificação (ao trocar de serviço).
    Reset,
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
    decode_tx: Sender<DecodeCommand>,
    last_sections: HashMap<SectionKey, Bytes>,
    /// Versão atual da PAT (SPEC-TABLE-001d).
    pat_version: Option<u8>,
    /// PIDs de PMT conhecidos da PAT atual (SPEC-TABLE-001d).
    pat_pmt_pids: HashSet<Pid>,
    /// Versão mais recente de cada PMT por PID de PMT.
    pmt_versions: HashMap<Pid, u8>,
    /// Cache de PMTs recebidas: program_number → Pmt.
    pmt_cache: HashMap<u16, Pmt>,
    /// PIDs A/V atualmente registrados no demuxer/assembler.
    active_av_pids: HashSet<Pid>,
    /// Serviço selecionado, compartilhado com o cmd-handler.
    selected_service: Arc<RwLock<Option<u16>>>,
    /// Última leitura do serviço selecionado (para detectar trocas).
    last_selected_service: Option<u16>,
    /// Seleciona automaticamente o primeiro serviço com A/V válidos se ainda
    /// não houver seleção manual (`selected_service == None`).
    auto_play: bool,
    /// Indica que o auto-play já disparou (ou foi inibido por seleção manual).
    auto_play_triggered: bool,
}

impl TableDispatcher {
    /// Cria um novo `TableDispatcher`.
    ///
    /// Usado nos testes unitários; em produção use [`Self::new_with_auto_play`].
    #[allow(dead_code)]
    pub fn new(
        rx: Receiver<CompleteSection>,
        tx: BoundedSender<TableEvent>,
        demux_tx: Sender<DemuxCommand>,
        pes_tx: Sender<PesCommand>,
        decode_tx: Sender<DecodeCommand>,
        selected_service: Arc<RwLock<Option<u16>>>,
    ) -> Self {
        Self::new_with_auto_play(rx, tx, demux_tx, pes_tx, decode_tx, selected_service, false)
    }

    /// Cria um novo `TableDispatcher` com controle explícito do auto-play.
    pub fn new_with_auto_play(
        rx: Receiver<CompleteSection>,
        tx: BoundedSender<TableEvent>,
        demux_tx: Sender<DemuxCommand>,
        pes_tx: Sender<PesCommand>,
        decode_tx: Sender<DecodeCommand>,
        selected_service: Arc<RwLock<Option<u16>>>,
        auto_play: bool,
    ) -> Self {
        Self {
            rx,
            tx,
            demux_tx,
            pes_tx,
            decode_tx,
            last_sections: HashMap::new(),
            pat_version: None,
            pat_pmt_pids: HashSet::new(),
            pmt_versions: HashMap::new(),
            pmt_cache: HashMap::new(),
            active_av_pids: HashSet::new(),
            selected_service,
            last_selected_service: None,
            auto_play,
            auto_play_triggered: false,
        }
    }

    /// Loop principal: drena `complete_sections` e despacha `TableEvent`.
    ///
    /// Termina quando o sender do canal `complete_sections` é fechado.
    pub fn run(mut self) {
        while let Ok(section) = self.rx.recv() {
            // Verifica troca de serviço antes de processar cada seção.
            let current_service = self.selected_service.read().map(|g| *g).unwrap_or(None);
            if current_service != self.last_selected_service {
                self.on_service_changed(current_service);
                self.last_selected_service = current_service;
            }

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

                    // Desregistra PIDs antigos deste programa que estejam ativos.
                    let old_pids: Vec<Pid> =
                        if let Some(old_pmt) = self.pmt_cache.get(&pmt.program_number) {
                            old_pmt
                                .streams
                                .iter()
                                .map(|s| s.elementary_pid)
                                .filter(|pid| self.active_av_pids.contains(pid))
                                .collect()
                        } else {
                            Vec::new()
                        };
                    for pid in old_pids {
                        self.active_av_pids.remove(&pid);
                        self.send_demux_command(DemuxCommand::DeregisterAvPid(pid));
                        self.send_pes_command(PesCommand::DeregisterPid { pid });
                    }

                    // Atualiza o cache de PMT.
                    self.pmt_cache.insert(pmt.program_number, pmt.clone());

                    // Registra PIDs do novo serviço apenas se ele está selecionado
                    // (ou se nenhum serviço está selecionado — modo "registra tudo").
                    let selected = self.selected_service.read().map(|g| *g).unwrap_or(None);
                    let should_register =
                        selected.is_none() || selected == Some(pmt.program_number);

                    if should_register {
                        for stream in &pmt.streams {
                            if let Some(codec) = MediaCodec::from_stream_type(stream.stream_type) {
                                self.active_av_pids.insert(stream.elementary_pid);
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
                }
                // Auto-play: seleciona automaticamente o primeiro serviço com
                // streams A/V válidos se nenhum serviço foi selecionado ainda.
                if self.auto_play && !self.auto_play_triggered {
                    let has_av = pmt
                        .streams
                        .iter()
                        .any(|s| MediaCodec::from_stream_type(s.stream_type).is_some());
                    if has_av {
                        let current_selected =
                            self.selected_service.read().map(|g| *g).unwrap_or(None);
                        if current_selected.is_none() {
                            if let Ok(mut guard) = self.selected_service.write() {
                                *guard = Some(pmt.program_number);
                            }
                            tracing::info!(
                                program_number = pmt.program_number,
                                "auto_play: primeiro serviço com A/V selecionado automaticamente"
                            );
                        }
                        // Marca como disparado independentemente de ter sobrescrito
                        // ou não (seleção manual já existia).
                        self.auto_play_triggered = true;
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

    /// Reage a uma troca de serviço selecionado.
    ///
    /// Desregistra todos os PIDs A/V ativos, registra apenas os do novo
    /// serviço (usando o cache de PMTs) e reinicia o decodificador.
    fn on_service_changed(&mut self, new_service: Option<u16>) {
        tracing::info!(
            old_service = ?self.last_selected_service,
            new_service = ?new_service,
            "serviço alterado — re-roteando PIDs A/V"
        );

        // Desregistra todos os PIDs ativos.
        let pids: Vec<Pid> = self.active_av_pids.drain().collect();
        for pid in pids {
            self.send_demux_command(DemuxCommand::DeregisterAvPid(pid));
            self.send_pes_command(PesCommand::DeregisterPid { pid });
        }

        // Reinicia o decodificador para descartar contextos obsoletos.
        if self.decode_tx.try_send(DecodeCommand::Reset).is_err() {
            warn!("canal decode-control cheio — Reset descartado");
        }

        // Registra PIDs do novo serviço (se a PMT já foi recebida).
        if let Some(service_id) = new_service {
            if let Some(pmt) = self.pmt_cache.get(&service_id).cloned() {
                for stream in &pmt.streams {
                    if let Some(codec) = MediaCodec::from_stream_type(stream.stream_type) {
                        self.active_av_pids.insert(stream.elementary_pid);
                        self.send_demux_command(DemuxCommand::RegisterAvPid(stream.elementary_pid));
                        self.send_pes_command(PesCommand::RegisterPid {
                            pid: stream.elementary_pid,
                            codec,
                        });
                    }
                }
            }
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
        crossbeam_channel::Receiver<DecodeCommand>,
    ) {
        let (sections_tx, sections_rx) = bounded(64);
        let (table_events_tx, table_events_rx) = bounded(64);
        let (demux_cmd_tx, demux_cmd_rx) = bounded(64);
        let (pes_cmd_tx, pes_cmd_rx) = bounded(64);
        let (decode_cmd_tx, decode_cmd_rx) = bounded(64);
        let bounded_tx = BoundedSender::new(table_events_tx, "test_table_events");
        let selected_service = Arc::new(RwLock::new(None));
        let dispatcher = TableDispatcher::new(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
        );
        (
            dispatcher,
            sections_tx,
            table_events_rx,
            demux_cmd_rx,
            pes_cmd_rx,
            decode_cmd_rx,
        )
    }

    /// SPEC-TABLE-001d: primeira PAT registra PMT PID e armazena versão.
    #[test]
    fn spec_table_001d_first_pat_registers_pmt_pid() {
        let (mut dispatcher, _tx, _events_rx, demux_rx, _pes_rx, _decode_rx) = make_dispatcher();
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
        let (mut dispatcher, _tx, _events_rx, demux_rx, _pes_rx, _decode_rx) = make_dispatcher();
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
        let (mut dispatcher, _tx, _events_rx, demux_rx, _pes_rx, _decode_rx) = make_dispatcher();

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
        // Primeiro desregistra o PID antigo, depois registra novamente
        let cmd = demux_rx
            .try_recv()
            .expect("PMT deve ser re-processada após invalidade de cache — DeregisterAvPid");
        assert_eq!(cmd, DemuxCommand::DeregisterAvPid(0x200));
        let cmd = demux_rx
            .try_recv()
            .expect("PMT deve re-registrar PID após invalidade de cache");
        assert_eq!(cmd, DemuxCommand::RegisterAvPid(0x200));
    }

    /// SPEC-TABLE-001d: mudança de versão PMT re-registra streams A/V.
    #[test]
    fn spec_table_001d_pmt_version_change_reregisters_av_pids() {
        let (mut dispatcher, _tx, _events_rx, demux_rx, pes_rx, _decode_rx) = make_dispatcher();

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

        // PMT versão 1 com vídeo PID 0x200 (versão mudou → deve desregistrar e re-registrar)
        let pmt_v1 = make_pmt_section(0x100, 1, 1, 0x200);
        dispatcher.process_section(pmt_v1);
        // Primeiro vem o DeregisterAvPid do PID antigo (mesmo PID, mas versão nova)
        assert_eq!(
            demux_rx.try_recv().unwrap(),
            DemuxCommand::DeregisterAvPid(0x200),
            "versão PMT mudou — deve desregistrar PID antigo"
        );
        assert_eq!(
            demux_rx.try_recv().unwrap(),
            DemuxCommand::RegisterAvPid(0x200),
            "versão PMT mudou — deve re-registrar stream A/V"
        );
    }

    /// Troca de serviço desregistra PIDs do serviço anterior e registra os do novo.
    #[test]
    fn spec_table_service_change_reroutes_av_pids() {
        let (sections_tx, sections_rx) = crossbeam_channel::bounded(64);
        let (table_events_tx, _table_events_rx) = crossbeam_channel::bounded(64);
        let (demux_cmd_tx, demux_cmd_rx) = crossbeam_channel::bounded(64);
        let (pes_cmd_tx, pes_cmd_rx) = crossbeam_channel::bounded(64);
        let (decode_cmd_tx, decode_cmd_rx) = crossbeam_channel::bounded(64);
        let bounded_tx = BoundedSender::new(table_events_tx, "test_service_change");
        let selected_service: Arc<RwLock<Option<u16>>> = Arc::new(RwLock::new(None));
        let selected_service_clone = Arc::clone(&selected_service);

        let mut dispatcher = TableDispatcher::new(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
        );

        // Envia PAT com dois programas: 1 → PID 0x100, 2 → PID 0x200
        let pat = make_pat_section(0x0001, 1, &[(1, 0x100), (2, 0x200)]);
        dispatcher.process_section(pat);
        // Drena RegisterPmtPid x2
        let _ = demux_cmd_rx.try_recv();
        let _ = demux_cmd_rx.try_recv();

        // PMT do programa 1 com vídeo PID 0x101
        let pmt1 = make_pmt_section(0x100, 1, 0, 0x101);
        dispatcher.process_section(pmt1);
        // PMT do programa 2 com vídeo PID 0x201
        let pmt2 = make_pmt_section(0x200, 2, 0, 0x201);
        dispatcher.process_section(pmt2);

        // Sem serviço selecionado: ambos os PIDs são registrados
        let cmds: Vec<DemuxCommand> = std::iter::from_fn(|| demux_cmd_rx.try_recv().ok()).collect();
        assert!(cmds.contains(&DemuxCommand::RegisterAvPid(0x101)));
        assert!(cmds.contains(&DemuxCommand::RegisterAvPid(0x201)));
        // Drena PesCommands
        while pes_cmd_rx.try_recv().is_ok() {}

        // Seleciona o serviço 1 → deve desregistrar 0x201 e manter 0x101
        *selected_service_clone.write().unwrap() = Some(1);
        // A próxima seção vai disparar on_service_changed
        let _ = sections_tx.send(make_pat_section(0x0001, 1, &[(1, 0x100), (2, 0x200)]));
        drop(sections_tx); // fecha o canal para o recv retornar

        // Processa usando run() para que on_service_changed seja chamado
        // (mas o PAT será deduplicado — sem RegisterPmtPid novo)
        dispatcher.run();

        // Coleta todos os comandos demux emitidos durante on_service_changed
        let cmds: Vec<DemuxCommand> = std::iter::from_fn(|| demux_cmd_rx.try_recv().ok()).collect();
        // Ambos os PIDs devem ser desregistrados
        assert!(
            cmds.contains(&DemuxCommand::DeregisterAvPid(0x101)),
            "deve desregistrar PID 0x101"
        );
        assert!(
            cmds.contains(&DemuxCommand::DeregisterAvPid(0x201)),
            "deve desregistrar PID 0x201"
        );
        // Apenas o PID do serviço 1 deve ser re-registrado
        assert!(
            cmds.contains(&DemuxCommand::RegisterAvPid(0x101)),
            "deve re-registrar PID do serviço selecionado"
        );
        assert!(
            !cmds.contains(&DemuxCommand::RegisterAvPid(0x201)),
            "não deve registrar PID de serviço não selecionado"
        );
        // Reset do decoder deve ter sido enviado
        assert_eq!(
            decode_cmd_rx.try_recv().unwrap(),
            DecodeCommand::Reset,
            "deve enviar Reset ao decodificador"
        );
        // Drena PesCommands
        while pes_cmd_rx.try_recv().is_ok() {}
    }

    // ── Testes de integração — Task 5 ─────────────────────────────────────────

    /// Integration: com serviço já selecionado, PMTs de outros serviços NÃO
    /// registram PIDs no demuxer/assembler.
    ///
    /// Valida que apenas os PIDs do serviço selecionado chegam ao decoder.
    #[test]
    fn spec_integration_multi_service_pid_isolation() {
        let (_sections_tx, sections_rx) = crossbeam_channel::bounded(64);
        let (table_events_tx, _table_events_rx) = crossbeam_channel::bounded(64);
        let (demux_cmd_tx, demux_cmd_rx) = crossbeam_channel::bounded(64);
        let (pes_cmd_tx, pes_cmd_rx) = crossbeam_channel::bounded(64);
        let (decode_cmd_tx, decode_cmd_rx) = crossbeam_channel::bounded(64);
        let bounded_tx = BoundedSender::new(table_events_tx, "test_pid_isolation");
        let selected_service: Arc<RwLock<Option<u16>>> = Arc::new(RwLock::new(Some(1)));

        let mut dispatcher = TableDispatcher::new(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
        );
        // Sem troca pendente (last_selected_service = Some(1))
        dispatcher.last_selected_service = Some(1);

        // PAT com 2 programas
        let pat = make_pat_section(0x0001, 1, &[(1, 0x100), (2, 0x200)]);
        dispatcher.process_section(pat);
        while demux_cmd_rx.try_recv().is_ok() {} // drena RegisterPmtPid

        // PMT serviço 1 (vídeo PID 0x101) e serviço 2 (vídeo PID 0x201)
        dispatcher.process_section(make_pmt_section(0x100, 1, 0, 0x101));
        dispatcher.process_section(make_pmt_section(0x200, 2, 0, 0x201));

        let cmds: Vec<DemuxCommand> = std::iter::from_fn(|| demux_cmd_rx.try_recv().ok()).collect();
        let pes_cmds: Vec<PesCommand> = std::iter::from_fn(|| pes_cmd_rx.try_recv().ok()).collect();

        // Apenas PID do serviço 1 deve estar registrado
        assert!(
            cmds.contains(&DemuxCommand::RegisterAvPid(0x101)),
            "PID do serviço selecionado (0x101) deve ser registrado no demuxer"
        );
        assert!(
            !cmds.contains(&DemuxCommand::RegisterAvPid(0x201)),
            "PID do serviço não selecionado (0x201) NÃO deve ser registrado no demuxer"
        );
        assert!(
            pes_cmds
                .iter()
                .any(|c| matches!(c, PesCommand::RegisterPid { pid, .. } if *pid == 0x101)),
            "PesAssembler deve registrar PID do serviço selecionado (0x101)"
        );
        assert!(
            !pes_cmds
                .iter()
                .any(|c| matches!(c, PesCommand::RegisterPid { pid, .. } if *pid == 0x201)),
            "PesAssembler NÃO deve registrar PID de serviço não selecionado (0x201)"
        );
        // Nenhuma troca de serviço → nenhum Reset
        assert!(
            decode_cmd_rx.try_recv().is_err(),
            "sem troca de serviço, DecodeCommand::Reset não deve ser enviado"
        );
    }

    /// Integration: troca de serviço via Arc<RwLock> (simulando comando UI)
    /// desregistra todos os PIDs ativos, envia Reset ao decoder e registra
    /// apenas os PIDs do novo serviço.
    #[test]
    fn spec_integration_service_switch_via_ui_resets_decoder() {
        let (_sections_tx, sections_rx) = crossbeam_channel::bounded(64);
        let (table_events_tx, _table_events_rx) = crossbeam_channel::bounded(64);
        let (demux_cmd_tx, demux_cmd_rx) = crossbeam_channel::bounded(64);
        let (pes_cmd_tx, pes_cmd_rx) = crossbeam_channel::bounded(64);
        let (decode_cmd_tx, decode_cmd_rx) = crossbeam_channel::bounded(64);
        let bounded_tx = BoundedSender::new(table_events_tx, "test_switch_reset");
        let selected_service: Arc<RwLock<Option<u16>>> = Arc::new(RwLock::new(None));
        let selected_service_ctrl = Arc::clone(&selected_service);

        let mut dispatcher = TableDispatcher::new(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
        );

        // Bootstrap: PAT + ambas as PMTs sem serviço selecionado → todos os PIDs registrados
        dispatcher.process_section(make_pat_section(0x0001, 1, &[(1, 0x100), (2, 0x200)]));
        while demux_cmd_rx.try_recv().is_ok() {}

        dispatcher.process_section(make_pmt_section(0x100, 1, 0, 0x101));
        dispatcher.process_section(make_pmt_section(0x200, 2, 0, 0x201));
        while demux_cmd_rx.try_recv().is_ok() {} // drena RegisterAvPid x2
        while pes_cmd_rx.try_recv().is_ok() {} // drena RegisterPid x2

        // UI seleciona serviço 2 via Arc<RwLock>
        *selected_service_ctrl.write().unwrap() = Some(2);
        let new_service = dispatcher
            .selected_service
            .read()
            .map(|g| *g)
            .unwrap_or(None);
        dispatcher.on_service_changed(new_service);
        dispatcher.last_selected_service = new_service;

        let cmds: Vec<DemuxCommand> = std::iter::from_fn(|| demux_cmd_rx.try_recv().ok()).collect();

        // Ambos os PIDs anteriores devem ser desregistrados
        assert!(
            cmds.contains(&DemuxCommand::DeregisterAvPid(0x101)),
            "PID 0x101 (serviço 1) deve ser desregistrado ao trocar para serviço 2"
        );
        assert!(
            cmds.contains(&DemuxCommand::DeregisterAvPid(0x201)),
            "PID 0x201 deve ser desregistrado antes de ser re-registrado"
        );
        // Apenas PID do novo serviço (2) deve ser re-registrado
        assert!(
            cmds.contains(&DemuxCommand::RegisterAvPid(0x201)),
            "PID 0x201 do novo serviço deve ser re-registrado"
        );
        assert!(
            !cmds.contains(&DemuxCommand::RegisterAvPid(0x101)),
            "PID 0x101 do serviço anterior NÃO deve ser re-registrado"
        );
        // Reset do decoder deve ter sido enviado
        assert_eq!(
            decode_cmd_rx.try_recv().unwrap(),
            DecodeCommand::Reset,
            "DecodeCommand::Reset deve ser enviado ao trocar de serviço"
        );
        while pes_cmd_rx.try_recv().is_ok() {}
    }

    /// Integration: auto_play seleciona o primeiro serviço com streams A/V e
    /// não sobrescreve a seleção ao chegar a PMT de outros serviços.
    #[test]
    fn spec_integration_auto_play_selects_first_av_service() {
        let (_sections_tx, sections_rx) = crossbeam_channel::bounded(64);
        let (table_events_tx, _table_events_rx) = crossbeam_channel::bounded(64);
        let (demux_cmd_tx, demux_cmd_rx) = crossbeam_channel::bounded(64);
        let (pes_cmd_tx, pes_cmd_rx) = crossbeam_channel::bounded(64);
        let (decode_cmd_tx, _decode_cmd_rx) = crossbeam_channel::bounded(64);
        let bounded_tx = BoundedSender::new(table_events_tx, "test_auto_play");
        let selected_service: Arc<RwLock<Option<u16>>> = Arc::new(RwLock::new(None));
        let selected_service_read = Arc::clone(&selected_service);

        let mut dispatcher = TableDispatcher::new_with_auto_play(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
            true,
        );

        let pat = make_pat_section(0x0001, 1, &[(1, 0x100), (2, 0x200)]);
        dispatcher.process_section(pat);
        while demux_cmd_rx.try_recv().is_ok() {}

        // PMT do programa 1 chega primeiro — auto_play deve selecionar programa 1
        dispatcher.process_section(make_pmt_section(0x100, 1, 0, 0x101));

        let selected_after_pmt1 = *selected_service_read.read().unwrap();
        assert_eq!(
            selected_after_pmt1,
            Some(1),
            "auto_play deve selecionar o programa 1 (primeiro com A/V)"
        );
        assert!(
            dispatcher.auto_play_triggered,
            "auto_play_triggered deve ser true após o primeiro serviço com A/V"
        );
        while demux_cmd_rx.try_recv().is_ok() {}
        while pes_cmd_rx.try_recv().is_ok() {}

        // PMT do programa 2 chega depois — auto_play NÃO deve alterar a seleção
        dispatcher.process_section(make_pmt_section(0x200, 2, 0, 0x201));
        let selected_after_pmt2 = *selected_service_read.read().unwrap();
        assert_eq!(
            selected_after_pmt2,
            Some(1),
            "auto_play não deve sobrescrever seleção já feita ao receber PMT do programa 2"
        );
    }

    /// Integration: auto_play NÃO sobrescreve seleção manual anterior.
    #[test]
    fn spec_integration_auto_play_respects_manual_selection() {
        let (_sections_tx, sections_rx) = crossbeam_channel::bounded(64);
        let (table_events_tx, _table_events_rx) = crossbeam_channel::bounded(64);
        let (demux_cmd_tx, demux_cmd_rx) = crossbeam_channel::bounded(64);
        let (pes_cmd_tx, pes_cmd_rx) = crossbeam_channel::bounded(64);
        let (decode_cmd_tx, _decode_cmd_rx) = crossbeam_channel::bounded(64);
        let bounded_tx = BoundedSender::new(table_events_tx, "test_auto_play_manual");
        let selected_service: Arc<RwLock<Option<u16>>> = Arc::new(RwLock::new(None));
        let selected_service_ctrl = Arc::clone(&selected_service);

        let mut dispatcher = TableDispatcher::new_with_auto_play(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
            true,
        );

        // Usuário seleciona manualmente o serviço 2 ANTES das PMTs chegarem
        *selected_service_ctrl.write().unwrap() = Some(2);
        dispatcher.last_selected_service = Some(2); // sem troca pendente

        let pat = make_pat_section(0x0001, 1, &[(1, 0x100), (2, 0x200)]);
        dispatcher.process_section(pat);
        while demux_cmd_rx.try_recv().is_ok() {}

        // PMT do programa 1 (primeiro com A/V) — auto_play deve respeitar seleção manual
        dispatcher.process_section(make_pmt_section(0x100, 1, 0, 0x101));

        let selected = *selected_service_ctrl.read().unwrap();
        assert_eq!(
            selected,
            Some(2),
            "auto_play não deve sobrescrever seleção manual (serviço 2)"
        );
        // auto_play_triggered deve ser true (disparou mas não sobrescreveu)
        assert!(
            dispatcher.auto_play_triggered,
            "auto_play_triggered deve ser true mesmo sem sobrescrever"
        );
        while demux_cmd_rx.try_recv().is_ok() {}
        while pes_cmd_rx.try_recv().is_ok() {}
    }

    /// Integration: run() em thread separada + troca de serviço via Arc<RwLock>
    /// encerra sem deadlock dentro de 1 segundo.
    #[test]
    fn spec_integration_run_service_switch_no_deadlock() {
        use std::time::Duration;

        let (sections_tx, sections_rx) = crossbeam_channel::bounded(32);
        let (table_events_tx, _table_events_rx) = crossbeam_channel::bounded(64);
        let (demux_cmd_tx, _demux_cmd_rx) = crossbeam_channel::bounded(64);
        let (pes_cmd_tx, _pes_cmd_rx) = crossbeam_channel::bounded(64);
        let (decode_cmd_tx, _decode_cmd_rx) = crossbeam_channel::bounded(64);
        let bounded_tx = BoundedSender::new(table_events_tx, "test_no_deadlock");
        let selected_service: Arc<RwLock<Option<u16>>> = Arc::new(RwLock::new(None));
        let selected_service_ctrl = Arc::clone(&selected_service);

        let dispatcher = TableDispatcher::new_with_auto_play(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
            false,
        );

        // Pré-carrega seções no canal antes de spawnar
        sections_tx
            .send(make_pat_section(0x0001, 1, &[(1, 0x100), (2, 0x200)]))
            .unwrap();
        sections_tx
            .send(make_pmt_section(0x100, 1, 0, 0x101))
            .unwrap();
        sections_tx
            .send(make_pmt_section(0x200, 2, 0, 0x201))
            .unwrap();

        // Spawn do dispatcher.run() em thread separada
        let handle = std::thread::spawn(move || {
            dispatcher.run();
        });

        // Simula UI trocando de serviço
        std::thread::sleep(Duration::from_millis(5));
        *selected_service_ctrl.write().unwrap() = Some(2);

        // Fecha o canal → run() encerra no próximo recv()
        drop(sections_tx);

        // Verifica que a thread encerrou sem deadlock dentro de 1 segundo
        let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(1);
        std::thread::spawn(move || {
            let _ = handle.join();
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(1)).is_ok(),
            "dispatcher.run() deve encerrar sem deadlock em <= 1s ao fechar o canal"
        );
    }
}
