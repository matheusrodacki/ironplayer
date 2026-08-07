//! Séries temporais com rollup e linha do tempo de saúde.
//!
//! SPEC-PROBE-005 · SPEC-PROBE-009 · SPEC-PROBE-010

use chrono::{DateTime, Utc};

use crate::severity::{Severity, RGB_NO_DATA, RGB_OK};

/// Métricas com série temporal mantida em memória.
///
/// SPEC-PROBE-010 lista bitrate total, PDV/inter-arrival, perda RTP/s,
/// CC errors/s e CRC errors/s.  PDV/inter-arrival pertencem à camada IP
/// (spec-14) e entram nesta mesma estrutura quando aquela camada existir.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MetricId {
    /// Bitrate total do multiplex, em kbps.
    BitrateKbps,
    /// Continuity counter errors por segundo.
    CcErrorsPerS,
    /// Erros de CRC por segundo.
    CrcErrorsPerS,
    /// Pacotes RTP perdidos/fora de ordem por segundo.
    RtpLossPerS,
    /// Descartes locais por segundo (SPEC-PROBE-013).
    LocalDropsPerS,
    /// Jitter de agendamento do tick, em ms (SPEC-PROBE-013).
    SchedJitterMs,
}

impl MetricId {
    /// Todas as métricas, na ordem em que aparecem nos gráficos do detalhe.
    ///
    /// SPEC-PROBE-010
    pub const ALL: [MetricId; 6] = [
        MetricId::BitrateKbps,
        MetricId::CcErrorsPerS,
        MetricId::CrcErrorsPerS,
        MetricId::RtpLossPerS,
        MetricId::LocalDropsPerS,
        MetricId::SchedJitterMs,
    ];

    /// Rótulo exibido no card do gráfico.
    pub fn label(self) -> &'static str {
        match self {
            Self::BitrateKbps => "BITRATE",
            Self::CcErrorsPerS => "CC ERRORS/S",
            Self::CrcErrorsPerS => "CRC ERRORS/S",
            Self::RtpLossPerS => "PERDA RTP/S",
            Self::LocalDropsPerS => "DESCARTE LOCAL/S",
            Self::SchedJitterMs => "JITTER DE TICK",
        }
    }

    /// Unidade exibida no card.
    pub fn unit(self) -> &'static str {
        match self {
            Self::BitrateKbps => "kbps",
            Self::SchedJitterMs => "ms",
            _ => "/s",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::BitrateKbps => 0,
            Self::CcErrorsPerS => 1,
            Self::CrcErrorsPerS => 2,
            Self::RtpLossPerS => 3,
            Self::LocalDropsPerS => 4,
            Self::SchedJitterMs => 5,
        }
    }
}

/// Um bucket de rollup: min/avg/max/count da janela.
///
/// SPEC-PROBE-005
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RollupBucket {
    /// Início do bucket (relógio de parede; só para exportação).
    pub start_utc: DateTime<Utc>,
    pub min: f64,
    pub max: f64,
    sum: f64,
    pub count: u32,
}

impl RollupBucket {
    fn new(start_utc: DateTime<Utc>, value: f64) -> Self {
        Self {
            start_utc,
            min: value,
            max: value,
            sum: value,
            count: 1,
        }
    }

    fn push(&mut self, value: f64) {
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.sum += value;
        self.count = self.count.saturating_add(1);
    }

    /// Média da janela.
    ///
    /// SPEC-PROBE-005
    pub fn avg(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum / self.count as f64
        }
    }
}

/// Célula da linha do tempo de saúde.
///
/// SPEC-PROBE-009 — "célula sem dado é cinza (distinta de verde)".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimelineBucket {
    pub start_utc: DateTime<Utc>,
    /// Pior severidade observada no bucket; `None` = nenhum check aberto.
    pub worst: Option<Severity>,
    /// Amostras contribuintes; 0 significa "sem dado".
    pub samples: u32,
    /// Amostras em que o feed estava conectado.
    pub connected_samples: u32,
}

