//! Ponte entre `probe::ProbeSnapshot` e os modelos Slint do modo Probe.
//!
//! A UI nunca alcança as estruturas internas do `ProbeEngine`: lê um snapshot
//! imutável publicado a 1 Hz e o traduz para os `struct`s do `probe.slint`.
//! A repintura fica em ≤ 1 Hz (§8.2) — o `Poller` chama [`ProbeView::refresh`]
//! só quando o snapshot muda de geração.
//!
//! SPEC-PROBE-009 · SPEC-PROBE-010 · SPEC-PROBE-013 · SPEC-PROBE-018 ·
//! SPEC-PROBE-019

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, RwLock};

use slint::{Color, ModelRc, SharedString, VecModel};

use probe::{
    DegradationStage, EventRow, FeedSnapshot, MetricId, ProbeSnapshot, SeriesPoints, SeriesWindow,
    Severity,
};

use crate::state::{AppCommand, SharedThumbnails};
use crate::{line_path, ProbeChart, ProbeEventRow, ProbeInfoRow, ProbeTile, ProbeTimelineCell};

/// Estado compartilhado do modo Probe, publicado pelo backend a 1 Hz.
pub type SharedProbe = Arc<RwLock<ProbeSnapshot>>;

/// Converte um RGB 0xRRGGBB do crate `probe` numa `slint::Color`.
///
/// As cores de severidade nascem em `probe::severity` porque o relatório HTML
/// (que não conhece Slint) usa exatamente as mesmas — uma única fonte de
/// verdade para "o que é amarelo" evita a UI e o relatório discordarem.
fn rgb(value: u32) -> Color {
    Color::from_rgb_u8(
        ((value >> 16) & 0xFF) as u8,
        ((value >> 8) & 0xFF) as u8,
        (value & 0xFF) as u8,
    )
}

/// Formata uma disponibilidade como percentual com uma casa (§8.1).
fn pct(frac: Option<f64>) -> String {
    match frac {
        Some(v) => format!("{:.1} %", v * 100.0).replace('.', ","),
        None => "—".to_string(),
    }
}

/// Formata um bitrate em Mbps.
fn mbps(kbps: f64) -> String {
    format!("{:.2} Mbps", kbps / 1000.0).replace('.', ",")
}

/// Filtro de severidade do event log (§8.2).
///
/// Ciclado por clique: todas → warning+ → error+ → critical → todas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SeverityFilter {
    #[default]
    All,
    WarningUp,
    ErrorUp,
    CriticalOnly,
}

impl SeverityFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "todas",
            Self::WarningUp => "warning +",
            Self::ErrorUp => "error +",
            Self::CriticalOnly => "critical",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::All => Self::WarningUp,
            Self::WarningUp => Self::ErrorUp,
            Self::ErrorUp => Self::CriticalOnly,
            Self::CriticalOnly => Self::All,
        }
    }

    fn accepts(self, sev: Severity) -> bool {
        match self {
            Self::All => true,
            Self::WarningUp => sev >= Severity::Warning,
            Self::ErrorUp => sev >= Severity::Error,
            Self::CriticalOnly => sev == Severity::Critical,
        }
    }
}

/// Modelos e estado local da tela Probe.
pub(crate) struct ProbeView {
    probe_rx: SharedProbe,
    thumbnails: SharedThumbnails,
    cmd_tx: crossbeam_channel::Sender<AppCommand>,

    tiles: Rc<VecModel<ProbeTile>>,
    timeline: Rc<VecModel<ProbeTimelineCell>>,
    charts: Rc<VecModel<ProbeChart>>,
    events: Rc<VecModel<ProbeEventRow>>,
    summary: Rc<VecModel<ProbeInfoRow>>,
    health: Rc<VecModel<ProbeInfoRow>>,

    /// Slot aberto no detalhe; `None` = mosaico.
    detail_slot: Option<usize>,
    window: SeriesWindow,
    /// Célula da linha do tempo selecionada (filtra o log, SPEC-PROBE-009).
    timeline_selected: Option<usize>,
    severity_filter: SeverityFilter,

