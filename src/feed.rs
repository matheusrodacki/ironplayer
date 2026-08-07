//! `FeedPipeline` — o wiring de recepção/demux replicável por slot.
//!
//! Antes desta spec, [`crate::channels::AppChannels`] criava **um** conjunto
//! global de canais e `main.rs` montava **um** pipeline.  Suportar 2 feeds
//! simultâneos (SPEC-PROBE-017) exige extrair esse conjunto para uma unidade
//! replicável — é o item mais arriscado da spec e por isso vem cedo (§12).
//!
//! Regras que este módulo materializa (§5.2):
//!
//! - Nomes de thread recebem sufixo de slot (`net-recv-0`, `ts-demux-1`); sem
//!   o sufixo, dois feeds saturando ficam indistinguíveis no log.
//! - O encerramento é **por feed**: parar o feed 1 não fecha nada do feed 0.
//! - Nada aqui assume "exatamente 2" — índices, nomes e capacidades derivam do
//!   slot (SPEC-PROBE-017a).
//!
//! O pipeline A/V (decoder, áudio, `VideoQueue`) **não** faz parte do
//! `FeedPipeline`: em modo Probe ele não existe (SPEC-PROBE-002), e em
//! Cinema/Broadcast só o slot 0 o instancia, em `main.rs`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use crossbeam_channel::bounded;
use net::{
    NetEvent, ReceiverConfig, RtpStripper, StopHandle as NetStopHandle, StopToken as NetStopToken,
    StreamUrl, UdpReceiver,
};
use probe::{Encapsulation, FecMode, ProbeConfig};
use ts::aggregator::{
    AggregatorNetEvent, MetricsAggregator, SnapshotReceiver, StopHandle as MetricsStopHandle,
    StopToken as MetricsStopToken,
};
use ts::{CompleteSection, Pid, SectionAssembler, TsDemuxer};

use crate::channels::BoundedSender;

/// Limite de feeds simultâneos.
///
/// SPEC-PROBE-017a — constante única, reexportada do crate `probe` para que
/// exista **um** lugar a mudar.
pub const MAX_FEEDS: usize = probe::MAX_FEEDS;

/// Sem datagrama por este tempo, o feed é considerado indisponível.
///
/// SPEC-PROBE-011 — dois períodos de amostragem: um único segundo sem pacote
/// num stream de 15 Mbps já seria anômalo, mas exigir dois evita marcar
/// indisponibilidade por um hiccup de agendamento da própria probe.
const OFFLINE_AFTER: Duration = Duration::from_millis(2_000);

/// Descrição de um feed a instanciar.
#[derive(Debug, Clone)]
pub struct FeedSpec {
    pub slot: usize,
    pub name: String,
    pub url_text: String,
    pub url: StreamUrl,
    pub fec: FecMode,
}

impl FeedSpec {
    /// Grupo e porta, usados na validação de duplicidade.
    ///
    /// SPEC-PROBE-017 — dois feeds no mesmo grupo/porta são rejeitados: seria
    /// duplicar tráfego sem ganho, e as pastas de sessão colidiriam (§6).
    pub fn group_port(&self) -> (std::net::Ipv4Addr, u16) {
        match self.url {
            StreamUrl::UdpMulticast { group, port, .. }
            | StreamUrl::RtpMulticast { group, port, .. } => (group, port),
        }
    }
}

/// Erros de validação da lista de feeds.
#[derive(Debug)]
pub enum FeedError {
    /// URL inválida.
    Url { url: String, reason: String },
    /// Dois feeds no mesmo grupo/porta (SPEC-PROBE-017).
    Duplicate { url: String },
    /// Mais feeds do que `MAX_FEEDS`.
    TooMany { requested: usize },
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Url { url, reason } => write!(f, "URL de feed inválida '{url}': {reason}"),
            Self::Duplicate { url } => write!(
                f,
                "feed duplicado em '{url}' — dois feeds no mesmo grupo/porta duplicariam tráfego sem ganho"
            ),
            Self::TooMany { requested } => write!(
                f,
                "{requested} feeds configurados, mas o limite desta versão é {MAX_FEEDS}"
            ),
        }
    }
}

impl std::error::Error for FeedError {}

/// Valida e resolve a lista de feeds do `[probe]`.
///
/// SPEC-PROBE-017 · SPEC-PROBE-017a
pub fn resolve_feeds(cfg: &ProbeConfig) -> Result<Vec<FeedSpec>, FeedError> {
    let configured = cfg.effective_feeds();
    if configured.len() > MAX_FEEDS {
        return Err(FeedError::TooMany {
            requested: configured.len(),
        });
    }

    let mut seen: HashSet<(std::net::Ipv4Addr, u16)> = HashSet::new();
    let mut out = Vec::with_capacity(configured.len());

    for (slot, feed) in configured.into_iter().enumerate() {
        let url = StreamUrl::parse(&feed.url).map_err(|e| FeedError::Url {
            url: feed.url.clone(),
            reason: e.to_string(),
        })?;
        let spec = FeedSpec {
            slot,
            name: feed.name,
            url_text: feed.url,
            url,
            fec: feed.fec,
        };
        if !seen.insert(spec.group_port()) {
            return Err(FeedError::Duplicate { url: spec.url_text });
        }
        out.push(spec);
    }

    Ok(out)
}

