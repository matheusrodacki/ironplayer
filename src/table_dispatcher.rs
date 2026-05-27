/// SPEC-TABLE — Dispatcher de tabelas PSI/SI para a UI.
///
/// Recebe [`CompleteSection`] do `SectionAssembler`, roteia por `table_id` e
/// emite [`TableEvent`] para o `AppState`.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use av::MediaCodec;
use bytes::Bytes;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use tracing::{trace, warn};
use ts::tables::{Bat, Cat, Descriptor, Eit, Nit, Pat, Pmt, Sdt, Tdt, Tot};
use ts::{CompleteSection, Pid};
use ui::{AudioOperationalState, AudioStatusSnapshot, AudioTrackInfo, TableEvent};

use crate::channels::BoundedSender;

/// SPEC-TABLE
/// Comando de controle enviado ao `TsDemuxer` após parse de PAT/PMT.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemuxCommand {
    /// Limpa estado dinâmico de PIDs ao trocar/reiniciar a fonte.
    Reset,
    /// Registra um PID de PMT descoberto na PAT.
    RegisterPmtPid(Pid),
    /// Registra o PID dinâmico da NIT descoberto na PAT (`program_number == 0`).
    ///
    /// SPEC-TS-NIT-DYN-001
    RegisterNitPid(Pid),
    /// Registra um PID A/V descoberto na PMT.
    RegisterAvPid(Pid),
    /// Remove um PID A/V do roteamento (ao trocar de serviço).
    DeregisterAvPid(Pid),
}

/// SPEC-TABLE
/// Comando de controle enviado ao `PesAssembler` após parse de PMT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PesCommand {
    /// Limpa registros de PID e buffers parciais ao trocar/reiniciar a fonte.
    Reset,
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
    /// Aplica um novo modo de aceleração de hardware no decoder.
    ///
    /// SPEC-CFG-HW-001
    SetHwAccel {
        choice: crate::config::HwAccelChoice,
    },
    /// Notifica o decoder que o render encontrou `DXGI_ERROR_DEVICE_REMOVED`.
    HandleDeviceRemoved,
}

