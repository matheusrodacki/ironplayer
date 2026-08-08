//! Temporização: inter-arrival, percentis e jitter RFC 3550.
//!
//! **Honestidade obrigatória (§4).** O timestamp sai do retorno do `recv_from`,
//! então carrega o ruído de agendamento do Windows (0,1–2 ms) — três ordens de
//! grandeza acima do SD de 0,34 µs de uma probe de captura em hardware.  O que
//! esta camada entrega é **medida relativa**: tendência ao longo de 12 h, picos
//! e rajadas.  Perda e reordenação, essas sim, são exatas — mas moram em
//! [`crate::rtp_seq`], não aqui.
//!
//! SPEC-PROBE-IP-004 · SPEC-PROBE-IP-005 · SPEC-PROBE-IP-025 …
//! SPEC-PROBE-IP-028

use std::time::{Duration, Instant};

/// Frequência do relógio de timestamp RTP para MPEG-TS (RFC 3551).
pub const RTP_CLOCK_HZ: f64 = 90_000.0;

/// Buckets do histograma de inter-arrival.
///
/// SPEC-PROBE-IP-026 — 64 buckets log-espaçados, memória constante em 12 h.
pub const HIST_BUCKETS: usize = 64;
/// Borda inferior do histograma, em µs.
pub const HIST_MIN_US: f64 = 10.0;
/// Borda superior do histograma, em µs.
pub const HIST_MAX_US: f64 = 100_000.0;

/// Média e desvio-padrão por Welford.
///
/// SPEC-PROBE-IP-025 — sem alocação por pacote e numericamente estável ao longo
/// de 12 h; a fórmula ingênua (soma dos quadrados) perde precisão muito antes
/// disso num stream de 1400 pacotes por segundo.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Welford {
    count: u64,
    mean: f64,
    m2: f64,
    min: f64,
    max: f64,
}

impl Welford {
    /// Estatística vazia.
    pub fn new() -> Self {
        Self::default()
    }