/// Estado observável de um feed, compartilhado entre as threads do slot e o
/// motor de checks.
///
/// Tudo aqui é atômico ou `RwLock` de leitura curta: o `probe-engine` amostra
/// isto uma vez por segundo e não pode nunca bloquear a recepção.
#[derive(Debug)]
pub struct FeedShared {
    epoch: Instant,
    /// Milissegundos desde `epoch` do último datagrama recebido.
    last_packet_ms: AtomicU64,
    /// Datagramas recebidos na vida do feed.
    packets: AtomicU64,
    /// Descartes atribuídos à própria probe (SPEC-PROBE-013).
    local_drops: AtomicU64,
    /// Tentativas de reconexão desde a última queda (SPEC-PROBE-011).
    reconnect_attempts: AtomicU32,
    /// [`Encapsulation`] serializado como `u8` (SPEC-PROBE-018a).
    encapsulation: AtomicU8Encap,
    /// `transport_scrambling_control ≠ 0` visto em algum pacote.
    scrambled: AtomicBool,
    /// `SO_RCVBUF` efetivo, para o `session.toml`.
    so_rcvbuf: AtomicUsize,
    /// PIDs de vídeo e de áudio conhecidos pela PMT.
    video_pids: RwLock<Vec<Pid>>,
    audio_pids: RwLock<Vec<Pid>>,
    /// Nome do serviço vindo da SDT, quando houver.
    service_name: RwLock<Option<String>>,
    /// Codec do PID de vídeo primário, para armar o decoder de snapshot.
    video_codec: RwLock<Option<av::MediaCodec>>,
    /// Altura do vídeo observada no último snapshot (badge `HD`/`SD`).
    video_height: AtomicU32,
    /// Último resultado do tick de snapshot (SPEC-PROBE-003a).
    snapshot_state: AtomicU32,
    /// Thumbnail suspenso pelo 1º estágio de degradação (SPEC-PROBE-013a).
    snapshot_suspended: AtomicBool,
}

/// `Encapsulation` guardado num átomo.
///
/// Um `RwLock` para um enum de 4 variantes lido a 1 Hz e escrito a cada
/// datagrama seria desperdício; o mapeamento para `u8` é explícito e local.
#[derive(Debug)]
struct AtomicU8Encap(AtomicU32);

impl AtomicU8Encap {
    fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    fn set(&self, value: Encapsulation) {
        let v = match value {
            Encapsulation::Unknown => 0,
            Encapsulation::Udp => 1,
            Encapsulation::Rtp => 2,
            Encapsulation::RtpFec => 3,
        };
        self.0.store(v, Ordering::Relaxed);
    }

    fn get(&self) -> Encapsulation {
        match self.0.load(Ordering::Relaxed) {
            1 => Encapsulation::Udp,
            2 => Encapsulation::Rtp,
            3 => Encapsulation::RtpFec,
            _ => Encapsulation::Unknown,
        }
    }
}

