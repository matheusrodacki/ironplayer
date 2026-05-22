//! MetricsAggregator — loop de drenagem de canais e publicação de snapshot.
//!
//! Roda em thread dedicada, consome eventos de `TsEvent`, `PcrEvent` e
//! `AggregatorNetEvent`, e publica `MetricsSnapshot` imutável via
//! `tokio::sync::watch` a cada 1 segundo.
//!
//! SPEC-METRICS-003

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use crate::error::{PcrEvent, TsEvent};
use crate::metrics::{
    BitrateMonitor, ErrorSnapshot, ErrorTracker, MetricsSnapshot, PcrDiscontinuityRecord,
    PcrJitterRecord, PidEntry, PidType,
};
use crate::Pid;

// ---------------------------------------------------------------------------
// StopToken — cancelamento limpo do loop
// ---------------------------------------------------------------------------

/// Token de parada para o loop do `MetricsAggregator`.
///
/// SPEC-METRICS-003
#[derive(Clone)]
pub struct StopToken(Arc<AtomicBool>);

/// Handle que permite sinalizar a parada do `MetricsAggregator`.
///
/// SPEC-METRICS-003
pub struct StopHandle(Arc<AtomicBool>);

impl StopToken {
    /// Cria um par `(StopToken, StopHandle)`.
    ///
    /// SPEC-METRICS-003
    pub fn new() -> (Self, StopHandle) {
        let flag = Arc::new(AtomicBool::new(false));
        (StopToken(Arc::clone(&flag)), StopHandle(flag))
    }