    /// Geração do thumbnail já convertida para `slint::Image`, por slot —
    /// evita reconstruir a imagem a cada tick de UI.
    thumb_generation: HashMap<usize, u64>,
    thumb_cache: HashMap<usize, slint::Image>,
}

impl ProbeView {
    /// Cria os modelos e os instala na janela.
    pub(crate) fn new(
        window: &crate::AppWindow,
        probe_rx: SharedProbe,
        thumbnails: SharedThumbnails,
        cmd_tx: crossbeam_channel::Sender<AppCommand>,
    ) -> Self {
        let tiles: Rc<VecModel<ProbeTile>> = Rc::new(VecModel::default());
        let timeline: Rc<VecModel<ProbeTimelineCell>> = Rc::new(VecModel::default());
        let charts: Rc<VecModel<ProbeChart>> = Rc::new(VecModel::default());
        let events: Rc<VecModel<ProbeEventRow>> = Rc::new(VecModel::default());
        let summary: Rc<VecModel<ProbeInfoRow>> = Rc::new(VecModel::default());
        let health: Rc<VecModel<ProbeInfoRow>> = Rc::new(VecModel::default());

        window.set_probe_tiles(ModelRc::from(tiles.clone()));
        window.set_probe_timeline(ModelRc::from(timeline.clone()));
        window.set_probe_charts(ModelRc::from(charts.clone()));
        window.set_probe_events(ModelRc::from(events.clone()));
        window.set_probe_summary(ModelRc::from(summary.clone()));
        window.set_probe_health(ModelRc::from(health.clone()));

        Self {
            probe_rx,
            thumbnails,
            cmd_tx,
            tiles,
            timeline,
            charts,
            events,
            summary,
            health,
            detail_slot: None,
            window: SeriesWindow::OneHour,
            timeline_selected: None,
            severity_filter: SeverityFilter::All,
            thumb_generation: HashMap::new(),
            thumb_cache: HashMap::new(),
        }
    }

    /// Abre (ou fecha, com `slot < 0`) o detalhe de um feed.
    ///
    /// SPEC-PROBE-019 — alternar entre detalhes não pode parar a coleta de
    /// nenhum feed; por isso aqui só muda estado de UI, nunca o pipeline.
    pub(crate) fn open_detail(&mut self, slot: i32) {
        self.detail_slot = (slot >= 0).then_some(slot as usize);
        self.timeline_selected = None;
    }

    /// Slot aberto no detalhe, para a propriedade do Slint.
    pub(crate) fn detail_slot_index(&self) -> i32 {
        self.detail_slot.map_or(-1, |s| s as i32)
    }

    /// Seleciona a janela dos gráficos (SPEC-PROBE-010).
    pub(crate) fn select_window(&mut self, index: i32) {
        let idx = index.max(0) as usize;
        self.window = SeriesWindow::ALL
            .get(idx)
            .copied()
            .unwrap_or(SeriesWindow::OneHour);
        // A redução de pontos acontece no engine, não na UI (§8.2), então a
        // janela precisa chegar até lá para o próximo snapshot já vir certo.
        self.send(AppCommand::SetProbeWindow { index: idx });
    }

    /// Índice da janela ativa.
    pub(crate) fn window_index(&self) -> i32 {
        SeriesWindow::ALL
            .iter()
            .position(|w| *w == self.window)
            .unwrap_or(1) as i32
    }

    /// Seleciona/desseleciona uma célula da linha do tempo.
    ///
    /// SPEC-PROBE-009 — "clique na célula filtra o event log daquele intervalo".
    pub(crate) fn select_cell(&mut self, index: i32) {
        let idx = (index >= 0).then_some(index as usize);
        self.timeline_selected = if self.timeline_selected == idx {
            None
        } else {
            idx
        };
    }

    /// Avança o filtro de severidade do event log.
    pub(crate) fn cycle_severity(&mut self) {
        self.severity_filter = self.severity_filter.next();
    }

    /// Envia um comando de run ao backend.
    pub(crate) fn send(&self, cmd: AppCommand) {
        if self.cmd_tx.try_send(cmd).is_err() {
            tracing::warn!("probe: canal de comandos cheio — pedido descartado");
        }
    }