impl FeedShared {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
            last_packet_ms: AtomicU64::new(0),
            packets: AtomicU64::new(0),
            local_drops: AtomicU64::new(0),
            reconnect_attempts: AtomicU32::new(0),
            encapsulation: AtomicU8Encap::new(),
            scrambled: AtomicBool::new(false),
            so_rcvbuf: AtomicUsize::new(0),
            video_pids: RwLock::new(Vec::new()),
            audio_pids: RwLock::new(Vec::new()),
            service_name: RwLock::new(None),
            video_codec: RwLock::new(None),
            video_height: AtomicU32::new(0),
            snapshot_state: AtomicU32::new(0),
            snapshot_suspended: AtomicBool::new(false),
        }
    }

    fn mark_packet(&self) {
        self.last_packet_ms
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
        self.packets.fetch_add(1, Ordering::Relaxed);
    }

    /// `true` enquanto chegaram datagramas nos últimos [`OFFLINE_AFTER`].
    ///
    /// SPEC-PROBE-011
    pub fn connected(&self) -> bool {
        is_connected(
            self.packets.load(Ordering::Relaxed),
            self.last_packet_ms.load(Ordering::Relaxed),
            self.epoch.elapsed().as_millis() as u64,
        )
    }

    /// Descartes locais acumulados (SPEC-PROBE-013).
    pub fn local_drops(&self) -> u64 {
        self.local_drops.load(Ordering::Relaxed)
    }

    /// Contabiliza um descarte local.
    pub fn add_local_drops(&self, n: u64) {
        self.local_drops.fetch_add(n, Ordering::Relaxed);
    }

    /// Tentativas de reconexão desde a última queda.
    pub fn reconnect_attempts(&self) -> u32 {
        self.reconnect_attempts.load(Ordering::Relaxed)
    }

    /// Encapsulamento detectado (SPEC-PROBE-018a).
    pub fn encapsulation(&self) -> Encapsulation {
        self.encapsulation.get()
    }

    /// `transport_scrambling_control ≠ 0` observado.
    pub fn scrambled(&self) -> bool {
        self.scrambled.load(Ordering::Relaxed)
    }

    /// `SO_RCVBUF` configurado no socket.
    pub fn so_rcvbuf(&self) -> usize {
        self.so_rcvbuf.load(Ordering::Relaxed)
    }

    /// PIDs de vídeo conhecidos pela PMT.
    pub fn video_pids(&self) -> Vec<Pid> {
        self.video_pids
            .read()
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// PIDs de áudio conhecidos pela PMT.
    pub fn audio_pids(&self) -> Vec<Pid> {
        self.audio_pids
            .read()
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// Nome do serviço vindo da SDT.
    pub fn service_name(&self) -> Option<String> {
        self.service_name.read().ok().and_then(|g| g.clone())
    }

    /// Codec do PID de vídeo primário (SPEC-PROBE-003).
    pub fn video_codec(&self) -> Option<av::MediaCodec> {
        self.video_codec.read().ok().and_then(|g| *g)
    }

    /// Altura do vídeo, quando já houve um snapshot decodificado.
    ///
    /// SPEC-PROBE-018 — badge `HD`/`SD`. Vem do frame do thumbnail porque o
    /// modo Probe não roda o `StreamProbe` de Media Info (SPEC-PROBE-002).
    pub fn video_height(&self) -> Option<u32> {
        match self.video_height.load(Ordering::Relaxed) {
            0 => None,
            h => Some(h),
        }
    }

    /// Registra a altura observada num frame de snapshot.
    pub fn set_video_height(&self, height: u32) {
        self.video_height.store(height, Ordering::Relaxed);
    }

    /// Estado do último tick de snapshot (SPEC-PROBE-003a).
    pub fn snapshot_state(&self) -> probe::SnapshotState {
        match self.snapshot_state.load(Ordering::Relaxed) {
            1 => probe::SnapshotState::Ok,
            2 => probe::SnapshotState::NoKeyframe,
            3 => probe::SnapshotState::Suspended,
            4 => probe::SnapshotState::NoSignal,
            _ => probe::SnapshotState::Pending,
        }
    }

    /// Publica o estado do último tick de snapshot.
    pub fn set_snapshot_state(&self, state: probe::SnapshotState) {
        let v = match state {
            probe::SnapshotState::Pending => 0,
            probe::SnapshotState::Ok => 1,
            probe::SnapshotState::NoKeyframe => 2,
            probe::SnapshotState::Suspended => 3,
            probe::SnapshotState::NoSignal => 4,
        };
        self.snapshot_state.store(v, Ordering::Relaxed);
    }

    /// `true` quando o thumbnail está suspenso por degradação.
    ///
    /// SPEC-PROBE-013a
    pub fn snapshot_suspended(&self) -> bool {
        self.snapshot_suspended.load(Ordering::Relaxed)
    }

    /// Suspende/retoma o thumbnail.
    ///
    /// SPEC-PROBE-013a
    pub fn set_snapshot_suspended(&self, suspended: bool) {
        self.snapshot_suspended.store(suspended, Ordering::Relaxed);
    }
}

/// Derivação do snapshot de vídeo a partir do PES do feed.
///
/// SPEC-PROBE-003 — o decoder fica **desarmado** por padrão: fora da janela de
/// armação nenhum byte de PES sai do dreno, e portanto nenhum frame extra é
/// decodificado entre ticks.
#[derive(Debug)]
pub struct SnapshotTap {
    armed: AtomicBool,
    /// PID capturado enquanto armado; `u32::MAX` = nenhum.
    pid: AtomicU32,
    tx: crossbeam_channel::Sender<ts::PesData>,
}

impl SnapshotTap {
    /// Arma a captura de um PID de vídeo.
    ///
    /// SPEC-PROBE-003a
    pub fn arm(&self, pid: Pid) {
        self.pid.store(pid as u32, Ordering::Relaxed);
        self.armed.store(true, Ordering::Release);
    }

    /// Desarma a captura.
    ///
    /// SPEC-PROBE-003a — "desarma após 1 frame".
    pub fn disarm(&self) {
        self.armed.store(false, Ordering::Release);
        self.pid.store(u32::MAX, Ordering::Relaxed);
    }

    fn wants(&self, pid: Pid) -> bool {
        self.armed.load(Ordering::Acquire) && self.pid.load(Ordering::Relaxed) == pid as u32
    }
}

/// Um feed instanciado: canais, threads e estado.
///
/// SPEC-PROBE-017 — o equivalente por slot do que antes era global.
pub struct FeedPipeline {
    pub spec: FeedSpec,
    pub shared: Arc<FeedShared>,
    pub snapshot_rx: SnapshotReceiver,
    /// Derivação de PES para o snapshot de vídeo (SPEC-PROBE-003).
    pub tap: Arc<SnapshotTap>,
    /// PES capturado enquanto o tap está armado.
    pub tap_rx: crossbeam_channel::Receiver<ts::PesData>,
    /// Canal de controle do aggregator (usado no `Reset` da reconexão).
    agg_net_tx: crossbeam_channel::Sender<AggregatorNetEvent>,
    /// Pedido de encerramento cooperativo das threads do slot.
    stop: Arc<AtomicBool>,
    net_stop: Arc<Mutex<Option<NetStopHandle>>>,
    metrics_stop: Option<MetricsStopHandle>,
    handles: Vec<std::thread::JoinHandle<()>>,
    /// Senders mantidos vivos por RAII; dropá-los desencadeia a cascata de
    /// encerramento **deste** feed (§5.2).
    guard: Option<FeedSenderGuard>,
}

/// Registro de roteamento enviado ao demuxer do slot.
///
/// Sem isto o `TsDemuxer` cai no caminho "PID desconhecido → rotear como
/// seção" (`demux.rs`), e **todo PID elementar** vai parar no
/// `SectionAssembler`: o canal de seções satura, o CRC-32 é calculado sobre
/// payload de PES e a probe passa a reportar centenas de `crc_error` por
/// segundo que não existem no stream.  Em `main.rs` quem faz esse registro é
/// o `TableDispatcher`; num feed de Probe não há dispatcher, então é o
/// `feed-tables-{slot}` que assume o papel.
#[derive(Debug, Clone, Copy)]
enum DemuxRoute {
    /// PID que carrega PMT (vindo da PAT).
    Pmt(Pid),
    /// PID de NIT sinalizado na PAT (`program_number == 0`).
    Nit(Pid),
    /// PID elementar listado numa PMT — roteado como PES, não como seção.
    Elementary(Pid),
}

/// Mantém vivos os senders da cadeia do feed até o shutdown.
///
/// Dropar isto fecha `net_raw` do slot → `rtp-strip-{slot}` sai → `ts_raw`
/// fecha → `ts-demux-{slot}` sai → … Cada feed tem o seu, de modo que parar o
/// feed 1 não pode fechar o `net_raw` do feed 0 (§5.2).
#[allow(dead_code)]
struct FeedSenderGuard {
    net_raw_tx: BoundedSender<Bytes>,
    section_data_tx: BoundedSender<ts::SectionData>,
    ts_events_tx: BoundedSender<ts::TsEvent>,
    complete_sections_tx: BoundedSender<CompleteSection>,
}

impl FeedPipeline {
    /// Instancia o pipeline de um feed.
    ///
    /// SPEC-PROBE-017
    pub fn spawn(spec: FeedSpec, cfg: &ProbeConfig, receiver_cfg: ReceiverConfig) -> Self {
        let slot = spec.slot;
        let shared = Arc::new(FeedShared::new());
        shared
            .so_rcvbuf
            .store(receiver_cfg.buf_size, Ordering::Relaxed);
        // O encapsulamento nominal vem da URL; a detecção em runtime (RTP com
        // ou sem FEC) refina isso no `rtp-strip` (SPEC-PROBE-018a).
        shared.encapsulation.set(match spec.url {
            StreamUrl::UdpMulticast { .. } => Encapsulation::Udp,
            StreamUrl::RtpMulticast { .. } => Encapsulation::Rtp,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let net_stop: Arc<Mutex<Option<NetStopHandle>>> = Arc::new(Mutex::new(None));
        let mut handles = Vec::with_capacity(5);

        // Capacidades iguais às do pipeline global (SPEC-CHAN-001); os nomes
        // ganham sufixo de slot para que o log de backpressure identifique o
        // feed (§5.2).
        let (net_raw_tx, net_raw_rx) = bounded::<Bytes>(crate::channels::CAP_NET_RAW);
        let (ts_raw_tx, ts_raw_rx) = bounded::<Bytes>(crate::channels::CAP_TS_RAW);
        let (section_data_tx, section_data_rx) =
            bounded::<ts::SectionData>(crate::channels::CAP_SECTION_DATA);
        let (pes_data_tx, pes_data_rx) = bounded::<ts::PesData>(crate::channels::CAP_PES_DATA);
        let (ts_events_tx, ts_events_rx) = bounded::<ts::TsEvent>(crate::channels::CAP_TS_EVENTS);
        let (pcr_events_tx, pcr_events_rx) =
            bounded::<ts::PcrEvent>(crate::channels::CAP_PCR_EVENTS);
        let (complete_sections_tx, complete_sections_rx) =
            bounded::<CompleteSection>(crate::channels::CAP_COMPLETE_SECTIONS);
        let (net_events_tx, net_events_rx) = bounded::<NetEvent>(crate::channels::CAP_NET_EVENTS);
        let (rtp_events_tx, rtp_events_rx) = bounded::<net::RtpEvent>(64);
        // Registros de roteamento PAT/PMT → demuxer. Capacidade folgada: só
        // recebe tráfego quando a PSI muda.
        let (route_tx, route_rx) = bounded::<DemuxRoute>(256);
        let (agg_net_tx, agg_net_rx) = bounded::<AggregatorNetEvent>(64);

        let net_raw_tx = BoundedSender::new_named(net_raw_tx, format!("net_raw-{slot}"));
        let ts_raw_tx = BoundedSender::new_named(ts_raw_tx, format!("ts_raw-{slot}"));
        let section_data_tx =
            BoundedSender::new_named(section_data_tx, format!("section_data-{slot}"));
        let ts_events_tx = BoundedSender::new_named(ts_events_tx, format!("ts_events-{slot}"));
        let complete_sections_tx =
            BoundedSender::new_named(complete_sections_tx, format!("complete_sections-{slot}"));
        let pes_data_tx = BoundedSender::new_named(pes_data_tx, format!("pes_data-{slot}"));
        let pcr_events_tx = BoundedSender::new_named(pcr_events_tx, format!("pcr_events-{slot}"));

        // Em modo Probe o PES não é montado nem decodificado continuamente
        // (SPEC-PROBE-002). O dreno joga tudo fora, exceto na janela em que o
        // tap está armado para o snapshot de vídeo (SPEC-PROBE-003).
        let (tap_tx, tap_rx) = bounded::<ts::PesData>(512);
        let tap = Arc::new(SnapshotTap {
            armed: AtomicBool::new(false),
            pid: AtomicU32::new(u32::MAX),
            tx: tap_tx,
        });
        {
            let tap_t = tap.clone();
            // `iter()` e não polling com timeout: registrados os PIDs
            // elementares, este canal recebe uma entrada por pacote TS
            // (~27 k/s a 41 Mbps) e um `recv_timeout` por item seria puro
            // desperdício. O laço termina sozinho quando `ts-demux-{slot}`
            // sai e o sender do demuxer é dropado.
            handles.push(thread(format!("pes-drain-{slot}"), move || {
                for data in pes_data_rx.iter() {
                    if tap_t.wants(data.pid) {
                        let _ = tap_t.tx.try_send(data);
                    }
                }
            }));
        }

        // ── net-recv-{slot}: recepção com reconexão automática ──────────
        {
            let shared_t = shared.clone();
            let stop_flag = stop.clone();
            let net_stop_t = net_stop.clone();
            let url = spec.url.clone();
            let url_text = spec.url_text.clone();
            let raw_tx = net_raw_tx.sender();
            let events_tx = net_events_tx.clone();
            let backoff: Vec<Duration> = (0..8).map(|i| cfg.reconnect_backoff(i)).collect();

            handles.push(thread(format!("net-recv-{slot}"), move || {
                // SPEC-PROBE-013a — a recepção UDP nunca é a primeira a
                // degradar; roda acima do normal (§5.4).
                probe::power::raise_current_thread_priority();
                let mut attempt = 0usize;

                while !stop_flag.load(Ordering::Relaxed) {
                    let (token, handle) = NetStopToken::new();
                    *net_stop_t.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);

                    let receiver = UdpReceiver::new(
                        url.clone(),
                        raw_tx.clone(),
                        events_tx.clone(),
                        receiver_cfg.clone(),
                    );
                    match receiver.run(token) {
                        Ok(()) => {
                            if stop_flag.load(Ordering::Relaxed) {
                                break;
                            }
                            tracing::warn!(slot, url = %url_text, "net-recv: encerrou sozinho — reconectando");
                        }
                        Err(e) => {
                            tracing::warn!(slot, url = %url_text, error = %e, "net-recv: erro — reconectando");
                        }
                    }

                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }

                    // SPEC-PROBE-011 — backoff crescente, saturando no último
                    // degrau; a sessão é preservada durante toda a espera.
                    let wait = backoff
                        .get(attempt.min(backoff.len().saturating_sub(1)))
                        .copied()
                        .unwrap_or(Duration::from_secs(5));
                    attempt = attempt.saturating_add(1);
                    shared_t
                        .reconnect_attempts
                        .store(attempt as u32, Ordering::Relaxed);
                    tracing::info!(slot, attempt, wait_ms = wait.as_millis() as u64, "net-recv: aguardando para reconectar");

                    // Espera fatiada para responder ao stop sem esperar o
                    // backoff inteiro no shutdown.
                    let deadline = Instant::now() + wait;
                    while Instant::now() < deadline && !stop_flag.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                tracing::info!(slot, "net-recv: encerrado");
            }));
        }

        // ── net-events-{slot}: dreno dos eventos do receptor ────────────
        {
            let shared_t = shared.clone();
            let agg_tx = agg_net_tx.clone();
            handles.push(thread(format!("net-events-{slot}"), move || {
                for evt in net_events_rx.iter() {
                    match evt {
                        NetEvent::Timeout => {
                            tracing::debug!(slot, "net-recv: timeout de recepção");
                        }
                        NetEvent::Started | NetEvent::Stopped => {}
                    }
                    // Overflow de buffer UDP, quando o crate `net` passar a
                    // reportá-lo, entra aqui como descarte local.
                    let _ = (&shared_t, &agg_tx);
                }
            }));
        }

        // ── rtp-strip-{slot}: header RTP + detecção de encapsulamento ───
        {
            let shared_t = shared.clone();
            let ts_raw = ts_raw_tx.clone();
            let agg_tx = agg_net_tx.clone();
            handles.push(thread(format!("rtp-strip-{slot}"), move || {
                let mut stripper = RtpStripper::new(rtp_events_tx);
                let mut classified = false;

                while let Ok(bytes) = net_raw_rx.recv() {
                    shared_t.mark_packet();

                    // SPEC-PROBE-018a — o encapsulamento é detectado em
                    // runtime, não deduzido apenas do esquema da URL: um
                    // `rtp://` que na verdade entrega TS puro precisa ficar
                    // com os checks de RTP em `n/a`, não verdes.
                    if !classified {
                        if let Some(first) = bytes.first() {
                            shared_t.encapsulation.set(if *first == 0x47 {
                                Encapsulation::Udp
                            } else {
                                Encapsulation::Rtp
                            });
                            classified = true;
                        }
                    }

                    let stripped = stripper.strip(bytes);
                    if !stripped.is_empty() && !ts_raw.try_send(stripped) {
                        shared_t.add_local_drops(1);
                    }

                    while let Ok(evt) = rtp_events_rx.try_recv() {
                        let agg_evt = match evt {
                            net::RtpEvent::OutOfOrder { .. } => AggregatorNetEvent::RtpOutOfOrder,
                        };
                        if agg_tx.try_send(agg_evt).is_err() {
                            shared_t.add_local_drops(1);
                        }
                    }
                }
                tracing::info!(slot, "rtp-strip: encerrado");
            }));
        }

        // ── ts-demux-{slot} ─────────────────────────────────────────────
        {
            let shared_t = shared.clone();
            let demuxer = TsDemuxer::new(
                section_data_tx.sender(),
                pes_data_tx.sender(),
                ts_events_tx.sender(),
            )
            .with_pcr_tracker(pcr_events_tx.sender());

            handles.push(thread(format!("ts-demux-{slot}"), move || {
                let mut demuxer = demuxer;
                for bytes in ts_raw_rx.iter() {
                    // Aplica os registros de PSI antes do próximo chunk: sem
                    // eles o demuxer trata PID elementar como seção.
                    while let Ok(route) = route_rx.try_recv() {
                        match route {
                            DemuxRoute::Pmt(pid) => demuxer.register_pmt_pid(pid),
                            DemuxRoute::Nit(pid) => demuxer.register_nit_pid(pid),
                            DemuxRoute::Elementary(pid) => demuxer.register_av_pid(pid),
                        }
                    }

                    // SPEC-PROBE-018 — badge `SCR`: `transport_scrambling_control`
                    // é o campo do cabeçalho TS (bits 7-6 do byte 3), não um
                    // descritor de PMT. Ler direto do chunk custa uma varredura
                    // de 1 byte a cada 188 e evita mudar o crate `ts`.
                    if !shared_t.scrambled() && scrambling_seen(&bytes) {
                        shared_t.scrambled.store(true, Ordering::Relaxed);
                    }
                    demuxer.process_chunk(&bytes);
                }
                tracing::info!(slot, "ts-demux: encerrado");
            }));
        }

        // ── sec-asm-{slot} ──────────────────────────────────────────────
        {
            let asm = SectionAssembler::new(complete_sections_tx.sender(), ts_events_tx.sender());
            handles.push(thread(format!("sec-asm-{slot}"), move || {
                let mut asm = asm;
                for data in section_data_rx.iter() {
                    if let Err(e) = asm.push(data) {
                        tracing::debug!(slot, error = %e, "sec-asm: seção inválida isolada");
                    }
                }
            }));
        }

        // ── feed-tables-{slot}: PAT/PMT/SDT mínimos ─────────────────────
        {
            let shared_t = shared.clone();
            handles.push(thread(format!("feed-tables-{slot}"), move || {
                let mut tables = FeedTables::default();
                for section in complete_sections_rx.iter() {
                    tables.apply(&section, &shared_t, &route_tx);
                }
            }));
        }

        // ── metrics-{slot} ──────────────────────────────────────────────
        let (metrics_agg, snapshot_rx) =
            MetricsAggregator::new(ts_events_rx, pcr_events_rx, agg_net_rx);
        let (metrics_stop_token, metrics_stop_handle): (MetricsStopToken, MetricsStopHandle) =
            MetricsStopToken::new();
        handles.push(thread(format!("metrics-{slot}"), move || {
            metrics_agg.run(metrics_stop_token);
        }));

        tracing::info!(
            slot,
            url = %spec.url_text,
            threads = handles.len(),
            "feed: pipeline instanciado"
        );

        Self {
            spec,
            shared,
            snapshot_rx,
            tap,
            tap_rx,
            agg_net_tx,
            stop,
            net_stop,
            metrics_stop: Some(metrics_stop_handle),
            handles,
            guard: Some(FeedSenderGuard {
                net_raw_tx,
                section_data_tx,
                ts_events_tx,
                complete_sections_tx,
            }),
        }
    }

    /// Ponta de controle do aggregator deste feed.
    ///
    /// SPEC-PROBE-011 — usada pela thread de engine ao detectar reconexão.
    pub fn agg_reset_sender(&self) -> crossbeam_channel::Sender<AggregatorNetEvent> {
        self.agg_net_tx.clone()
    }

    /// Encerra o feed em cascata, sem tocar em nenhum outro slot.
    ///
    /// §5.2 — `PipelineGuard` por feed.
    pub fn shutdown(&mut self) {
        let slot = self.spec.slot;
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self
            .net_stop
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            h.stop();
        }
        if let Some(h) = self.metrics_stop.take() {
            h.stop();
        }
        drop(self.guard.take());

        let deadline = Instant::now() + Duration::from_secs(2);
        for handle in self.handles.drain(..) {
            crate::join_with_deadline(handle, deadline);
        }
        tracing::info!(slot, "feed: encerrado");
    }
}

impl Drop for FeedPipeline {
    fn drop(&mut self) {
        if !self.handles.is_empty() {
            self.shutdown();
        }
    }
}

/// Regra de disponibilidade, isolada para ser testável sem esperar 2 s de
/// relógio real.
///
/// SPEC-PROBE-011 — um feed que nunca recebeu datagrama **não** está
/// conectado; isso importa porque `now - 0` é pequeno nos primeiros segundos
/// de vida do processo e sem o teste de `packets` todo feed nasceria online.
fn is_connected(packets: u64, last_packet_ms: u64, now_ms: u64) -> bool {
    packets > 0 && now_ms.saturating_sub(last_packet_ms) < OFFLINE_AFTER.as_millis() as u64
}

/// `true` se algum pacote do chunk tem `transport_scrambling_control ≠ 0`.
///
/// SPEC-PROBE-018 — apenas presença, nunca tentativa de descriptografar
/// (descriptografia/CA está fora de escopo, §2.2).
fn scrambling_seen(chunk: &[u8]) -> bool {
    chunk
        .chunks_exact(188)
        .any(|pkt| pkt[0] == 0x47 && (pkt[3] & 0xC0) != 0)
}

/// `stream_type` de vídeo segundo ISO 13818-1 Table 2-36 + H.264/HEVC.
///
/// `PmtStream` já expõe `is_audio()`; o espelho para vídeo não existe no crate
/// `ts` e é pequeno demais para justificar mexer lá só por causa do tile.
fn is_video_stream_type(stream_type: u8) -> bool {
    matches!(
        stream_type,
        0x01 | 0x02 | 0x10 | 0x1B | 0x24 | 0x42 | 0xD1 | 0xEA
    )
}

fn thread<F>(name: String, f: F) -> std::thread::JoinHandle<()>
where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name(name.clone())
        .spawn(f)
        .unwrap_or_else(|e| panic!("falha ao criar thread {name}: {e}"))
}

