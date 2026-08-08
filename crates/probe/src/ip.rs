//! Camada 1 — análise de IP / UDP / RTP / FEC de **um** feed.
//!
//! Responde, antes de olhar para o TS: os pacotes estão chegando todos, na
//! ordem, no ritmo certo, e a FEC está coerente?  Sem isso, todo CC error do TS
//! fica sem causa atribuída — que é exatamente o problema operacional que a
//! spec-14 existe para resolver.
//!
//! Todo o estado aqui é **por feed**, nunca global: os três encapsulamentos
//! convivem no mesmo mosaico e um feed UDP puro não pode herdar contador de um
//! vizinho com RTP (SPEC-PROBE-IP-042 … 046).
//!
//! O que **não** está aqui, deliberadamente:
//!
//! - Recuperação de pacotes por FEC — fase 3 (§9); a v1 valida e mede.
//! - Buffer de reordenação — a v1 conta e deixa o pacote seguir na ordem de
//!   chegada; reordenar mascararia o defeito sem mudar o diagnóstico (§5.3).
//!
//! SPEC-PROBE-IP-005 … SPEC-PROBE-IP-046

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::SocketAddrV4;
use std::time::{Duration, Instant};

use bytes::Bytes;
use net::{
    ts_payload_shape, Datagram, FecAxis, FecHeader, FecMatrix, IatSummary, IatWindow,
    NoiseCalibration, Rfc3550Jitter, RtpHeader, RtpSeqState, SeqOutcome, FEC_HEADER_LEN,
    TS_SYNC_BYTE,
};

use crate::check::{CheckProfile, CHECK_RTP_LOSS_RATIO};
use crate::config::{FecMode, FecProfile, ProbeConfig};
use crate::session::Encapsulation;

/// Fração de datagramas começando em `0x47` que classifica o feed como UDP puro.
///
/// SPEC-PROBE-IP-042
const UDP_CLASSIFY_RATIO: f64 = 0.95;

/// Parâmetros da camada IP, resolvidos do perfil.
#[derive(Debug, Clone)]
pub struct IpAnalyzerConfig {
    /// Janela de reconciliação de lacunas (SPEC-PROBE-IP-020).
    pub reorder_window: Duration,
    /// Janela de detecção/reavaliação de encapsulamento (SPEC-PROBE-IP-042).
    pub detect_window: Duration,
    /// Calibração do piso de ruído (SPEC-PROBE-IP-005).
    pub calibration_window: Duration,
    /// Janela da razão de perda (SPEC-PROBE-IP-021).
    pub loss_ratio_window: Duration,
    /// Payload types aceitos (SPEC-PROBE-IP-015).
    pub payload_types: Vec<u8>,
    /// Pacotes TS por datagrama esperados (SPEC-PROBE-IP-018).
    pub ts_per_datagram: u32,
    /// Tolerância de variação de bitrate antes de a burstiness virar `n/a`.
    pub vbr_tolerance_pct: f64,
    /// Parâmetros de FEC.
    pub fec: FecProfile,
    /// Política de FEC deste feed.
    pub fec_mode: FecMode,
    /// Encapsulamento declarado na URL do feed (SPEC-PROBE-IP-045).
    pub declared: Encapsulation,
}

impl IpAnalyzerConfig {
    /// Resolve os parâmetros a partir da configuração do modo Probe.
    ///
    /// A janela da razão de perda sai do próprio perfil de checks, para que
    /// mudar `[probe.checks.rtp_loss_ratio] window_secs` no TOML mude a conta e
    /// não só o limiar.
    pub fn from_config(cfg: &ProbeConfig, fec_mode: FecMode, declared: Encapsulation) -> Self {
        let profile = CheckProfile::from_config(cfg);
        let loss_ratio_window = profile
            .get(CHECK_RTP_LOSS_RATIO)
            .map_or(Duration::from_secs(300), |d| d.window);
        Self {
            reorder_window: cfg.reorder_window(),
            detect_window: cfg.detect_window(),
            calibration_window: cfg.calibration_window(),
            loss_ratio_window,
            payload_types: cfg.rtp_payload_types.clone(),
            ts_per_datagram: cfg.ts_per_datagram,
            vbr_tolerance_pct: cfg.vbr_tolerance_pct,
            fec: cfg.fec.clone(),
            fec_mode,
            declared,
        }
    }

    fn accepts_payload_type(&self, pt: u8) -> bool {
        self.payload_types.is_empty() || self.payload_types.contains(&pt)
    }
}

/// Contadores RTP de uma janela de amostragem.
///
/// Deltas, não cumulativos: quem transforma cumulativo em delta no resto do
/// motor é a [`crate::sample::CounterBaseline`], mas aqui a janela é rolada
/// pelo próprio analisador, que é quem sabe o instante de cada pacote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RtpDelta {
    pub received: u64,
    pub missing: u64,
    pub dup: u64,
    pub reorder: u64,
    pub too_old: u64,
    pub source_restarts: u64,
    pub ssrc_changes: u64,
}

/// Violações de conformidade observadas na janela.
///
/// SPEC-PROBE-IP-015 … SPEC-PROBE-IP-018
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IpViolations {
    pub invalid_pt: u64,
    pub padding: u64,
    pub extension: u64,
    pub marker: u64,
    pub bad_payload_size: u64,
    pub ts_per_datagram: u64,
    /// Último PT fora do perfil, para o texto do evento.
    pub last_bad_pt: Option<u8>,
    /// Última contagem de TS por datagrama fora do perfil.
    pub last_bad_ts_count: Option<u32>,
}

impl IpViolations {
    /// `true` quando nada foi violado na janela.
    ///
    /// A aba `Rede` usa isto para mostrar "conformidade: ok" em vez de uma
    /// lista de doze zeros.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Estado da FEC no instante do tick.
///
/// SPEC-PROBE-IP-030 … SPEC-PROBE-IP-037
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FecStatus {
    /// `true` quando há tráfego em alguma das portas de FEC.
    pub present: bool,
    /// A probe está escutando as portas de FEC.
    pub listening: bool,
    pub l: Option<u32>,
    pub d: Option<u32>,
    /// Fluxos distintos observados (coluna e/ou linha).
    pub streams: u8,
    /// Overhead da FEC sobre o fluxo principal, em %.
    pub overhead_pct: Option<f64>,
    /// SSRC da FEC diverge do SSRC do fluxo principal.
    pub ssrc_mismatch: bool,
    /// Datagramas de FEC recebidos na janela.
    pub datagrams: u64,
}

impl FecStatus {
    /// Produto L×D, quando as duas dimensões são conhecidas.
    pub fn lxd(&self) -> Option<u32> {
        Some(self.l? * self.d?)
    }

