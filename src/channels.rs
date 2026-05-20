use av::{AudioFrame, PesPacket, VideoFrame};
/// SPEC-CHAN-001
/// Helper de canais bounded com monitoramento de backpressure.
///
/// Todos os canais do pipeline IronPlayer são criados aqui com capacidades
/// fixas em tempo de compilação. Produtores usam [`BoundedSender::try_send`]
/// que:
/// - Em ≥ 90% da capacidade: emite `tracing::warn!`
/// - Em 100% (canal cheio): dropa o item e emite `tracing::warn!`
use bytes::Bytes;
use crossbeam_channel::{bounded, Receiver, Sender};
use net::NetEvent;
use ts::{CompleteSection, PcrEvent, PesData, SectionData, TsEvent};
use ui::TableEvent;

// ─── Capacidades (SPEC-CHAN-001) ──────────────────────────────────────────────

/// Capacidade do canal `net_raw` (UdpReceiver → RtpStripper).
pub const CAP_NET_RAW: usize = 128;
/// Capacidade do canal `ts_raw` (RtpStripper → TsDemuxer).
pub const CAP_TS_RAW: usize = 128;
/// Capacidade do canal `section_data` (TsDemuxer → SectionAssembler).
pub const CAP_SECTION_DATA: usize = 64;
/// Capacidade do canal `pes_data` (TsDemuxer → PesAssembler).
pub const CAP_PES_DATA: usize = 256;
/// Capacidade do canal `ts_events` (TsDemuxer → MetricsAggregator).
pub const CAP_TS_EVENTS: usize = 1024;
/// Capacidade do canal `complete_sections` (SectionAssembler → TableDispatcher).
pub const CAP_COMPLETE_SECTIONS: usize = 64;
/// Capacidade do canal `pes_packets` (PesAssembler → FfmpegDecoder).
pub const CAP_PES_PACKETS: usize = 256;
/// Capacidade do canal `table_events` (TableDispatcher → AppState).
pub const CAP_TABLE_EVENTS: usize = 64;
/// Capacidade do canal `video_frames` (FfmpegDecoder → VideoRenderer).
pub const CAP_VIDEO_FRAMES: usize = 8;
/// Capacidade do canal `audio_frames` (FfmpegDecoder → AudioOutput).
pub const CAP_AUDIO_FRAMES: usize = 32;
/// Capacidade do canal `pcr_events` (PcrTracker → MetricsAggregator).
pub const CAP_PCR_EVENTS: usize = 256;
/// Capacidade do canal `net_events` (UdpReceiver → MetricsAggregator).
pub const CAP_NET_EVENTS: usize = 64;
/// Capacidade do canal `app_commands` (UI → CommandHandler).
pub const CAP_APP_COMMANDS: usize = 32;

// ─── Tipos placeholder ────────────────────────────────────────────────────────

/// Comando enviado pela UI ao pipeline. Placeholder até o crate `ui`.
#[derive(Debug)]
pub struct AppCommand;

// ─── BoundedSender ────────────────────────────────────────────────────────────

/// SPEC-CHAN-001
/// Wrapper sobre [`crossbeam_channel::Sender<T>`] com monitoramento de backpressure.
///
/// Emite `tracing::warn!` quando o canal atingir ≥ 90% da capacidade.
/// Dropa o item e emite `tracing::warn!` quando o canal estiver 100% cheio.
pub struct BoundedSender<T> {
    inner: Sender<T>,
    name: &'static str,
}

impl<T> BoundedSender<T> {
    pub(crate) fn new(inner: Sender<T>, name: &'static str) -> Self {
        Self { inner, name }
    }

    /// Retorna a capacidade máxima do canal.
    #[allow(dead_code)]
    pub fn capacity(&self) -> usize {
        self.inner.capacity().unwrap_or(0)
    }

    /// Retorna um clone do [`Sender<T>`] interno.
    ///
    /// Use para passar a componentes que exigem um sender crossbeam bruto.
    /// Envios via este sender **não** passam pelo monitoramento de backpressure
    /// do [`BoundedSender`]. Prefira [`BoundedSender::try_send`] quando possível.
    pub fn sender(&self) -> Sender<T> {
        self.inner.clone()
    }