impl TimelineBucket {
    /// Cor da célula: cinza sem dado, verde sem severidade, cor da severidade.
    ///
    /// SPEC-PROBE-009
    pub fn rgb(&self) -> u32 {
        if self.samples == 0 {
            RGB_NO_DATA
        } else {
            self.worst.map_or(RGB_OK, Severity::rgb)
        }
    }

    /// Disponibilidade do bucket (0.0–1.0); `None` quando não há dado.
    ///
    /// SPEC-PROBE-018
    pub fn availability(&self) -> Option<f64> {
        (self.samples > 0).then(|| self.connected_samples as f64 / self.samples as f64)
    }
}

/// Série reduzida pronta para desenho.
///
/// SPEC-PROBE-010 — "nunca desenha mais de 1000 pontos".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeriesPoints {
    /// Valores médios por bucket, do mais antigo ao mais recente.
    pub values: Vec<f64>,
    pub min: f64,
    pub max: f64,
    /// Último valor da série (o "big value" do card).
    pub last: f64,
    /// Largura efetiva de cada ponto, em segundos.
    pub bucket_secs: u64,
}

/// Janela de visualização de um gráfico.
///
/// SPEC-PROBE-010
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesWindow {
    FiveMinutes,
    OneHour,
    TwelveHours,
    WholeSession,
}

impl SeriesWindow {
    /// Duração da janela em segundos; `None` = sessão inteira.
    pub fn secs(self) -> Option<u64> {
        match self {
            Self::FiveMinutes => Some(300),
            Self::OneHour => Some(3_600),
            Self::TwelveHours => Some(43_200),
            Self::WholeSession => None,
        }
    }

    /// Rótulo do seletor de janela.
    pub fn label(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5 min",
            Self::OneHour => "1 h",
            Self::TwelveHours => "12 h",
            Self::WholeSession => "sessão",
        }
    }

    /// Todas as janelas, na ordem do seletor.
    pub const ALL: [SeriesWindow; 4] = [
        SeriesWindow::FiveMinutes,
        SeriesWindow::OneHour,
        SeriesWindow::TwelveHours,
        SeriesWindow::WholeSession,
    ];
}

/// Teto de pontos desenhados por gráfico.
///
/// SPEC-PROBE-010 — o femtovg já é o gargalo de render (L-007 do STATE.md);
/// 43 200 pontos por gráfico travariam a janela.
pub const MAX_PLOT_POINTS: usize = 1000;

/// Séries e linha do tempo de um feed.
///
/// SPEC-PROBE-005 · SPEC-PROBE-009
#[derive(Debug)]
pub struct SeriesStore {
    rollup_secs: u64,
    timeline_secs: u64,
    /// Um vetor de buckets por métrica, indexado por [`MetricId::index`].
    rollups: [Vec<RollupBucket>; 6],
    /// Índice do bucket corrente por métrica (`rollups[i].len() - 1`).
    open_bucket_start: [Option<i64>; 6],
    timeline: Vec<TimelineBucket>,
    timeline_open_start: Option<i64>,
    /// Total de amostras 1 Hz ingeridas (para o resumo da sessão).
    total_samples: u64,
    connected_samples: u64,
}

impl SeriesStore {
    /// Cria o armazenamento com as janelas configuradas.
    ///
    /// SPEC-PROBE-005 · SPEC-PROBE-009
    pub fn new(rollup_secs: u64, timeline_secs: u64) -> Self {
        Self {
            rollup_secs: rollup_secs.max(1),
            timeline_secs: timeline_secs.max(1),
            rollups: Default::default(),
            open_bucket_start: [None; 6],
            timeline: Vec::new(),
            timeline_open_start: None,
            total_samples: 0,
            connected_samples: 0,
        }
    }