    /// Rótulo `L×D` do painel.
    pub fn matrix_label(&self) -> String {
        let fmt = |v: Option<u32>| v.map_or("—".to_string(), |n| n.to_string());
        format!("{}×{}", fmt(self.l), fmt(self.d))
    }
}

/// Fotografia da camada IP de um segundo.
///
/// SPEC-PROBE-IP-011 · §6
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IpTick {
    pub encapsulation: Encapsulation,
    /// O encapsulamento declarado difere do observado (SPEC-PROBE-IP-045).
    pub encapsulation_mismatch: bool,
    /// Houve troca de encapsulamento em runtime nesta janela
    /// (SPEC-PROBE-IP-046).
    pub encapsulation_changed: bool,
    /// Datagramas na janela.
    pub datagrams: u64,
    /// Bytes de payload UDP na janela.
    pub bytes: u64,
    /// Taxa IP da janela, em Mbps.
    pub mbps: f64,
    /// Média de pacotes TS por datagrama na janela.
    pub ts_per_datagram: Option<f64>,
    /// Fontes distintas vistas na sessão (SPEC-PROBE-IP-010).
    pub sources: Vec<SocketAddrV4>,
    /// SSRC corrente, quando há RTP.
    pub ssrc: Option<u32>,
    /// Contadores RTP da janela; `None` num feed sem RTP (SPEC-PROBE-IP-043).
    pub rtp: Option<RtpDelta>,
    /// Razão de perda na janela longa (SPEC-PROBE-IP-021).
    pub loss_ratio: Option<f64>,
    pub violations: IpViolations,
    pub iat: IatSummary,
    /// Inter-arrival esperado pelo bitrate observado (SPEC-PROBE-IP-027).
    pub iat_expected_us: Option<f64>,
    /// Desvio relativo do inter-arrival; `n/a` em VBR (SPEC-PROBE-IP-027).
    pub burstiness: Option<f64>,
    /// Jitter RFC 3550; `n/a` sem timestamp utilizável (SPEC-PROBE-IP-028).
    pub jitter_us: Option<f64>,
    /// Piso de ruído medido pela própria probe (SPEC-PROBE-IP-005).
    pub noise_floor_us: Option<f64>,
    pub fec: FecStatus,
    /// Totais acumulados na sessão.
    pub total_datagrams: u64,
    pub total_bytes: u64,
}

impl IpTick {
    /// `true` quando os checks das camadas RTP e FEC se aplicam.
    ///
    /// SPEC-PROBE-IP-043 — num feed UDP puro eles ficam `n/a`, nunca verdes e
    /// nunca em alarme.
    pub fn rtp_applicable(&self) -> bool {
        self.encapsulation.has_rtp()
    }
}

/// Um fluxo de FEC (coluna ou linha).
#[derive(Debug, Default)]
struct FecStream {
    datagrams: u64,
    bytes: u64,
    ssrc: Option<u32>,
    last_seen: Option<Instant>,
}

/// Estado da FEC do feed.
#[derive(Debug, Default)]
struct FecState {
    column: FecStream,
    row: FecStream,
    matrix: FecMatrix,
    window_datagrams: u64,
    window_bytes: u64,
}

impl FecState {
    fn stream_mut(&mut self, axis: FecAxis) -> &mut FecStream {
        match axis {
            FecAxis::Column => &mut self.column,
            FecAxis::Row => &mut self.row,
        }
    }

    fn streams_seen(&self) -> u8 {
        u8::from(self.column.datagrams > 0) + u8::from(self.row.datagrams > 0)
    }

    fn ssrc(&self) -> Option<u32> {
        self.column.ssrc.or(self.row.ssrc)
    }
}

/// Detecção de encapsulamento por janela.
///
/// SPEC-PROBE-IP-042 · SPEC-PROBE-IP-046 — a janela se repete durante toda a
/// sessão, e não só nos primeiros segundos: uma troca em runtime precisa ser
/// notada, e reavaliar custa dois contadores.
#[derive(Debug, Default)]
struct EncapDetector {
    started: Option<Instant>,
    total: u64,
    ts_first: u64,
    rtp_like: u64,
}

impl EncapDetector {
    fn observe(&mut self, first_byte: Option<u8>, rtp_like: bool, at: Instant) {
        self.started.get_or_insert(at);
        self.total += 1;
        if first_byte == Some(TS_SYNC_BYTE) {
            self.ts_first += 1;
        }
        if rtp_like {
            self.rtp_like += 1;
        }
    }

    /// Conclui a janela, se ela venceu e tem amostra.
    fn conclude(&mut self, at: Instant, window: Duration) -> Option<Encapsulation> {
        let started = self.started?;
        if at.saturating_duration_since(started) < window || self.total == 0 {
            return None;
        }
        let ratio = self.ts_first as f64 / self.total as f64;
        let verdict = if ratio >= UDP_CLASSIFY_RATIO {
            Encapsulation::Udp
        } else if self.rtp_like > 0 {
            Encapsulation::Rtp
        } else {
            // Nem TS puro nem RTP reconhecível: não há o que afirmar, e afirmar
            // errado aqui deixaria uma faixa inteira de checks no lugar errado.
            Encapsulation::Unknown
        };
        *self = Self::default();
        self.started = Some(at);
        (verdict != Encapsulation::Unknown).then_some(verdict)
    }
}

/// Analisador da camada IP de um feed.
///
/// SPEC-PROBE-IP-005 … SPEC-PROBE-IP-046
#[derive(Debug)]
pub struct IpAnalyzer {
    cfg: IpAnalyzerConfig,
    observed: Encapsulation,
    detector: EncapDetector,
    changed_this_window: bool,

    sources: BTreeSet<SocketAddrV4>,
    total_datagrams: u64,
    total_bytes: u64,
    window_datagrams: u64,
    window_bytes: u64,
    window_ts_packets: u64,
    window_started: Option<Instant>,

    streams: BTreeMap<u32, RtpSeqState>,
    current_ssrc: Option<u32>,
    rtp_window: RtpDelta,
    /// Histórico `(instante, recebidos, perdidos)` da janela longa de perda.
    loss_history: VecDeque<(Instant, u64, u64)>,

    violations: IpViolations,
    iat: IatWindow,
    jitter: Rfc3550Jitter,
    noise: NoiseCalibration,
    last_arrival: Option<Instant>,
    fec: FecState,
    /// Bitrate visto no tick anterior, para decidir CBR × VBR.
    prev_bitrate_kbps: Option<f64>,
}

impl IpAnalyzer {
    /// Cria o analisador de um feed.
    pub fn new(cfg: IpAnalyzerConfig) -> Self {
        let noise = NoiseCalibration::new(cfg.calibration_window);
        Self {
            observed: Encapsulation::Unknown,
            detector: EncapDetector::default(),
            changed_this_window: false,
            sources: BTreeSet::new(),
            total_datagrams: 0,
            total_bytes: 0,
            window_datagrams: 0,
            window_bytes: 0,
            window_ts_packets: 0,
            window_started: None,
            streams: BTreeMap::new(),
            current_ssrc: None,
            rtp_window: RtpDelta::default(),
            loss_history: VecDeque::new(),
            violations: IpViolations::default(),
            iat: IatWindow::new(),
            jitter: Rfc3550Jitter::new(),
            noise,
            last_arrival: None,
            fec: FecState::default(),
            prev_bitrate_kbps: None,
            cfg,
        }
    }