    /// Reaplica todo o estado da tela Probe na janela.
    pub(crate) fn refresh(&mut self, win: &crate::AppWindow) {
        let snapshot = match self.probe_rx.read() {
            Ok(guard) => guard.clone(),
            Err(_) => return,
        };

        win.set_probe_run_clock(SharedString::from(snapshot.run_clock()));
        win.set_probe_run_id(SharedString::from(snapshot.run_id.as_str()));
        win.set_probe_recording(snapshot.recording);
        win.set_probe_status(SharedString::from(self.status_line(&snapshot)));

        // Detalhe apontando para um slot que sumiu (feed removido do TOML e
        // run reaberto): volta ao mosaico em vez de mostrar um tile vazio.
        if self.detail_slot.is_some_and(|s| snapshot.feed(s).is_none()) {
            self.detail_slot = None;
        }
        win.set_probe_detail_slot(self.detail_slot_index());

        let tiles: Vec<ProbeTile> = snapshot.feeds.iter().map(|f| self.build_tile(f)).collect();
        self.tiles.set_vec(tiles);

        match self.detail_slot.and_then(|s| snapshot.feed(s).cloned()) {
            Some(feed) => self.refresh_detail(win, &feed),
            None => {
                self.timeline.set_vec(Vec::new());
                self.charts.set_vec(Vec::new());
                self.events.set_vec(Vec::new());
                self.summary.set_vec(Vec::new());
                self.health.set_vec(Vec::new());
            }
        }
    }

    fn status_line(&self, snapshot: &ProbeSnapshot) -> String {
        if snapshot.feeds.is_empty() {
            return String::new();
        }
        let degraded = snapshot
            .feeds
            .iter()
            .map(|f| f.health.degradation)
            .max()
            .unwrap_or(DegradationStage::None);
        let open: usize = snapshot.feeds.iter().map(|f| f.open_events).sum();
        let drops: u64 = snapshot.feeds.iter().map(|f| f.health.local_drops).sum();

        let mut parts = vec![
            format!("{open} alarme(s)"),
            format!("{drops} descarte(s) local"),
        ];
        if degraded != DegradationStage::None {
            parts.push(format!("degradado: {}", degraded.label()));
        }
        parts.join(" · ")
    }

    fn build_tile(&mut self, feed: &FeedSnapshot) -> ProbeTile {
        let (thumbnail, has_thumbnail) = self.thumbnail_for(feed.slot);
        let health = |layer: probe::Layer| {
            rgb(feed
                .layer_health
                .get(&layer)
                .copied()
                .unwrap_or_default()
                .rgb())
        };

        ProbeTile {
            slot: feed.slot as i32,
            name: SharedString::from(feed.display_name()),
            url: SharedString::from(feed.url.as_str()),
            enc_badge: SharedString::from(feed.encapsulation.badge()),
            res_badge: SharedString::from(feed.resolution_badge().unwrap_or("")),
            scrambled: feed.scrambled,
            thumbnail,
            has_thumbnail,
            snapshot_state: SharedString::from(feed.snapshot_state.label()),
            connected: feed.connected,
            bitrate: SharedString::from(mbps(feed.bitrate_kbps)),
            availability: SharedString::from(pct(feed
                .availability_window
                .or(Some(feed.availability_session)))),
            availability_frac: feed
                .availability_window
                .unwrap_or(feed.availability_session) as f32,
            availability_color: rgb(match feed.availability_window {
                Some(v) if v < 0.99 => Severity::Error.rgb(),
                Some(v) if v < 0.999 => Severity::Warning.rgb(),
                _ => probe::severity::RGB_OK,
            }),
            ip_color: health(probe::Layer::Ip),
            rtp_color: health(probe::Layer::Rtp),
            ts_color: health(probe::Layer::Ts),
            v_color: health(probe::Layer::Video),
            a_color: health(probe::Layer::Audio),
            open_events: feed.open_events as i32,
            selected: self.detail_slot == Some(feed.slot),
        }
    }