    /// SPEC-CHAN-001
    /// Tenta enviar um item no canal com monitoramento de backpressure.
    ///
    /// Retorna `true` se o item foi enviado com sucesso.
    /// Retorna `false` se o item foi descartado (canal cheio ou desconectado).
    pub fn try_send(&self, msg: T) -> bool {
        let cap = self.inner.capacity().unwrap_or(0);
        let used = self.inner.len();

        // Alerta em ≥ 90% da capacidade
        if cap > 0 && used * 10 >= cap * 9 {
            tracing::warn!(
                "canal {} em {}% da capacidade ({}/{})",
                self.name,
                used * 100 / cap,
                used,
                cap,
            );
        }

        match self.inner.try_send(msg) {
            Ok(()) => true,
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                tracing::warn!(
                    "canal {} cheio ({}/{}) — item descartado",
                    self.name,
                    used,
                    cap,
                );
                false
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                tracing::warn!("canal {} desconectado — item descartado", self.name);
                false
            }
        }
    }
}

impl<T> Clone for BoundedSender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            name: self.name,
        }
    }
}

// ─── AppChannels ──────────────────────────────────────────────────────────────

/// SPEC-CHAN-001
/// Todos os canais bounded do pipeline IronPlayer criados com as capacidades
/// definidas na tabela de capacidades.
#[allow(dead_code)]
pub struct AppChannels {
    /// `net_raw`: UdpReceiver → RtpStripper
    pub net_raw_tx: BoundedSender<Bytes>,
    pub net_raw_rx: Receiver<Bytes>,

    /// `ts_raw`: RtpStripper → TsDemuxer
    pub ts_raw_tx: BoundedSender<Bytes>,
    pub ts_raw_rx: Receiver<Bytes>,

    /// `section_data`: TsDemuxer → SectionAssembler
    pub section_data_tx: BoundedSender<SectionData>,
    pub section_data_rx: Receiver<SectionData>,

    /// `pes_data`: TsDemuxer → PesAssembler
    pub pes_data_tx: BoundedSender<PesData>,
    pub pes_data_rx: Receiver<PesData>,

    /// `ts_events`: TsDemuxer → MetricsAggregator
    pub ts_events_tx: BoundedSender<TsEvent>,
    pub ts_events_rx: Receiver<TsEvent>,

    /// `complete_sections`: SectionAssembler → TableDispatcher
    pub complete_sections_tx: BoundedSender<CompleteSection>,
    pub complete_sections_rx: Receiver<CompleteSection>,

    /// `pes_packets`: PesAssembler → FfmpegDecoder
    pub pes_packets_tx: BoundedSender<PesPacket>,
    pub pes_packets_rx: Receiver<PesPacket>,

    /// `table_events`: TableDispatcher → AppState
    pub table_events_tx: BoundedSender<TableEvent>,
    pub table_events_rx: Receiver<TableEvent>,

    /// `video_frames`: FfmpegDecoder → VideoRenderer
    pub video_frames_tx: BoundedSender<VideoFrame>,
    pub video_frames_rx: Receiver<VideoFrame>,

    /// `audio_frames`: FfmpegDecoder → AudioOutput
    pub audio_frames_tx: BoundedSender<AudioFrame>,
    pub audio_frames_rx: Receiver<AudioFrame>,

    /// `pcr_events`: PcrTracker → MetricsAggregator
    pub pcr_events_tx: BoundedSender<PcrEvent>,
    pub pcr_events_rx: Receiver<PcrEvent>,

    /// `net_events`: UdpReceiver → MetricsAggregator
    pub net_events_tx: BoundedSender<NetEvent>,
    pub net_events_rx: Receiver<NetEvent>,

    /// `app_commands`: UI → CommandHandler
    pub app_commands_tx: BoundedSender<AppCommand>,
    pub app_commands_rx: Receiver<AppCommand>,
}