    /// Encapsulamento observado agora.
    pub fn encapsulation(&self) -> Encapsulation {
        self.observed
    }

    /// Piso de ruído medido, quando a calibração já fechou.
    ///
    /// SPEC-PROBE-IP-005
    pub fn noise_floor_us(&self) -> Option<f64> {
        self.noise.floor_us()
    }

    /// `true` depois do primeiro datagrama.
    ///
    /// Antes disso não há nada a publicar: um tick de zeros faria a planilha
    /// afirmar "medido e deu zero" sobre um feed que nunca ligou (§6).
    pub fn has_data(&self) -> bool {
        self.total_datagrams > 0
    }

    /// Processa um datagrama do fluxo principal e devolve o payload TS a
    /// entregar ao demux.
    ///
    /// Devolve `None` quando o datagrama **não** deve seguir:
    ///
    /// - duplicata RTP, que geraria um CC error espúrio (SPEC-PROBE-IP-022);
    /// - payload malformado, que dessincronizaria o demux
    ///   (SPEC-PROBE-IP-017).
    ///
    /// SPEC-PROBE-IP-014 … SPEC-PROBE-IP-028
    pub fn on_datagram(&mut self, datagram: &Datagram) -> Option<Bytes> {
        let at = datagram.at;
        self.window_started.get_or_insert(at);
        self.sources.insert(datagram.from);
        self.total_datagrams += 1;
        self.total_bytes += datagram.data.len() as u64;
        self.window_datagrams += 1;
        self.window_bytes += datagram.data.len() as u64;

        // SPEC-PROBE-IP-025 · IP-005 — temporização é do datagrama, qualquer
        // que seja o encapsulamento.
        if let Some(prev) = self.last_arrival {
            let iat_us = at.saturating_duration_since(prev).as_secs_f64() * 1e6;
            self.noise.observe(iat_us, at);
        }
        self.last_arrival = Some(at);
        self.iat.observe(at);

        // SPEC-PROBE-IP-021 — a reconciliação é fechada a cada datagrama, não
        // só no tick: com janela de 200 ms e tick de 1 s, um retardatário de
        // 500 ms dentro do mesmo segundo seria contado como reordenação, e a
        // perda que já estava confirmada sumiria.  O custo é um `retain` sobre
        // um mapa quase sempre vazio.
        self.expire_gaps(at);

        let header = RtpHeader::parse(&datagram.data);
        let rtp_like = header
            .as_ref()
            .is_some_and(|h| self.cfg.accepts_payload_type(h.payload_type));
        self.detector
            .observe(datagram.data.first().copied(), rtp_like, at);
        self.reclassify(at);

        // A extração do payload é decidida **por datagrama**, não pela
        // classificação: durante os primeiros `detect_secs` ainda não há
        // veredito, e o TS não pode ficar esperando por ele.
        let payload = match &header {
            Some(h) if datagram.data.first() != Some(&TS_SYNC_BYTE) => {
                if !self.on_rtp_header(h, at) {
                    return None;
                }
                datagram.data.slice(h.header_len.min(datagram.data.len())..)
            }
            _ => datagram.data.clone(),
        };

        // SPEC-PROBE-IP-017 · IP-018 — conformidade do payload de transporte.
        // Num feed UDP puro estes checks ficam `n/a` (SPEC-PROBE-IP-043), mas a
        // contagem continua acontecendo: é o tick que decide o que publicar.
        let shape = ts_payload_shape(&payload);
        if !shape.well_formed {
            self.violations.bad_payload_size += 1;
            return None;
        }
        self.window_ts_packets += shape.ts_packets as u64;
        if self.cfg.ts_per_datagram > 0 && shape.ts_packets as u32 != self.cfg.ts_per_datagram {
            self.violations.ts_per_datagram += 1;
            self.violations.last_bad_ts_count = Some(shape.ts_packets as u32);
        }

        Some(payload)
    }

    /// Processa um datagrama de um dos grupos de FEC.
    ///
    /// SPEC-PROBE-IP-031 … SPEC-PROBE-IP-036
    pub fn on_fec_datagram(&mut self, datagram: &Datagram) {
        let Some(header) = RtpHeader::parse(&datagram.data) else {
            return;
        };
        let body = datagram.data.slice(header.header_len.min(datagram.data.len())..);
        let Some(fec) = FecHeader::parse(&body) else {
            return;
        };
        if body.len() < FEC_HEADER_LEN {
            return;
        }

        let axis = fec.axis();
        self.fec.matrix.absorb(fec.matrix_hint());
        self.fec.window_datagrams += 1;
        self.fec.window_bytes += datagram.data.len() as u64;

        let stream = self.fec.stream_mut(axis);
        stream.datagrams += 1;
        stream.bytes += datagram.data.len() as u64;
        stream.ssrc = Some(header.ssrc);
        stream.last_seen = Some(datagram.at);

        // Tráfego na porta de FEC promove o encapsulamento, mesmo que a janela
        // de detecção já tenha concluído `Rtp` (SPEC-PROBE-IP-042).
        if self.observed == Encapsulation::Rtp {
            self.observed = Encapsulation::RtpFec;
        }
    }

    /// Fecha a janela e devolve o estado do segundo.
    ///
    /// `bitrate_kbps` é o bitrate do TS medido pela camada de transporte — é
    /// dele que sai o inter-arrival **esperado** (SPEC-PROBE-IP-027), e não do
    /// próprio inter-arrival, que tornaria a conta circular.
    pub fn take_tick(&mut self, now: Instant, bitrate_kbps: f64) -> IpTick {
        self.expire_gaps(now);
        self.reclassify(now);

        let elapsed = self
            .window_started
            .map(|start| now.saturating_duration_since(start).as_secs_f64())
            .filter(|s| *s > 0.0);
        let mbps = elapsed.map_or(0.0, |s| self.window_bytes as f64 * 8.0 / s / 1e6);

        let ts_per_datagram = (self.window_datagrams > 0)
            .then(|| self.window_ts_packets as f64 / self.window_datagrams as f64);

        let iat = self.iat.summary();
        let avg_payload = (self.window_datagrams > 0)
            .then(|| self.window_bytes as f64 / self.window_datagrams as f64);
        let iat_expected_us = avg_payload
            .and_then(|bytes| net::expected_iat_us(bytes, bitrate_kbps * 1000.0));

        // SPEC-PROBE-IP-027 — em VBR o esperado não é constante, e afirmar
        // rajada em cima de uma referência que muda seria inventar defeito.
        let cbr = match (self.prev_bitrate_kbps, bitrate_kbps) {
            (Some(prev), now_kbps) if prev > 0.0 => {
                (now_kbps - prev).abs() / prev * 100.0 <= self.cfg.vbr_tolerance_pct
            }
            _ => false,
        };
        self.prev_bitrate_kbps = (bitrate_kbps > 0.0).then_some(bitrate_kbps);
        let burstiness = match (cbr, iat.p99_us, iat_expected_us) {
            (true, Some(p99), Some(expected)) => net::burstiness(p99, expected),
            _ => None,
        };

        let rtp_applicable = self.observed.has_rtp();
        let rtp = rtp_applicable.then_some(self.rtp_window);
        if rtp_applicable {
            self.loss_history.push_back((
                now,
                self.rtp_window.received,
                self.rtp_window.missing,
            ));
        }
        let cutoff = now
            .checked_sub(self.cfg.loss_ratio_window)
            .unwrap_or(now);
        while self.loss_history.front().is_some_and(|(t, _, _)| *t < cutoff) {
            self.loss_history.pop_front();
        }
        let (received, missing) = self
            .loss_history
            .iter()
            .fold((0u64, 0u64), |(r, m), (_, dr, dm)| (r + dr, m + dm));
        let loss_ratio = (rtp_applicable && received + missing > 0)
            .then(|| missing as f64 / (received + missing) as f64);

        let fec = self.fec_status(rtp_applicable);
        let tick = IpTick {
            encapsulation: self.observed,
            encapsulation_mismatch: self.mismatch(),
            encapsulation_changed: self.changed_this_window,
            datagrams: self.window_datagrams,
            bytes: self.window_bytes,
            mbps,
            ts_per_datagram,
            sources: self.sources.iter().copied().collect(),
            ssrc: self.current_ssrc,
            rtp,
            loss_ratio,
            violations: if rtp_applicable {
                self.violations
            } else {
                // SPEC-PROBE-IP-043 — os checks de §5.2 são de RTP; num feed
                // UDP puro eles não podem virar alarme.
                IpViolations::default()
            },
            iat,
            iat_expected_us,
            burstiness,
            jitter_us: self.jitter.jitter_us(),
            noise_floor_us: self.noise.floor_us(),
            fec,
            total_datagrams: self.total_datagrams,
            total_bytes: self.total_bytes,
        };

        self.roll_window(now);
        tick
    }