    /// Acrescenta uma amostra; valores não finitos são ignorados.
    pub fn push(&mut self, value: f64) {
        if !value.is_finite() {
            return;
        }
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.count += 1;
        let delta = value - self.mean;
        self.mean += delta / self.count as f64;
        self.m2 += delta * (value - self.mean);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Média, ou `None` sem amostra — nunca `0.0`, que significaria "medido".
    pub fn mean(&self) -> Option<f64> {
        (self.count > 0).then_some(self.mean)
    }

    pub fn min(&self) -> Option<f64> {
        (self.count > 0).then_some(self.min)
    }

    pub fn max(&self) -> Option<f64> {
        (self.count > 0).then_some(self.max)
    }

    /// Desvio-padrão amostral; exige ao menos duas amostras.
    pub fn sd(&self) -> Option<f64> {
        (self.count > 1).then(|| (self.m2 / (self.count - 1) as f64).sqrt())
    }

    /// Zera para a próxima janela.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Histograma log-espaçado de inter-arrival.
///
/// SPEC-PROBE-IP-026 — percentis com memória constante: 64 contadores por feed,
/// independentemente da duração da sessão.
#[derive(Debug, Clone, Copy)]
pub struct LogHistogram {
    buckets: [u32; HIST_BUCKETS],
    count: u64,
}

impl Default for LogHistogram {
    fn default() -> Self {
        Self {
            buckets: [0; HIST_BUCKETS],
            count: 0,
        }
    }
}

impl LogHistogram {
    /// Histograma vazio.
    pub fn new() -> Self {
        Self::default()
    }

    /// Índice do bucket de um valor em µs.
    ///
    /// O bucket 0 é o underflow (< 10 µs) e o último é o overflow (≥ 100 ms):
    /// juntar as caudas nos extremos mantém os 62 buckets internos onde a
    /// resolução importa.
    pub fn bucket_of(us: f64) -> usize {
        if !us.is_finite() || us <= HIST_MIN_US {
            return 0;
        }
        if us >= HIST_MAX_US {
            return HIST_BUCKETS - 1;
        }
        let span = (HIST_MAX_US / HIST_MIN_US).ln();
        let pos = (us / HIST_MIN_US).ln() / span;
        1 + (pos * (HIST_BUCKETS - 2) as f64) as usize
    }

    /// Borda superior de um bucket, em µs.
    pub fn upper_edge(index: usize) -> f64 {
        if index == 0 {
            return HIST_MIN_US;
        }
        if index >= HIST_BUCKETS - 1 {
            return HIST_MAX_US;
        }
        let span = (HIST_MAX_US / HIST_MIN_US).ln();
        HIST_MIN_US * (span * index as f64 / (HIST_BUCKETS - 2) as f64).exp()
    }

    /// Registra uma amostra em µs.
    pub fn push(&mut self, us: f64) {
        let idx = Self::bucket_of(us);
        self.buckets[idx] = self.buckets[idx].saturating_add(1);
        self.count += 1;
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Contagem por bucket, para o gráfico da aba `Rede`.
    pub fn buckets(&self) -> &[u32; HIST_BUCKETS] {
        &self.buckets
    }

    /// Percentil `p` ∈ (0,1], em µs, ou `None` sem amostra.
    ///
    /// SPEC-PROBE-IP-026 — devolve a borda superior do bucket que contém o
    /// percentil, o que mantém o erro dentro de um bucket por construção.
    pub fn percentile(&self, p: f64) -> Option<f64> {
        if self.count == 0 || !(0.0..=1.0).contains(&p) {
            return None;
        }
        let target = (p * self.count as f64).ceil().max(1.0) as u64;
        let mut cumulative = 0u64;
        for (i, n) in self.buckets.iter().enumerate() {
            cumulative += u64::from(*n);
            if cumulative >= target {
                return Some(Self::upper_edge(i));
            }
        }
        Some(HIST_MAX_US)
    }

    /// Zera para a próxima janela.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Estatística de inter-arrival de uma janela.
///
/// SPEC-PROBE-IP-025 · SPEC-PROBE-IP-026
#[derive(Debug, Clone, Copy, Default)]
pub struct IatWindow {
    stats: Welford,
    hist: LogHistogram,
    last: Option<Instant>,
}

/// Resumo de uma janela de inter-arrival.
///
/// Todos os campos são `Option`: um feed sem datagrama no segundo tem "sem
/// dado", que no CSV sai vazio e **não** como zero (§6).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IatSummary {
    pub count: u64,
    pub min_us: Option<f64>,
    pub avg_us: Option<f64>,
    pub max_us: Option<f64>,
    pub sd_us: Option<f64>,
    pub p50_us: Option<f64>,
    pub p95_us: Option<f64>,
    pub p99_us: Option<f64>,
    /// Contagem por bucket log-espaçado — alimenta o histograma da aba `Rede`.
    ///
    /// SPEC-PROBE-IP-026 · SPEC-PROBE-IP-047
    pub histogram: [u32; HIST_BUCKETS],
}

impl Default for IatSummary {
    fn default() -> Self {
        Self {
            count: 0,
            min_us: None,
            avg_us: None,
            max_us: None,
            sd_us: None,
            p50_us: None,
            p95_us: None,
            p99_us: None,
            histogram: [0; HIST_BUCKETS],
        }
    }
}

impl IatWindow {
    /// Janela vazia.
    pub fn new() -> Self {
        Self::default()
    }

    /// Carimba a chegada de um datagrama e acumula o intervalo desde o anterior.
    ///
    /// O primeiro datagrama de cada janela só ancora o relógio: medir o
    /// intervalo contra a janela anterior misturaria segundos e produziria um
    /// pico artificial toda vez que a janela virasse.
    pub fn observe(&mut self, at: Instant) {
        if let Some(prev) = self.last {
            let us = at.saturating_duration_since(prev).as_secs_f64() * 1e6;
            self.stats.push(us);
            self.hist.push(us);
        }
        self.last = Some(at);
    }

    /// Resumo da janela corrente.
    pub fn summary(&self) -> IatSummary {
        IatSummary {
            count: self.stats.count(),
            min_us: self.stats.min(),
            avg_us: self.stats.mean(),
            max_us: self.stats.max(),
            sd_us: self.stats.sd(),
            p50_us: self.hist.percentile(0.50),
            p95_us: self.hist.percentile(0.95),
            p99_us: self.hist.percentile(0.99),
            histogram: *self.hist.buckets(),
        }
    }

    /// Estatística acumulada (usada na calibração de ruído).
    pub fn stats(&self) -> &Welford {
        &self.stats
    }

    /// Histograma acumulado.
    pub fn histogram(&self) -> &LogHistogram {
        &self.hist
    }

    /// Fecha a janela mantendo o instante do último datagrama, para que o
    /// primeiro intervalo da janela seguinte continue sendo medido.
    pub fn roll(&mut self) {
        self.stats.reset();
        self.hist.reset();
    }
}

/// Inter-arrival **esperado** de um CBR, em µs.
///
/// SPEC-PROBE-IP-027 — é o teste de sanidade da implementação inteira:
/// `1316 × 8 / 15,0024 Mbps = 701,9 µs`, contra os 701,87 µs medidos pela probe
/// de referência.  Se a média medida não bater com isto dentro de 1 % num CBR
/// conhecido, a medição está errada — não a rede.
pub fn expected_iat_us(payload_bytes: f64, bitrate_bps: f64) -> Option<f64> {
    (payload_bytes > 0.0 && bitrate_bps > 0.0).then(|| payload_bytes * 8.0 / bitrate_bps * 1e6)
}

/// Desvio relativo do inter-arrival observado em relação ao esperado.
///
/// SPEC-PROBE-IP-027 — usa o p99, e não a média: num CBR a média bate com o
/// esperado mesmo quando o tráfego chega em rajadas, porque o que a rajada faz
/// é encurtar uns intervalos e alongar outros.  É a cauda que denuncia.
pub fn burstiness(p99_us: f64, expected_us: f64) -> Option<f64> {
    (expected_us > 0.0 && p99_us.is_finite()).then(|| (p99_us / expected_us - 1.0).max(0.0))
}

/// Jitter de rede segundo RFC 3550 §6.4.1.
///
/// SPEC-PROBE-IP-028 — `J += (|D| − J)/16`.  Quando o timestamp RTP é constante
/// ou não monotônico a métrica fica **`n/a`**: um encoder que carimba tudo com o
/// mesmo timestamp produziria um jitter aparente igual ao inter-arrival, e um
/// alarme daí seria puro artefato.
#[derive(Debug, Clone, Copy, Default)]
pub struct Rfc3550Jitter {
    jitter: f64,
    prev_transit: Option<f64>,
    prev_ts: Option<u32>,
    epoch: Option<Instant>,
    /// `true` assim que dois timestamps RTP diferentes forem observados.
    ts_varies: bool,
    /// `true` se algum timestamp andou para trás.
    ts_backwards: bool,
    samples: u64,
}

impl Rfc3550Jitter {
    /// Estado zerado.
    pub fn new() -> Self {
        Self::default()
    }

    /// Acumula um pacote.
    pub fn observe(&mut self, rtp_timestamp: u32, at: Instant) {
        let epoch = *self.epoch.get_or_insert(at);

        if let Some(prev) = self.prev_ts {
            if prev != rtp_timestamp {
                self.ts_varies = true;
            }
            // Diferença assinada de 32 bits: um wrap normal do timestamp não é
            // "andar para trás".
            let delta = rtp_timestamp.wrapping_sub(prev);
            if delta > u32::MAX / 2 {
                self.ts_backwards = true;
            }
        }
        self.prev_ts = Some(rtp_timestamp);

        let arrival = at.saturating_duration_since(epoch).as_secs_f64() * RTP_CLOCK_HZ;
        let transit = arrival - f64::from(rtp_timestamp);
        if let Some(prev) = self.prev_transit {
            let d = transit - prev;
            self.jitter += (d.abs() - self.jitter) / 16.0;
            self.samples += 1;
        }
        self.prev_transit = Some(transit);
    }

    /// `true` quando o timestamp RTP serve para medir jitter.
    ///
    /// SPEC-PROBE-IP-028
    pub fn usable(&self) -> bool {
        self.ts_varies && !self.ts_backwards && self.samples > 0
    }

    /// Jitter em µs, ou `None` quando o timestamp não é utilizável.
    pub fn jitter_us(&self) -> Option<f64> {
        self.usable()
            .then(|| self.jitter / RTP_CLOCK_HZ * 1e6)
    }

    /// Reinicia o estado (troca de SSRC, reinício de fonte).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Piso de ruído do próprio instrumento.
///
/// SPEC-PROBE-IP-005 — o SD do inter-arrival medido nos primeiros `calib_secs`
/// vira `noise_floor_us`; SPEC-PROBE-IP-006 usa esse valor para não abrir alarme
/// de jitter em cima do agendamento do notebook.
#[derive(Debug, Clone)]
pub struct NoiseCalibration {
    stats: Welford,
    window: Duration,
    started: Option<Instant>,
    floor_us: Option<f64>,
}

impl NoiseCalibration {
    /// Calibração de `window` de duração.
    pub fn new(window: Duration) -> Self {
        Self {
            stats: Welford::new(),
            window,
            started: None,
            floor_us: None,
        }
    }

    /// Acumula um intervalo em µs enquanto a calibração estiver aberta.
    pub fn observe(&mut self, iat_us: f64, at: Instant) {
        if self.floor_us.is_some() {
            return;
        }
        let started = *self.started.get_or_insert(at);
        self.stats.push(iat_us);
        if at.saturating_duration_since(started) >= self.window {
            // Menos de duas amostras não produzem SD; nesse caso a calibração
            // fecha sem piso e os limiares absolutos valem sozinhos.
            self.floor_us = Some(self.stats.sd().unwrap_or(0.0));
        }
    }

    /// Piso medido, ou `None` enquanto a calibração não fechou.
    pub fn floor_us(&self) -> Option<f64> {
        self.floor_us
    }

    /// Limiar efetivo de um alarme de temporização.
    ///
    /// SPEC-PROBE-IP-006 — `max(threshold, k × noise_floor_us)`: num notebook
    /// carregado o piso sobe, e o alarme sobe junto em vez de virar ruído.
    pub fn effective_threshold(&self, threshold_us: f64, k: f64) -> f64 {
        match self.floor_us {
            Some(floor) if floor.is_finite() && k > 0.0 => threshold_us.max(k * floor),
            _ => threshold_us,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC-PROBE-IP-025 — Welford dá média, SD, min e max sem alocar, e
    /// distingue "sem amostra" de zero.
    #[test]
    fn spec_probe_ip_025_welford_reports_stable_moments() {
        let mut w = Welford::new();
        assert_eq!(w.mean(), None);
        assert_eq!(w.sd(), None);

        for v in [700.0, 702.0, 701.0, 703.0, 699.0] {
            w.push(v);
        }
        assert_eq!(w.count(), 5);
        assert!((w.mean().expect("média") - 701.0).abs() < 1e-9);
        assert!((w.sd().expect("sd") - 1.5811).abs() < 1e-3);
        assert_eq!(w.min(), Some(699.0));
        assert_eq!(w.max(), Some(703.0));

        // NaN de um relógio maluco não contamina a estatística.
        w.push(f64::NAN);
        assert_eq!(w.count(), 5);
    }

    /// SPEC-PROBE-IP-025 — 12 h de amostras não degradam a média nem a SD, que
    /// é justamente o motivo de usar Welford em vez de somar quadrados.
    #[test]
    fn spec_probe_ip_025_welford_is_stable_over_a_long_session() {
        let mut w = Welford::new();
        // 43 200 s × 1 400 pacotes/s seria caro no teste; 200 000 amostras já
        // estouram a precisão da fórmula ingênua com esta magnitude.
        for i in 0..200_000u32 {
            w.push(1_000_000.0 + f64::from(i % 3));
        }
        let mean = w.mean().expect("média");
        assert!((mean - 1_000_001.0).abs() < 1e-3, "{mean}");
        let sd = w.sd().expect("sd");
        assert!((sd - 0.8165).abs() < 1e-3, "{sd}");
    }

    /// SPEC-PROBE-IP-026 — o p99 fica dentro de um bucket do valor exato, com
    /// memória constante.
    #[test]
    fn spec_probe_ip_026_percentiles_stay_within_one_bucket() {
        let mut h = LogHistogram::new();
        assert_eq!(h.percentile(0.99), None);

        // 985 amostras em 700 µs e 15 em 5 ms: a 990ª amostra ordenada — o p99
        // de 1000 — cai na cauda de 5 ms.
        for _ in 0..985 {
            h.push(700.0);
        }
        for _ in 0..15 {
            h.push(5_000.0);
        }
        let p50 = h.percentile(0.50).expect("p50");
        let p99 = h.percentile(0.99).expect("p99");

        let bucket = |v: f64| LogHistogram::bucket_of(v);
        assert!(
            bucket(p50).abs_diff(bucket(700.0)) <= 1,
            "p50 = {p50} µs deveria cair no bucket de 700 µs"
        );
        assert!(
            bucket(p99).abs_diff(bucket(5_000.0)) <= 1,
            "p99 = {p99} µs deveria cair no bucket de 5 ms"
        );
        assert_eq!(h.count(), 1_000);
        assert_eq!(h.buckets().len(), HIST_BUCKETS);
    }

    /// SPEC-PROBE-IP-026 — as caudas caem nos buckets extremos em vez de
    /// estourar o índice.
    #[test]
    fn spec_probe_ip_026_histogram_clamps_both_tails() {
        assert_eq!(LogHistogram::bucket_of(0.0), 0);
        assert_eq!(LogHistogram::bucket_of(1.0), 0);
        assert_eq!(LogHistogram::bucket_of(f64::NAN), 0);
        assert_eq!(LogHistogram::bucket_of(1e9), HIST_BUCKETS - 1);
        assert!(LogHistogram::bucket_of(700.0) > 0);
        assert!(LogHistogram::bucket_of(700.0) < HIST_BUCKETS - 1);
        // Monotônico: valores maiores nunca caem num bucket menor.
        let mut prev = 0;
        for us in [10.0, 50.0, 200.0, 700.0, 2_000.0, 20_000.0, 90_000.0] {
            let b = LogHistogram::bucket_of(us);
            assert!(b >= prev, "{us} µs caiu no bucket {b}, antes de {prev}");
            prev = b;
        }
    }

    /// SPEC-PROBE-IP-027 — o cálculo do §5.4.1 reproduz a medição de
    /// referência: 15,0024 Mbps com 7 TS/datagrama ⇒ 701,9 µs.
    #[test]
    fn spec_probe_ip_027_expected_interarrival_matches_cbr() {
        let payload = 7.0 * 188.0;
        let expected = expected_iat_us(payload, 15_002_400.0).expect("CBR conhecido");
        assert!(
            (expected - 701.87).abs() / 701.87 < 0.01,
            "esperado {expected:.2} µs, referência 701,87 µs"
        );

        // Sem bitrate não há esperado — e "sem dado" nunca é zero.
        assert_eq!(expected_iat_us(payload, 0.0), None);
        assert_eq!(expected_iat_us(0.0, 15e6), None);
    }

    /// SPEC-PROBE-IP-027 — um CBR bem comportado tem burstiness ≈ 0; uma
    /// rajada aparece na cauda.
    #[test]
    fn spec_probe_ip_027_burstiness_reacts_to_the_tail() {
        let expected = 701.9;
        assert!(burstiness(702.0, expected).expect("cbr") < 0.01);
        let bursty = burstiness(5_000.0, expected).expect("rajada");
        assert!(bursty > 6.0, "{bursty}");
        // Chegar mais rápido do que o esperado não é rajada negativa.
        assert_eq!(burstiness(100.0, expected), Some(0.0));
        assert_eq!(burstiness(700.0, 0.0), None);
    }

    /// SPEC-PROBE-IP-025 — a janela mede o intervalo entre datagramas e o
    /// primeiro de cada janela só ancora o relógio.
    #[test]
    fn spec_probe_ip_025_iat_window_measures_gaps_not_arrivals() {
        let t0 = Instant::now();
        let mut w = IatWindow::new();
        for i in 0..4u32 {
            w.observe(t0 + Duration::from_micros(u64::from(i) * 700));
        }
        let s = w.summary();
        assert_eq!(s.count, 3, "4 chegadas produzem 3 intervalos");
        assert!((s.avg_us.expect("média") - 700.0).abs() < 1.0);
        assert!(s.sd_us.expect("sd") < 1.0);

        // Janela sem datagrama: tudo `None`, nada de zero.
        w.roll();
        let empty = w.summary();
        assert_eq!(empty.count, 0);
        assert_eq!(empty.avg_us, None);
        assert_eq!(empty.p99_us, None);
    }

    /// SPEC-PROBE-IP-028 — timestamp constante torna o jitter `n/a` em vez de
    /// produzir um alarme que só existe no artefato da medição.
    #[test]
    fn spec_probe_ip_028_constant_timestamp_marks_jitter_not_applicable() {
        let t0 = Instant::now();
        let mut j = Rfc3550Jitter::new();
        for i in 0..10u64 {
            j.observe(0, t0 + Duration::from_micros(i * 700));
        }
        assert!(!j.usable());
        assert_eq!(j.jitter_us(), None);
    }

    /// SPEC-PROBE-IP-028 — com timestamp utilizável, um fluxo perfeitamente
    /// espaçado tem jitter ≈ 0 e um fluxo irregular tem jitter > 0.
    #[test]
    fn spec_probe_ip_028_jitter_follows_rfc3550() {
        let t0 = Instant::now();
        let step_us = 700u64;
        let step_ticks = (step_us as f64 * RTP_CLOCK_HZ / 1e6) as u32;

        let mut steady = Rfc3550Jitter::new();
        for i in 0..200u64 {
            steady.observe(
                (i as u32).wrapping_mul(step_ticks),
                t0 + Duration::from_micros(i * step_us),
            );
        }
        assert!(steady.usable());
        let quiet = steady.jitter_us().expect("jitter");
        assert!(quiet < 20.0, "fluxo regular não deveria ter jitter: {quiet}");

        let mut jumpy = Rfc3550Jitter::new();
        for i in 0..200u64 {
            // Metade dos pacotes chega 3 ms atrasada em relação ao seu
            // timestamp — é isso que o jitter de rede mede.
            let skew = if i % 2 == 0 { 0 } else { 3_000 };
            jumpy.observe(
                (i as u32).wrapping_mul(step_ticks),
                t0 + Duration::from_micros(i * step_us + skew),
            );
        }
        let noisy = jumpy.jitter_us().expect("jitter");
        assert!(noisy > quiet * 10.0, "quieto {quiet} · ruidoso {noisy}");
    }

    /// SPEC-PROBE-IP-005 · SPEC-PROBE-IP-006 — a probe mede o próprio ruído e
    /// levanta o limiar por ele, para que um notebook carregado não vire um
    /// falso positivo de jitter.
    #[test]
    fn spec_probe_ip_006_alarm_threshold_floors_at_k_times_noise() {
        let t0 = Instant::now();
        let mut cal = NoiseCalibration::new(Duration::from_secs(30));
        assert_eq!(cal.floor_us(), None);
        // Sem calibração fechada, o limiar do perfil vale sozinho.
        assert_eq!(cal.effective_threshold(5_000.0, 3.0), 5_000.0);

        for i in 0..1_000u64 {
            let noise = if i % 2 == 0 { 700.0 } else { 4_700.0 };
            cal.observe(noise, t0 + Duration::from_millis(i * 40));
        }
        let floor = cal.floor_us().expect("calibração fecha em 30 s");
        assert!(floor > 1_000.0, "SD do ruído sintético: {floor}");
        assert_eq!(cal.effective_threshold(5_000.0, 3.0), 3.0 * floor);
        // Ruído baixo não derruba o limiar do perfil.
        assert_eq!(cal.effective_threshold(1e9, 3.0), 1e9);
    }
}