// ---------------------------------------------------------------------------
// FeedTables — PSI mínima do modo Probe
// ---------------------------------------------------------------------------

/// Consumidor de PSI reduzido ao que o tile precisa.
///
/// O `TableDispatcher` completo (auto-play, roteamento de decode, menu de
/// contexto) não faz sentido num feed sem player: aqui só interessa saber
/// quais PIDs são de vídeo e de áudio, para os indicadores `V`/`A` e para o
/// bitrate por tipo (§8.1).
#[derive(Default)]
struct FeedTables {
    pmt_pids: HashSet<Pid>,
    /// PIDs por serviço, para não perder trilhas ao reprocessar uma PMT.
    video: HashMap<u16, Vec<Pid>>,
    audio: HashMap<u16, Vec<Pid>>,
    /// Rotas já enviadas ao demuxer — a PMT se repete a cada ~100 ms e
    /// reenviar tudo a cada repetição saturaria o canal de controle à toa.
    routed: HashSet<Pid>,
}

impl FeedTables {
    fn route(&mut self, tx: &crossbeam_channel::Sender<DemuxRoute>, route: DemuxRoute) {
        let pid = match route {
            DemuxRoute::Pmt(pid) | DemuxRoute::Nit(pid) | DemuxRoute::Elementary(pid) => pid,
        };
        if self.routed.insert(pid) && tx.try_send(route).is_err() {
            // Falhou o envio: desfaz a marcação para tentar na próxima
            // repetição da tabela, em vez de deixar o PID sem rota para sempre.
            self.routed.remove(&pid);
        }
    }