    /// Reinicia o estado RTP/FEC sem derrubar a sessão.
    ///
    /// SPEC-PROBE-IP-046 — usado na troca de encapsulamento em runtime; o
    /// `metrics.csv` continua sem buraco porque o tick é publicado do mesmo
    /// jeito, só com os contadores reiniciados.
    pub fn reset_rtp_state(&mut self) {
        self.streams.clear();
        self.current_ssrc = None;
        self.rtp_window = RtpDelta::default();
        self.loss_history.clear();
        self.violations = IpViolations::default();
        self.jitter.reset();
        self.fec = FecState::default();
    }

    /// Processa o header RTP; devolve `false` se o datagrama não deve seguir.
    fn on_rtp_header(&mut self, header: &RtpHeader, at: Instant) -> bool {
        // SPEC-PROBE-IP-015 — PT fora do perfil é registrado com o valor
        // observado, mas o datagrama continua seguindo: parar a análise por
        // causa de um PT inesperado esconderia tudo o que vem depois.
        if !self.cfg.accepts_payload_type(header.payload_type) {
            self.violations.invalid_pt += 1;
            self.violations.last_bad_pt = Some(header.payload_type);
        }
        // SPEC-PROBE-IP-016 — bits proibidos pelo perfil ST 2022-2, cada um
        // contado em separado.
        if header.padding {
            self.violations.padding += 1;
        }
        if header.extension {
            self.violations.extension += 1;
        }
        if header.marker {
            self.violations.marker += 1;
        }

        // SPEC-PROBE-IP-019 — estado por SSRC.
        if self.current_ssrc != Some(header.ssrc) {
            if self.current_ssrc.is_some() {
                self.rtp_window.ssrc_changes += 1;
                // As lacunas pendentes do fluxo antigo morrem com ele: confirmá-las
                // como perda transformaria uma troca de fonte numa rajada que
                // nunca existiu.
                self.streams.clear();
                self.jitter.reset();
            }
            self.current_ssrc = Some(header.ssrc);
        }

        self.jitter.observe(header.timestamp, at);

        let state = self
            .streams
            .entry(header.ssrc)
            .or_insert_with(|| RtpSeqState::new(header.ssrc));
        match state.observe(header.sequence, at) {
            SeqOutcome::Init | SeqOutcome::InOrder => self.rtp_window.received += 1,
            SeqOutcome::Gap { .. } => self.rtp_window.received += 1,
            SeqOutcome::Reordered => {
                self.rtp_window.received += 1;
                self.rtp_window.reorder += 1;
            }
            SeqOutcome::Duplicate => {
                self.rtp_window.dup += 1;
                // SPEC-PROBE-IP-022 — a duplicata **não** vai para o demux: o
                // TS a veria como um pacote repetido e abriria um CC error que
                // não existe no stream.
                return false;
            }
            SeqOutcome::TooOld => {
                self.rtp_window.too_old += 1;
            }
            SeqOutcome::SourceRestart { .. } => {
                self.rtp_window.received += 1;
                self.rtp_window.source_restarts += 1;
            }
        }
        true
    }

    /// Confirma as lacunas cuja janela de reconciliação venceu.
    ///
    /// SPEC-PROBE-IP-021
    fn expire_gaps(&mut self, now: Instant) {
        let window = self.cfg.reorder_window;
        let mut confirmed = 0u64;
        for state in self.streams.values_mut() {
            confirmed += state.expire(now, window);
        }
        self.rtp_window.missing += confirmed;
    }

    /// Reavalia o encapsulamento quando a janela de detecção fecha.
    ///
    /// SPEC-PROBE-IP-042 · SPEC-PROBE-IP-046
    fn reclassify(&mut self, at: Instant) {
        let Some(verdict) = self.detector.conclude(at, self.cfg.detect_window) else {
            return;
        };
        // A promoção `Rtp → RtpFec` vem do tráfego nas portas de FEC, e é
        // decidida pelo **estado** e não pelo evento: a FEC costuma chegar
        // antes de a janela de detecção fechar, e uma promoção que dependesse
        // da ordem de chegada perderia o badge nesse caso.
        let verdict = match verdict {
            Encapsulation::Rtp if self.fec.streams_seen() > 0 => Encapsulation::RtpFec,
            v => v,
        };
        if self.observed == verdict {
            return;
        }
        let previous = self.observed;
        self.observed = verdict;
        if previous != Encapsulation::Unknown {
            tracing::warn!(
                de = previous.badge(),
                para = verdict.badge(),
                "probe: encapsulamento mudou em runtime — estado RTP/FEC reiniciado"
            );
            self.changed_this_window = true;
            self.reset_rtp_state();
        }
    }

    /// O encapsulamento declarado difere do observado.
    ///
    /// SPEC-PROBE-IP-045 — vale o observado; o declarado só abre o evento.
    fn mismatch(&self) -> bool {
        !matches!(self.observed, Encapsulation::Unknown)
            && !matches!(self.cfg.declared, Encapsulation::Unknown)
            // `Rtp` declarado e `RtpFec` observado é o mesmo encapsulamento com
            // FEC por cima, não divergência.
            && self.cfg.declared.has_rtp() != self.observed.has_rtp()
    }

