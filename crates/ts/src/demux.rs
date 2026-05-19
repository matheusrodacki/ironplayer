//! Demultiplexador MPEG-TS por PID, validação de Continuity Counter e recuperação de sync.
//!
//! SPEC-TS-002

use std::collections::{HashMap, HashSet};

use bytes::Bytes;
use crossbeam_channel::Sender;
use tracing::warn;

use crate::packet::TsPacket;
use crate::{Pid, TsError, TsEvent};

// ── PIDs de seção bem conhecidos (ISO 13818-1) ───────────────────────────────

/// PAT — Program Association Table.
const PID_PAT: Pid = 0x0000;
/// NIT — Network Information Table.
const PID_NIT: Pid = 0x0010;
/// SDT/BAT — Service Description / Bouquet Association Table.
const PID_SDT: Pid = 0x0011;
/// EIT — Event Information Table.
const PID_EIT: Pid = 0x0012;
/// TDT/TOT — Time and Date / Time Offset Table.
const PID_TDT: Pid = 0x0014;
/// Null packet — sempre descartado.
const PID_NULL: Pid = 0x1FFF;

// ── Tipos de dados roteados ───────────────────────────────────────────────────

/// Payload de pacote TS roteado para o `SectionAssembler`.
///
/// SPEC-TS-002a
#[derive(Debug, Clone)]
pub struct SectionData {
    /// PID do pacote.
    pub pid: Pid,
    /// Payload Unit Start Indicator — início de nova seção neste pacote.
    pub pusi: bool,
    /// Bytes de payload (até 184 bytes).
    pub payload: Bytes,
}

/// Payload de pacote TS roteado para o decoder A/V (PES).
///
/// SPEC-TS-002a
#[derive(Debug, Clone)]
pub struct PesData {
    /// PID do pacote.
    pub pid: Pid,
    /// Bytes de payload.
    pub data: Bytes,
}

// ── TsDemuxer ─────────────────────────────────────────────────────────────────

/// Demultiplexador MPEG-TS: roteia pacotes por PID, valida Continuity Counter
/// e recupera sincronização após perda do byte 0x47.
///
/// SPEC-TS-002
pub struct TsDemuxer {
    section_tx: Sender<SectionData>,
    pes_tx: Sender<PesData>,
    event_tx: Sender<TsEvent>,
    /// Último CC observado por PID (para validação de sequência).
    cc_state: HashMap<Pid, u8>,
    /// PIDs de PMT registrados dinamicamente ao parsear a PAT.
    pmt_pids: HashSet<Pid>,
    /// PIDs de A/V registrados dinamicamente ao parsear a PMT.
    av_pids: HashSet<Pid>,
}

impl TsDemuxer {
    /// Cria um novo `TsDemuxer` com os canais de saída fornecidos.
    ///
    /// SPEC-TS-002
    pub fn new(
        section_tx: Sender<SectionData>,
        pes_tx: Sender<PesData>,
        event_tx: Sender<TsEvent>,
    ) -> Self {
        Self {
            section_tx,
            pes_tx,
            event_tx,
            cc_state: HashMap::new(),
            pmt_pids: HashSet::new(),
            av_pids: HashSet::new(),
        }
    }

    /// Registra um PID como PMT (chamado ao parsear a PAT).
    ///
    /// SPEC-TS-002a
    pub fn register_pmt_pid(&mut self, pid: Pid) {
        self.pmt_pids.insert(pid);
    }

    /// Registra um PID como A/V (chamado ao parsear a PMT).
    ///
    /// SPEC-TS-002a
    pub fn register_av_pid(&mut self, pid: Pid) {
        self.av_pids.insert(pid);
    }