    /// Retorna `true` se o sinal de parada foi enviado.
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl Default for StopToken {
    fn default() -> Self {
        StopToken(Arc::new(AtomicBool::new(false)))
    }
}

impl StopHandle {
    /// Sinaliza a parada do loop.
    pub fn stop(&self) {
        self.0.store(true, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// SnapshotSender / SnapshotReceiver — publicação lock-free do MetricsSnapshot
// ---------------------------------------------------------------------------

/// Publica o `MetricsSnapshot` mais recente para os leitores.
pub struct SnapshotSender(std::sync::Arc<std::sync::RwLock<crate::metrics::MetricsSnapshot>>);

/// Permite ler o `MetricsSnapshot` mais recente publicado pelo aggregator.
///
/// Obtido via `MetricsAggregator::new`. Use `borrow()` para clonar o snapshot
/// atual; a leitura nunca bloqueia o pipeline.
#[derive(Clone)]
pub struct SnapshotReceiver(std::sync::Arc<std::sync::RwLock<crate::metrics::MetricsSnapshot>>);

impl SnapshotSender {
    fn send(&self, snapshot: crate::metrics::MetricsSnapshot) {
        if let Ok(mut guard) = self.0.write() {
            *guard = snapshot;
        }
    }
}

impl SnapshotReceiver {
    /// Retorna uma cópia do snapshot mais recente.
    pub fn borrow(&self) -> crate::metrics::MetricsSnapshot {
        self.0
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| crate::metrics::MetricsSnapshot {
                pid_table: vec![],
                total_bitrate_kbps: 0.0,
                null_ratio: 0.0,
                errors: crate::metrics::ErrorSnapshot {
                    cc_errors: std::collections::HashMap::new(),
                    pcr_jitter_events: vec![],
                    pcr_discontinuities: vec![],
                    crc_errors: std::collections::HashMap::new(),
                    sync_losses: 0,
                    rtp_out_of_order: 0,
                    udp_overflows: 0,
                },
                tdt_offset_secs: None,
                timestamp: std::time::Instant::now(),
                av_sync_offset_ms: 0,
                late_frames_dropped: 0,
                early_frames_held: 0,
                pts_discontinuities: 0,
                video_queue_depth: 0,
            })
    }
}

// ---------------------------------------------------------------------------
// AggregatorNetEvent — eventos de rede relevantes para o aggregator
// ---------------------------------------------------------------------------

/// Eventos de rede relevantes para o `MetricsAggregator`.
///
/// Definido localmente para manter o crate `ts` autossuficiente (sem dep em
/// `net`). O wiring layer converte `net::NetEvent` / `net::RtpEvent` para este
/// tipo antes de enviar ao aggregator.
///
/// SPEC-METRICS-003
#[derive(Debug, Clone)]
pub enum AggregatorNetEvent {
    /// Limpa métricas acumuladas ao reiniciar/trocar a fonte do stream.
    Reset,
    /// Overflow do buffer UDP.
    UdpBufferOverflow,
    /// Pacote RTP recebido fora de ordem.
    RtpOutOfOrder,
}

// ---------------------------------------------------------------------------
// PidInfo — metadados estáticos por PID
// ---------------------------------------------------------------------------

/// Informação estática de um PID: tipo funcional e label legível.
#[derive(Debug, Clone)]
struct PidInfo {
    pid_type: PidType,
    label: String,
}

// ---------------------------------------------------------------------------
// MetricsAggregator
// ---------------------------------------------------------------------------

/// Aggregator central de métricas do pipeline MPEG-TS.
///
/// Roda em thread dedicada (via `run`), consome eventos dos canais e publica
/// `MetricsSnapshot` imutável via `tokio::sync::watch` a cada 1 segundo.
///
/// ## Criação
///
/// ```rust,ignore
/// let (ts_tx, ts_rx) = crossbeam_channel::bounded(256);
/// let (pcr_tx, pcr_rx) = crossbeam_channel::bounded(64);
/// let (net_tx, net_rx) = crossbeam_channel::bounded(64);
/// let (agg, snap_rx) = MetricsAggregator::new(ts_rx, pcr_rx, net_rx);
/// let (stop, handle) = StopToken::new();
/// std::thread::spawn(move || agg.run(stop));
/// ```
///
/// SPEC-METRICS-003
pub struct MetricsAggregator {
    ts_rx: Receiver<TsEvent>,
    pcr_rx: Receiver<PcrEvent>,
    net_rx: Receiver<AggregatorNetEvent>,
    snapshot_tx: SnapshotSender,
    bitrate: BitrateMonitor,
    errors: ErrorTracker,
    pid_info: HashMap<Pid, PidInfo>,
}

impl MetricsAggregator {
    /// Cria o aggregator e retorna o receiver do canal de snapshots.
    ///
    /// O snapshot inicial é vazio (nenhum PID, todos os contadores em zero).
    ///
    /// SPEC-METRICS-003
    pub fn new(
        ts_rx: Receiver<TsEvent>,
        pcr_rx: Receiver<PcrEvent>,
        net_rx: Receiver<AggregatorNetEvent>,
    ) -> (Self, SnapshotReceiver) {
        let initial = MetricsSnapshot {
            pid_table: vec![],
            total_bitrate_kbps: 0.0,
            null_ratio: 0.0,
            errors: ErrorSnapshot {
                cc_errors: HashMap::new(),
                pcr_jitter_events: vec![],
                pcr_discontinuities: vec![],
                crc_errors: HashMap::new(),
                sync_losses: 0,
                rtp_out_of_order: 0,
                udp_overflows: 0,
            },
            tdt_offset_secs: None,
            timestamp: Instant::now(),
            av_sync_offset_ms: 0,
            late_frames_dropped: 0,
            early_frames_held: 0,
            pts_discontinuities: 0,
            video_queue_depth: 0,
        };
        let shared = std::sync::Arc::new(std::sync::RwLock::new(initial));
        let snapshot_tx = SnapshotSender(std::sync::Arc::clone(&shared));
        let snapshot_rx = SnapshotReceiver(shared);
        let agg = Self {
            ts_rx,
            pcr_rx,
            net_rx,
            snapshot_tx,
            bitrate: BitrateMonitor::new(Duration::from_secs(1)),
            errors: ErrorTracker::new(1000),
            pid_info: HashMap::new(),
        };
        (agg, snapshot_rx)
    }

    /// Loop principal do aggregator.
    ///
    /// Drena os três canais de eventos e publica um `MetricsSnapshot` via
    /// `watch` a cada 1 segundo. Termina quando `stop.is_stopped()` retorna
    /// `true`.
    ///
    /// **Não bloqueia o pipeline:** usa `try_recv` para drenagem non-blocking;
    /// dorme 10 ms quando os canais estão vazios para evitar busy-wait.
    ///
    /// SPEC-METRICS-003
    pub fn run(mut self, stop: StopToken) {
        const PUBLISH_INTERVAL: Duration = Duration::from_secs(1);
        const IDLE_SLEEP: Duration = Duration::from_millis(10);

        let mut last_publish = Instant::now();

        loop {
            let mut had_events = false;

            // --- Drenar canal AggregatorNetEvent ---
            while let Ok(event) = self.net_rx.try_recv() {
                had_events = true;
                self.handle_net_event(event);
            }

            // --- Drenar canal TsEvent ---
            while let Ok(event) = self.ts_rx.try_recv() {
                had_events = true;
                self.handle_ts_event(event);
            }

            // --- Drenar canal PcrEvent ---
            while let Ok(event) = self.pcr_rx.try_recv() {
                had_events = true;
                self.handle_pcr_event(event);
            }

            // --- Publicar snapshot a cada 1 s ---
            if last_publish.elapsed() >= PUBLISH_INTERVAL {
                let snapshot = self.build_snapshot();
                self.snapshot_tx.send(snapshot);
                last_publish = Instant::now();
            }

            // --- Verificar parada ---
            if stop.is_stopped() {
                break;
            }

            // --- Dormir brevemente quando os canais estão vazios ---
            if !had_events {
                std::thread::sleep(IDLE_SLEEP);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Handlers internos
    // -----------------------------------------------------------------------

    fn handle_ts_event(&mut self, event: TsEvent) {
        match event {
            TsEvent::CcError { pid, .. } => {
                self.errors.record_cc_error(pid);
            }
            TsEvent::CrcError { pid, table_id } => {
                self.errors.record_crc_error(pid, table_id);
            }
            TsEvent::SyncLost { .. } => {
                self.errors.record_sync_loss();
            }
            TsEvent::Packet { pid, bytes } => {
                self.bitrate.update(pid, bytes);
            }
        }
    }

    fn handle_pcr_event(&mut self, event: PcrEvent) {
        match event {
            PcrEvent::Jitter {
                pid,
                expected_us,
                measured_us,
            } => {
                self.errors.record_pcr_jitter(PcrJitterRecord {
                    pid,
                    timestamp: Instant::now(),
                    expected_us,
                    measured_us,
                });
            }
            PcrEvent::Discontinuity { pid, .. } => {
                self.errors
                    .record_pcr_discontinuity(PcrDiscontinuityRecord {
                        pid,
                        timestamp: Instant::now(),
                    });
            }
        }
    }

    fn handle_net_event(&mut self, event: AggregatorNetEvent) {
        match event {
            AggregatorNetEvent::Reset => {
                while self.ts_rx.try_recv().is_ok() {}
                while self.pcr_rx.try_recv().is_ok() {}
                self.bitrate = BitrateMonitor::new(Duration::from_secs(1));
                self.errors.reset();
                self.pid_info.clear();
                self.snapshot_tx.send(MetricsSnapshot::default());
            }
            AggregatorNetEvent::UdpBufferOverflow => {
                self.errors.record_udp_overflow();
            }
            AggregatorNetEvent::RtpOutOfOrder => {
                self.errors.record_rtp_out_of_order();
            }
        }
    }

    // -----------------------------------------------------------------------
    // Construção do snapshot
    // -----------------------------------------------------------------------

    fn build_snapshot(&self) -> MetricsSnapshot {
        let bitrate_entries = self.bitrate.snapshot();
        let error_snap = self.errors.snapshot();

        let pid_table: Vec<PidEntry> = bitrate_entries
            .into_iter()
            .map(|entry| {
                let info = self.pid_info.get(&entry.pid);
                let cc = error_snap.cc_errors.get(&entry.pid).copied().unwrap_or(0);
                PidEntry {
                    pid: entry.pid,
                    pid_type: info.map(|i| i.pid_type.clone()).unwrap_or(PidType::Unknown),
                    label: info.map(|i| i.label.clone()).unwrap_or_default(),
                    bitrate_kbps: entry.bitrate_kbps,
                    cc_errors: cc,
                    packet_count: entry.packet_count,
                }
            })
            .collect();

        MetricsSnapshot {
            pid_table,
            total_bitrate_kbps: self.bitrate.total_bitrate_kbps(),
            null_ratio: self.bitrate.null_packet_ratio(),
            errors: error_snap,
            tdt_offset_secs: None,
            timestamp: Instant::now(),
            av_sync_offset_ms: 0,
            late_frames_dropped: 0,
            early_frames_held: 0,
            pts_discontinuities: 0,
            video_queue_depth: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{DiscontinuityReason, TsEvent};
    use crossbeam_channel::bounded;
    use std::thread;

    /// SPEC-METRICS-003 — aggregator publica snapshot após 1 s.
    #[test]
    fn spec_metrics_003_aggregator_publishes_snapshot() {
        let (ts_tx, ts_rx) = bounded::<TsEvent>(64);
        let (pcr_tx, pcr_rx) = bounded::<PcrEvent>(32);
        let (net_tx, net_rx) = bounded::<AggregatorNetEvent>(32);

        let (agg, snap_rx) = MetricsAggregator::new(ts_rx, pcr_rx, net_rx);
        let (stop, handle) = StopToken::new();

        // Envia um pacote para garantir que o bitrate seja > 0 no snapshot
        ts_tx
            .send(TsEvent::Packet {
                pid: 0x0100,
                bytes: 188,
            })
            .unwrap();
        // Força pedido de CC error para validar processamento
        ts_tx
            .send(TsEvent::CcError {
                pid: 0x0100,
                expected: 1,
                got: 2,
            })
            .unwrap();

        let _pcr_tx = pcr_tx;
        let _net_tx = net_tx;

        let t = thread::spawn(move || agg.run(stop));

        // Aguarda snapshot ser publicado (intervalo de 1 s + margem)
        std::thread::sleep(Duration::from_millis(1300));

        handle.stop();
        t.join().expect("aggregator thread deve finalizar");

        let snap = snap_rx.borrow();
        let published = snap.errors.total_cc_errors() > 0;
        assert!(published, "snapshot deve refletir CC error após 1 s");
    }

    /// SPEC-METRICS-003 — stop token encerra o loop limpo.
    #[test]
    fn spec_metrics_003_stop_token_stops_loop() {
        let (_ts_tx, ts_rx) = bounded::<TsEvent>(4);
        let (_pcr_tx, pcr_rx) = bounded::<PcrEvent>(4);
        let (_net_tx, net_rx) = bounded::<AggregatorNetEvent>(4);

        let (agg, _snap_rx) = MetricsAggregator::new(ts_rx, pcr_rx, net_rx);
        let (stop, handle) = StopToken::new();

        let t = thread::spawn(move || agg.run(stop));

        // Sinaliza parada imediatamente
        handle.stop();

        // Thread deve finalizar dentro de 1 s
        t.join().expect("aggregator deve finalizar após stop token");
    }

    /// SPEC-METRICS-003 — eventos CC e Packet são processados antes do snapshot.
    #[test]
    fn spec_metrics_003_events_reflected_in_snapshot() {
        let (ts_tx, ts_rx) = bounded::<TsEvent>(64);
        let (_pcr_tx, pcr_rx) = bounded::<PcrEvent>(4);
        let (_net_tx, net_rx) = bounded::<AggregatorNetEvent>(4);

        let (mut agg, _snap_rx) = MetricsAggregator::new(ts_rx, pcr_rx, net_rx);

        // Processa eventos diretamente sem spawnar thread
        ts_tx
            .send(TsEvent::Packet {
                pid: 0x0100,
                bytes: 188,
            })
            .unwrap();
        ts_tx
            .send(TsEvent::CcError {
                pid: 0x0200,
                expected: 0,
                got: 2,
            })
            .unwrap();
        ts_tx.send(TsEvent::SyncLost { bytes_skipped: 4 }).unwrap();
        ts_tx
            .send(TsEvent::CrcError {
                pid: 0x0011,
                table_id: 0x42,
            })
            .unwrap();

        // Drena manualmente
        while let Ok(ev) = agg.ts_rx.try_recv() {
            agg.handle_ts_event(ev);
        }

        let snap = agg.build_snapshot();

        // Verifica que o pacote contribui para bitrate
        assert!(snap.total_bitrate_kbps > 0.0, "bitrate deve ser > 0");
        // Verifica CC error registrado
        assert_eq!(snap.errors.cc_errors.get(&0x0200).copied().unwrap_or(0), 1);
        // Verifica sync loss
        assert_eq!(snap.errors.sync_losses, 1);
        // Verifica CRC error
        assert_eq!(
            snap.errors
                .crc_errors
                .get(&(0x0011, 0x42u8))
                .copied()
                .unwrap_or(0),
            1
        );
    }

    /// SPEC-METRICS-003 — AggregatorNetEvent atualiza contadores de rede.
    #[test]
    fn spec_metrics_003_net_events_update_counters() {
        let (_ts_tx, ts_rx) = bounded::<TsEvent>(4);
        let (_pcr_tx, pcr_rx) = bounded::<PcrEvent>(4);
        let (net_tx, net_rx) = bounded::<AggregatorNetEvent>(4);

        let (mut agg, _snap_rx) = MetricsAggregator::new(ts_rx, pcr_rx, net_rx);

        net_tx.send(AggregatorNetEvent::UdpBufferOverflow).unwrap();
        net_tx.send(AggregatorNetEvent::RtpOutOfOrder).unwrap();
        net_tx.send(AggregatorNetEvent::RtpOutOfOrder).unwrap();

        while let Ok(ev) = agg.net_rx.try_recv() {
            agg.handle_net_event(ev);
        }

        let snap = agg.build_snapshot();
        assert_eq!(snap.errors.udp_overflows, 1);
        assert_eq!(snap.errors.rtp_out_of_order, 2);
    }

    /// SPEC-METRICS-003 — Reset limpa métricas acumuladas e publica snapshot vazio.
    #[test]
    fn spec_metrics_003_reset_clears_snapshot() {
        let (ts_tx, ts_rx) = bounded::<TsEvent>(4);
        let (_pcr_tx, pcr_rx) = bounded::<PcrEvent>(4);
        let (net_tx, net_rx) = bounded::<AggregatorNetEvent>(4);

        let (mut agg, snap_rx) = MetricsAggregator::new(ts_rx, pcr_rx, net_rx);

        ts_tx
            .send(TsEvent::Packet {
                pid: 0x0100,
                bytes: 188,
            })
            .unwrap();
        ts_tx
            .send(TsEvent::CcError {
                pid: 0x0100,
                expected: 1,
                got: 2,
            })
            .unwrap();
        while let Ok(event) = agg.ts_rx.try_recv() {
            agg.handle_ts_event(event);
        }
        assert!(agg.build_snapshot().total_bitrate_kbps > 0.0);

        net_tx.send(AggregatorNetEvent::Reset).unwrap();
        while let Ok(event) = agg.net_rx.try_recv() {
            agg.handle_net_event(event);
        }

        let snap = snap_rx.borrow();
        assert!(snap.pid_table.is_empty());
        assert_eq!(snap.total_bitrate_kbps, 0.0);
        assert!(snap.errors.cc_errors.is_empty());
    }

    /// SPEC-METRICS-003 — PcrEvent Jitter e Discontinuity são processados.
    #[test]
    fn spec_metrics_003_pcr_events_recorded() {
        let (_ts_tx, ts_rx) = bounded::<TsEvent>(4);
        let (pcr_tx, pcr_rx) = bounded::<PcrEvent>(4);
        let (_net_tx, net_rx) = bounded::<AggregatorNetEvent>(4);

        let (mut agg, _snap_rx) = MetricsAggregator::new(ts_rx, pcr_rx, net_rx);

        pcr_tx
            .send(PcrEvent::Jitter {
                pid: 0x0100,
                expected_us: 1_000_000,
                measured_us: 1_000_600,
            })
            .unwrap();
        pcr_tx
            .send(PcrEvent::Discontinuity {
                pid: 0x0100,
                reason: DiscontinuityReason::Flag,
            })
            .unwrap();

        while let Ok(ev) = agg.pcr_rx.try_recv() {
            agg.handle_pcr_event(ev);
        }

        let snap = agg.build_snapshot();
        assert_eq!(snap.errors.pcr_jitter_events.len(), 1);
        assert_eq!(snap.errors.pcr_discontinuities.len(), 1);
    }
}