    fn fec_status(&self, rtp_applicable: bool) -> FecStatus {
        let listening = self.cfg.fec_mode.listens() && rtp_applicable;
        if !listening {
            return FecStatus {
                listening: false,
                ..FecStatus::default()
            };
        }
        let present = self.fec.column.datagrams > 0 || self.fec.row.datagrams > 0;
        let overhead_pct = (self.window_bytes > 0 && self.fec.window_bytes > 0)
            .then(|| self.fec.window_bytes as f64 / self.window_bytes as f64 * 100.0);
        FecStatus {
            present,
            listening: true,
            l: self.fec.matrix.l,
            d: self.fec.matrix.d,
            streams: self.fec.streams_seen(),
            overhead_pct,
            // SPEC-PROBE-IP-036 — coerência de SSRC entre FEC e fluxo principal.
            ssrc_mismatch: match (self.fec.ssrc(), self.current_ssrc) {
                (Some(fec), Some(main)) => fec != main,
                _ => false,
            },
            datagrams: self.fec.window_datagrams,
        }
    }

    fn roll_window(&mut self, now: Instant) {
        self.window_datagrams = 0;
        self.window_bytes = 0;
        self.window_ts_packets = 0;
        self.window_started = Some(now);
        self.rtp_window = RtpDelta::default();
        self.violations = IpViolations::default();
        self.changed_this_window = false;
        self.fec.window_datagrams = 0;
        self.fec.window_bytes = 0;
        self.iat.roll();
    }
}

/// Buckets do histograma de inter-arrival publicado no tick.
///
/// SPEC-PROBE-IP-026
pub const IAT_HIST_BUCKETS: usize = net::HIST_BUCKETS;

/// Índice do bucket de um inter-arrival em µs.
///
/// Reexportado para que a UI possa realçar a barra do p99 sem depender do
/// crate `net` (SPEC-PROBE-IP-047).
pub fn iat_bucket_of(us: f64) -> usize {
    net::LogHistogram::bucket_of(us)
}

/// Borda superior de um bucket do histograma, em µs.
pub fn iat_bucket_upper_us(index: usize) -> f64 {
    net::LogHistogram::upper_edge(index)
}

/// Formata um valor opcional para o CSV: ausente sai **vazio**, nunca `0`.
///
/// §6 — zero significa "medido e deu zero"; uma coluna vazia significa "não
/// medido".  É essa diferença que decide se um feed UDP puro estava sem perda
/// ou se a perda simplesmente não era observável, quando a planilha for aberta
/// 12 h depois.
pub fn csv_opt<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or(String::new(), |v| v.to_string())
}