    /// Processa um chunk de bytes (múltiplos de 188 bytes).
    ///
    /// Itera o buffer em janelas de 188 bytes. Se o byte na posição atual não
    /// for `0x47`, avança byte a byte até encontrar o próximo sync byte e emite
    /// [`TsEvent::SyncLost`] com o número de bytes ignorados.
    ///
    /// SPEC-TS-002c
    pub fn process_chunk(&mut self, raw: &[u8]) {
        let mut pos = 0;

        while pos < raw.len() {
            // ── Recuperação de sync (SPEC-TS-002c) ──────────────────────────
            if raw[pos] != 0x47 {
                let sync_start = pos;
                while pos < raw.len() && raw[pos] != 0x47 {
                    pos += 1;
                }
                let bytes_skipped = pos - sync_start;
                if self
                    .event_tx
                    .try_send(TsEvent::SyncLost { bytes_skipped })
                    .is_err()
                {
                    warn!("event_tx cheio; SyncLost descartado (bytes_skipped={})", bytes_skipped);
                }
                // Se não há mais bytes, encerrar.
                if pos >= raw.len() {
                    break;
                }
            }

            // Precisamos de exatamente 188 bytes para montar um pacote.
            if pos + 188 > raw.len() {
                break;
            }

            let pkt_raw = &raw[pos..pos + 188];
            pos += 188;

            match TsPacket::parse(pkt_raw) {
                Ok(pkt) => self.dispatch(pkt),
                Err(TsError::InvalidSyncByte(_)) => {
                    // Não deveria ocorrer: verificamos raw[pos] == 0x47 acima.
                    warn!("sync byte inválido inesperado após verificação de sync");
                }
                Err(e) => {
                    warn!("falha ao parsear pacote TS: {e}");
                }
            }
        }
    }

    /// Despacha um pacote TS: valida CC e roteia o payload para o canal correto.
    fn dispatch(&mut self, pkt: TsPacket) {
        let pid = pkt.pid;

        // Emite evento de métricas para todo pacote não-null.
        // (Null packets também são contabilizados para cálculo de bitrate.)
        if self
            .event_tx
            .try_send(TsEvent::Packet { pid, bytes: 188 })
            .is_err()
        {
            warn!("event_tx cheio; Packet(pid=0x{:04X}) descartado", pid);
        }

        // ── Null packets — descartar após contabilizar ───────────────────────
        if pid == PID_NULL {
            return;
        }

        // ── Validação de Continuity Counter (SPEC-TS-002b) ───────────────────
        //
        // Ignorar CC se:
        //   - AFC indica adaptation field apenas (sem payload)
        //   - pacote está embaralhado (scrambling != 0)
        //   - TEI está setado (erro de transmissão)
        let is_adaptation_only = pkt.payload.is_none() && pkt.adaptation_field.is_some();
        let should_validate_cc = !is_adaptation_only && pkt.scrambling == 0 && !pkt.tei;

        if should_validate_cc {
            if let Some(&prev_cc) = self.cc_state.get(&pid) {
                let expected = (prev_cc + 1) & 0x0F;
                if pkt.continuity_counter != expected
                    && self
                        .event_tx
                        .try_send(TsEvent::CcError {
                            pid,
                            expected,
                            got: pkt.continuity_counter,
                        })
                        .is_err()
                {
                    warn!("event_tx cheio; CcError(pid=0x{:04X}) descartado", pid);
                }
            }
            self.cc_state.insert(pid, pkt.continuity_counter);
        }

        // ── Roteamento de payload (SPEC-TS-002a) ─────────────────────────────
        let Some(payload) = pkt.payload else {
            // Pacotes adaptation-only não carregam dados para montar seções/PES.
            return;
        };

        if self.av_pids.contains(&pid) {
            // PID de A/V → canal PES.
            if self.pes_tx.try_send(PesData { pid, data: payload }).is_err() {
                warn!("pes_tx cheio; PesData(pid=0x{:04X}) descartado", pid);
            }
        } else if self.is_section_pid(pid) {
            // PID de seção conhecida ou PMT → canal de seções.
            if self
                .section_tx
                .try_send(SectionData {
                    pid,
                    pusi: pkt.pusi,
                    payload,
                })
                .is_err()
            {
                warn!("section_tx cheio; SectionData(pid=0x{:04X}) descartado", pid);
            }
        } else {
            // PID desconhecido → rotear como seção (tentativa).
            if self
                .section_tx
                .try_send(SectionData {
                    pid,
                    pusi: pkt.pusi,
                    payload,
                })
                .is_err()
            {
                warn!(
                    "section_tx cheio; SectionData(pid=0x{:04X}, desconhecido) descartado",
                    pid
                );
            }
        }
    }

