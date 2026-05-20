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
            if self.is_repeated_section(&section) {
                continue;
            }
            self.dispatch(section);
        }
    }

    fn dispatch(&self, section: CompleteSection) {
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

    fn dispatch_pat(&self, section: &CompleteSection) {
        let Some(body) = section_body(section) else {
            return;
        };

        match Pat::from_section_body(body) {
            Ok(pat) => {
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

    fn dispatch_pmt(&self, section: &CompleteSection) {
        let Some(body) = section_body(section) else {
            return;
        };

        match Pmt::from_section_body(body) {
            Ok(pmt) => {
                for stream in &pmt.streams {
                    if let Some(codec) = MediaCodec::from_stream_type(stream.stream_type) {
                        self.send_demux_command(DemuxCommand::RegisterAvPid(stream.elementary_pid));
                        self.send_pes_command(PesCommand::RegisterPid {
                            pid: stream.elementary_pid,
                            codec,
                        });
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