/// Formata um `f64` opcional com casas fixas; ausente sai vazio.
pub fn csv_opt_f64(value: Option<f64>, decimals: usize) -> String {
    value.map_or(String::new(), |v| format!("{v:.decimals$}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use net::{RTP_PT_MPEGTS, TS_PACKET_LEN};

    fn config(declared: Encapsulation, fec_mode: FecMode) -> IpAnalyzerConfig {
        IpAnalyzerConfig::from_config(&ProbeConfig::default(), fec_mode, declared)
    }

    fn analyzer(declared: Encapsulation) -> IpAnalyzer {
        IpAnalyzer::new(config(declared, FecMode::Auto))
    }

    fn addr(last: u8) -> SocketAddrV4 {
        SocketAddrV4::new(std::net::Ipv4Addr::new(10, 0, 0, last), 50_000)
    }

    fn datagram(data: Vec<u8>, at: Instant) -> Datagram {
        Datagram {
            data: Bytes::from(data),
            from: addr(1),
            at,
        }
    }

    /// Payload de `n` pacotes TS.
    fn ts_payload(n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n * TS_PACKET_LEN];
        for chunk in v.chunks_exact_mut(TS_PACKET_LEN) {
            chunk[0] = TS_SYNC_BYTE;
        }
        v
    }

    /// Datagrama RTP com 7 pacotes TS.
    fn rtp(seq: u16, ssrc: u32, at: Instant) -> Datagram {
        rtp_with(seq, ssrc, RTP_PT_MPEGTS, 0x00, 7, at)
    }

    fn rtp_with(
        seq: u16,
        ssrc: u32,
        pt: u8,
        flags: u8,
        ts_packets: usize,
        at: Instant,
    ) -> Datagram {
        let mut pkt = vec![0x80 | flags, pt];
        pkt.extend_from_slice(&seq.to_be_bytes());
        pkt.extend_from_slice(&(u32::from(seq) * 3_600).to_be_bytes());
        pkt.extend_from_slice(&ssrc.to_be_bytes());
        pkt.extend_from_slice(&ts_payload(ts_packets));
        datagram(pkt, at)
    }

    /// Tempo que a detecção de encapsulamento leva para concluir, com folga.
    const WARM_UP: Duration = Duration::from_secs(4);

    /// Alimenta o analisador com RTP contínuo até a detecção concluir.
    ///
    /// Sem isto o encapsulamento fica `Unknown` e os contadores de RTP não são
    /// publicados — que é justamente o comportamento correto de
    /// SPEC-PROBE-IP-042, e não algo a contornar no código de produção.
    fn warm_up(a: &mut IpAnalyzer, t0: Instant) {
        for i in 0..100u64 {
            a.on_datagram(&rtp(i as u16, 1, t0 + Duration::from_millis(i * 40)));
        }
        a.take_tick(t0 + WARM_UP, 15_000.0);
    }

    /// Datagrama de FEC no eixo pedido.
    fn fec_datagram(d: bool, offset: u8, na: u8, ssrc: u32, at: Instant) -> Datagram {
        let mut pkt = vec![0x80, 96];
        pkt.extend_from_slice(&1u16.to_be_bytes());
        pkt.extend_from_slice(&0u32.to_be_bytes());
        pkt.extend_from_slice(&ssrc.to_be_bytes());
        let mut fec = [0u8; FEC_HEADER_LEN];
        fec[12] = if d { 0x40 } else { 0x00 };
        fec[13] = offset;
        fec[14] = na;
        pkt.extend_from_slice(&fec);
        pkt.extend_from_slice(&[0u8; 1_316]);
        datagram(pkt, at)
    }

    /// SPEC-PROBE-IP-042 — datagramas começando em `0x47` classificam o feed
    /// como UDP puro, e aí toda a faixa RTP/FEC fica `n/a`.
    #[test]
    fn spec_probe_ip_042_pure_udp_is_detected_and_rtp_is_not_applicable() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Udp);
        for i in 0..100u64 {
            let dg = datagram(ts_payload(7), t0 + Duration::from_millis(i * 10));
            assert!(a.on_datagram(&dg).is_some());
        }
        let tick = a.take_tick(t0 + Duration::from_secs(4), 15_000.0);

        assert_eq!(tick.encapsulation, Encapsulation::Udp);
        assert!(!tick.rtp_applicable());
        assert!(tick.rtp.is_none(), "sem RTP não há contador de RTP");
        assert!(tick.violations.is_empty());
        assert!(!tick.fec.listening, "sem RTP, FEC nem é escutada");
        assert!(!tick.encapsulation_mismatch);
        assert_eq!(tick.datagrams, 100);
    }

    /// SPEC-PROBE-IP-043 — 12 h de feed UDP puro não podem produzir **um**
    /// evento de RTP ou FEC.  A versão curta: mil datagramas, zero contadores.
    #[test]
    fn spec_probe_ip_043_udp_feed_never_produces_rtp_counters() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Udp);
        for i in 0..1_000u64 {
            let dg = datagram(ts_payload(7), t0 + Duration::from_millis(i * 10));
            a.on_datagram(&dg);
            if i % 100 == 99 {
                let tick = a.take_tick(t0 + Duration::from_millis(i * 10), 15_000.0);
                assert!(tick.rtp.is_none());
                assert!(tick.loss_ratio.is_none());
                assert!(tick.violations.is_empty());
                assert!(!tick.fec.present);
            }
        }
    }

    /// SPEC-PROBE-IP-045 — UDP puro declarado como `rtp` abre um evento de
    /// divergência, e vale o **observado**.
    #[test]
    fn spec_probe_ip_045_declared_rtp_arriving_as_udp_is_a_mismatch() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        for i in 0..50u64 {
            a.on_datagram(&datagram(ts_payload(7), t0 + Duration::from_millis(i * 20)));
        }
        let tick = a.take_tick(t0 + Duration::from_secs(4), 15_000.0);
        assert_eq!(tick.encapsulation, Encapsulation::Udp, "vale o observado");
        assert!(tick.encapsulation_mismatch);
        assert!(tick.rtp.is_none(), "a análise continua, só que como UDP");
    }

    /// SPEC-PROBE-IP-042 — RTP é detectado por V=2 + PT do perfil.
    #[test]
    fn spec_probe_ip_042_rtp_is_detected_and_counted() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        for i in 0..500u64 {
            let dg = rtp(i as u16, 0x1111, t0 + Duration::from_micros(i * 700));
            let payload = a.on_datagram(&dg).expect("payload entregue ao demux");
            assert_eq!(payload.len(), 7 * TS_PACKET_LEN);
            assert_eq!(payload[0], TS_SYNC_BYTE);
        }
        let tick = a.take_tick(t0 + Duration::from_secs(4), 15_000.0);

        assert_eq!(tick.encapsulation, Encapsulation::Rtp);
        assert!(tick.rtp_applicable());
        let rtp_delta = tick.rtp.expect("contadores de RTP");
        assert_eq!(rtp_delta.received, 500);
        assert_eq!(rtp_delta.missing, 0);
        assert_eq!(tick.ssrc, Some(0x1111));
        assert_eq!(tick.loss_ratio, Some(0.0));
        assert_eq!(tick.ts_per_datagram, Some(7.0));
        assert!(!tick.encapsulation_mismatch);
    }

    /// SPEC-PROBE-IP-022 — a duplicata é contada e **não** segue para o demux:
    /// senão o TS abriria um CC error que não existe no stream.
    #[test]
    fn spec_probe_ip_022_duplicate_is_not_forwarded_to_the_demux() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        assert!(a.on_datagram(&rtp(100, 1, t0)).is_some());
        assert!(
            a.on_datagram(&rtp(100, 1, t0 + Duration::from_micros(700)))
                .is_none(),
            "duplicata não pode chegar ao demux"
        );
        let tick = a.take_tick(t0 + Duration::from_secs(4), 15_000.0);
        assert_eq!(tick.rtp.expect("rtp").dup, 1);
    }

    /// §7 — payload de 1315 bytes vira `bad_payload_size` e o datagrama não é
    /// passado ao demux.
    #[test]
    fn spec_probe_ip_017_bad_payload_size_is_not_forwarded() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        // Aquece a detecção para que o encapsulamento já seja RTP.
        for i in 0..10u64 {
            a.on_datagram(&rtp(i as u16, 1, t0 + Duration::from_millis(i)));
        }
        a.take_tick(t0 + Duration::from_secs(4), 15_000.0);

        let mut pkt = vec![0x80, RTP_PT_MPEGTS, 0, 20, 0, 0, 0, 0, 0, 0, 0, 1];
        let mut payload = vec![0u8; 1_315];
        payload[0] = TS_SYNC_BYTE;
        pkt.extend_from_slice(&payload);
        assert!(a
            .on_datagram(&datagram(pkt, t0 + Duration::from_secs(4)))
            .is_none());

        let tick = a.take_tick(t0 + Duration::from_secs(5), 15_000.0);
        assert_eq!(tick.violations.bad_payload_size, 1);
    }

    /// §7 — datagrama com 4 TS num perfil de 7 gera evento e a análise segue.
    #[test]
    fn spec_probe_ip_018_wrong_ts_per_datagram_is_flagged_without_stopping() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        for i in 0..10u64 {
            a.on_datagram(&rtp(i as u16, 1, t0 + Duration::from_millis(i)));
        }
        a.take_tick(t0 + Duration::from_secs(4), 15_000.0);

        let dg = rtp_with(20, 1, RTP_PT_MPEGTS, 0, 4, t0 + Duration::from_secs(4));
        assert!(
            a.on_datagram(&dg).is_some(),
            "o datagrama continua indo para o demux"
        );
        let tick = a.take_tick(t0 + Duration::from_secs(5), 15_000.0);
        assert_eq!(tick.violations.ts_per_datagram, 1);
        assert_eq!(tick.violations.last_bad_ts_count, Some(4));
    }

    /// §7 — PT 97 num perfil que aceita 33 e 96 vira evento com o valor
    /// observado, sem interromper a análise (SPEC-PROBE-IP-015 · IP-038).
    #[test]
    fn spec_probe_ip_015_invalid_payload_type_reports_the_observed_value() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        for i in 0..10u64 {
            a.on_datagram(&rtp(i as u16, 1, t0 + Duration::from_millis(i)));
        }
        a.take_tick(t0 + Duration::from_secs(4), 15_000.0);

        let dg = rtp_with(20, 1, 97, 0, 7, t0 + Duration::from_secs(4));
        assert!(a.on_datagram(&dg).is_some(), "a análise do TS não para");
        let tick = a.take_tick(t0 + Duration::from_secs(5), 15_000.0);
        assert_eq!(tick.violations.invalid_pt, 1);
        assert_eq!(tick.violations.last_bad_pt, Some(97));
    }

    /// SPEC-PROBE-IP-016 — padding, extension e marker são contados em
    /// separado, cada um seu próprio check.
    #[test]
    fn spec_probe_ip_016_forbidden_bits_are_counted_independently() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        for i in 0..10u64 {
            a.on_datagram(&rtp(i as u16, 1, t0 + Duration::from_millis(i)));
        }
        a.take_tick(t0 + Duration::from_secs(4), 15_000.0);

        // P = 0x20 no primeiro byte; M = 0x80 no segundo (via `pt`).
        a.on_datagram(&rtp_with(
            20,
            1,
            RTP_PT_MPEGTS,
            0x20,
            7,
            t0 + Duration::from_secs(4),
        ));
        a.on_datagram(&rtp_with(
            21,
            1,
            RTP_PT_MPEGTS | 0x80,
            0x00,
            7,
            t0 + Duration::from_secs(4),
        ));

        let tick = a.take_tick(t0 + Duration::from_secs(5), 15_000.0);
        assert_eq!(tick.violations.padding, 1);
        assert_eq!(tick.violations.marker, 1);
        assert_eq!(tick.violations.extension, 0);
    }

    /// SPEC-PROBE-IP-019 — a troca de SSRC abre evento e reinicia os
    /// contadores, sem explosão de perda falsa.
    #[test]
    fn spec_probe_ip_019_ssrc_change_rebases_without_phantom_loss() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        for i in 0..50u64 {
            a.on_datagram(&rtp(60_000 + i as u16, 0xAAAA, t0 + Duration::from_millis(i)));
        }
        a.take_tick(t0 + Duration::from_secs(4), 15_000.0);

        // Nova fonte, sequência recomeçando do zero.
        for i in 0..50u64 {
            a.on_datagram(&rtp(
                i as u16,
                0xBBBB,
                t0 + Duration::from_secs(4) + Duration::from_millis(i),
            ));
        }
        let tick = a.take_tick(t0 + Duration::from_secs(5), 15_000.0);
        let rtp_delta = tick.rtp.expect("rtp");
        assert_eq!(rtp_delta.ssrc_changes, 1);
        assert_eq!(rtp_delta.missing, 0, "troca de fonte não é perda");
        assert_eq!(tick.ssrc, Some(0xBBBB));
    }

    /// SPEC-PROBE-IP-020 · IP-021 — reordenação dentro da janela não é perda;
    /// a lacuna que não volta vira perda confirmada.
    #[test]
    fn spec_probe_ip_021_gap_becomes_loss_only_after_the_window() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        warm_up(&mut a, t0);
        let base = t0 + WARM_UP;

        a.on_datagram(&rtp(100, 1, base));
        a.on_datagram(&rtp(102, 1, base + Duration::from_millis(1)));

        // Dentro da janela de 200 ms: reordenação.
        a.on_datagram(&rtp(101, 1, base + Duration::from_millis(50)));
        let tick = a.take_tick(base + Duration::from_millis(60), 15_000.0);
        let d = tick.rtp.expect("rtp");
        assert_eq!(d.reorder, 1);
        assert_eq!(d.missing, 0);

        // Nova lacuna que não volta.
        a.on_datagram(&rtp(104, 1, base + Duration::from_millis(100)));
        let tick = a.take_tick(base + Duration::from_millis(400), 15_000.0);
        assert_eq!(tick.rtp.expect("rtp").missing, 1);
    }

    /// SPEC-PROBE-IP-023 — o retardatário que passa da janela é `too_old`
    /// **dentro do mesmo tick**.
    ///
    /// Regressão real: com a reconciliação fechada só no tick de 1 s, um pacote
    /// 500 ms atrasado seria contado como reordenação e a perda desapareceria —
    /// exatamente o caso que a spec fixa na tabela do §7.
    #[test]
    fn spec_probe_ip_023_late_arrival_within_the_same_tick_is_too_old() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        warm_up(&mut a, t0);
        let base = t0 + WARM_UP;

        a.on_datagram(&rtp(100, 1, base));
        a.on_datagram(&rtp(102, 1, base + Duration::from_millis(1)));
        // 500 ms depois: a janela de 200 ms já venceu.
        a.on_datagram(&rtp(101, 1, base + Duration::from_millis(500)));

        let tick = a.take_tick(base + Duration::from_millis(900), 15_000.0);
        let d = tick.rtp.expect("rtp");
        assert_eq!(d.missing, 1, "a lacuna virou perda antes do retardatário");
        assert_eq!(d.too_old, 1);
        assert_eq!(d.reorder, 0, "500 ms não cabe numa janela de 200 ms");
    }

    /// SPEC-PROBE-IP-021 — a razão de perda usa a janela longa do perfil.
    #[test]
    fn spec_probe_ip_021_loss_ratio_uses_the_long_window() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        let mut seq = 0u16;
        for tick_index in 0..10u64 {
            let base = t0 + Duration::from_secs(tick_index);
            for i in 0..100u64 {
                a.on_datagram(&rtp(seq, 1, base + Duration::from_millis(i)));
                seq = seq.wrapping_add(1);
            }
            // Uma lacuna por segundo.
            seq = seq.wrapping_add(1);
            a.take_tick(base + Duration::from_millis(999), 15_000.0);
        }
        let tick = a.take_tick(t0 + Duration::from_secs(11), 15_000.0);
        let ratio = tick.loss_ratio.expect("com RTP há razão");
        assert!(ratio > 0.0 && ratio < 0.02, "razão medida: {ratio}");
    }

    /// SPEC-PROBE-IP-010 — duas fontes no mesmo grupo/porta aparecem as duas.
    #[test]
    fn spec_probe_ip_010_multiple_sources_are_listed() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        let mut first = rtp(1, 1, t0);
        first.from = addr(1);
        a.on_datagram(&first);
        let mut second = rtp(2, 1, t0 + Duration::from_millis(1));
        second.from = addr(2);
        a.on_datagram(&second);

        let tick = a.take_tick(t0 + Duration::from_secs(4), 15_000.0);
        assert_eq!(tick.sources.len(), 2);
        assert!(tick.sources.contains(&addr(1)));
        assert!(tick.sources.contains(&addr(2)));
    }

    /// SPEC-PROBE-IP-027 — num CBR de 15 Mbps com 7 TS/datagrama, o
    /// inter-arrival médio medido bate com `payload_bits / bitrate` dentro de
    /// 1 %.  É o teste de sanidade da medição inteira (§5.4.1).
    #[test]
    fn spec_probe_ip_027_expected_interarrival_matches_cbr() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        // 1 s de CBR real: 1316 bytes de payload a cada 701,9 µs.
        let step_ns = 701_870u64;
        for i in 0..1_400u64 {
            a.on_datagram(&rtp(
                i as u16,
                1,
                t0 + Duration::from_nanos(i * step_ns),
            ));
        }
        let tick = a.take_tick(t0 + Duration::from_secs(1), 15_002.4);

        let measured = tick.iat.avg_us.expect("média medida");
        let expected = tick.iat_expected_us.expect("esperado do bitrate");
        // O payload UDP inclui os 12 bytes do header RTP, então o esperado é o
        // do datagrama inteiro; medido e esperado precisam bater dentro de 1 %.
        assert!(
            (measured - 701.87).abs() / 701.87 < 0.01,
            "inter-arrival medido {measured:.2} µs, referência 701,87 µs"
        );
        assert!(
            (measured - expected).abs() / expected < 0.02,
            "medido {measured:.2} µs × esperado {expected:.2} µs"
        );
    }

    /// SPEC-PROBE-IP-030 — feed sem FEC com `fec = auto` não gera alarme:
    /// `fec_present = false` e ponto.
    #[test]
    fn spec_probe_ip_030_auto_without_fec_traffic_is_silent() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        for i in 0..50u64 {
            a.on_datagram(&rtp(i as u16, 1, t0 + Duration::from_millis(i)));
        }
        let tick = a.take_tick(t0 + Duration::from_secs(4), 15_000.0);
        assert!(tick.fec.listening);
        assert!(!tick.fec.present);
        assert_eq!(tick.fec.streams, 0);
        assert_eq!(tick.fec.l, None);
        assert_eq!(tick.encapsulation, Encapsulation::Rtp, "sem FEC, sem badge");
    }

    /// §7 — FEC coluna com offset 8 e NA 5 dá L=8, D=5, L×D=40: dentro do
    /// perfil, sem alarme.  Com os dois fluxos o badge vira `RTP+FEC`.
    #[test]
    fn spec_probe_ip_032_fec_matrix_is_read_from_both_ports() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        for i in 0..100u64 {
            a.on_datagram(&rtp(i as u16, 0xC0FF, t0 + Duration::from_millis(i * 40)));
        }
        a.take_tick(t0 + WARM_UP, 15_000.0);

        // O overhead é a razão entre os bytes de FEC e os do fluxo principal na
        // **mesma** janela; sem tráfego principal no segundo, ele fica `n/a`.
        let base = t0 + WARM_UP;
        for i in 0..100u64 {
            a.on_datagram(&rtp(
                100 + i as u16,
                0xC0FF,
                base + Duration::from_millis(i),
            ));
        }
        a.on_fec_datagram(&fec_datagram(false, 8, 5, 0xC0FF, base));
        a.on_fec_datagram(&fec_datagram(true, 1, 8, 0xC0FF, base));

        let tick = a.take_tick(base + Duration::from_secs(1), 15_000.0);
        assert_eq!(tick.encapsulation, Encapsulation::RtpFec);
        assert!(tick.fec.present);
        assert_eq!(tick.fec.l, Some(8));
        assert_eq!(tick.fec.d, Some(5));
        assert_eq!(tick.fec.lxd(), Some(40));
        assert_eq!(tick.fec.matrix_label(), "8×5");
        assert_eq!(tick.fec.streams, 2);
        assert!(!tick.fec.ssrc_mismatch);
        assert!(tick.fec.overhead_pct.is_some());
    }

    /// SPEC-PROBE-IP-034 · SPEC-PROBE-IP-036 — L×D acima do teto e SSRC
    /// divergente são observáveis no mesmo tick.
    #[test]
    fn spec_probe_ip_034_large_matrix_and_ssrc_mismatch_are_visible() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        for i in 0..50u64 {
            a.on_datagram(&rtp(i as u16, 0xC0FF, t0 + Duration::from_millis(i)));
        }
        a.take_tick(t0 + Duration::from_secs(4), 15_000.0);

        a.on_fec_datagram(&fec_datagram(false, 20, 8, 0xDEAD, t0 + Duration::from_secs(4)));
        let tick = a.take_tick(t0 + Duration::from_secs(5), 15_000.0);
        assert_eq!(tick.fec.lxd(), Some(160));
        assert!(tick.fec.ssrc_mismatch);
        assert_eq!(tick.fec.streams, 1, "só o fluxo de coluna chegou");
    }

    /// SPEC-PROBE-IP-030 — com `fec = off` a probe nem escuta, e os checks de
    /// FEC ficam `n/a` em vez de acusar ausência.
    #[test]
    fn spec_probe_ip_030_fec_off_marks_the_block_not_applicable() {
        let t0 = Instant::now();
        let mut a = IpAnalyzer::new(config(Encapsulation::Rtp, FecMode::Off));
        for i in 0..50u64 {
            a.on_datagram(&rtp(i as u16, 1, t0 + Duration::from_millis(i)));
        }
        let tick = a.take_tick(t0 + Duration::from_secs(4), 15_000.0);
        assert!(!tick.fec.listening);
        assert!(!tick.fec.present);
    }

    /// SPEC-PROBE-IP-046 — a troca de encapsulamento em runtime reinicia o
    /// estado RTP/FEC sem derrubar a sessão nem deixar buraco no tick.
    #[test]
    fn spec_probe_ip_046_runtime_encapsulation_change_resets_rtp_state() {
        let t0 = Instant::now();
        let mut a = analyzer(Encapsulation::Rtp);
        for i in 0..100u64 {
            a.on_datagram(&rtp(i as u16, 0x1234, t0 + Duration::from_millis(i)));
        }
        let tick = a.take_tick(t0 + Duration::from_secs(4), 15_000.0);
        assert_eq!(tick.encapsulation, Encapsulation::Rtp);
        assert!(tick.rtp.is_some());

        // A fonte passa a mandar TS puro.
        let base = t0 + Duration::from_secs(4);
        for i in 0..100u64 {
            a.on_datagram(&datagram(ts_payload(7), base + Duration::from_millis(i)));
        }
        let tick = a.take_tick(base + Duration::from_secs(4), 15_000.0);
        assert_eq!(tick.encapsulation, Encapsulation::Udp);
        assert!(tick.encapsulation_changed);
        assert!(tick.rtp.is_none(), "os contadores de RTP saem de cena");
        assert!(tick.datagrams > 0, "o tick continua sendo publicado");
    }

    /// SPEC-PROBE-IP-005 — a probe mede o próprio piso de ruído e o publica.
    #[test]
    fn spec_probe_ip_005_noise_floor_is_measured_and_published() {
        let t0 = Instant::now();
        let mut cfg = config(Encapsulation::Rtp, FecMode::Auto);
        cfg.calibration_window = Duration::from_secs(1);
        let mut a = IpAnalyzer::new(cfg);

        for i in 0..2_000u64 {
            // Ruído sintético: metade dos intervalos alongados.
            let jitter = if i % 2 == 0 { 0 } else { 300 };
            a.on_datagram(&rtp(
                i as u16,
                1,
                t0 + Duration::from_micros(i * 700 + jitter),
            ));
        }
        let tick = a.take_tick(t0 + Duration::from_secs(2), 15_000.0);
        let floor = tick.noise_floor_us.expect("calibração fecha em 1 s");
        assert!(floor > 0.0, "piso medido: {floor}");
        assert_eq!(a.noise_floor_us(), Some(floor));
    }

    /// §6 — campo não observável sai **vazio** no CSV, nunca como `0`: é o que
    /// distingue "não medido" de "medido e deu zero" quando a planilha for
    /// aberta 12 h depois.
    #[test]
    fn spec_probe_ip_011_csv_formats_missing_values_as_empty() {
        assert_eq!(csv_opt(Some(7u32)), "7");
        assert_eq!(csv_opt(None::<u32>), "");
        assert_eq!(csv_opt_f64(Some(701.87), 1), "701.9");
        assert_eq!(csv_opt_f64(None, 1), "");
        assert_eq!(csv_opt_f64(Some(0.0), 1), "0.0", "zero medido continua zero");
    }
}