    /// Retorna `true` se o PID pertence à tabela de seções conhecidas ou às PMTs
    /// registradas dinamicamente.
    #[inline]
    fn is_section_pid(&self, pid: Pid) -> bool {
        matches!(pid, PID_PAT | PID_NIT | PID_SDT | PID_EIT | PID_TDT)
            || self.pmt_pids.contains(&pid)
    }
}

// ── Testes ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Constrói um pacote TS de 188 bytes com payload only (AFC=0b01).
    fn build_payload_packet(pid: Pid, cc: u8) -> [u8; 188] {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = ((pid >> 8) & 0x1F) as u8;
        pkt[2] = (pid & 0xFF) as u8;
        // AFC=0b01 (payload only) | CC
        pkt[3] = (0b01 << 4) | (cc & 0x0F);
        pkt
    }

    /// Constrói um pacote TS de 188 bytes com adaptation-only (AFC=0b10).
    fn build_adaptation_only_packet(pid: Pid, cc: u8) -> [u8; 188] {
        let mut pkt = [0x00u8; 188];
        pkt[0] = 0x47;
        pkt[1] = ((pid >> 8) & 0x1F) as u8;
        pkt[2] = (pid & 0xFF) as u8;
        // AFC=0b10 (adaptation only) | CC
        pkt[3] = (0b10 << 4) | (cc & 0x0F);
        // adaptation_field_length = 0 (stuffing)
        pkt[4] = 0x00;
        pkt
    }

    /// Constrói um pacote TS null (PID=0x1FFF).
    fn build_null_packet(cc: u8) -> [u8; 188] {
        build_payload_packet(PID_NULL, cc)
    }

    // ── SPEC-TS-002b — Detecção de CC error ──────────────────────────────────

    /// CC errors são detectados e emitidos quando a sequência de continuity
    /// counter é violada (salto de 2 em vez de 1).
    ///
    /// SPEC-TS-002b
    #[test]
    fn spec_ts_002_cc_error_detection() {
        let (sec_tx, _sec_rx) = bounded(64);
        let (pes_tx, _pes_rx) = bounded(64);
        let (evt_tx, evt_rx) = bounded(64);

        let mut demuxer = TsDemuxer::new(sec_tx, pes_tx, evt_tx);

        let pid: Pid = 0x0100;

        // 5 pacotes: CC 0,1,2,3 são corretos; o 5º usa CC=5 (esperado: 4).
        let mut chunk = Vec::with_capacity(5 * 188);
        for cc in [0u8, 1, 2, 3, 5] {
            chunk.extend_from_slice(&build_payload_packet(pid, cc));
        }

        demuxer.process_chunk(&chunk);

        // Coletar eventos (ignorar TsEvent::Packet)
        let events: Vec<TsEvent> = evt_rx.try_iter().collect();
        let cc_errors: Vec<&TsEvent> = events
            .iter()
            .filter(|e| matches!(e, TsEvent::CcError { .. }))
            .collect();

        assert_eq!(cc_errors.len(), 1, "exatamente um CcError esperado");

        match cc_errors[0] {
            TsEvent::CcError { pid: epid, expected, got } => {
                assert_eq!(*epid, pid);
                assert_eq!(*expected, 4, "esperado CC=4");
                assert_eq!(*got, 5, "recebido CC=5");
            }
            _ => panic!("evento inesperado"),
        }
    }

    /// Múltiplos CC errors consecutivos são todos detectados.
    ///
    /// SPEC-TS-002b
    #[test]
    fn spec_ts_002_cc_multiple_errors() {
        let (sec_tx, _sec_rx) = bounded(64);
        let (pes_tx, _pes_rx) = bounded(64);
        let (evt_tx, evt_rx) = bounded(64);

        let mut demuxer = TsDemuxer::new(sec_tx, pes_tx, evt_tx);

        let pid: Pid = 0x0200;

        // Pacotes com CC=0, CC=2, CC=5 — dois saltos errôneos.
        let mut chunk = Vec::with_capacity(3 * 188);
        for cc in [0u8, 2, 5] {
            chunk.extend_from_slice(&build_payload_packet(pid, cc));
        }

        demuxer.process_chunk(&chunk);

        let events: Vec<TsEvent> = evt_rx.try_iter().collect();
        let cc_errors: Vec<&TsEvent> = events
            .iter()
            .filter(|e| matches!(e, TsEvent::CcError { .. }))
            .collect();

        assert_eq!(cc_errors.len(), 2, "dois CcErrors esperados");
    }

    // ── SPEC-TS-002b — Null packet — CC ignorado ─────────────────────────────

    /// CC de null packets (PID=0x1FFF) nunca é verificado.
    ///
    /// SPEC-TS-002b
    #[test]
    fn spec_ts_002_cc_null_packet_ignored() {
        let (sec_tx, _sec_rx) = bounded(64);
        let (pes_tx, _pes_rx) = bounded(64);
        let (evt_tx, evt_rx) = bounded(64);

        let mut demuxer = TsDemuxer::new(sec_tx, pes_tx, evt_tx);

        // Cinco null packets com CCs que seriam inválidos se verificados.
        let mut chunk = Vec::with_capacity(5 * 188);
        for cc in [0u8, 5, 10, 3, 1] {
            chunk.extend_from_slice(&build_null_packet(cc));
        }

        demuxer.process_chunk(&chunk);

        let events: Vec<TsEvent> = evt_rx.try_iter().collect();
        let cc_errors: Vec<&TsEvent> = events
            .iter()
            .filter(|e| matches!(e, TsEvent::CcError { .. }))
            .collect();

        assert!(cc_errors.is_empty(), "nenhum CcError esperado para null packets");
    }

    // ── SPEC-TS-002b — Adaptation-only — CC não incrementa ───────────────────

    /// Pacotes com AFC=adaptation-only não incrementam o CC e não geram CcError.
    ///
    /// SPEC-TS-002b
    #[test]
    fn spec_ts_002_cc_adaptation_only_no_increment() {
        let (sec_tx, _sec_rx) = bounded(64);
        let (pes_tx, _pes_rx) = bounded(64);
        let (evt_tx, evt_rx) = bounded(64);

        let mut demuxer = TsDemuxer::new(sec_tx, pes_tx, evt_tx);

        let pid: Pid = 0x0300;

        // Primeiro pacote normal para inicializar estado CC (CC=0).
        let mut chunk = Vec::with_capacity(3 * 188);
        chunk.extend_from_slice(&build_payload_packet(pid, 0));

        // Pacote adaptation-only com CC=0 (repetindo; seria erro se verificado).
        chunk.extend_from_slice(&build_adaptation_only_packet(pid, 0));

        // Pacote normal seguinte com CC=1 (correto: incremento do último payload).
        chunk.extend_from_slice(&build_payload_packet(pid, 1));

        demuxer.process_chunk(&chunk);

        let events: Vec<TsEvent> = evt_rx.try_iter().collect();
        let cc_errors: Vec<&TsEvent> = events
            .iter()
            .filter(|e| matches!(e, TsEvent::CcError { .. }))
            .collect();

        assert!(cc_errors.is_empty(), "pacote adaptation-only não deve gerar CcError");
    }

    // ── SPEC-TS-002c — Recuperação de sync ───────────────────────────────────

    /// Quando o buffer começa com bytes inválidos (não 0x47), o demuxer re-sincroniza,
    /// emite SyncLost com o número correto de bytes ignorados e processa o pacote
    /// que segue o sync byte.
    ///
    /// SPEC-TS-002c
    #[test]
    fn spec_ts_002_sync_recovery() {
        let (sec_tx, sec_rx) = bounded(64);
        let (pes_tx, _pes_rx) = bounded(64);
        let (evt_tx, evt_rx) = bounded(64);

        let mut demuxer = TsDemuxer::new(sec_tx, pes_tx, evt_tx);

        let pid: Pid = PID_PAT;

        // 7 bytes de lixo seguidos de um pacote TS válido.
        let mut chunk = vec![0xFFu8; 7];
        chunk.extend_from_slice(&build_payload_packet(pid, 0));

        demuxer.process_chunk(&chunk);

        // SyncLost deve ter sido emitido com bytes_skipped=7.
        let events: Vec<TsEvent> = evt_rx.try_iter().collect();
        let sync_lost: Vec<&TsEvent> = events
            .iter()
            .filter(|e| matches!(e, TsEvent::SyncLost { .. }))
            .collect();

        assert_eq!(sync_lost.len(), 1, "exatamente um SyncLost esperado");
        match sync_lost[0] {
            TsEvent::SyncLost { bytes_skipped } => {
                assert_eq!(*bytes_skipped, 7);
            }
            _ => panic!("evento inesperado"),
        }

        // O pacote após o sync deve ter sido processado (chegou ao section_tx).
        let sections: Vec<SectionData> = sec_rx.try_iter().collect();
        assert_eq!(sections.len(), 1, "um pacote de seção esperado após re-sync");
        assert_eq!(sections[0].pid, pid);
    }

    /// Chunk sem nenhum sync byte válido emite SyncLost e não trava o demuxer.
    ///
    /// SPEC-TS-002c
    #[test]
    fn spec_ts_002_sync_recovery_no_valid_sync() {
        let (sec_tx, _sec_rx) = bounded(64);
        let (pes_tx, _pes_rx) = bounded(64);
        let (evt_tx, evt_rx) = bounded(64);

        let mut demuxer = TsDemuxer::new(sec_tx, pes_tx, evt_tx);

        let chunk = vec![0xAAu8; 376]; // dois pacotes de "lixo"
        demuxer.process_chunk(&chunk);

        let events: Vec<TsEvent> = evt_rx.try_iter().collect();
        let sync_lost: Vec<&TsEvent> = events
            .iter()
            .filter(|e| matches!(e, TsEvent::SyncLost { .. }))
            .collect();

        assert_eq!(sync_lost.len(), 1);
        match sync_lost[0] {
            TsEvent::SyncLost { bytes_skipped } => {
                assert_eq!(*bytes_skipped, 376);
            }
            _ => panic!("evento inesperado"),
        }
    }

    // ── SPEC-TS-002a — Roteamento por PID ────────────────────────────────────

    /// PIDs bem conhecidos de seção (PAT, NIT, SDT, EIT, TDT) são roteados para
    /// o canal `section_tx`.
    ///
    /// SPEC-TS-002a
    #[test]
    fn spec_ts_002_routing_known_section_pids() {
        let known_pids = [PID_PAT, PID_NIT, PID_SDT, PID_EIT, PID_TDT];

        for pid in known_pids {
            let (sec_tx, sec_rx) = bounded(64);
            let (pes_tx, _pes_rx) = bounded(64);
            let (evt_tx, _evt_rx) = bounded(64);

            let mut demuxer = TsDemuxer::new(sec_tx, pes_tx, evt_tx);
            demuxer.process_chunk(&build_payload_packet(pid, 0));

            let sections: Vec<SectionData> = sec_rx.try_iter().collect();
            assert_eq!(
                sections.len(),
                1,
                "PID 0x{:04X} deve ser roteado para section_tx",
                pid
            );
            assert_eq!(sections[0].pid, pid);
        }
    }

    /// PID registrado via `register_pmt_pid` é roteado para `section_tx`.
    ///
    /// SPEC-TS-002a
    #[test]
    fn spec_ts_002_routing_registered_pmt_pid() {
        let (sec_tx, sec_rx) = bounded(64);
        let (pes_tx, _pes_rx) = bounded(64);
        let (evt_tx, _evt_rx) = bounded(64);

        let mut demuxer = TsDemuxer::new(sec_tx, pes_tx, evt_tx);
        let pmt_pid: Pid = 0x0100;
        demuxer.register_pmt_pid(pmt_pid);

        demuxer.process_chunk(&build_payload_packet(pmt_pid, 0));

        let sections: Vec<SectionData> = sec_rx.try_iter().collect();
        assert_eq!(sections.len(), 1, "PID de PMT deve ser roteado para section_tx");
        assert_eq!(sections[0].pid, pmt_pid);
    }

    /// PID registrado via `register_av_pid` é roteado para `pes_tx`.
    ///
    /// SPEC-TS-002a
    #[test]
    fn spec_ts_002_routing_registered_av_pid() {
        let (sec_tx, _sec_rx) = bounded(64);
        let (pes_tx, pes_rx) = bounded(64);
        let (evt_tx, _evt_rx) = bounded(64);

        let mut demuxer = TsDemuxer::new(sec_tx, pes_tx, evt_tx);
        let av_pid: Pid = 0x0200;
        demuxer.register_av_pid(av_pid);

        demuxer.process_chunk(&build_payload_packet(av_pid, 0));

        let pes_pkts: Vec<PesData> = pes_rx.try_iter().collect();
        assert_eq!(pes_pkts.len(), 1, "PID de A/V deve ser roteado para pes_tx");
        assert_eq!(pes_pkts[0].pid, av_pid);
    }

    /// Null packets (PID=0x1FFF) não são roteados para nenhum canal de dados.
    ///
    /// SPEC-TS-002a
    #[test]
    fn spec_ts_002_routing_null_packet_discarded() {
        let (sec_tx, sec_rx) = bounded(64);
        let (pes_tx, pes_rx) = bounded(64);
        let (evt_tx, _evt_rx) = bounded(64);

        let mut demuxer = TsDemuxer::new(sec_tx, pes_tx, evt_tx);

        demuxer.process_chunk(&build_null_packet(0));

        assert!(sec_rx.try_iter().next().is_none(), "null packet não deve chegar em section_tx");
        assert!(pes_rx.try_iter().next().is_none(), "null packet não deve chegar em pes_tx");
    }

    /// Pacote vazio não causa panic.
    #[test]
    fn spec_ts_002_empty_chunk_no_panic() {
        let (sec_tx, _sec_rx) = bounded(64);
        let (pes_tx, _pes_rx) = bounded(64);
        let (evt_tx, _evt_rx) = bounded(64);

        let mut demuxer = TsDemuxer::new(sec_tx, pes_tx, evt_tx);
        demuxer.process_chunk(&[]);
    }

    /// CC wraps-around corretamente: após CC=15 o próximo esperado é CC=0.
    ///
    /// SPEC-TS-002b
    #[test]
    fn spec_ts_002_cc_wraparound() {
        let (sec_tx, _sec_rx) = bounded(64);
        let (pes_tx, _pes_rx) = bounded(64);
        let (evt_tx, evt_rx) = bounded(64);

        let mut demuxer = TsDemuxer::new(sec_tx, pes_tx, evt_tx);

        let pid: Pid = 0x0400;

        // Pacote com CC=15, depois CC=0 (wrap-around correto).
        let mut chunk = Vec::with_capacity(2 * 188);
        chunk.extend_from_slice(&build_payload_packet(pid, 15));
        chunk.extend_from_slice(&build_payload_packet(pid, 0));

        demuxer.process_chunk(&chunk);

        let events: Vec<TsEvent> = evt_rx.try_iter().collect();
        let cc_errors: Vec<&TsEvent> = events
            .iter()
            .filter(|e| matches!(e, TsEvent::CcError { .. }))
            .collect();

        assert!(cc_errors.is_empty(), "wrap-around CC=15→0 não deve gerar CcError");
    }
}