    /// Alinha um instante ao início do bucket de largura `width`.
    fn bucket_start(ts: DateTime<Utc>, width: u64) -> i64 {
        let secs = ts.timestamp();
        secs - secs.rem_euclid(width as i64)
    }

    /// Ingere uma leitura de métrica.
    ///
    /// SPEC-PROBE-005
    pub fn push_metric(&mut self, metric: MetricId, ts: DateTime<Utc>, value: f64) {
        if !value.is_finite() {
            return; // RNF-PRB-003: NaN de divisão por zero não polui a série.
        }
        let i = metric.index();
        let start = Self::bucket_start(ts, self.rollup_secs);
        let start_utc = DateTime::from_timestamp(start, 0).unwrap_or(ts);

        if self.open_bucket_start[i] == Some(start) {
            if let Some(b) = self.rollups[i].last_mut() {
                b.push(value);
                return;
            }
        }
        self.rollups[i].push(RollupBucket::new(start_utc, value));
        self.open_bucket_start[i] = Some(start);
    }

    /// Ingere o veredito de saúde de uma amostra na linha do tempo.
    ///
    /// SPEC-PROBE-009
    pub fn push_health(&mut self, ts: DateTime<Utc>, worst: Option<Severity>, connected: bool) {
        self.total_samples += 1;
        if connected {
            self.connected_samples += 1;
        }

        let start = Self::bucket_start(ts, self.timeline_secs);
        let start_utc = DateTime::from_timestamp(start, 0).unwrap_or(ts);

        if self.timeline_open_start != Some(start) {
            self.timeline.push(TimelineBucket {
                start_utc,
                worst: None,
                samples: 0,
                connected_samples: 0,
            });
            self.timeline_open_start = Some(start);
        }
        if let Some(b) = self.timeline.last_mut() {
            b.samples = b.samples.saturating_add(1);
            if connected {
                b.connected_samples = b.connected_samples.saturating_add(1);
            }
            if let Some(sev) = worst {
                b.worst = Some(b.worst.map_or(sev, |cur| cur.max(sev)));
            }
        }
    }

    /// Linha do tempo completa, do mais antigo ao mais recente.
    ///
    /// SPEC-PROBE-009
    pub fn timeline(&self) -> &[TimelineBucket] {
        &self.timeline
    }

    /// Últimas `n` células da linha do tempo (144 células = 12 h a 5 min).
    ///
    /// SPEC-PROBE-009
    pub fn timeline_tail(&self, n: usize) -> &[TimelineBucket] {
        let from = self.timeline.len().saturating_sub(n);
        &self.timeline[from..]
    }

    /// Buckets de rollup de uma métrica.
    ///
    /// SPEC-PROBE-005
    pub fn rollups(&self, metric: MetricId) -> &[RollupBucket] {
        &self.rollups[metric.index()]
    }

    /// Disponibilidade da sessão inteira (0.0–1.0).
    ///
    /// SPEC-PROBE-018
    pub fn availability(&self) -> f64 {
        if self.total_samples == 0 {
            0.0
        } else {
            self.connected_samples as f64 / self.total_samples as f64
        }
    }

    /// Disponibilidade da janela corrente, em segundos.
    ///
    /// SPEC-PROBE-018 — o tile mostra os últimos 60 min por default.
    pub fn availability_window(&self, secs: u64) -> Option<f64> {
        let n = secs.div_ceil(self.timeline_secs).max(1) as usize;
        let tail = self.timeline_tail(n);
        let samples: u32 = tail.iter().map(|b| b.samples).sum();
        let connected: u32 = tail.iter().map(|b| b.connected_samples).sum();
        (samples > 0).then(|| connected as f64 / samples as f64)
    }

    /// Total de amostras 1 Hz ingeridas.
    pub fn total_samples(&self) -> u64 {
        self.total_samples
    }

