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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use crossbeam_channel::bounded;
use net::{
    Datagram, NetEvent, PacketSource, ReceiverConfig, SocketSource, SocketSourceConfig,
    SourceBinding, StopHandle as NetStopHandle, StopToken as NetStopToken, StreamUrl, UdpReceiver,
};
use probe::{
    Encapsulation, FecMode, IpAnalyzer, IpAnalyzerConfig, IpTick, ProbeConfig, ServiceInfo,
    ServiceStream, ServiceVisual, StreamKind,
};
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

/// Piso da janela em que os joins de FEC ficam abertos sem tráfego.
///
/// SPEC-PROBE-IP-030 manda liberar o join extra "após `detect_secs`" quando não
/// há FEC.  Com o default de 3 s isso tornaria SPEC-PROBE-IP-037 inalcançável —
/// os eventos de FEC ausente/inesperada têm debounce de ≥ 10 s, e a probe teria
/// parado de escutar antes de qualquer um deles poder concluir.  O piso
/// reconcilia os dois: solta o grupo que não existe, mas só depois de o
/// diagnóstico sobre ele ter tido chance de fechar.
const FEC_PROBE_GRACE: Duration = Duration::from_secs(15);

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
    /// Inventário de serviços montado a partir de PAT/PMT/SDT.
    ///
    /// `Arc` porque o `probe-engine` lê isto a 1 Hz e a thread de snapshot a
    /// cada tick de round-robin: clonar o `Vec` inteiro em cada leitura seria
    /// desperdício, e segurar o `RwLock` durante o tick bloquearia a PSI.
    ///
    /// SPEC-PROBE-021
    services: RwLock<Arc<Vec<ServiceInfo>>>,
    /// Último resultado do thumbnail, por serviço (SPEC-PROBE-024).
    visuals: RwLock<BTreeMap<u16, ServiceVisual>>,
    /// Último resultado do tick de snapshot do feed, quando não há serviço
    /// algum para atribuir (PSI ainda não chegou) — SPEC-PROBE-003a.
    snapshot_state: AtomicU32,
    /// Thumbnail suspenso pelo 1º estágio de degradação (SPEC-PROBE-013a).
    snapshot_suspended: AtomicBool,
    /// Análise da camada 1 (spec-14).
    ///
    /// Compartilhado entre a thread de recepção — que o alimenta datagrama a
    /// datagrama — e a de engine, que fecha a janela uma vez por segundo.  O
    /// `Mutex` é tomado ~1 400 vezes por segundo sem contenção real, e a única
    /// alternativa (canal por datagrama) custaria uma alocação por pacote.
    ip: Mutex<IpAnalyzer>,
    /// Parâmetros efetivos do join principal (SPEC-PROBE-IP-013 · IP-051).
    binding: Mutex<Option<SourceBinding>>,
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
    fn new(ip: IpAnalyzerConfig) -> Self {
        Self {
            ip: Mutex::new(IpAnalyzer::new(ip)),
            binding: Mutex::new(None),
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
            services: RwLock::new(Arc::new(Vec::new())),
            visuals: RwLock::new(BTreeMap::new()),
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

    /// Inventário de serviços conhecido agora.
    ///
    /// SPEC-PROBE-021
    pub fn services(&self) -> Arc<Vec<ServiceInfo>> {
        self.services
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| Arc::new(Vec::new()))
    }

    /// Resultado do último thumbnail de cada serviço.
    ///
    /// SPEC-PROBE-024
    pub fn visuals(&self) -> BTreeMap<u16, ServiceVisual> {
        self.visuals.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// Publica o resultado do thumbnail de um serviço.
    ///
    /// A altura vem do frame decodificado porque o modo Probe não roda o
    /// `StreamProbe` de Media Info (SPEC-PROBE-002); é ela que decide o badge
    /// `HD`/`SD` do tile.
    ///
    /// SPEC-PROBE-024
    pub fn set_visual(&self, service_id: u16, visual: ServiceVisual) {
        if let Ok(mut guard) = self.visuals.write() {
            // A altura só é conhecida quando um frame decodifica; um tick
            // "sem keyframe" não pode apagar o badge que já estava certo.
            let entry = guard.entry(service_id).or_default();
            entry.state = visual.state;
            if visual.video_height.is_some() {
                entry.video_height = visual.video_height;
            }
        }
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

    /// Fecha a janela da camada IP e devolve a fotografia do segundo.
    ///
    /// `None` enquanto nenhum datagrama chegou: sem pacote não há o que
    /// afirmar, e publicar zeros faria a planilha dizer "medido e deu zero"
    /// sobre um feed que nunca ligou (§6).
    ///
    /// SPEC-PROBE-IP-011
    pub fn take_ip_tick(&self, bitrate_kbps: f64) -> Option<IpTick> {
        let mut guard = self.ip.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .has_data()
            .then(|| guard.take_tick(Instant::now(), bitrate_kbps))
    }

    /// Piso de ruído medido pela camada IP (SPEC-PROBE-IP-005).
    pub fn noise_floor_us(&self) -> Option<f64> {
        self.ip
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .noise_floor_us()
    }

    /// Parâmetros efetivos do join principal.
    ///
    /// SPEC-PROBE-IP-013 · SPEC-PROBE-IP-051
    pub fn binding(&self) -> Option<SourceBinding> {
        *self.binding.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_binding(&self, binding: SourceBinding) {
        *self.binding.lock().unwrap_or_else(|e| e.into_inner()) = Some(binding);
        self.so_rcvbuf
            .store(binding.so_rcvbuf_bytes, Ordering::Relaxed);
    }

    /// Alimenta a camada IP com um datagrama do fluxo principal e devolve o
    /// payload TS a repassar ao demux.
    fn on_datagram(&self, datagram: &Datagram) -> Option<Bytes> {
        let (payload, observed) = {
            let mut ip = self.ip.lock().unwrap_or_else(|e| e.into_inner());
            (ip.on_datagram(datagram), ip.encapsulation())
        };
        // O encapsulamento publicado acompanha o detectado pela camada IP, que
        // é o único que olha para o tráfego em vez do esquema da URL.
        if observed != Encapsulation::Unknown {
            self.encapsulation.set(observed);
        }
        payload
    }

    /// Alimenta a camada IP com um datagrama de um dos grupos de FEC.
    ///
    /// SPEC-PROBE-IP-031 … SPEC-PROBE-IP-036
    fn on_fec_datagram(&self, datagram: &Datagram) {
        self.ip
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_fec_datagram(datagram);
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
    net_raw_tx: BoundedSender<Datagram>,
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
        // O encapsulamento **declarado** vem da URL; o observado vem do
        // tráfego, e é ele que vale (SPEC-PROBE-IP-045).
        let declared = match spec.url {
            StreamUrl::UdpMulticast { .. } => Encapsulation::Udp,
            StreamUrl::RtpMulticast { .. } => Encapsulation::Rtp,
        };
        let shared = Arc::new(FeedShared::new(IpAnalyzerConfig::from_config(
            cfg, spec.fec, declared,
        )));
        shared
            .so_rcvbuf
            .store(receiver_cfg.buf_size, Ordering::Relaxed);
        shared.encapsulation.set(declared);

        // Os sockets de FEC nascem com o mesmo `SO_RCVBUF` do principal e um
        // timeout curto, para responderem ao stop sem segurar o shutdown.
        let fec_socket_cfg = SocketSourceConfig {
            buf_size: receiver_cfg.buf_size,
            timeout: Duration::from_millis(receiver_cfg.timeout_ms.min(500)),
        };

        let stop = Arc::new(AtomicBool::new(false));
        let net_stop: Arc<Mutex<Option<NetStopHandle>>> = Arc::new(Mutex::new(None));
        let mut handles = Vec::with_capacity(5);

        // Capacidades iguais às do pipeline global (SPEC-CHAN-001); os nomes
        // ganham sufixo de slot para que o log de backpressure identifique o
        // feed (§5.2).
        // SPEC-PROBE-IP-004 · SPEC-PROBE-IP-009 — o canal carrega o datagrama
        // inteiro, com o instante de chegada e o endereço de origem: sem eles
        // não há inter-arrival nem detecção de múltiplas fontes.
        let (net_raw_tx, net_raw_rx) = bounded::<Datagram>(crate::channels::CAP_NET_RAW);
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

                    let receiver = UdpReceiver::with_datagrams(
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
            handles.push(thread(format!("net-events-{slot}"), move || {
                for evt in net_events_rx.iter() {
                    match evt {
                        NetEvent::Timeout => {
                            tracing::debug!(slot, "net-recv: timeout de recepção");
                        }
                        // SPEC-PROBE-IP-013 · SPEC-PROBE-IP-051 — a interface e
                        // o `SO_RCVBUF` **efetivos** do join são o que vai para
                        // o `session.toml`.  Foi a falta deles que custou uma
                        // sessão inteira procurando regressão no código quando
                        // o multicast estava preso num adaptador virtual.
                        NetEvent::Joined(binding) => {
                            tracing::info!(
                                slot,
                                group = %binding.group,
                                port = binding.port,
                                iface = %binding.iface_label(),
                                so_rcvbuf = binding.so_rcvbuf_bytes,
                                "net-recv: join efetivo"
                            );
                            shared_t.set_binding(binding);
                        }
                        NetEvent::JoinFailed { reason } => {
                            tracing::warn!(slot, %reason, "net-recv: falha de join");
                        }
                        NetEvent::SourceSeen(addr) => {
                            tracing::info!(slot, source = %addr, "net-recv: nova fonte no grupo");
                        }
                        NetEvent::Left | NetEvent::Started | NetEvent::Stopped => {}
                    }
                }
            }));
        }

        // ── ip-analyze-{slot}: camada 1 completa (spec-14) ──────────────
        {
            let shared_t = shared.clone();
            let ts_raw = ts_raw_tx.clone();
            // O `RtpStripper` (SPEC-NET-003) não participa deste caminho: a
            // contagem de perda vem da máquina de sequência por SSRC, que é
            // exata, e manter os dois abriria dois alarmes para o mesmo pacote.
            handles.push(thread(format!("ip-analyze-{slot}"), move || {
                while let Ok(datagram) = net_raw_rx.recv() {
                    shared_t.mark_packet();
                    // Duplicata RTP e payload malformado param aqui: repassá-los
                    // ao demux produziria erro de continuidade que não existe no
                    // stream (SPEC-PROBE-IP-017 · SPEC-PROBE-IP-022).
                    let Some(payload) = shared_t.on_datagram(&datagram) else {
                        continue;
                    };
                    if !payload.is_empty() && !ts_raw.try_send(payload) {
                        shared_t.add_local_drops(1);
                    }
                }
                tracing::info!(slot, "ip-analyze: encerrado");
            }));
        }

        // ── fec-recv-{slot}-{eixo}: joins de FEC em base+2/+4 ───────────
        //
        // SPEC-PROBE-IP-030a — independentes do join principal: um grupo de FEC
        // inexistente não pode derrubar a recepção do feed.
        // SPEC-PROBE-IP-030b · SPEC-PROBE-IP-051 — a interface é a **mesma** do
        // join principal; caindo na interface default enquanto o principal está
        // fixado, a FEC apareceria como ausente por motivo de rota, e não de
        // stream — um falso negativo caro.
        if let Some((column, row)) = spec.fec.ports(spec.group_port().1, cfg.fec.port_offsets) {
            let (group, iface, source) = match spec.url {
                StreamUrl::UdpMulticast {
                    group,
                    iface,
                    source,
                    ..
                }
                | StreamUrl::RtpMulticast {
                    group,
                    iface,
                    source,
                    ..
                } => (group, iface, source),
            };
            let grace = cfg.detect_window().max(FEC_PROBE_GRACE);
            let socket_cfg = fec_socket_cfg;
            for (axis, port) in [("col", column), ("row", row)] {
                let shared_t = shared.clone();
                let stop_flag = stop.clone();
                handles.push(thread(format!("fec-recv-{slot}-{axis}"), move || {
                    fec_receive_loop(
                        slot, axis, group, port, iface, source, socket_cfg, grace, &shared_t,
                        &stop_flag,
                    );
                }));
            }
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

/// Loop de recepção de um dos grupos de FEC.
///
/// Três regras que este loop materializa:
///
/// - Falha de join **não** derruba o feed: loga, deixa `fec_present = false` e
///   encerra a thread (SPEC-PROBE-IP-030a).
/// - A interface é a do join principal (SPEC-PROBE-IP-030b · IP-051).
/// - Sem tráfego em `grace`, o join extra é liberado (SPEC-PROBE-IP-030) — não
///   faz sentido segurar uma associação IGMP num grupo que não existe.
#[allow(clippy::too_many_arguments)]
fn fec_receive_loop(
    slot: usize,
    axis: &str,
    group: std::net::Ipv4Addr,
    port: u16,
    iface: Option<std::net::Ipv4Addr>,
    source: Option<std::net::Ipv4Addr>,
    cfg: SocketSourceConfig,
    grace: Duration,
    shared: &FeedShared,
    stop: &AtomicBool,
) {
    let mut socket = match SocketSource::join(group, port, iface, source, cfg) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                slot, axis, %group, port, error = %e,
                "fec-recv: join falhou — a FEC fica ausente, a recepção do feed segue intacta"
            );
            return;
        }
    };
    tracing::info!(
        slot, axis, %group, port,
        iface = %socket.binding().iface_label(),
        "fec-recv: escutando"
    );

    let mut buf = vec![0u8; 65_536];
    let deadline = Instant::now() + grace;
    let mut seen = false;

    while !stop.load(Ordering::Relaxed) {
        match socket.recv(&mut buf) {
            Ok(Some(datagram)) => {
                seen = true;
                shared.on_fec_datagram(&datagram);
            }
            Ok(None) => {
                if !seen && Instant::now() >= deadline {
                    tracing::info!(
                        slot, axis, %group, port,
                        "fec-recv: sem tráfego na janela de detecção — liberando o join"
                    );
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(slot, axis, error = %e, "fec-recv: erro de recepção — encerrando");
                break;
            }
        }
    }
    socket.leave();
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

/// Descrição de um serviço vinda da SDT, antes de casar com a PMT.
///
/// A SDT costuma chegar antes das PMTs: guardar o nome à parte evita descartá-lo
/// e ficar com o mosaico cheio de "Serviço 1097" até a próxima repetição.
#[derive(Debug, Clone, Default)]
struct SdtEntry {
    name: Option<String>,
    provider: Option<String>,
    scrambled: bool,
}

/// Consumidor de PSI reduzido ao que as telas de Probe precisam.
///
/// O `TableDispatcher` completo (auto-play, roteamento de decode, menu de
/// contexto) não faz sentido num feed sem player.  O que interessa aqui é o
/// **inventário**: quais serviços o multiplex carrega, quais PIDs são de cada
/// um e o que cada PID é — é isso que separa "o transporte está ruim" de
/// "**este** serviço está ruim" (SPEC-PROBE-021).
#[derive(Default)]
struct FeedTables {
    pmt_pids: HashSet<Pid>,
    /// `service_id` → PID da PMT, na ordem da PAT.
    programs: Vec<(u16, Pid)>,
    /// Streams por serviço, vindos da PMT.
    streams: HashMap<u16, Vec<ServiceStream>>,
    /// PCR PID por serviço.
    pcr_pid: HashMap<u16, Pid>,
    /// Descrições da SDT, guardadas mesmo antes da PMT correspondente.
    sdt: HashMap<u16, SdtEntry>,
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
                    self.programs = pat
                        .programs
                        .iter()
                        .filter(|p| p.program_number != 0)
                        .map(|p| (p.program_number, p.pid))
                        .collect();
                    // Serviço que saiu da PAT sai do inventário: a grade de
                    // saúde derruba o escopo dele no tick seguinte.
                    let live: HashSet<u16> = self.programs.iter().map(|(id, _)| *id).collect();
                    self.streams.retain(|id, _| live.contains(id));
                    self.pcr_pid.retain(|id, _| live.contains(id));

                    for program in &pat.programs {
                        let route = if program.program_number == 0 {
                            DemuxRoute::Nit(program.pid)
                        } else {
                            DemuxRoute::Pmt(program.pid)
                        };
                        self.route(route_tx, route);
                    }
                    self.publish(shared);
                }
            }
            // PMT
            0x02 if self.pmt_pids.contains(&section.pid) => {
                if let Ok(pmt) = ts::tables::Pmt::from_section_body(body) {
                    let mut streams = Vec::with_capacity(pmt.streams.len());
                    for stream in &pmt.streams {
                        // Todo PID listado numa PMT é elementar, seja ele
                        // vídeo, áudio, legenda ou dado: o que importa aqui é
                        // tirá-lo do caminho de seções.
                        self.route(route_tx, DemuxRoute::Elementary(stream.elementary_pid));
                        streams.push(ServiceStream {
                            pid: stream.elementary_pid,
                            stream_type: stream.stream_type,
                            kind: classify_stream(stream),
                            codec: stream.label().to_string(),
                            language: language_of(stream),
                        });
                    }
                    self.pcr_pid.insert(pmt.program_number, pmt.pcr_pid);
                    self.streams.insert(pmt.program_number, streams);
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
                    if !sdt.actual {
                        return;
                    }
                    for svc in &sdt.services {
                        self.sdt.insert(
                            svc.service_id,
                            SdtEntry {
                                name: svc.service_name.clone(),
                                provider: svc.provider_name.clone(),
                                scrambled: svc.free_ca_mode,
                            },
                        );
                    }
                    self.publish(shared);
                }
            }
            _ => {}
        }
    }

    /// Recompõe o inventário e os agregados derivados dele.
    fn publish(&self, shared: &FeedShared) {
        let services: Vec<ServiceInfo> = self
            .programs
            .iter()
            .map(|(service_id, pmt_pid)| {
                let sdt = self.sdt.get(service_id).cloned().unwrap_or_default();
                ServiceInfo {
                    service_id: *service_id,
                    name: sdt.name,
                    provider: sdt.provider,
                    pmt_pid: *pmt_pid,
                    pcr_pid: self.pcr_pid.get(service_id).copied().unwrap_or(0),
                    scrambled: sdt.scrambled,
                    streams: self.streams.get(service_id).cloned().unwrap_or_default(),
                }
            })
            .collect();

        // Os PIDs achatados continuam alimentando os indicadores `V`/`A` e a
        // presença do feed inteiro (SPEC-PROBE-018) — a visão por serviço é
        // adicional, não substituta.
        let collect_kind = |kind: StreamKind| {
            let mut v: Vec<Pid> = services
                .iter()
                .flat_map(|s| s.pids_of(kind))
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        if let Ok(mut guard) = shared.video_pids.write() {
            *guard = collect_kind(StreamKind::Video);
        }
        if let Ok(mut guard) = shared.audio_pids.write() {
            *guard = collect_kind(StreamKind::Audio);
        }
        if let Ok(mut guard) = shared.service_name.write() {
            *guard = services
                .iter()
                .find(|s| s.primary_video_pid().is_some())
                .or_else(|| services.first())
                .and_then(|s| s.name.clone());
        }
        if let Ok(mut guard) = shared.services.write() {
            *guard = Arc::new(services);
        }
    }
}

/// Classifica um stream da PMT no papel que a grade de saúde usa.
///
/// A ordem importa: `is_audio()` do crate `ts` já resolve o 0x06 ambíguo
/// (AC-3/E-AC-3/AAC via descriptor), então legenda e teletexto só são testados
/// depois — senão um AC-3 com `subtitling_descriptor` viraria legenda.
fn classify_stream(stream: &ts::tables::PmtStream) -> StreamKind {
    if is_video_stream_type(stream.stream_type) {
        StreamKind::Video
    } else if stream.is_audio() {
        StreamKind::Audio
    } else if stream
        .descriptors
        .iter()
        .any(|d| matches!(d.tag, 0x56 | 0x59))
    {
        // 0x56 teletext_descriptor · 0x59 subtitling_descriptor
        StreamKind::Subtitle
    } else {
        StreamKind::Data
    }
}

/// Idioma ISO-639 de um stream, quando sinalizado.
///
/// Aceita `iso_639_language_descriptor` (0x0A), `subtitling` (0x59) e
/// `teletext` (0x56) — os três começam o payload com os 3 bytes do código.
fn language_of(stream: &ts::tables::PmtStream) -> Option<String> {
    stream
        .descriptors
        .iter()
        .filter(|d| matches!(d.tag, 0x0A | 0x56 | 0x59))
        .find_map(|d| {
            let code = d.data.get(..3)?;
            // Dado externo: só aceita ASCII imprimível, nunca `from_utf8`
            // otimista sobre bytes de rede (RNF-PRB-003).
            code.iter()
                .all(|b| b.is_ascii_alphabetic())
                .then(|| String::from_utf8_lossy(code).to_lowercase())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use probe::FeedConfig;

    /// Camada IP de bancada: os defaults do perfil, com feed declarado RTP.
    fn test_ip_config() -> IpAnalyzerConfig {
        IpAnalyzerConfig::from_config(&ProbeConfig::default(), FecMode::Auto, Encapsulation::Rtp)
    }

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

    /// SPEC-PROBE-IP-030 · SPEC-PROBE-IP-051 — os grupos de FEC saem da porta
    /// do próprio feed (`base+2` coluna, `base+4` linha) e herdam a interface
    /// do join principal.
    ///
    /// A regressão que este teste evita é cara: com os joins de FEC caindo na
    /// interface default enquanto o principal está fixado em `?iface=`, a FEC
    /// aparece como ausente por motivo de **rota**, não de stream — e o
    /// diagnóstico aponta para o lugar errado.
    #[test]
    fn spec_probe_ip_051_fec_joins_inherit_port_and_interface_from_the_feed() {
        let cfg = ProbeConfig::default();
        let feeds = resolve_feeds(&cfg_with(&["rtp://@239.15.0.183:50000?iface=10.0.0.7"]))
            .expect("feed válido");
        let spec = &feeds[0];

        let (group, port) = spec.group_port();
        assert_eq!(port, 50_000);
        assert_eq!(
            spec.fec.ports(port, cfg.fec.port_offsets),
            Some((50_002, 50_004)),
            "convenção do ST 2022-1 confirmada com a operação"
        );

        let iface = match spec.url {
            StreamUrl::UdpMulticast { iface, .. } | StreamUrl::RtpMulticast { iface, .. } => iface,
        };
        assert_eq!(iface, Some("10.0.0.7".parse().expect("iface")));
        assert_eq!(group, "239.15.0.183".parse::<std::net::Ipv4Addr>().expect("grupo"));

        // `fec = off` não abre join nenhum.
        let mut off = spec.clone();
        off.fec = FecMode::Off;
        assert_eq!(off.fec.ports(port, cfg.fec.port_offsets), None);
    }

    /// SPEC-PROBE-IP-030a — falha no join de FEC não derruba a recepção do
    /// feed: o loop encerra sozinho, sem panic, e a camada IP segue sem FEC.
    #[test]
    fn spec_probe_ip_030a_fec_join_failure_does_not_stop_the_feed() {
        let shared = FeedShared::new(test_ip_config());
        let stop = AtomicBool::new(false);
        // Interface que não existe na máquina: o join falha na hora.
        let bogus: std::net::Ipv4Addr = "203.0.113.9".parse().expect("iface");

        let started = Instant::now();
        fec_receive_loop(
            0,
            "col",
            "239.255.31.1".parse().expect("grupo"),
            56_502,
            Some(bogus),
            None,
            SocketSourceConfig {
                buf_size: 65_536,
                timeout: Duration::from_millis(50),
            },
            Duration::from_millis(100),
            &shared,
            &stop,
        );

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a falha de join precisa ser imediata, não travar o slot"
        );
        let tick = shared.ip.lock().expect("mutex").take_tick(Instant::now(), 15_000.0);
        assert!(!tick.fec.present, "sem join não há FEC a reportar");
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
        let shared = FeedShared::new(test_ip_config());
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

    /// SPEC-PROBE-021 · SPEC-PROBE-022 — a PSI monta o **inventário** de
    /// serviços, não só uma lista achatada de PIDs: é o que permite o mosaico
    /// de serviços e a atribuição de erro por canal num MPTS.
    #[test]
    fn spec_probe_021_psi_builds_the_service_inventory() {
        let (tx, rx) = bounded::<DemuxRoute>(64);
        let shared = FeedShared::new(test_ip_config());
        let mut tables = FeedTables::default();

        // PAT: NIT no 0x0010, programa 1 → PMT 0x0100, programa 2 → PMT 0x0101.
        let pat_body = [
            0x00, 0x01, //
            0x01, 0x00, 0x00, //
            0x00, 0x00, 0xE0, 0x10, // NIT
            0x00, 0x01, 0xE1, 0x00, // programa 1
            0x00, 0x02, 0xE1, 0x01, // programa 2
        ];
        tables.apply(&section(0x0000, 0x00, &pat_body), &shared, &tx);
        assert!(
            shared.services().len() == 2,
            "os dois programas da PAT já entram no inventário, mesmo sem PMT"
        );

        // PMT do programa 1: vídeo H.264, áudio AC-3 em português e legenda.
        let pmt1 = [
            0x00, 0x01, //
            0x01, 0x00, 0x00, //
            0xE2, 0x00, // PCR_PID = 0x0200
            0xF0, 0x00, // program_info_length = 0
            0x1B, 0xE2, 0x00, 0xF0, 0x00, // vídeo 0x0200
            // áudio 0x0201 com iso_639_language_descriptor "por"
            0x81, 0xE2, 0x01, 0xF0, 0x06, 0x0A, 0x04, b'p', b'o', b'r', 0x00,
            // legenda 0x0202 com subtitling_descriptor "eng"
            0x06, 0xE2, 0x02, 0xF0, 0x0A, 0x59, 0x08, b'e', b'n', b'g', 0x10, 0x00, 0x01, 0x00,
            0x01,
        ];
        tables.apply(&section(0x0100, 0x02, &pmt1), &shared, &tx);

        // PMT do programa 2: só áudio (rádio).
        let pmt2 = [
            0x00, 0x02, //
            0x01, 0x00, 0x00, //
            0xE3, 0x00, // PCR_PID = 0x0300
            0xF0, 0x00, //
            0x03, 0xE3, 0x00, 0xF0, 0x00, // MPEG-1 áudio 0x0300
        ];
        tables.apply(&section(0x0101, 0x02, &pmt2), &shared, &tx);

        // SDT actual da fixture do crate `ts`: nomeia o serviço 1. O
        // `SectionAssembler` entrega a seção **sem** os 4 bytes de CRC, que é
        // como `FeedTables` a recebe no ar.
        let sdt = include_bytes!("../crates/ts/tests/fixtures/sdt_actual.bin");
        tables.apply(
            &CompleteSection {
                pid: 0x0011,
                table_id: 0x42,
                data: bytes::Bytes::copy_from_slice(&sdt[..sdt.len() - 4]),
            },
            &shared,
            &tx,
        );

        let services = shared.services();
        assert_eq!(services.len(), 2, "ordem e cardinalidade vêm da PAT");

        let a = &services[0];
        assert_eq!(a.service_id, 1);
        assert_eq!(a.pmt_pid, 0x0100);
        assert_eq!(a.pcr_pid, 0x0200);
        assert_eq!(
            a.name.as_deref(),
            Some("Channel 1"),
            "o nome vem da SDT, não da PMT"
        );
        assert_eq!(a.provider.as_deref(), Some("IronTV"));
        assert_eq!(a.streams.len(), 3);
        assert_eq!(a.streams[0].kind, StreamKind::Video);
        assert_eq!(a.streams[1].kind, StreamKind::Audio);
        assert_eq!(
            a.streams[1].language.as_deref(),
            Some("por"),
            "idioma sai do iso_639_language_descriptor"
        );
        // 0x06 com subtitling_descriptor é legenda, não áudio nem dado solto.
        assert_eq!(a.streams[2].kind, StreamKind::Subtitle);
        assert_eq!(a.streams[2].language.as_deref(), Some("eng"));
        assert_eq!(a.primary_video_pid(), Some(0x0200));
        assert_eq!(a.primary_video_stream_type(), Some(0x1B));

        let b = &services[1];
        assert_eq!(b.service_id, 2);
        assert_eq!(
            b.name, None,
            "serviço fora da SDT fica sem nome e cai no rótulo por id"
        );
        assert_eq!(b.display_name(), "Serviço 2");
        assert_eq!(b.primary_video_pid(), None, "rádio não tem vídeo");

        // A visão achatada continua alimentando os indicadores do feed (§8.1).
        assert_eq!(shared.video_pids(), vec![0x0200]);
        assert_eq!(shared.audio_pids(), vec![0x0201, 0x0300]);
        assert_eq!(shared.service_name().as_deref(), Some("Channel 1"));

        // Todo PID elementar saiu do caminho de seções (regressão do CRC
        // fantasma), inclusive os do segundo programa.
        let routed: Vec<Pid> = rx
            .try_iter()
            .filter_map(|r| match r {
                DemuxRoute::Elementary(pid) => Some(pid),
                _ => None,
            })
            .collect();
        assert_eq!(routed, vec![0x0200, 0x0201, 0x0202, 0x0300]);
    }

    /// SPEC-PROBE-021 — programa que sai da PAT sai do inventário: o mosaico
    /// de serviços não pode mostrar canal que o multiplex não carrega mais.
    #[test]
    fn spec_probe_021_service_removed_from_pat_leaves_the_inventory() {
        let (tx, _rx) = bounded::<DemuxRoute>(64);
        let shared = FeedShared::new(test_ip_config());
        let mut tables = FeedTables::default();

        let pat_two = [
            0x00, 0x01, 0x01, 0x00, 0x00, //
            0x00, 0x01, 0xE1, 0x00, //
            0x00, 0x02, 0xE1, 0x01,
        ];
        tables.apply(&section(0x0000, 0x00, &pat_two), &shared, &tx);
        let pmt2 = [
            0x00, 0x02, 0x01, 0x00, 0x00, 0xE3, 0x00, 0xF0, 0x00, //
            0x03, 0xE3, 0x00, 0xF0, 0x00,
        ];
        tables.apply(&section(0x0101, 0x02, &pmt2), &shared, &tx);
        assert_eq!(shared.services().len(), 2);
        assert_eq!(shared.audio_pids(), vec![0x0300]);

        // Nova PAT sem o programa 2.
        let pat_one = [
            0x00, 0x01, 0x02, 0x00, 0x00, //
            0x00, 0x01, 0xE1, 0x00,
        ];
        tables.apply(&section(0x0000, 0x00, &pat_one), &shared, &tx);

        let services = shared.services();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].service_id, 1);
        assert!(
            shared.audio_pids().is_empty(),
            "os PIDs do programa removido também saem da visão achatada"
        );
    }

    /// SPEC-PROBE-024 — o resultado do thumbnail é por serviço, e um tick sem
    /// keyframe não apaga o badge `HD` que já estava correto.
    #[test]
    fn spec_probe_024_visual_keeps_known_height_across_a_missed_tick() {
        let shared = FeedShared::new(test_ip_config());
        assert!(shared.visuals().is_empty());

        shared.set_visual(
            55,
            ServiceVisual {
                video_height: Some(1080),
                state: probe::SnapshotState::Ok,
            },
        );
        shared.set_visual(
            55,
            ServiceVisual {
                video_height: None,
                state: probe::SnapshotState::NoKeyframe,
            },
        );

        let v = shared.visuals();
        let entry = v.get(&55).expect("serviço 55");
        assert_eq!(entry.video_height, Some(1080));
        assert_eq!(entry.state, probe::SnapshotState::NoKeyframe);
        assert!(
            !v.contains_key(&56),
            "serviço sem captura não inventa dado"
        );
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
        let shared = FeedShared::new(test_ip_config());
        assert!(!shared.connected());
        shared.mark_packet();
        assert!(shared.connected());
    }

    /// SPEC-PROBE-013 — descartes locais são contabilizados, nunca silenciosos.
    #[test]
    fn spec_probe_013_local_drops_are_counted() {
        let shared = FeedShared::new(test_ip_config());
        assert_eq!(shared.local_drops(), 0);
        shared.add_local_drops(3);
        shared.add_local_drops(1);
        assert_eq!(shared.local_drops(), 4);
    }
}