impl AppChannels {
    /// SPEC-CHAN-001
    /// Cria todos os canais bounded conforme a tabela de capacidades.
    pub fn create() -> Self {
        macro_rules! chan {
            ($cap:expr, $name:expr) => {{
                let (tx, rx) = bounded($cap);
                (BoundedSender::new(tx, $name), rx)
            }};
        }

        let (net_raw_tx, net_raw_rx) = chan!(CAP_NET_RAW, "net_raw");
        let (ts_raw_tx, ts_raw_rx) = chan!(CAP_TS_RAW, "ts_raw");
        let (section_data_tx, section_data_rx) = chan!(CAP_SECTION_DATA, "section_data");
        let (pes_data_tx, pes_data_rx) = chan!(CAP_PES_DATA, "pes_data");
        let (ts_events_tx, ts_events_rx) = chan!(CAP_TS_EVENTS, "ts_events");
        let (complete_sections_tx, complete_sections_rx) =
            chan!(CAP_COMPLETE_SECTIONS, "complete_sections");
        let (pes_packets_tx, pes_packets_rx) = chan!(CAP_PES_PACKETS, "pes_packets");
        let (table_events_tx, table_events_rx) = chan!(CAP_TABLE_EVENTS, "table_events");
        let (video_frames_tx, video_frames_rx) = chan!(CAP_VIDEO_FRAMES, "video_frames");
        let (audio_frames_tx, audio_frames_rx) = chan!(CAP_AUDIO_FRAMES, "audio_frames");
        let (pcr_events_tx, pcr_events_rx) = chan!(CAP_PCR_EVENTS, "pcr_events");
        let (net_events_tx, net_events_rx) = chan!(CAP_NET_EVENTS, "net_events");
        let (app_commands_tx, app_commands_rx) = chan!(CAP_APP_COMMANDS, "app_commands");

        Self {
            net_raw_tx,
            net_raw_rx,
            ts_raw_tx,
            ts_raw_rx,
            section_data_tx,
            section_data_rx,
            pes_data_tx,
            pes_data_rx,
            ts_events_tx,
            ts_events_rx,
            complete_sections_tx,
            complete_sections_rx,
            pes_packets_tx,
            pes_packets_rx,
            table_events_tx,
            table_events_rx,
            video_frames_tx,
            video_frames_rx,
            audio_frames_tx,
            audio_frames_rx,
            pcr_events_tx,
            pcr_events_rx,
            net_events_tx,
            net_events_rx,
            app_commands_tx,
            app_commands_rx,
        }
    }
}

// ─── Testes ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC-CHAN-001
    /// `try_send` descarta item e retorna `false` quando o canal está cheio.
    #[test]
    fn spec_chan_001_try_send_drops_on_full() {
        let (tx, _rx) = bounded::<u32>(2);
        let sender = BoundedSender::new(tx, "test_chan");

        assert!(sender.try_send(1), "primeiro envio deve ter sucesso");
        assert!(sender.try_send(2), "segundo envio deve ter sucesso");
        // Canal cheio — deve descartar e retornar false
        assert!(
            !sender.try_send(3),
            "envio com canal cheio deve retornar false"
        );
    }

    /// SPEC-CHAN-001
    /// `try_send` ainda retorna `true` (envia) quando canal está em exatamente 90%.
    #[test]
    fn spec_chan_001_try_send_succeeds_at_90_percent() {
        let (tx, _rx) = bounded::<u32>(10);
        let sender = BoundedSender::new(tx, "test_chan_90");

        // Preenche até 90% (9/10)
        for i in 0..9 {
            assert!(sender.try_send(i), "envio {i} deve ter sucesso");
        }
        // 90%: deve emitir WARN mas ainda enviar (true)
        assert!(
            sender.try_send(9),
            "envio em 90% deve ainda ter sucesso (com WARN)"
        );
        // 100%: deve descartar (false)
        assert!(
            !sender.try_send(10),
            "envio com canal cheio deve retornar false"
        );
    }

    /// SPEC-CHAN-001
    /// `AppChannels::create()` cria todos os canais com as capacidades corretas.
    #[test]
    fn spec_chan_001_all_channels_created_with_correct_capacities() {
        let ch = AppChannels::create();

        assert_eq!(ch.net_raw_tx.capacity(), CAP_NET_RAW);
        assert_eq!(ch.ts_raw_tx.capacity(), CAP_TS_RAW);
        assert_eq!(ch.section_data_tx.capacity(), CAP_SECTION_DATA);
        assert_eq!(ch.pes_data_tx.capacity(), CAP_PES_DATA);
        assert_eq!(ch.ts_events_tx.capacity(), CAP_TS_EVENTS);
        assert_eq!(ch.complete_sections_tx.capacity(), CAP_COMPLETE_SECTIONS);
        assert_eq!(ch.pes_packets_tx.capacity(), CAP_PES_PACKETS);
        assert_eq!(ch.table_events_tx.capacity(), CAP_TABLE_EVENTS);
        assert_eq!(ch.video_frames_tx.capacity(), CAP_VIDEO_FRAMES);
        assert_eq!(ch.audio_frames_tx.capacity(), CAP_AUDIO_FRAMES);
        assert_eq!(ch.pcr_events_tx.capacity(), CAP_PCR_EVENTS);
        assert_eq!(ch.net_events_tx.capacity(), CAP_NET_EVENTS);
        assert_eq!(ch.app_commands_tx.capacity(), CAP_APP_COMMANDS);
    }
}