/// SPEC-TABLE
/// Comando de controle do próprio `TableDispatcher`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableCommand {
    /// Limpa caches PSI/SI e notifica a UI para zerar dados do stream.
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
    control_rx: Option<Receiver<TableCommand>>,
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
    /// PID de áudio selecionado manualmente no serviço atual.
    selected_audio_pid: Arc<RwLock<Option<Pid>>>,
    /// Snapshot compartilhado de telemetria e estado operacional do áudio.
    audio_status: Arc<RwLock<AudioStatusSnapshot>>,
    /// Última leitura do serviço selecionado (para detectar trocas).
    last_selected_service: Option<u16>,
    /// Última leitura do PID de áudio selecionado (para detectar trocas).
    last_selected_audio_pid: Option<Pid>,
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
        audio_status: Arc<RwLock<AudioStatusSnapshot>>,
    ) -> Self {
        Self::new_with_auto_play(
            rx,
            tx,
            demux_tx,
            pes_tx,
            decode_tx,
            selected_service,
            Arc::new(RwLock::new(None)),
            audio_status,
            false,
        )
    }

    /// Cria um novo `TableDispatcher` com controle explícito do auto-play.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_auto_play(
        rx: Receiver<CompleteSection>,
        tx: BoundedSender<TableEvent>,
        demux_tx: Sender<DemuxCommand>,
        pes_tx: Sender<PesCommand>,
        decode_tx: Sender<DecodeCommand>,
        selected_service: Arc<RwLock<Option<u16>>>,
        selected_audio_pid: Arc<RwLock<Option<Pid>>>,
        audio_status: Arc<RwLock<AudioStatusSnapshot>>,
        auto_play: bool,
    ) -> Self {
        Self::new_with_auto_play_and_control(
            rx,
            tx,
            demux_tx,
            pes_tx,
            decode_tx,
            selected_service,
            selected_audio_pid,
            audio_status,
            auto_play,
            None,
        )
    }

    /// Cria um novo `TableDispatcher` com canal de controle opcional.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_auto_play_and_control(
        rx: Receiver<CompleteSection>,
        tx: BoundedSender<TableEvent>,
        demux_tx: Sender<DemuxCommand>,
        pes_tx: Sender<PesCommand>,
        decode_tx: Sender<DecodeCommand>,
        selected_service: Arc<RwLock<Option<u16>>>,
        selected_audio_pid: Arc<RwLock<Option<Pid>>>,
        audio_status: Arc<RwLock<AudioStatusSnapshot>>,
        auto_play: bool,
        control_rx: Option<Receiver<TableCommand>>,
    ) -> Self {
        Self {
            rx,
            control_rx,
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
            selected_audio_pid,
            audio_status,
            last_selected_service: None,
            last_selected_audio_pid: None,
            auto_play,
            auto_play_triggered: false,
        }
    }

    /// Loop principal: drena `complete_sections` e despacha `TableEvent`.
    ///
    /// Termina quando o sender do canal `complete_sections` é fechado.
    pub fn run(mut self) {
        loop {
            self.drain_control_commands();
            let section = match self.rx.recv_timeout(Duration::from_millis(10)) {
                Ok(section) => section,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };

            // Verifica troca de serviço antes de processar cada seção.
            let current_service = self.selected_service.read().map(|g| *g).unwrap_or(None);
            let current_audio_pid = self.selected_audio_pid.read().map(|g| *g).unwrap_or(None);
            if current_service != self.last_selected_service {
                self.on_service_changed(current_service);
                self.last_selected_service = current_service;
                self.last_selected_audio_pid = current_audio_pid;
            } else if current_audio_pid != self.last_selected_audio_pid {
                self.on_audio_changed(current_service, current_audio_pid);
                self.last_selected_audio_pid = current_audio_pid;
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

    fn drain_control_commands(&mut self) {
        let Some(control_rx) = self.control_rx.as_ref().cloned() else {
            return;
        };

        while let Ok(command) = control_rx.try_recv() {
            match command {
                TableCommand::Reset => self.reset_stream_data(),
            }
        }
    }

    fn reset_stream_data(&mut self) {
        self.last_sections.clear();
        self.pat_version = None;
        self.pat_pmt_pids.clear();
        self.pmt_versions.clear();
        self.pmt_cache.clear();
        self.active_av_pids.clear();
        self.last_selected_service = None;
        self.last_selected_audio_pid = None;
        self.auto_play_triggered = false;

        if let Ok(mut audio_status) = self.audio_status.write() {
            audio_status.reset_stream_runtime(AudioOperationalState::Idle);
        }

        self.tx.try_send(TableEvent::Reset);
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
            0x01 => self.dispatch_cat(&section),
            0x02 => self.dispatch_pmt(&section),
            0x40 | 0x41 => self.dispatch_full_section("NIT", &section, Nit::parse, TableEvent::Nit),
            0x42 | 0x46 => self.dispatch_full_section("SDT", &section, Sdt::parse, TableEvent::Sdt),
            0x4A => self.dispatch_full_section("BAT", &section, Bat::parse, TableEvent::Bat),
            0x4E | 0x4F => self.dispatch_eit_pf(&section),
            0x70 => self.dispatch_tdt(&section),
            0x73 => self.dispatch_tot(&section),
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
                // SPEC-TABLE-001d: quando a PAT muda, invalida o cache para
                // todos os PIDs de PMT conhecidos, forçando o re-parse das PMTs.
                let new_pmt_pids: HashSet<Pid> = pat.pmt_pids().collect();
                let pat_changed =
                    self.pat_version != Some(pat.version) || self.pat_pmt_pids != new_pmt_pids;
                if pat_changed {
                    if self.pat_version.is_some() {
                        tracing::info!(
                            old_version = self.pat_version,
                            new_version = pat.version,
                            "PAT mudou — invalidando cache de PMTs"
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
                    self.pat_pmt_pids = new_pmt_pids;
                }
                for pid in pat.pmt_pids() {
                    self.send_demux_command(DemuxCommand::RegisterPmtPid(pid));
                }
                // SPEC-TS-NIT-DYN-001: registra PID dinâmico da NIT quando
                // a PAT declara program_number == 0 com PID diferente do padrão.
                if let Some(nit_pid) = pat.nit_pid() {
                    self.send_demux_command(DemuxCommand::RegisterNitPid(nit_pid));
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
                let pmt_changed = self.pmt_versions.get(&pmt_pid).copied() != Some(pmt.version)
                    || self.pmt_cache.get(&pmt.program_number) != Some(&pmt);
                if pmt_changed {
                    if self.pmt_versions.contains_key(&pmt_pid) {
                        tracing::info!(
                            pid = pmt_pid,
                            program = pmt.program_number,
                            new_version = pmt.version,
                            "PMT mudou — re-registrando streams A/V"
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
                        for (pid, codec) in self.registerable_streams(&pmt) {
                            self.active_av_pids.insert(pid);
                            self.send_demux_command(DemuxCommand::RegisterAvPid(pid));
                            self.send_pes_command(PesCommand::RegisterPid { pid, codec });
                        }
                    }
                }
                // Auto-play: seleciona automaticamente o primeiro serviço com
                // streams A/V válidos se nenhum serviço foi selecionado ainda.
                if self.auto_play && !self.auto_play_triggered {
                    let has_av = pmt
                        .streams
                        .iter()
                        .any(|s| MediaCodec::from_pmt_stream(s).is_some());
                    if has_av {
                        let current_selected =
                            self.selected_service.read().map(|g| *g).unwrap_or(None);
                        if current_selected.is_none() {
                            if let Ok(mut guard) = self.selected_service.write() {
                                *guard = Some(pmt.program_number);
                            }
                            if let Ok(mut audio_guard) = self.selected_audio_pid.write() {
                                *audio_guard = None;
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

                self.sync_active_audio_track();

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

        if new_service.is_none() {
            self.auto_play_triggered = false;
        }

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
                for (pid, codec) in self.registerable_streams(&pmt) {
                    self.active_av_pids.insert(pid);
                    self.send_demux_command(DemuxCommand::RegisterAvPid(pid));
                    self.send_pes_command(PesCommand::RegisterPid { pid, codec });
                }
            }
        }

        self.sync_active_audio_track();
    }

    fn registerable_streams(&self, pmt: &Pmt) -> Vec<(Pid, MediaCodec)> {
        let mut registerable = Vec::new();
        let selected_audio_pid = self.selected_audio_pid.read().map(|g| *g).unwrap_or(None);
        let active_audio_pid = selected_audio_pid
            .filter(|pid| pmt_audio_codec(pmt, *pid).is_some())
            .or_else(|| first_audio_track_from_pmt(pmt).map(|track| track.pid));

        for stream in &pmt.streams {
            let Some(codec) = MediaCodec::from_pmt_stream(stream) else {
                continue;
            };

            match codec {
                MediaCodec::Video(_) => registerable.push((stream.elementary_pid, codec)),
                MediaCodec::Audio(_) if Some(stream.elementary_pid) == active_audio_pid => {
                    registerable.push((stream.elementary_pid, codec));
                }
                MediaCodec::Audio(_) => {}
            }
        }

        registerable
    }

    fn sync_active_audio_track(&self) {
        let selected_service = self.selected_service.read().map(|g| *g).unwrap_or(None);
        let selected_audio_pid = self.selected_audio_pid.read().map(|g| *g).unwrap_or(None);
        let active_track = selected_service
            .and_then(|service_id| self.pmt_cache.get(&service_id))
            .and_then(|pmt| active_audio_track_from_pmt(pmt, selected_audio_pid));

        if let Ok(mut audio_status) = self.audio_status.write() {
            audio_status.active_track = active_track;
            if audio_status.active_track.is_none() {
                audio_status.sample_rate_hz = None;
                audio_status.channels = None;
                audio_status.buffer_level = 0.0;
                if !matches!(audio_status.state, AudioOperationalState::Recovering) {
                    audio_status.state = AudioOperationalState::Idle;
                }
            } else if matches!(audio_status.state, AudioOperationalState::Idle) {
                audio_status.state = AudioOperationalState::Buffering;
            }
        }
    }

    fn on_audio_changed(&mut self, selected_service: Option<u16>, selected_audio_pid: Option<Pid>) {
        let Some(service_id) = selected_service else {
            self.sync_active_audio_track();
            return;
        };
        let Some(pmt) = self.pmt_cache.get(&service_id).cloned() else {
            self.sync_active_audio_track();
            return;
        };

        for stream in &pmt.streams {
            if stream.is_audio() && self.active_av_pids.remove(&stream.elementary_pid) {
                self.send_demux_command(DemuxCommand::DeregisterAvPid(stream.elementary_pid));
                self.send_pes_command(PesCommand::DeregisterPid {
                    pid: stream.elementary_pid,
                });
            }
        }

        let active_audio_pid = selected_audio_pid
            .filter(|pid| pmt_audio_codec(&pmt, *pid).is_some())
            .or_else(|| first_audio_track_from_pmt(&pmt).map(|track| track.pid));
        if let Some(pid) = active_audio_pid {
            if let Some(codec) = pmt_audio_codec(&pmt, pid) {
                self.active_av_pids.insert(pid);
                self.send_demux_command(DemuxCommand::RegisterAvPid(pid));
                self.send_pes_command(PesCommand::RegisterPid { pid, codec });
            }
        }

        if self.decode_tx.try_send(DecodeCommand::Reset).is_err() {
            warn!("canal decode-control cheio — Reset descartado");
        }

        self.sync_active_audio_track();
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

    fn dispatch_tot(&self, section: &CompleteSection) {
        match Tot::parse(&section.data) {
            Ok(tot) => {
                self.tx.try_send(TableEvent::Tot(tot));
            }
            Err(error) => warn!(
                pid = section.pid,
                error = %error,
                "falha ao parsear TOT"
            ),
        }
    }

    fn dispatch_cat(&self, section: &CompleteSection) {
        match Cat::parse(&section.data) {
            Ok(cat) => {
                self.tx.try_send(TableEvent::Cat(cat));
            }
            Err(error) => warn!(
                pid = section.pid,
                error = %error,
                "falha ao parsear CAT"
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

fn first_audio_track_from_pmt(pmt: &Pmt) -> Option<AudioTrackInfo> {
    active_audio_track_from_pmt(pmt, None)
}

fn active_audio_track_from_pmt(
    pmt: &Pmt,
    selected_audio_pid: Option<Pid>,
) -> Option<AudioTrackInfo> {
    if let Some(pid) = selected_audio_pid {
        if let Some(track) = audio_track_by_pid(pmt, pid) {
            return Some(track);
        }
    }

    pmt.streams.iter().find_map(|stream| {
        let codec = MediaCodec::from_pmt_stream(stream)?;
        let MediaCodec::Audio(audio_codec) = codec else {
            return None;
        };

        Some(AudioTrackInfo {
            service_id: pmt.program_number,
            pid: stream.elementary_pid,
            codec_label: audio_codec.name().to_string(),
            language: audio_language(&stream.descriptors),
        })
    })
}

fn audio_track_by_pid(pmt: &Pmt, pid: Pid) -> Option<AudioTrackInfo> {
    pmt.streams.iter().find_map(|stream| {
        if stream.elementary_pid != pid {
            return None;
        }
        let codec = MediaCodec::from_pmt_stream(stream)?;
        let MediaCodec::Audio(audio_codec) = codec else {
            return None;
        };

        Some(AudioTrackInfo {
            service_id: pmt.program_number,
            pid: stream.elementary_pid,
            codec_label: audio_codec.name().to_string(),
            language: audio_language(&stream.descriptors),
        })
    })
}

fn pmt_audio_codec(pmt: &Pmt, pid: Pid) -> Option<MediaCodec> {
    pmt.streams.iter().find_map(|stream| {
        if stream.elementary_pid != pid {
            return None;
        }
        match MediaCodec::from_pmt_stream(stream)? {
            codec @ MediaCodec::Audio(_) => Some(codec),
            MediaCodec::Video(_) => None,
        }
    })
}

fn audio_language(descriptors: &[Descriptor]) -> Option<String> {
    descriptors
        .iter()
        .find(|descriptor| descriptor.tag == 0x0A && descriptor.data.len() >= 3)
        .map(|descriptor| {
            String::from_utf8_lossy(&descriptor.data[..3])
                .trim()
                .to_lowercase()
        })
        .filter(|language| !language.is_empty())
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

    fn make_pmt_section_with_streams(
        pmt_pid: u16,
        program_number: u16,
        version: u8,
        pcr_pid: u16,
        streams: &[(u8, u16, &[u8])],
    ) -> CompleteSection {
        let version_byte = ((version & 0x1F) << 1) | 0x01;
        let mut body: Vec<u8> = vec![
            (program_number >> 8) as u8,
            program_number as u8,
            version_byte,
            0x00,
            0x00,
            0xE0 | ((pcr_pid >> 8) as u8 & 0x1F),
            pcr_pid as u8,
            0xF0,
            0x00,
        ];

        for (stream_type, elementary_pid, descriptors) in streams {
            body.push(*stream_type);
            body.push(0xE0 | ((*elementary_pid >> 8) as u8 & 0x1F));
            body.push(*elementary_pid as u8);
            body.push(0xF0 | (((descriptors.len() as u16) >> 8) as u8 & 0x0F));
            body.push((descriptors.len() as u16 & 0xFF) as u8);
            body.extend_from_slice(descriptors);
        }

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
        Arc<RwLock<AudioStatusSnapshot>>,
    ) {
        let (sections_tx, sections_rx) = bounded(64);
        let (table_events_tx, table_events_rx) = bounded(64);
        let (demux_cmd_tx, demux_cmd_rx) = bounded(64);
        let (pes_cmd_tx, pes_cmd_rx) = bounded(64);
        let (decode_cmd_tx, decode_cmd_rx) = bounded(64);
        let bounded_tx = BoundedSender::new(table_events_tx, "test_table_events");
        let selected_service = Arc::new(RwLock::new(None));
        let audio_status = Arc::new(RwLock::new(AudioStatusSnapshot::default()));
        let dispatcher = TableDispatcher::new(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
            Arc::clone(&audio_status),
        );
        (
            dispatcher,
            sections_tx,
            table_events_rx,
            demux_cmd_rx,
            pes_cmd_rx,
            decode_cmd_rx,
            audio_status,
        )
    }

    /// SPEC-TABLE-001d: primeira PAT registra PMT PID e armazena versão.
    #[test]
    fn spec_table_001d_first_pat_registers_pmt_pid() {
        let (mut dispatcher, _tx, _events_rx, demux_rx, _pes_rx, _decode_rx, _audio_status) =
            make_dispatcher();
        let pat = make_pat_section(0x0001, 1, &[(1, 0x100)]);
        dispatcher.process_section(pat);
        let cmd = demux_rx.try_recv().expect("deve ter RegisterPmtPid");
        assert_eq!(cmd, DemuxCommand::RegisterPmtPid(0x100));
        assert_eq!(dispatcher.pat_version, Some(1));
        assert!(dispatcher.pat_pmt_pids.contains(&0x100));
    }

    #[test]
    fn spec_table_reset_clears_cached_stream_data() {
        let (mut dispatcher, _tx, events_rx, _demux_rx, _pes_rx, _decode_rx, audio_status) =
            make_dispatcher();
        dispatcher.process_section(make_pat_section(0x0001, 1, &[(1, 0x100)]));
        dispatcher.process_section(make_pmt_section(0x100, 1, 1, 0x0101));
        if let Ok(mut status) = audio_status.write() {
            status.active_track = Some(AudioTrackInfo {
                service_id: 1,
                pid: 0x0101,
                codec_label: "H.264".to_owned(),
                language: None,
            });
        }

        assert!(dispatcher.pat_version.is_some());
        assert!(!dispatcher.pat_pmt_pids.is_empty());
        assert!(!dispatcher.pmt_cache.is_empty());
        assert!(!dispatcher.active_av_pids.is_empty());
        while events_rx.try_recv().is_ok() {}

        dispatcher.reset_stream_data();

        assert!(dispatcher.last_sections.is_empty());
        assert!(dispatcher.pat_version.is_none());
        assert!(dispatcher.pat_pmt_pids.is_empty());
        assert!(dispatcher.pmt_versions.is_empty());
        assert!(dispatcher.pmt_cache.is_empty());
        assert!(dispatcher.active_av_pids.is_empty());
        assert!(!dispatcher.auto_play_triggered);
        assert!(audio_status.read().unwrap().active_track.is_none());
        assert!(matches!(events_rx.try_recv(), Ok(TableEvent::Reset)));
    }

    /// SPEC-TABLE-001d: mesma versão PAT não re-registra PIDs (dedup ativo).
    #[test]
    fn spec_table_001d_same_pat_version_is_deduped() {
        let (mut dispatcher, _tx, _events_rx, demux_rx, _pes_rx, _decode_rx, _audio_status) =
            make_dispatcher();
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
        let (mut dispatcher, _tx, _events_rx, demux_rx, _pes_rx, _decode_rx, _audio_status) =
            make_dispatcher();

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
        let (mut dispatcher, _tx, _events_rx, demux_rx, pes_rx, _decode_rx, _audio_status) =
            make_dispatcher();

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

    #[test]
    fn spec_table_reused_pmt_pid_same_version_new_program_reregisters_streams() {
        let (mut dispatcher, _tx, _events_rx, demux_rx, pes_rx, _decode_rx, _audio_status) =
            make_dispatcher();

        dispatcher.process_section(make_pat_section(0x0001, 1, &[(1, 0x0101)]));
        while demux_rx.try_recv().is_ok() {}

        dispatcher.process_section(make_pmt_section_with_streams(
            0x0101,
            1,
            0,
            0x0101,
            &[(0x1B, 0x0101, &[])],
        ));
        while demux_rx.try_recv().is_ok() {}
        while pes_rx.try_recv().is_ok() {}

        dispatcher.process_section(make_pat_section(0x0010, 1, &[(16, 0x0101)]));
        while demux_rx.try_recv().is_ok() {}

        dispatcher.process_section(make_pmt_section_with_streams(
            0x0101,
            16,
            0,
            0x0111,
            &[(0x1B, 0x0111, &[]), (0x11, 0x0112, &[])],
        ));

        let demux_cmds: Vec<DemuxCommand> =
            std::iter::from_fn(|| demux_rx.try_recv().ok()).collect();
        let pes_cmds: Vec<PesCommand> = std::iter::from_fn(|| pes_rx.try_recv().ok()).collect();

        assert!(
            demux_cmds.contains(&DemuxCommand::RegisterAvPid(0x0111)),
            "vídeo do novo programa deve ser registrado mesmo com PMT PID/version reutilizados"
        );
        assert!(
            demux_cmds.contains(&DemuxCommand::RegisterAvPid(0x0112)),
            "áudio LATM do novo programa deve ser registrado"
        );
        assert!(
            pes_cmds.iter().any(|command| {
                matches!(
                    command,
                    PesCommand::RegisterPid {
                        pid: 0x0112,
                        codec: MediaCodec::Audio(av::AudioCodec::AacLatm),
                    }
                )
            }),
            "PesAssembler deve receber AAC LATM do novo programa"
        );
    }

    #[test]
    fn spec_table_private_audio_descriptor_registers_audio_pid() {
        let (mut dispatcher, _tx, _events_rx, demux_rx, pes_rx, _decode_rx, _audio_status) =
            make_dispatcher();

        dispatcher.process_section(make_pat_section(0x0001, 1, &[(1, 0x100)]));
        let _ = demux_rx.try_recv();

        let pmt =
            make_pmt_section_with_streams(0x100, 1, 0, 0x0110, &[(0x06, 0x0120, &[0x6A, 0x00])]);
        dispatcher.process_section(pmt);

        assert_eq!(
            demux_rx.try_recv().unwrap(),
            DemuxCommand::RegisterAvPid(0x0120)
        );
        assert_eq!(
            pes_rx.try_recv().unwrap(),
            PesCommand::RegisterPid {
                pid: 0x0120,
                codec: MediaCodec::Audio(av::AudioCodec::Ac3),
            }
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
        let audio_status = Arc::new(RwLock::new(AudioStatusSnapshot::default()));

        let mut dispatcher = TableDispatcher::new(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
            audio_status,
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
        let audio_status = Arc::new(RwLock::new(AudioStatusSnapshot::default()));

        let mut dispatcher = TableDispatcher::new(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
            audio_status,
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
        let audio_status = Arc::new(RwLock::new(AudioStatusSnapshot::default()));

        let mut dispatcher = TableDispatcher::new(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
            audio_status,
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
        let selected_audio_pid: Arc<RwLock<Option<Pid>>> = Arc::new(RwLock::new(None));
        let audio_status = Arc::new(RwLock::new(AudioStatusSnapshot::default()));

        let mut dispatcher = TableDispatcher::new_with_auto_play(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
            selected_audio_pid,
            audio_status,
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
        let selected_audio_pid: Arc<RwLock<Option<Pid>>> = Arc::new(RwLock::new(None));
        let audio_status = Arc::new(RwLock::new(AudioStatusSnapshot::default()));

        let mut dispatcher = TableDispatcher::new_with_auto_play(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
            selected_audio_pid,
            audio_status,
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

    #[test]
    fn spec_integration_auto_play_rearms_when_selection_is_cleared() {
        let (_sections_tx, sections_rx) = crossbeam_channel::bounded(64);
        let (table_events_tx, _table_events_rx) = crossbeam_channel::bounded(64);
        let (demux_cmd_tx, demux_cmd_rx) = crossbeam_channel::bounded(64);
        let (pes_cmd_tx, pes_cmd_rx) = crossbeam_channel::bounded(64);
        let (decode_cmd_tx, decode_cmd_rx) = crossbeam_channel::bounded(64);
        let bounded_tx = BoundedSender::new(table_events_tx, "test_auto_play_rearm");
        let selected_service: Arc<RwLock<Option<u16>>> = Arc::new(RwLock::new(None));
        let selected_service_ctrl = Arc::clone(&selected_service);
        let selected_audio_pid: Arc<RwLock<Option<Pid>>> = Arc::new(RwLock::new(None));
        let audio_status = Arc::new(RwLock::new(AudioStatusSnapshot::default()));

        let mut dispatcher = TableDispatcher::new_with_auto_play(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
            selected_audio_pid,
            audio_status,
            true,
        );

        dispatcher.process_section(make_pmt_section(0x0100, 1, 0, 0x0101));
        assert_eq!(*selected_service_ctrl.read().unwrap(), Some(1));
        dispatcher.last_selected_service = Some(1);
        while demux_cmd_rx.try_recv().is_ok() {}
        while pes_cmd_rx.try_recv().is_ok() {}

        *selected_service_ctrl.write().unwrap() = None;
        dispatcher.on_service_changed(None);
        dispatcher.last_selected_service = None;
        assert!(
            !dispatcher.auto_play_triggered,
            "limpar seleção deve rearmar auto-play para a próxima fonte"
        );
        let _ = decode_cmd_rx.try_recv();

        dispatcher.process_section(make_pmt_section_with_streams(
            0x0101,
            16,
            0,
            0x0111,
            &[(0x1B, 0x0111, &[]), (0x11, 0x0112, &[])],
        ));

        assert_eq!(
            *selected_service_ctrl.read().unwrap(),
            Some(16),
            "auto-play deve selecionar o serviço da nova fonte"
        );
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
        let selected_audio_pid: Arc<RwLock<Option<Pid>>> = Arc::new(RwLock::new(None));
        let audio_status = Arc::new(RwLock::new(AudioStatusSnapshot::default()));

        let dispatcher = TableDispatcher::new_with_auto_play(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
            selected_audio_pid,
            audio_status,
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

    /// Integration: codecs obrigatórios continuam registrando vídeo, mantêm o
    /// parse de PMT e atualizam o snapshot de áudio ao selecionar um serviço.
    #[test]
    fn spec_integration_mandatory_audio_codecs_keep_video_pid_and_ui_snapshot() {
        let (_sections_tx, sections_rx) = crossbeam_channel::bounded(64);
        let (table_events_tx, table_events_rx) = crossbeam_channel::bounded(64);
        let (demux_cmd_tx, demux_cmd_rx) = crossbeam_channel::bounded(128);
        let (pes_cmd_tx, pes_cmd_rx) = crossbeam_channel::bounded(128);
        let (decode_cmd_tx, decode_cmd_rx) = crossbeam_channel::bounded(16);
        let bounded_tx = BoundedSender::new(table_events_tx, "test_mandatory_audio_codecs");
        let selected_service: Arc<RwLock<Option<u16>>> = Arc::new(RwLock::new(None));
        let selected_service_ctrl = Arc::clone(&selected_service);
        let audio_status = Arc::new(RwLock::new(AudioStatusSnapshot::default()));

        let mut dispatcher = TableDispatcher::new(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
            Arc::clone(&audio_status),
        );

        dispatcher.process_section(make_pat_section(
            0x0001,
            1,
            &[(1, 0x100), (2, 0x200), (3, 0x300), (4, 0x400)],
        ));

        let mp2_lang = [0x0A, 0x04, b'p', b'o', b'r', 0x00];
        let aac_adts_lang = [0x0A, 0x04, b'e', b'n', b'g', 0x00];
        let aac_latm_desc = [
            0x7C, 0x03, 0x11, 0x90, 0x00, 0x0A, 0x04, b's', b'p', b'a', 0x00,
        ];
        let ac3_desc = [0x6A, 0x00, 0x0A, 0x04, b'd', b'e', b'u', 0x00];

        dispatcher.process_section(make_pmt_section_with_streams(
            0x100,
            1,
            0,
            0x101,
            &[(0x1B, 0x101, &[]), (0x03, 0x120, &mp2_lang)],
        ));
        dispatcher.process_section(make_pmt_section_with_streams(
            0x200,
            2,
            0,
            0x201,
            &[(0x1B, 0x201, &[]), (0x0F, 0x220, &aac_adts_lang)],
        ));
        dispatcher.process_section(make_pmt_section_with_streams(
            0x300,
            3,
            0,
            0x301,
            &[(0x1B, 0x301, &[]), (0x06, 0x320, &aac_latm_desc)],
        ));
        dispatcher.process_section(make_pmt_section_with_streams(
            0x400,
            4,
            0,
            0x401,
            &[(0x1B, 0x401, &[]), (0x06, 0x420, &ac3_desc)],
        ));

        let demux_cmds: Vec<DemuxCommand> =
            std::iter::from_fn(|| demux_cmd_rx.try_recv().ok()).collect();
        let pes_cmds: Vec<PesCommand> = std::iter::from_fn(|| pes_cmd_rx.try_recv().ok()).collect();
        let pmt_events: Vec<u16> = table_events_rx
            .try_iter()
            .filter_map(|event| match event {
                TableEvent::Pmt(pmt) => Some(pmt.program_number),
                _ => None,
            })
            .collect();

        assert_eq!(
            pmt_events,
            vec![1, 2, 3, 4],
            "PMTs válidas devem continuar chegando à UI sem regressão de parse"
        );
        for pid in [0x101, 0x120, 0x201, 0x220, 0x301, 0x320, 0x401, 0x420] {
            assert!(
                demux_cmds.contains(&DemuxCommand::RegisterAvPid(pid)),
                "PID 0x{pid:04X} deve ser registrado no demuxer"
            );
        }
        assert!(
            pes_cmds.iter().any(|command| {
                matches!(
                    command,
                    PesCommand::RegisterPid {
                        pid: 0x120,
                        codec: MediaCodec::Audio(av::AudioCodec::Mp2),
                    }
                )
            }),
            "MP2 obrigatório deve ser identificado"
        );
        assert!(
            pes_cmds.iter().any(|command| {
                matches!(
                    command,
                    PesCommand::RegisterPid {
                        pid: 0x220,
                        codec: MediaCodec::Audio(av::AudioCodec::AacAdts),
                    }
                )
            }),
            "AAC ADTS obrigatório deve ser identificado"
        );
        assert!(
            pes_cmds.iter().any(|command| {
                matches!(
                    command,
                    PesCommand::RegisterPid {
                        pid: 0x320,
                        codec: MediaCodec::Audio(av::AudioCodec::AacLatm),
                    }
                )
            }),
            "AAC LATM/HE-AAC obrigatório deve ser identificado"
        );
        assert!(
            pes_cmds.iter().any(|command| {
                matches!(
                    command,
                    PesCommand::RegisterPid {
                        pid: 0x420,
                        codec: MediaCodec::Audio(av::AudioCodec::Ac3),
                    }
                )
            }),
            "AC-3 obrigatório deve ser identificado"
        );
        assert!(
            decode_cmd_rx.try_recv().is_err(),
            "processar PMTs não deve resetar o decoder"
        );

        *selected_service_ctrl.write().unwrap() = Some(3);
        dispatcher.on_service_changed(Some(3));

        let switch_demux_cmds: Vec<DemuxCommand> =
            std::iter::from_fn(|| demux_cmd_rx.try_recv().ok()).collect();
        let switch_pes_cmds: Vec<PesCommand> =
            std::iter::from_fn(|| pes_cmd_rx.try_recv().ok()).collect();

        assert!(
            switch_demux_cmds.contains(&DemuxCommand::RegisterAvPid(0x301)),
            "troca de serviço deve preservar o PID de vídeo do serviço selecionado"
        );
        assert!(
            switch_demux_cmds.contains(&DemuxCommand::RegisterAvPid(0x320)),
            "troca de serviço deve registrar o PID de áudio LATM"
        );
        assert!(
            !switch_demux_cmds.contains(&DemuxCommand::RegisterAvPid(0x420)),
            "troca de serviço não deve manter áudio de outros serviços"
        );
        assert!(
            switch_pes_cmds.iter().any(|command| {
                matches!(
                    command,
                    PesCommand::RegisterPid {
                        pid: 0x320,
                        codec: MediaCodec::Audio(av::AudioCodec::AacLatm),
                    }
                )
            }),
            "assembler deve receber o codec LATM do serviço ativo"
        );
        assert_eq!(
            decode_cmd_rx.try_recv().unwrap(),
            DecodeCommand::Reset,
            "troca de serviço deve resetar o decoder"
        );

        let snapshot = audio_status.read().unwrap().clone();
        assert_eq!(
            snapshot.active_track,
            Some(AudioTrackInfo {
                service_id: 3,
                pid: 0x320,
                codec_label: av::AudioCodec::AacLatm.name().to_string(),
                language: Some("spa".to_string()),
            })
        );
        assert_eq!(snapshot.state, AudioOperationalState::Buffering);
    }

    /// Integration: mudança de PMT no mesmo serviço troca a trilha ativa,
    /// preserva o PID de vídeo e atualiza o snapshot consumido pela UI.
    #[test]
    fn spec_integration_audio_track_switch_updates_snapshot_without_video_regression() {
        let (_sections_tx, sections_rx) = crossbeam_channel::bounded(64);
        let (table_events_tx, _table_events_rx) = crossbeam_channel::bounded(64);
        let (demux_cmd_tx, demux_cmd_rx) = crossbeam_channel::bounded(64);
        let (pes_cmd_tx, pes_cmd_rx) = crossbeam_channel::bounded(64);
        let (decode_cmd_tx, decode_cmd_rx) = crossbeam_channel::bounded(16);
        let bounded_tx = BoundedSender::new(table_events_tx, "test_audio_track_switch");
        let selected_service: Arc<RwLock<Option<u16>>> = Arc::new(RwLock::new(Some(1)));
        let audio_status = Arc::new(RwLock::new(AudioStatusSnapshot::default()));

        let mut dispatcher = TableDispatcher::new(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
            Arc::clone(&audio_status),
        );
        dispatcher.last_selected_service = Some(1);

        dispatcher.process_section(make_pat_section(0x0001, 1, &[(1, 0x100)]));
        let _ = demux_cmd_rx.try_recv();

        let mp2_lang = [0x0A, 0x04, b'p', b'o', b'r', 0x00];
        let aac_adts_lang = [0x0A, 0x04, b'e', b'n', b'g', 0x00];

        dispatcher.process_section(make_pmt_section_with_streams(
            0x100,
            1,
            0,
            0x101,
            &[
                (0x1B, 0x101, &[]),
                (0x03, 0x120, &mp2_lang),
                (0x0F, 0x121, &aac_adts_lang),
            ],
        ));

        let initial_demux_cmds: Vec<DemuxCommand> =
            std::iter::from_fn(|| demux_cmd_rx.try_recv().ok()).collect();
        let initial_pes_cmds: Vec<PesCommand> =
            std::iter::from_fn(|| pes_cmd_rx.try_recv().ok()).collect();

        assert!(
            initial_demux_cmds.contains(&DemuxCommand::RegisterAvPid(0x101)),
            "vídeo do serviço selecionado deve continuar registrado"
        );
        assert!(
            initial_demux_cmds.contains(&DemuxCommand::RegisterAvPid(0x120)),
            "primeira trilha de áudio deve ser registrada"
        );
        assert!(
            !initial_demux_cmds.contains(&DemuxCommand::RegisterAvPid(0x121)),
            "segunda trilha de áudio não deve ser registrada enquanto não estiver ativa"
        );
        assert!(
            initial_pes_cmds.iter().any(|command| {
                matches!(
                    command,
                    PesCommand::RegisterPid {
                        pid: 0x120,
                        codec: MediaCodec::Audio(av::AudioCodec::Mp2),
                    }
                )
            }),
            "trilha MP2 inicial deve ser encaminhada ao assembler"
        );

        let initial_snapshot = audio_status.read().unwrap().clone();
        assert_eq!(
            initial_snapshot.active_track,
            Some(AudioTrackInfo {
                service_id: 1,
                pid: 0x120,
                codec_label: av::AudioCodec::Mp2.name().to_string(),
                language: Some("por".to_string()),
            })
        );
        assert_eq!(initial_snapshot.state, AudioOperationalState::Buffering);

        dispatcher.process_section(make_pmt_section_with_streams(
            0x100,
            1,
            1,
            0x101,
            &[
                (0x1B, 0x101, &[]),
                (0x0F, 0x121, &aac_adts_lang),
                (0x03, 0x120, &mp2_lang),
            ],
        ));

        let updated_demux_cmds: Vec<DemuxCommand> =
            std::iter::from_fn(|| demux_cmd_rx.try_recv().ok()).collect();
        let updated_pes_cmds: Vec<PesCommand> =
            std::iter::from_fn(|| pes_cmd_rx.try_recv().ok()).collect();

        assert!(
            updated_demux_cmds.contains(&DemuxCommand::DeregisterAvPid(0x101)),
            "mudança de PMT deve desregistrar o PID de vídeo antigo antes do re-registro"
        );
        assert!(
            updated_demux_cmds.contains(&DemuxCommand::RegisterAvPid(0x101)),
            "PID de vídeo deve ser re-registrado na troca de trilha"
        );
        assert!(
            updated_demux_cmds.contains(&DemuxCommand::DeregisterAvPid(0x120)),
            "trilha MP2 antiga deve ser removida"
        );
        assert!(
            updated_demux_cmds.contains(&DemuxCommand::RegisterAvPid(0x121)),
            "nova trilha AAC ADTS deve ser registrada"
        );
        assert!(
            !updated_demux_cmds.contains(&DemuxCommand::RegisterAvPid(0x120)),
            "trilha antiga não deve voltar a ser registrada após a troca"
        );
        assert!(
            updated_pes_cmds.contains(&PesCommand::DeregisterPid { pid: 0x120 }),
            "assembler deve remover a trilha antiga"
        );
        assert!(
            updated_pes_cmds.iter().any(|command| {
                matches!(
                    command,
                    PesCommand::RegisterPid {
                        pid: 0x121,
                        codec: MediaCodec::Audio(av::AudioCodec::AacAdts),
                    }
                )
            }),
            "assembler deve registrar a nova trilha AAC ADTS"
        );
        assert!(
            decode_cmd_rx.try_recv().is_err(),
            "troca automática de trilha via PMT não deve resetar o decoder inteiro"
        );

        let updated_snapshot = audio_status.read().unwrap().clone();
        assert_eq!(
            updated_snapshot.active_track,
            Some(AudioTrackInfo {
                service_id: 1,
                pid: 0x121,
                codec_label: av::AudioCodec::AacAdts.name().to_string(),
                language: Some("eng".to_string()),
            })
        );
        assert_eq!(updated_snapshot.state, AudioOperationalState::Buffering);
    }

    #[test]
    fn spec_integration_manual_audio_selection_reroutes_selected_pid() {
        let (_sections_tx, sections_rx) = crossbeam_channel::bounded(64);
        let (table_events_tx, _table_events_rx) = crossbeam_channel::bounded(64);
        let (demux_cmd_tx, demux_cmd_rx) = crossbeam_channel::bounded(64);
        let (pes_cmd_tx, pes_cmd_rx) = crossbeam_channel::bounded(64);
        let (decode_cmd_tx, decode_cmd_rx) = crossbeam_channel::bounded(16);
        let bounded_tx = BoundedSender::new(table_events_tx, "test_manual_audio_switch");
        let selected_service: Arc<RwLock<Option<u16>>> = Arc::new(RwLock::new(Some(1)));
        let selected_audio_pid: Arc<RwLock<Option<Pid>>> = Arc::new(RwLock::new(None));
        let audio_status = Arc::new(RwLock::new(AudioStatusSnapshot::default()));

        let mut dispatcher = TableDispatcher::new_with_auto_play(
            sections_rx,
            bounded_tx,
            demux_cmd_tx,
            pes_cmd_tx,
            decode_cmd_tx,
            selected_service,
            Arc::clone(&selected_audio_pid),
            Arc::clone(&audio_status),
            false,
        );
        dispatcher.last_selected_service = Some(1);

        dispatcher.process_section(make_pat_section(0x0001, 1, &[(1, 0x100)]));
        let _ = demux_cmd_rx.try_recv();

        let por_lang = [0x0A, 0x04, b'p', b'o', b'r', 0x00];
        let eng_lang = [0x0A, 0x04, b'e', b'n', b'g', 0x00];
        dispatcher.process_section(make_pmt_section_with_streams(
            0x100,
            1,
            0,
            0x101,
            &[
                (0x1B, 0x101, &[]),
                (0x11, 0x120, &por_lang),
                (0x11, 0x121, &eng_lang),
            ],
        ));
        while demux_cmd_rx.try_recv().is_ok() {}
        while pes_cmd_rx.try_recv().is_ok() {}

        *selected_audio_pid.write().unwrap() = Some(0x121);
        dispatcher.on_audio_changed(Some(1), Some(0x121));

        let demux_cmds: Vec<DemuxCommand> =
            std::iter::from_fn(|| demux_cmd_rx.try_recv().ok()).collect();
        let pes_cmds: Vec<PesCommand> = std::iter::from_fn(|| pes_cmd_rx.try_recv().ok()).collect();

        assert!(
            demux_cmds.contains(&DemuxCommand::DeregisterAvPid(0x120)),
            "áudio anterior deve ser removido do demuxer"
        );
        assert!(
            demux_cmds.contains(&DemuxCommand::RegisterAvPid(0x121)),
            "áudio escolhido deve ser registrado no demuxer"
        );
        assert!(
            !demux_cmds.contains(&DemuxCommand::DeregisterAvPid(0x101)),
            "troca de áudio não deve desregistrar o vídeo"
        );
        assert!(
            pes_cmds.contains(&PesCommand::DeregisterPid { pid: 0x120 }),
            "assembler deve descartar a faixa antiga"
        );
        assert!(
            pes_cmds.iter().any(|command| {
                matches!(
                    command,
                    PesCommand::RegisterPid {
                        pid: 0x121,
                        codec: MediaCodec::Audio(av::AudioCodec::AacLatm),
                    }
                )
            }),
            "assembler deve registrar a faixa escolhida"
        );
        assert_eq!(decode_cmd_rx.try_recv().unwrap(), DecodeCommand::Reset);

        let snapshot = audio_status.read().unwrap().clone();
        assert_eq!(
            snapshot.active_track,
            Some(AudioTrackInfo {
                service_id: 1,
                pid: 0x121,
                codec_label: av::AudioCodec::AacLatm.name().to_string(),
                language: Some("eng".to_string()),
            })
        );
        assert_eq!(snapshot.state, AudioOperationalState::Buffering);
    }

    // ── SPEC-TS-CAT-001 / SPEC-TABLE-TOT-001 / SPEC-TS-NIT-DYN-001 ───────────

    /// Constrói uma seção CAT mínima para testes do dispatcher.
    fn make_cat_section(version: u8) -> CompleteSection {
        use ts::crc::crc32_mpeg2;
        let section_length = 2 + 3 + 0 + 4; // reserved(2) + version_bytes(3) + no descs + CRC
        let mut data = vec![
            0x01u8,
            0x80 | ((section_length >> 8) as u8),
            (section_length & 0xFF) as u8,
            0xFF,
            0xFF,
            0xC0 | ((version & 0x1F) << 1) | 0x01,
            0x00,
            0x00,
        ];
        let crc_pos = data.len();
        data.extend_from_slice(&[0, 0, 0, 0]);
        let crc = crc32_mpeg2(&data[..crc_pos]);
        data[crc_pos..].copy_from_slice(&crc.to_be_bytes());
        CompleteSection {
            pid: 0x0001,
            table_id: 0x01,
            data: Bytes::from(data),
        }
    }

    /// Constrói uma seção TOT mínima para testes do dispatcher.
    fn make_tot_section() -> CompleteSection {
        use ts::crc::crc32_mpeg2;
        // MJD para 2024-01-15 + 12:00:00 BCD
        let mjd: u16 = 60324;
        let section_length = 5 + 2 + 0 + 4; // MJD+BCD + desc_loop_len + no descs + CRC
        let mut data = vec![
            0x73u8,
            0x70 | ((section_length >> 8) as u8),
            (section_length & 0xFF) as u8,
            (mjd >> 8) as u8,
            mjd as u8,
            0x12,
            0x00,
            0x00, // 12:00:00 BCD
            0xF0,
            0x00, // desc_loop_len = 0
        ];
        let crc_pos = data.len();
        data.extend_from_slice(&[0, 0, 0, 0]);
        let crc = crc32_mpeg2(&data[..crc_pos]);
        data[crc_pos..].copy_from_slice(&crc.to_be_bytes());
        CompleteSection {
            pid: 0x0014,
            table_id: 0x73,
            data: Bytes::from(data),
        }
    }

    /// Constrói uma seção PAT que inclui um programa 0 apontando para NIT PID dinâmico.
    fn make_pat_section_with_nit(
        ts_id: u16,
        version: u8,
        nit_pid: u16,
        pmt_pids: &[(u16, u16)],
    ) -> CompleteSection {
        let version_byte = ((version & 0x1F) << 1) | 0x01;
        let mut body: Vec<u8> = vec![
            (ts_id >> 8) as u8,
            ts_id as u8,
            version_byte,
            0x00,
            0x00,
            // program_number = 0 → NIT PID
            0x00,
            0x00,
            0xE0 | ((nit_pid >> 8) as u8 & 0x1F),
            nit_pid as u8,
        ];
        for (prog_num, pmt_pid) in pmt_pids {
            body.push((*prog_num >> 8) as u8);
            body.push(*prog_num as u8);
            body.push(0xE0 | ((*pmt_pid >> 8) as u8 & 0x1F));
            body.push(*pmt_pid as u8);
        }
        let mut data = vec![0x00u8, 0x80, (body.len() + 4) as u8];
        data.extend_from_slice(&body);
        CompleteSection {
            pid: 0x0000,
            table_id: 0x00,
            data: Bytes::from(data),
        }
    }

    /// SPEC-TS-CAT-001: dispatcher processa seção CAT (PID 0x0001) e emite TableEvent::Cat.
    #[test]
    fn spec_ts_cat_001_dispatcher_emits_cat_event() {
        let (mut dispatcher, _tx, events_rx, _demux_rx, _pes_rx, _decode_rx, _audio) =
            make_dispatcher();
        let cat = make_cat_section(2);
        dispatcher.process_section(cat);
        let event = events_rx.try_recv().expect("deve ter TableEvent::Cat");
        assert!(
            matches!(event, TableEvent::Cat(_)),
            "evento deve ser TableEvent::Cat, foi: {event:?}"
        );
    }

    /// SPEC-TABLE-TOT-001: dispatcher processa seção TOT (table_id 0x73) e emite TableEvent::Tot.
    #[test]
    fn spec_table_tot_001_dispatcher_emits_tot_event() {
        let (mut dispatcher, _tx, events_rx, _demux_rx, _pes_rx, _decode_rx, _audio) =
            make_dispatcher();
        let tot = make_tot_section();
        dispatcher.process_section(tot);
        let event = events_rx.try_recv().expect("deve ter TableEvent::Tot");
        assert!(
            matches!(event, TableEvent::Tot(_)),
            "evento deve ser TableEvent::Tot, foi: {event:?}"
        );
    }

    /// SPEC-TS-NIT-DYN-001: PAT com program_number=0 (NIT PID dinâmico) envia
    /// DemuxCommand::RegisterNitPid com o PID declarado.
    #[test]
    fn spec_ts_nit_dyn_001_pat_with_nit_pid_sends_register_nit_pid() {
        let (mut dispatcher, _tx, _events_rx, demux_rx, _pes_rx, _decode_rx, _audio) =
            make_dispatcher();
        // PAT com NIT PID dinâmico = 0x0020 e um programa normal
        let pat = make_pat_section_with_nit(0x0001, 1, 0x0020, &[(1, 0x0100)]);
        dispatcher.process_section(pat);

        // Coleta todos os comandos demux emitidos
        let cmds: Vec<DemuxCommand> = std::iter::from_fn(|| demux_rx.try_recv().ok()).collect();
        assert!(
            cmds.contains(&DemuxCommand::RegisterNitPid(0x0020)),
            "deve emitir RegisterNitPid(0x0020); comandos recebidos: {cmds:?}"
        );
        assert!(
            cmds.contains(&DemuxCommand::RegisterPmtPid(0x0100)),
            "deve emitir RegisterPmtPid(0x0100); comandos recebidos: {cmds:?}"
        );
    }

    /// SPEC-TS-NIT-DYN-001: PAT sem program_number=0 NÃO emite RegisterNitPid.
    #[test]
    fn spec_ts_nit_dyn_001_pat_without_nit_entry_no_register_nit_pid() {
        let (mut dispatcher, _tx, _events_rx, demux_rx, _pes_rx, _decode_rx, _audio) =
            make_dispatcher();
        let pat = make_pat_section(0x0001, 1, &[(1, 0x0100)]);
        dispatcher.process_section(pat);

        let cmds: Vec<DemuxCommand> = std::iter::from_fn(|| demux_rx.try_recv().ok()).collect();
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, DemuxCommand::RegisterNitPid(_))),
            "PAT sem program_number=0 não deve emitir RegisterNitPid; comandos: {cmds:?}"
        );
    }
}