    /// Converte o thumbnail publicado pela thread `probe-snapshot`.
    ///
    /// SPEC-PROBE-003 — só há uma imagem viva por feed; a conversão para
    /// `slint::Image` é feita uma vez por geração, não a cada tick de UI.
    fn thumbnail_for(&mut self, slot: usize) -> (slint::Image, bool) {
        let latest = match self.thumbnails.read() {
            Ok(map) => map
                .get(&slot)
                .map(|t| (t.generation, t.width, t.height, t.rgba.clone())),
            Err(_) => None,
        };

        let Some((generation, width, height, rgba)) = latest else {
            self.thumb_cache.remove(&slot);
            self.thumb_generation.remove(&slot);
            return (slint::Image::default(), false);
        };

        if self.thumb_generation.get(&slot) != Some(&generation) {
            let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
            let bytes = buf.make_mut_bytes();
            let n = bytes.len().min(rgba.len());
            bytes[..n].copy_from_slice(&rgba[..n]);
            self.thumb_cache.insert(slot, slint::Image::from_rgba8(buf));
            self.thumb_generation.insert(slot, generation);
        }

        match self.thumb_cache.get(&slot) {
            Some(img) => (img.clone(), true),
            None => (slint::Image::default(), false),
        }
    }

    fn refresh_detail(&mut self, win: &crate::AppWindow, feed: &FeedSnapshot) {
        let tile = self.build_tile(feed);
        win.set_probe_detail_tile(tile);

        // ── Linha do tempo (SPEC-PROBE-009) ─────────────────────────────
        let cells: Vec<ProbeTimelineCell> = feed
            .timeline
            .iter()
            .enumerate()
            .map(|(i, b)| ProbeTimelineCell {
                cell_color: rgb(b.rgb()),
                tip: SharedString::from(format!(
                    "{} · {}",
                    b.start_utc.format("%d/%m %H:%M"),
                    b.worst.map_or("sem alarme", Severity::label)
                )),
                index: i as i32,
            })
            .collect();
        let caption = match (feed.timeline.first(), feed.timeline.last()) {
            (Some(a), Some(b)) => format!(
                "{} células · {} → {}",
                cells.len(),
                a.start_utc.format("%d/%m %H:%M"),
                b.start_utc.format("%d/%m %H:%M")
            ),
            _ => "aguardando amostras".to_string(),
        };
        self.timeline.set_vec(cells);
        win.set_probe_timeline_caption(SharedString::from(caption));
        win.set_probe_timeline_selected(self.timeline_selected.map_or(-1, |i| i as i32));

        // ── Gráficos (SPEC-PROBE-010) ───────────────────────────────────
        let charts: Vec<ProbeChart> = MetricId::ALL
            .iter()
            .filter_map(|m| feed.series.get(m).map(|p| build_chart(*m, p)))
            .collect();
        self.charts.set_vec(charts);
        win.set_probe_window_index(self.window_index());

        // ── Event log (§8.2) ────────────────────────────────────────────
        let range = self.selected_range(feed);
        let rows: Vec<ProbeEventRow> = feed
            .events
            .iter()
            .filter(|e| self.severity_filter.accepts(e.severity))
            .filter(|e| range.is_none_or(|(from, to)| e.ts_utc >= from && e.ts_utc < to))
            .map(event_row)
            .collect();
        self.events.set_vec(rows);
        win.set_probe_severity_filter(SharedString::from(self.filter_label(range.is_some())));

        // ── Resumo e saúde da probe (SPEC-PROBE-013) ────────────────────
        let hh = feed.uptime_secs / 3600;
        let mm = (feed.uptime_secs % 3600) / 60;
        let ss = feed.uptime_secs % 60;
        self.summary.set_vec(vec![
            kv("uptime", format!("{hh:02}:{mm:02}:{ss:02}")),
            kv("disponibilidade (60 min)", pct(feed.availability_window)),
            kv(
                "disponibilidade (sessão)",
                pct(Some(feed.availability_session)),
            ),
            kv("bitrate", mbps(feed.bitrate_kbps)),
            kv("null ratio", format!("{:.2} %", feed.null_ratio * 100.0)),
            kv("vídeo", mbps(feed.video_kbps)),
            // O indicador `A` é presença e bitrate do PID de áudio, nunca
            // nível — o mosaico de referência mostra VU meter, que exigiria
            // decodificar áudio continuamente (§8.1).
            kv("áudio (presença)", mbps(feed.audio_kbps)),
            kv("alarmes abertos", feed.open_events.to_string()),
            kv(
                "pior evento",
                feed.worst_severity
                    .map_or("—".to_string(), |s| s.label().to_string()),
            ),
            kv(
                "indisponibilidade",
                format!(
                    "{}× · {} s",
                    feed.unavailable.periods, feed.unavailable.total_secs
                ),
            ),
        ]);

        self.health.set_vec(vec![
            kv("descartes locais", feed.health.local_drops.to_string()),
            kv(
                "descartes no último s",
                feed.health.local_drops_last.to_string(),
            ),
            kv(
                "jitter de tick",
                format!("{:.1} ms", feed.health.sched_jitter_ms),
            ),
            kv(
                "jitter de tick (pico)",
                format!("{:.1} ms", feed.health.sched_jitter_peak_ms),
            ),
            kv(
                "eventos descartados",
                feed.health.dropped_events.to_string(),
            ),
            kv(
                "writer: linhas perdidas",
                feed.health.writer_drops.to_string(),
            ),
            kv("degradação", feed.health.degradation.label().to_string()),
            kv("snapshot", feed.snapshot_state.label().to_string()),
        ]);
    }