    /// Reduz uma métrica para desenho, respeitando [`MAX_PLOT_POINTS`].
    ///
    /// A UI desenha **buckets do rollup**, nunca a série 1 Hz crua (§8.2).
    /// Quando a janela ainda assim excede o teto, buckets vizinhos são
    /// agregados pela média — e o `max` da janela é preservado à parte, para
    /// que um pico não desapareça na redução.
    ///
    /// SPEC-PROBE-010
    pub fn points(&self, metric: MetricId, window: SeriesWindow) -> SeriesPoints {
        let buckets = self.rollups(metric);
        if buckets.is_empty() {
            return SeriesPoints {
                bucket_secs: self.rollup_secs,
                ..Default::default()
            };
        }

        let slice = match window.secs() {
            None => buckets,
            Some(secs) => {
                let n = (secs.div_ceil(self.rollup_secs).max(1) as usize).min(buckets.len());
                &buckets[buckets.len() - n..]
            }
        };

        let group = slice.len().div_ceil(MAX_PLOT_POINTS).max(1);
        let mut values = Vec::with_capacity(slice.len().div_ceil(group));
        for chunk in slice.chunks(group) {
            let total: f64 = chunk.iter().map(|b| b.avg()).sum();
            values.push(total / chunk.len() as f64);
        }

        let min = slice.iter().map(|b| b.min).fold(f64::INFINITY, f64::min);
        let max = slice
            .iter()
            .map(|b| b.max)
            .fold(f64::NEG_INFINITY, f64::max);

        SeriesPoints {
            last: slice.last().map_or(0.0, RollupBucket::avg),
            values,
            min: if min.is_finite() { min } else { 0.0 },
            max: if max.is_finite() { max } else { 0.0 },
            bucket_secs: self.rollup_secs * group as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("timestamp")
    }

    fn store() -> SeriesStore {
        SeriesStore::new(60, 300)
    }

    /// SPEC-PROBE-005 — 12 h a 1 Hz produzem 720 buckets de 60 s.
    #[test]
    fn spec_probe_005_twelve_hours_yields_720_rollup_buckets() {
        let mut s = store();
        for i in 0..43_200i64 {
            s.push_metric(MetricId::BitrateKbps, ts(i), 15_000.0);
        }
        assert_eq!(s.rollups(MetricId::BitrateKbps).len(), 720);
    }

    /// SPEC-PROBE-005 — o consumo do rollup fica muito abaixo de 2 MB.
    #[test]
    fn spec_probe_005_rollup_memory_stays_under_2mb() {
        let per_bucket = std::mem::size_of::<RollupBucket>();
        let total = per_bucket * 720 * MetricId::ALL.len();
        assert!(
            total < 2 * 1024 * 1024,
            "rollup de 12 h ocuparia {total} bytes"
        );
    }

    /// SPEC-PROBE-005 — o bucket guarda min/avg/max/count.
    #[test]
    fn spec_probe_005_bucket_tracks_min_avg_max_count() {
        let mut s = store();
        for (i, v) in [10.0, 30.0, 20.0].into_iter().enumerate() {
            s.push_metric(MetricId::BitrateKbps, ts(i as i64), v);
        }
        let b = s.rollups(MetricId::BitrateKbps)[0];
        assert_eq!(b.min, 10.0);
        assert_eq!(b.max, 30.0);
        assert_eq!(b.count, 3);
        assert!((b.avg() - 20.0).abs() < 1e-9);
    }

    /// SPEC-PROBE-009 — 12 h com bucket de 5 min produzem 144 células.
    #[test]
    fn spec_probe_009_twelve_hours_yields_144_timeline_cells() {
        let mut s = store();
        for i in 0..43_200i64 {
            s.push_health(ts(i), None, true);
        }
        assert_eq!(s.timeline().len(), 144);
        assert_eq!(s.timeline_tail(144).len(), 144);
    }

    /// SPEC-PROBE-009 — a célula pega a **pior** severidade do bucket.
    #[test]
    fn spec_probe_009_cell_takes_worst_severity_of_bucket() {
        let mut s = store();
        s.push_health(ts(0), Some(Severity::Warning), true);
        s.push_health(ts(1), Some(Severity::Critical), true);
        s.push_health(ts(2), Some(Severity::Info), true);
        let cell = s.timeline()[0];
        assert_eq!(cell.worst, Some(Severity::Critical));
        assert_eq!(cell.rgb(), Severity::Critical.rgb());
    }

    /// SPEC-PROBE-009 — célula sem dado é cinza, distinta de verde.
    #[test]
    fn spec_probe_009_empty_cell_is_grey_not_green() {
        let empty = TimelineBucket {
            start_utc: ts(0),
            worst: None,
            samples: 0,
            connected_samples: 0,
        };
        assert_eq!(empty.rgb(), RGB_NO_DATA);

        let healthy = TimelineBucket {
            samples: 300,
            connected_samples: 300,
            ..empty
        };
        assert_eq!(healthy.rgb(), RGB_OK);
        assert_ne!(empty.rgb(), healthy.rgb());
    }

    /// SPEC-PROBE-010 — nenhuma janela desenha mais de 1000 pontos.
    #[test]
    fn spec_probe_010_never_draws_more_than_1000_points() {
        let mut s = SeriesStore::new(1, 300); // rollup de 1 s: pior caso
        for i in 0..43_200i64 {
            s.push_metric(MetricId::BitrateKbps, ts(i), i as f64);
        }
        for window in SeriesWindow::ALL {
            let p = s.points(MetricId::BitrateKbps, window);
            assert!(
                p.values.len() <= MAX_PLOT_POINTS,
                "{:?} devolveu {} pontos",
                window,
                p.values.len()
            );
        }
    }

    /// SPEC-PROBE-010 — cada janela cobre o intervalo pedido.
    #[test]
    fn spec_probe_010_window_selects_expected_span() {
        let mut s = store(); // rollup 60 s
        for i in 0..43_200i64 {
            s.push_metric(MetricId::BitrateKbps, ts(i), 1.0);
        }
        assert_eq!(
            s.points(MetricId::BitrateKbps, SeriesWindow::FiveMinutes)
                .values
                .len(),
            5
        );
        assert_eq!(
            s.points(MetricId::BitrateKbps, SeriesWindow::OneHour)
                .values
                .len(),
            60
        );
        assert_eq!(
            s.points(MetricId::BitrateKbps, SeriesWindow::WholeSession)
                .values
                .len(),
            720
        );
    }

    /// SPEC-PROBE-010 — série vazia não estoura nem devolve NaN.
    #[test]
    fn spec_probe_010_empty_series_is_safe() {
        let s = store();
        let p = s.points(MetricId::CcErrorsPerS, SeriesWindow::OneHour);
        assert!(p.values.is_empty());
        assert_eq!(p.min, 0.0);
        assert_eq!(p.max, 0.0);
    }

    /// RNF-PRB-003 — NaN vindo de divisão por zero não entra na série.
    #[test]
    fn rnf_prb_003_non_finite_values_are_rejected() {
        let mut s = store();
        s.push_metric(MetricId::BitrateKbps, ts(0), f64::NAN);
        s.push_metric(MetricId::BitrateKbps, ts(0), f64::INFINITY);
        assert!(s.rollups(MetricId::BitrateKbps).is_empty());
    }

    /// SPEC-PROBE-018 — disponibilidade da janela reflete os buckets recentes.
    #[test]
    fn spec_probe_018_availability_window_uses_recent_buckets() {
        let mut s = store();
        // 30 min conectado, 30 min desconectado.
        for i in 0..1_800i64 {
            s.push_health(ts(i), None, true);
        }
        for i in 1_800..3_600i64 {
            s.push_health(ts(i), Some(Severity::Critical), false);
        }
        let av = s.availability_window(3_600).expect("há dados");
        assert!((av - 0.5).abs() < 0.01, "disponibilidade foi {av}");
        assert!((s.availability() - 0.5).abs() < 0.01);
    }
}