    fn apply(
        &mut self,
        section: &CompleteSection,
        shared: &FeedShared,
        route_tx: &crossbeam_channel::Sender<DemuxRoute>,
    ) {
        // O corpo da seção começa depois dos 3 bytes de cabeçalho PSI; é o que
        // `from_section_body` espera (mesmo contrato do `TableDispatcher`).
        let body: &[u8] = match section.data.len() {
            0..=2 => &[],
            _ => &section.data[3..],
        };

        match section.table_id {
            // PAT
            0x00 => {
                if let Ok(pat) = ts::tables::Pat::from_section_body(body) {
                    // `program_number == 0` aponta para a NIT, não para uma PMT.
                    self.pmt_pids = pat.pmt_pids().collect();
                    for program in &pat.programs {
                        let route = if program.program_number == 0 {
                            DemuxRoute::Nit(program.pid)
                        } else {
                            DemuxRoute::Pmt(program.pid)
                        };
                        self.route(route_tx, route);
                    }
                }
            }
            // PMT
            0x02 if self.pmt_pids.contains(&section.pid) => {
                if let Ok(pmt) = ts::tables::Pmt::from_section_body(body) {
                    let mut video = Vec::new();
                    let mut audio = Vec::new();
                    for stream in &pmt.streams {
                        // Todo PID listado numa PMT é elementar, seja ele
                        // vídeo, áudio, legenda ou dado: o que importa aqui é
                        // tirá-lo do caminho de seções.
                        self.route(route_tx, DemuxRoute::Elementary(stream.elementary_pid));

                        if is_video_stream_type(stream.stream_type) {
                            if video.is_empty() {
                                if let Ok(mut guard) = shared.video_codec.write() {
                                    *guard = av::MediaCodec::from_stream_type(stream.stream_type);
                                }
                            }
                            video.push(stream.elementary_pid);
                        } else if stream.is_audio() {
                            audio.push(stream.elementary_pid);
                        }
                    }
                    self.video.insert(pmt.program_number, video);
                    self.audio.insert(pmt.program_number, audio);
                    self.publish(shared);
                }
            }
            // SDT atual
            0x42 => {
                // `Sdt::parse` recebe a seção inteira **com** CRC; o
                // `SectionAssembler` entrega sem os 4 bytes finais, então o
                // padding vazio recompõe o tamanho esperado (mesmo truque do
                // `TableDispatcher`).
                let mut with_crc = section.data.to_vec();
                with_crc.extend_from_slice(&[0, 0, 0, 0]);
                if let Ok(sdt) = ts::tables::Sdt::parse(&with_crc) {
                    if let Some(name) = sdt.services.iter().find_map(|s| s.service_name.clone()) {
                        if let Ok(mut guard) = shared.service_name.write() {
                            *guard = Some(name);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn publish(&self, shared: &FeedShared) {
        let flatten = |m: &HashMap<u16, Vec<Pid>>| {
            let mut v: Vec<Pid> = m.values().flatten().copied().collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        if let Ok(mut guard) = shared.video_pids.write() {
            *guard = flatten(&self.video);
        }
        if let Ok(mut guard) = shared.audio_pids.write() {
            *guard = flatten(&self.audio);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use probe::FeedConfig;

    fn cfg_with(urls: &[&str]) -> ProbeConfig {
        ProbeConfig {
            feeds: urls
                .iter()
                .enumerate()
                .map(|(i, u)| FeedConfig {
                    name: format!("feed{i}"),
                    url: (*u).to_string(),
                    fec: FecMode::Auto,
                })
                .collect(),
            ..Default::default()
        }
    }

    /// SPEC-PROBE-017 — feeds válidos viram slots 0..n na ordem do TOML.
    #[test]
    fn spec_probe_017_resolves_feeds_into_ordered_slots() {
        let cfg = cfg_with(&["rtp://@239.15.0.183:50000", "udp://@239.15.0.190:50000"]);
        let feeds = resolve_feeds(&cfg).expect("feeds válidos");
        assert_eq!(feeds.len(), 2);
        assert_eq!(feeds[0].slot, 0);
        assert_eq!(feeds[1].slot, 1);
        assert_eq!(feeds[0].name, "feed0");
    }

    /// SPEC-PROBE-017 — dois feeds no mesmo grupo/porta são rejeitados.
    #[test]
    fn spec_probe_017_rejects_duplicate_group_and_port() {
        let cfg = cfg_with(&["rtp://@239.15.0.183:50000", "udp://@239.15.0.183:50000"]);
        let err = resolve_feeds(&cfg).expect_err("duplicado deve falhar");
        assert!(matches!(err, FeedError::Duplicate { .. }), "{err}");
        assert!(err.to_string().contains("duplicariam tráfego"));
    }

    /// SPEC-PROBE-017 — grupos iguais em portas diferentes são feeds válidos.
    #[test]
    fn spec_probe_017_same_group_different_port_is_allowed() {
        let cfg = cfg_with(&["rtp://@239.15.0.183:50000", "rtp://@239.15.0.183:50010"]);
        assert_eq!(resolve_feeds(&cfg).expect("válidos").len(), 2);
    }

    /// SPEC-PROBE-017 — URL inválida é reportada com contexto, sem panic.
    #[test]
    fn spec_probe_017_invalid_url_is_reported() {
        let cfg = cfg_with(&["http://exemplo/stream.ts"]);
        let err = resolve_feeds(&cfg).expect_err("URL inválida deve falhar");
        assert!(matches!(err, FeedError::Url { .. }), "{err}");
        assert!(err.to_string().contains("http://exemplo/stream.ts"));
    }

    /// SPEC-PROBE-017a — o limite vem de uma constante única, e `MAX_FEEDS`
    /// é a mesma do crate `probe`.
    #[test]
    fn spec_probe_017a_max_feeds_is_a_single_constant() {
        assert_eq!(MAX_FEEDS, probe::MAX_FEEDS);
        let many: Vec<String> = (0..MAX_FEEDS + 2)
            .map(|i| format!("udp://@239.0.0.{}:1234", i + 1))
            .collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        // `effective_feeds` já trunca em `max_feeds`, então a lista resolvida
        // nunca passa do limite — nada aqui conta "2" à mão.
        let feeds = resolve_feeds(&cfg_with(&refs)).expect("truncado, não erro");
        assert_eq!(feeds.len(), MAX_FEEDS);
    }

    /// SPEC-PROBE-015 · SPEC-PROBE-018a — a seção `[probe]` do playout de
    /// bancada (TSDuck `-O ip`, UDP puro) atravessa serde e vira um feed no
    /// slot 0 com a camada RTP inaplicável.
    ///
    /// Vale como teste de regressão do formato do TOML: `fec` é um enum
    /// serializado em minúsculas, `[[probe.feeds]]` precisa vir depois dos
    /// escalares de `[probe]`, e o bloco é lido **aninhado no `AppConfig`** —
    /// desserializar o texto direto em `ProbeConfig` silenciosamente devolve
    /// zero feeds, porque `probe` vira um campo desconhecido e é ignorado.
    #[test]
    fn spec_probe_015_bench_playout_config_resolves_to_one_udp_feed() {
        let toml_str = r#"
[network]
timeout_ms = 5000

[probe]
profile_version = 1
max_feeds = 2

[[probe.feeds]]
name = "GLOBO_RJ_S_LL_COPA (playout local)"
url  = "udp://@239.0.0.1:1234"
fec  = "off"
"#;
        let app: crate::config::AppConfig =
            toml::from_str(toml_str).expect("ironstream.toml do playout deve parsear");
        let cfg = app.probe;
        assert_eq!(cfg.profile_version, 1);

        let feeds = resolve_feeds(&cfg).expect("feed válido");
        assert_eq!(feeds.len(), 1, "um feed configurado, um slot ocupado");
        assert_eq!(feeds[0].slot, 0);
        assert_eq!(feeds[0].fec, FecMode::Off);
        assert_eq!(
            feeds[0].group_port(),
            ("239.0.0.1".parse().expect("ipv4"), 1234)
        );
        // `-O ip` sem `--rtp` é UDP puro: o parse da URL precisa refletir isso,
        // senão o tile prometeria checks de RTP que ninguém está avaliando.
        assert!(
            matches!(feeds[0].url, StreamUrl::UdpMulticast { .. }),
            "esperado UDP puro, veio {:?}",
            feeds[0].url
        );

        // O slot restante fica livre no mosaico (§8.1), não vira erro.
        assert!(cfg.max_feeds > feeds.len());
    }

    /// Monta uma `CompleteSection` a partir do corpo (sem cabeçalho PSI nem
    /// CRC), no formato que o `SectionAssembler` entrega.
    fn section(pid: Pid, table_id: u8, body: &[u8]) -> CompleteSection {
        let mut data = vec![table_id, 0xB0, body.len() as u8];
        data.extend_from_slice(body);
        CompleteSection {
            pid,
            table_id,
            data: bytes::Bytes::from(data),
        }
    }

    /// SPEC-PROBE-005 — a PSI do feed **registra o roteamento no demuxer**.
    ///
    /// Regressão de um defeito real: sem estes registros o `TsDemuxer` cai no
    /// caminho "PID desconhecido → rotear como seção", entrega todo PID
    /// elementar ao `SectionAssembler` e a probe passa a reportar centenas de
    /// `crc_error` por segundo calculados sobre payload de PES — erros que não
    /// existem no stream. Um instrumento de diagnóstico inventando defeito é
    /// pior do que não medir.
    #[test]
    fn spec_probe_005_psi_registers_demux_routing_for_elementary_pids() {
        let (tx, rx) = bounded::<DemuxRoute>(64);
        let shared = FeedShared::new();
        let mut tables = FeedTables::default();

        // PAT: programa 1 → PMT no PID 0x0100; programa 0 → NIT no PID 0x0010.
        let pat_body = [
            0x00, 0x01, // transport_stream_id
            0x01, 0x00, 0x00, // version/current_next, section_number, last
            0x00, 0x00, 0xE0, 0x10, // program_number 0 (NIT) → pid 0x0010
            0x00, 0x01, 0xE1, 0x00, // program_number 1 → pid 0x0100
        ];
        tables.apply(&section(0x0000, 0x00, &pat_body), &shared, &tx);

        let routed: Vec<DemuxRoute> = rx.try_iter().collect();
        assert!(
            routed.iter().any(|r| matches!(r, DemuxRoute::Nit(0x0010))),
            "NIT da PAT deve ser registrada: {routed:?}"
        );
        assert!(
            routed.iter().any(|r| matches!(r, DemuxRoute::Pmt(0x0100))),
            "PMT da PAT deve ser registrada: {routed:?}"
        );

        // PMT do programa 1: vídeo H.264 (0x1B) no PID 0x0200, áudio AC-3
        // (0x81) no 0x0201 e uma legenda/dado (0x06) no 0x0202.
        let pmt_body = [
            0x00, 0x01, // program_number
            0x01, 0x00, 0x00, // version/current_next, section_number, last
            0xE2, 0x00, // PCR_PID = 0x0200
            0xF0, 0x00, // program_info_length = 0
            0x1B, 0xE2, 0x00, 0xF0, 0x00, // vídeo   pid 0x0200
            0x81, 0xE2, 0x01, 0xF0, 0x00, // áudio   pid 0x0201
            0x06, 0xE2, 0x02, 0xF0, 0x00, // privado pid 0x0202
        ];
        tables.apply(&section(0x0100, 0x02, &pmt_body), &shared, &tx);

        let routed: Vec<Pid> = rx
            .try_iter()
            .filter_map(|r| match r {
                DemuxRoute::Elementary(pid) => Some(pid),
                _ => None,
            })
            .collect();
        assert_eq!(
            routed,
            vec![0x0200, 0x0201, 0x0202],
            "todo PID da PMT sai do caminho de seções, inclusive o privado"
        );

        // E a classificação para os indicadores `V`/`A` do tile continua certa.
        assert_eq!(shared.video_pids(), vec![0x0200]);
        assert_eq!(shared.audio_pids(), vec![0x0201]);

        // Repetição da PMT (a cada ~100 ms no ar) não reenvia nada: o canal de
        // controle tem 256 vagas e saturaria em segundos.
        tables.apply(&section(0x0100, 0x02, &pmt_body), &shared, &tx);
        assert_eq!(rx.try_iter().count(), 0, "registro é idempotente");
    }

    /// SPEC-PROBE-018 — o badge `SCR` sai de `transport_scrambling_control`.
    #[test]
    fn spec_probe_018_detects_transport_scrambling_control() {
        let mut clear = vec![0u8; 188 * 2];
        clear[0] = 0x47;
        clear[188] = 0x47;
        assert!(!scrambling_seen(&clear));

        let mut scrambled = clear.clone();
        scrambled[188 + 3] = 0x80; // scrambling_control = 0b10
        assert!(scrambling_seen(&scrambled));

        // Sem sync byte, nada é afirmado (dado externo, RNF-PRB-003).
        let garbage = vec![0xFFu8; 188];
        assert!(!scrambling_seen(&garbage));
    }

    /// SPEC-PROBE-011 — o feed nasce desconectado e só fica online após um
    /// datagrama; passado o limiar sem tráfego, volta a offline.
    #[test]
    fn spec_probe_011_connected_tracks_recent_datagrams() {
        let limit = OFFLINE_AFTER.as_millis() as u64;

        // Sem nenhum datagrama, não está conectado — mesmo no instante zero,
        // quando `now - last` seria trivialmente pequeno.
        assert!(!is_connected(0, 0, 0));
        assert!(!is_connected(0, 0, 10));

        // Datagrama recente: online.
        assert!(is_connected(1, 10_000, 10_000));
        assert!(is_connected(1, 10_000, 10_000 + limit - 1));

        // Silêncio maior que o limiar: offline.
        assert!(!is_connected(1, 10_000, 10_000 + limit));
        assert!(!is_connected(1, 10_000, 10_000 + limit * 10));

        // E o caminho real, ligado ao relógio, concorda.
        let shared = FeedShared::new();
        assert!(!shared.connected());
        shared.mark_packet();
        assert!(shared.connected());
    }

    /// SPEC-PROBE-013 — descartes locais são contabilizados, nunca silenciosos.
    #[test]
    fn spec_probe_013_local_drops_are_counted() {
        let shared = FeedShared::new();
        assert_eq!(shared.local_drops(), 0);
        shared.add_local_drops(3);
        shared.add_local_drops(1);
        assert_eq!(shared.local_drops(), 4);
    }
}