    /// Intervalo da célula selecionada, se houver.
    ///
    /// SPEC-PROBE-009
    fn selected_range(
        &self,
        feed: &FeedSnapshot,
    ) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
        let idx = self.timeline_selected?;
        let cell = feed.timeline.get(idx)?;
        let next = feed
            .timeline
            .get(idx + 1)
            .map(|b| b.start_utc)
            // Último bucket ainda aberto: usa a largura do bucket anterior.
            .unwrap_or_else(|| {
                let width = feed
                    .timeline
                    .get(idx.wrapping_sub(1))
                    .map(|prev| cell.start_utc - prev.start_utc)
                    .unwrap_or_else(|| chrono::Duration::seconds(300));
                cell.start_utc + width
            });
        Some((cell.start_utc, next))
    }

    fn filter_label(&self, range_active: bool) -> String {
        if range_active {
            format!("{} · janela", self.severity_filter.label())
        } else {
            self.severity_filter.label().to_string()
        }
    }
}

fn kv(key: &str, value: String) -> ProbeInfoRow {
    ProbeInfoRow {
        key: SharedString::from(key),
        value: SharedString::from(value),
    }
}

fn event_row(e: &EventRow) -> ProbeEventRow {
    ProbeEventRow {
        time: SharedString::from(e.ts_utc.format("%H:%M:%S").to_string()),
        level: SharedString::from(e.severity.label()),
        level_color: rgb(e.severity.rgb()),
        check: SharedString::from(e.check_id.as_str()),
        count: SharedString::from(format!("×{}", e.count)),
        measured: SharedString::from(format!("{:.3} {}", e.measured, e.unit)),
        context: SharedString::from(e.context.as_str()),
    }
}

/// Constrói o card de um gráfico a partir dos pontos já reduzidos.
///
/// SPEC-PROBE-010 — os pontos vêm do rollup, nunca da série 1 Hz crua; aqui
/// só resta normalizar para o viewbox 0..100 do `Path`.
fn build_chart(metric: MetricId, points: &SeriesPoints) -> ProbeChart {
    let color = match metric {
        MetricId::BitrateKbps => probe::severity::RGB_OK,
        MetricId::SchedJitterMs => 0x5a_a0_d0,
        _ => Severity::Warning.rgb(),
    };

    let (line, area) = normalize(points);
    let big = match metric {
        MetricId::BitrateKbps => mbps(points.last),
        MetricId::SchedJitterMs => format!("{:.1}", points.last).replace('.', ","),
        _ => format!("{:.0}", points.last),
    };

    ProbeChart {
        title: SharedString::from(metric.label()),
        unit: SharedString::from(metric.unit()),
        big_value: SharedString::from(big),
        sub_value: SharedString::from(if points.values.is_empty() {
            "sem dados".to_string()
        } else {
            format!("máx {:.0} · {} s/pt", points.max, points.bucket_secs)
        }),
        line: SharedString::from(line),
        area: SharedString::from(area),
        line_color: rgb(color),
    }
}

/// Normaliza a série para o viewbox 0..100, com 8 % de headroom no topo.
fn normalize(points: &SeriesPoints) -> (String, String) {
    if points.values.len() < 2 {
        return (String::new(), String::new());
    }
    // Escala sempre a partir do zero: num gráfico de erros/s, ancorar o piso
    // no mínimo observado transformaria "3 e 4 erros" num degrau dramático.
    let top = points.max.max(1.0);
    let n = points.values.len();
    let pts: Vec<(f32, f32)> = points
        .values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = (i as f64 / (n - 1) as f64 * 100.0) as f32;
            let y = (100.0 - (v / top * 92.0).clamp(0.0, 92.0)) as f32;
            (x, y)
        })
        .collect();

    let line = line_path(&pts);
    let area = {
        let mut s = line.clone();
        if let (Some(first), Some(last)) = (pts.first(), pts.last()) {
            s.push_str(&format!(" L {:.2} 100 L {:.2} 100 Z", last.0, first.0));
        }
        s
    };
    (line, area)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC-PROBE-018 — a formatação de disponibilidade usa vírgula decimal e
    /// distingue "sem dado" de 0 %.
    #[test]
    fn spec_probe_018_availability_formatting() {
        assert_eq!(pct(Some(0.994)), "99,4 %");
        assert_eq!(pct(Some(1.0)), "100,0 %");
        assert_eq!(pct(None), "—");
    }

    /// §8.2 — o filtro de severidade cicla e é monotônico.
    #[test]
    fn spec_probe_008_severity_filter_cycles() {
        let mut f = SeverityFilter::All;
        assert!(f.accepts(Severity::Info));

        f = f.next();
        assert_eq!(f, SeverityFilter::WarningUp);
        assert!(!f.accepts(Severity::Info));
        assert!(f.accepts(Severity::Warning));

        f = f.next();
        assert!(!f.accepts(Severity::Warning));
        assert!(f.accepts(Severity::Error));

        f = f.next();
        assert_eq!(f, SeverityFilter::CriticalOnly);
        assert!(!f.accepts(Severity::Error));
        assert!(f.accepts(Severity::Critical));

        assert_eq!(f.next(), SeverityFilter::All);
    }

    /// SPEC-PROBE-010 — a normalização nunca sai do viewbox 0..100 nem
    /// produz NaN com série constante em zero.
    #[test]
    fn spec_probe_010_normalize_stays_inside_viewbox() {
        let points = SeriesPoints {
            values: vec![0.0, 0.0, 0.0],
            min: 0.0,
            max: 0.0,
            last: 0.0,
            bucket_secs: 60,
        };
        let (line, area) = normalize(&points);
        assert!(line.starts_with("M 0.00 100.00"), "{line}");
        assert!(area.ends_with('Z'));
        assert!(!line.contains("NaN"));

        let points = SeriesPoints {
            values: vec![0.0, 50.0, 100.0],
            min: 0.0,
            max: 100.0,
            last: 100.0,
            bucket_secs: 60,
        };
        let (line, _) = normalize(&points);
        assert!(line.contains("M 0.00 100.00"));
        assert!(line.contains("L 100.00 8.00"), "{line}");
    }

    /// SPEC-PROBE-010 — série com menos de 2 pontos não desenha nada em vez
    /// de gerar um path degenerado.
    #[test]
    fn spec_probe_010_short_series_draws_nothing() {
        let points = SeriesPoints {
            values: vec![1.0],
            max: 1.0,
            ..Default::default()
        };
        assert_eq!(normalize(&points), (String::new(), String::new()));
    }

    /// SPEC-PROBE-009 — a conversão de cor preserva o RGB do crate `probe`.
    #[test]
    fn spec_probe_009_color_conversion_matches_probe_palette() {
        let c = rgb(Severity::Critical.rgb());
        assert_eq!(c.red(), 0x8e);
        assert_eq!(c.green(), 0x2c);
        assert_eq!(c.blue(), 0x2b);

        let nodata = rgb(probe::severity::RGB_NO_DATA);
        let ok = rgb(probe::severity::RGB_OK);
        assert_ne!(nodata, ok, "sem dado nunca pode parecer verde");
    }
}
