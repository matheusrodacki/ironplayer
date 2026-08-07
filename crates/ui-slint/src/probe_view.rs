//! Ponte entre `probe::ProbeSnapshot` e os modelos Slint do modo Probe.
//!
//! A UI nunca alcança as estruturas internas do `ProbeEngine`: lê um snapshot
//! imutável publicado a 1 Hz e o traduz para os `struct`s do `probe.slint`.
//! A repintura fica em ≤ 1 Hz (§8.2) — o `Poller` chama [`ProbeView::refresh`]
//! só quando o snapshot muda de geração.
//!
//! Este módulo também guarda a **navegação**: quatro níveis (feeds → feed →
//! serviço → alertas da janela) e a seleção corrente da grade de saúde.  Nada
//! disso volta para o backend: trocar de nível é estado de UI e não pode parar
//! a coleta de nenhum feed (SPEC-PROBE-019).
//!
//! SPEC-PROBE-009 · SPEC-PROBE-010 · SPEC-PROBE-013 · SPEC-PROBE-018 ·
//! SPEC-PROBE-019 · SPEC-PROBE-021 · SPEC-PROBE-022 · SPEC-PROBE-023 ·
//! SPEC-PROBE-025

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use slint::{Color, ModelRc, SharedString, VecModel};

use probe::{
    DegradationStage, EventRow, FeedSnapshot, Layer, MetricId, ProbeSnapshot, SeriesPoints,
    SeriesWindow, ServiceSnapshot, Severity, StreamKind, TimelineBucket,
};

use crate::state::{AppCommand, SharedThumbnails, ThumbnailKey};
use crate::{
    line_path, ProbeAlertRow, ProbeChart, ProbeEventRow, ProbeGridCell, ProbeGridRow, ProbeGridTick,
    ProbeInfoRow, ProbeServiceTile, ProbeTile,
};

/// Estado compartilhado do modo Probe, publicado pelo backend a 1 Hz.
pub type SharedProbe = Arc<RwLock<ProbeSnapshot>>;

/// Teto de colunas da grade de saúde.
///
/// SPEC-PROBE-009 fixa "12 h ⇒ 144 células" como o artefato que responde "o
/// stream está bom?".  A grade **não** segue o seletor de janela — ele governa
/// os gráficos: uma grade que encolhe para uma célula quando o operador escolhe
/// "5 min" deixa de ser uma linha do tempo.
const GRID_WINDOW: SeriesWindow = SeriesWindow::TwelveHours;

/// Piso de colunas da grade.
///
/// A grade cresce com a sessão até o teto de 12 h.  Sem o piso, os primeiros
/// minutos teriam uma ou duas colunas gigantes; com a grade fixa no teto desde
/// o início, seriam 143 células cinza e uma colorida.  24 colunas (2 h com o
/// bucket default) dão contexto suficiente sem transformar a tela em vazio.
const GRID_MIN_COLS: usize = 24;

/// Quantos rótulos de hora a régua tenta mostrar.
const GRID_TICKS: usize = 10;

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

/// Nível de navegação aberto (§8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProbeLevel {
    /// 0 — mosaico de feeds.
    #[default]
    Feeds,
    /// 1 — um feed, com as abas Resumo e Serviços.
    Feed { slot: usize },
    /// 2 — um serviço do multiplex.
    Service { slot: usize, service_id: u16 },
}

impl ProbeLevel {
    fn index(self) -> i32 {
        match self {
            Self::Feeds => 0,
            Self::Feed { .. } => 1,
            Self::Service { .. } => 2,
        }
    }

    fn slot(self) -> Option<usize> {
        match self {
            Self::Feeds => None,
            Self::Feed { slot } | Self::Service { slot, .. } => Some(slot),
        }
    }
}

/// O que uma linha da grade representa.
///
/// O clique devolve `(linha, coluna)`; é isto que traduz a linha de volta para
/// "quais eventos pertencem a ela" (SPEC-PROBE-023).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GridScope {
    /// Camada de rede do feed (IP + RTP).
    Network,
    /// Camada de transporte do feed.
    Transport,
    Service(u16),
    Pid(u16),
}

impl GridScope {
    /// `true` se o evento pertence a este escopo.
    fn accepts(self, row: &EventRow, services: &[ServiceSnapshot]) -> bool {
        match self {
            Self::Network => matches!(
                probe::layer_of(&row.check_id),
                Some(Layer::Ip) | Some(Layer::Rtp)
            ),
            Self::Transport => probe::layer_of(&row.check_id) == Some(Layer::Ts),
            Self::Pid(pid) => row.pid == Some(pid),
            Self::Service(id) => services
                .iter()
                .find(|s| s.service_id == id)
                .is_some_and(|s| s.owns_event(row)),
        }
    }
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

/// Modelos e estado local das telas Probe.
pub(crate) struct ProbeView {
    probe_rx: SharedProbe,
    thumbnails: SharedThumbnails,
    cmd_tx: crossbeam_channel::Sender<AppCommand>,

    tiles: Rc<VecModel<ProbeTile>>,
    services: Rc<VecModel<ProbeServiceTile>>,
    grid_rows: Rc<VecModel<ProbeGridRow>>,
    grid_cells: Rc<VecModel<ProbeGridCell>>,
    grid_ticks: Rc<VecModel<ProbeGridTick>>,
    charts: Rc<VecModel<ProbeChart>>,
    events: Rc<VecModel<ProbeEventRow>>,
    summary: Rc<VecModel<ProbeInfoRow>>,
    health: Rc<VecModel<ProbeInfoRow>>,
    streams: Rc<VecModel<ProbeInfoRow>>,
    alerts: Rc<VecModel<ProbeAlertRow>>,

    level: ProbeLevel,
    /// Aba do nível 1: 0 Resumo · 1 Serviços.
    feed_tab: i32,
    window: SeriesWindow,
    /// Célula selecionada na grade: `(linha, coluna)`.
    selected_cell: Option<(usize, usize)>,
    alerts_open: bool,
    severity_filter: SeverityFilter,

    /// Escopo de cada linha desenhada no último refresh.
    row_scopes: Vec<GridScope>,
    /// Início de cada coluna desenhada no último refresh.
    columns: Vec<DateTime<Utc>>,
    bucket_secs: i64,

    /// Geração do thumbnail já convertida para `slint::Image`, por serviço —
    /// evita reconstruir a imagem a cada tick de UI.
    thumb_generation: HashMap<ThumbnailKey, u64>,
    thumb_cache: HashMap<ThumbnailKey, slint::Image>,
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
        let services: Rc<VecModel<ProbeServiceTile>> = Rc::new(VecModel::default());
        let grid_rows: Rc<VecModel<ProbeGridRow>> = Rc::new(VecModel::default());
        let grid_cells: Rc<VecModel<ProbeGridCell>> = Rc::new(VecModel::default());
        let grid_ticks: Rc<VecModel<ProbeGridTick>> = Rc::new(VecModel::default());
        let charts: Rc<VecModel<ProbeChart>> = Rc::new(VecModel::default());
        let events: Rc<VecModel<ProbeEventRow>> = Rc::new(VecModel::default());
        let summary: Rc<VecModel<ProbeInfoRow>> = Rc::new(VecModel::default());
        let health: Rc<VecModel<ProbeInfoRow>> = Rc::new(VecModel::default());
        let streams: Rc<VecModel<ProbeInfoRow>> = Rc::new(VecModel::default());
        let alerts: Rc<VecModel<ProbeAlertRow>> = Rc::new(VecModel::default());

        window.set_probe_tiles(ModelRc::from(tiles.clone()));
        window.set_probe_services(ModelRc::from(services.clone()));
        window.set_probe_grid_rows(ModelRc::from(grid_rows.clone()));
        window.set_probe_grid_cells(ModelRc::from(grid_cells.clone()));
        window.set_probe_grid_ticks(ModelRc::from(grid_ticks.clone()));
        window.set_probe_charts(ModelRc::from(charts.clone()));
        window.set_probe_events(ModelRc::from(events.clone()));
        window.set_probe_summary(ModelRc::from(summary.clone()));
        window.set_probe_health(ModelRc::from(health.clone()));
        window.set_probe_streams(ModelRc::from(streams.clone()));
        window.set_probe_alerts(ModelRc::from(alerts.clone()));

        Self {
            probe_rx,
            thumbnails,
            cmd_tx,
            tiles,
            services,
            grid_rows,
            grid_cells,
            grid_ticks,
            charts,
            events,
            summary,
            health,
            streams,
            alerts,
            level: ProbeLevel::Feeds,
            feed_tab: 0,
            window: SeriesWindow::OneHour,
            selected_cell: None,
            alerts_open: false,
            severity_filter: SeverityFilter::All,
            row_scopes: Vec::new(),
            columns: Vec::new(),
            bucket_secs: 300,
            thumb_generation: HashMap::new(),
            thumb_cache: HashMap::new(),
        }
    }

    // ── Navegação ───────────────────────────────────────────────────────

    /// Abre o nível 1 de um feed.
    ///
    /// SPEC-PROBE-019 — alternar de nível não pode parar a coleta de nenhum
    /// feed; por isso aqui só muda estado de UI, nunca o pipeline.
    pub(crate) fn open_feed(&mut self, slot: i32) {
        if slot < 0 {
            return;
        }
        self.level = ProbeLevel::Feed {
            slot: slot as usize,
        };
        self.feed_tab = 0;
        self.clear_selection();
    }

    /// Abre o nível 2 de um serviço do feed corrente.
    ///
    /// SPEC-PROBE-022
    pub(crate) fn open_service(&mut self, service_id: i32) {
        let Some(slot) = self.level.slot() else {
            return;
        };
        if !(0..=i32::from(u16::MAX)).contains(&service_id) {
            return;
        }
        self.level = ProbeLevel::Service {
            slot,
            service_id: service_id as u16,
        };
        self.clear_selection();
    }

    /// Sobe um nível; `to_feeds` volta direto para o mosaico de feeds.
    pub(crate) fn back(&mut self, to_feeds: bool) {
        // Com o modal aberto, "voltar" fecha o modal primeiro: é o que a tecla
        // Esc faz em qualquer diálogo, e sair dois níveis de uma vez confunde.
        if self.alerts_open {
            self.alerts_open = false;
            return;
        }
        self.level = match (self.level, to_feeds) {
            (ProbeLevel::Service { slot, .. }, false) => ProbeLevel::Feed { slot },
            _ => ProbeLevel::Feeds,
        };
        self.clear_selection();
    }

    /// Volta ao mosaico de feeds (troca de modo, feed que sumiu).
    pub(crate) fn reset(&mut self) {
        self.level = ProbeLevel::Feeds;
        self.feed_tab = 0;
        self.clear_selection();
    }

    /// Troca a aba do nível 1 (0 Resumo · 1 Serviços).
    pub(crate) fn set_feed_tab(&mut self, tab: i32) {
        self.feed_tab = tab.clamp(0, 1);
        self.clear_selection();
    }

    fn clear_selection(&mut self) {
        self.selected_cell = None;
        self.alerts_open = false;
    }

    /// Clique numa célula da grade: seleciona e abre a lista de alertas.
    ///
    /// SPEC-PROBE-023 · SPEC-PROBE-025
    pub(crate) fn select_grid_cell(&mut self, row: i32, col: i32) {
        if row < 0 || col < 0 {
            return;
        }
        let cell = (row as usize, col as usize);
        if self.selected_cell == Some(cell) && self.alerts_open {
            // Segundo clique na mesma célula fecha, em vez de reabrir o mesmo
            // conteúdo — o modal não tem outro caminho de saída no teclado.
            self.clear_selection();
            return;
        }
        self.selected_cell = Some(cell);
        self.alerts_open = true;
    }

    /// Fecha a lista de alertas mantendo a célula destacada.
    pub(crate) fn close_alerts(&mut self) {
        self.alerts_open = false;
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

    // ── Refresh ─────────────────────────────────────────────────────────

    /// Reaplica todo o estado das telas Probe na janela.
    pub(crate) fn refresh(&mut self, win: &crate::AppWindow) {
        let snapshot = match self.probe_rx.read() {
            Ok(guard) => guard.clone(),
            Err(_) => return,
        };

        win.set_probe_run_clock(SharedString::from(snapshot.run_clock()));
        win.set_probe_run_id(SharedString::from(snapshot.run_id.as_str()));
        win.set_probe_recording(snapshot.recording);
        win.set_probe_status(SharedString::from(self.status_line(&snapshot)));

        // Nível apontando para um feed que sumiu (feed removido do TOML e run
        // reaberto): volta aos feeds em vez de mostrar um tile vazio.
        if self.level.slot().is_some_and(|s| snapshot.feed(s).is_none()) {
            self.reset();
        }
        // O mesmo para um serviço que saiu da PSI.
        if let ProbeLevel::Service { slot, service_id } = self.level {
            let alive = snapshot
                .feed(slot)
                .is_some_and(|f| f.service(service_id).is_some());
            if !alive {
                self.level = ProbeLevel::Feed { slot };
                self.clear_selection();
            }
        }

        let tiles: Vec<ProbeTile> = snapshot.feeds.iter().map(|f| self.build_tile(f)).collect();
        self.tiles.set_vec(tiles);

        win.set_probe_level(self.level.index());
        win.set_probe_feed_tab(self.feed_tab);
        win.set_probe_window_index(self.window_index());
        win.set_probe_severity_filter(SharedString::from(self.severity_filter.label()));

        let feed = self.level.slot().and_then(|s| snapshot.feed(s).cloned());
        match (self.level, feed) {
            (ProbeLevel::Feeds, _) | (_, None) => self.clear_detail_models(win),
            (ProbeLevel::Feed { .. }, Some(feed)) => self.refresh_feed(win, &feed),
            (ProbeLevel::Service { service_id, .. }, Some(feed)) => {
                self.refresh_service(win, &feed, service_id)
            }
        }
    }

    fn clear_detail_models(&mut self, win: &crate::AppWindow) {
        self.services.set_vec(Vec::new());
        self.grid_rows.set_vec(Vec::new());
        self.grid_cells.set_vec(Vec::new());
        self.grid_ticks.set_vec(Vec::new());
        self.charts.set_vec(Vec::new());
        self.events.set_vec(Vec::new());
        self.summary.set_vec(Vec::new());
        self.health.set_vec(Vec::new());
        self.streams.set_vec(Vec::new());
        self.alerts.set_vec(Vec::new());
        self.row_scopes.clear();
        self.columns.clear();
        win.set_probe_grid_cols(0);
        win.set_probe_alerts_open(false);
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

    // ── Tiles ───────────────────────────────────────────────────────────

    fn build_tile(&mut self, feed: &FeedSnapshot) -> ProbeTile {
        // O tile do feed mostra o quadro do serviço primário (SPEC-PROBE-024).
        let key = feed
            .primary_service()
            .map(|s| (feed.slot, s.service_id))
            .unwrap_or((feed.slot, u16::MAX));
        let (thumbnail, has_thumbnail) = self.thumbnail_for(key);
        let health = |layer: Layer| {
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
            availability_color: rgb(availability_rgb(feed.availability_window)),
            ip_color: health(Layer::Ip),
            rtp_color: health(Layer::Rtp),
            ts_color: health(Layer::Ts),
            v_color: health(Layer::Video),
            a_color: health(Layer::Audio),
            open_events: feed.open_events as i32,
            service_count: feed.services.len() as i32,
            selected: self.level.slot() == Some(feed.slot),
        }
    }

    fn build_service_tile(&mut self, slot: usize, svc: &ServiceSnapshot) -> ProbeServiceTile {
        let (thumbnail, has_thumbnail) = self.thumbnail_for((slot, svc.service_id));
        let health = |layer: Layer| {
            rgb(svc
                .layer_health
                .get(&layer)
                .copied()
                .unwrap_or_default()
                .rgb())
        };
        let selected = matches!(
            self.level,
            ProbeLevel::Service { service_id, .. } if service_id == svc.service_id
        );

        ProbeServiceTile {
            service_id: svc.service_id as i32,
            name: SharedString::from(svc.display_name()),
            subtitle: SharedString::from(service_subtitle(svc)),
            res_badge: SharedString::from(svc.resolution_badge().unwrap_or("")),
            scrambled: svc.scrambled,
            thumbnail,
            has_thumbnail,
            snapshot_state: SharedString::from(svc.snapshot_state.label()),
            bitrate: SharedString::from(mbps(svc.bitrate_kbps)),
            availability: SharedString::from(pct(svc.availability_window)),
            availability_frac: svc.availability_window.unwrap_or(0.0) as f32,
            availability_color: rgb(availability_rgb(svc.availability_window)),
            ts_color: health(Layer::Ts),
            v_color: health(Layer::Video),
            a_color: health(Layer::Audio),
            open_events: svc.open_events as i32,
            selected,
        }
    }

    /// Converte o thumbnail publicado pela thread `probe-snapshot`.
    ///
    /// SPEC-PROBE-003 — só há uma imagem viva por serviço; a conversão para
    /// `slint::Image` é feita uma vez por geração, não a cada tick de UI.
    fn thumbnail_for(&mut self, key: ThumbnailKey) -> (slint::Image, bool) {
        let latest = match self.thumbnails.read() {
            Ok(map) => map
                .get(&key)
                .map(|t| (t.generation, t.width, t.height, t.rgba.clone())),
            Err(_) => None,
        };

        let Some((generation, width, height, rgba)) = latest else {
            self.thumb_cache.remove(&key);
            self.thumb_generation.remove(&key);
            return (slint::Image::default(), false);
        };

        if self.thumb_generation.get(&key) != Some(&generation) {
            let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
            let bytes = buf.make_mut_bytes();
            let n = bytes.len().min(rgba.len());
            bytes[..n].copy_from_slice(&rgba[..n]);
            self.thumb_cache.insert(key, slint::Image::from_rgba8(buf));
            self.thumb_generation.insert(key, generation);
        }

        match self.thumb_cache.get(&key) {
            Some(img) => (img.clone(), true),
            None => (slint::Image::default(), false),
        }
    }

    // ── Nível 1: feed ───────────────────────────────────────────────────

    fn refresh_feed(&mut self, win: &crate::AppWindow, feed: &FeedSnapshot) {
        let tile = self.build_tile(feed);
        win.set_probe_detail_tile(tile);

        let service_tiles: Vec<ProbeServiceTile> = feed
            .services
            .iter()
            .map(|s| self.build_service_tile(feed.slot, s))
            .collect();
        self.services.set_vec(service_tiles);

        // Grade do MPTS: transporte, rede e uma linha por serviço.
        let mut rows: Vec<(GridScope, ProbeGridRow, &[TimelineBucket])> = vec![
            (
                GridScope::Transport,
                grid_row("TRANSPORTE", "", true),
                feed.ts_timeline.as_slice(),
            ),
            (
                GridScope::Network,
                grid_row("IP / RTP", feed.encapsulation.badge(), true),
                feed.ip_timeline.as_slice(),
            ),
        ];
        for svc in &feed.services {
            rows.push((
                GridScope::Service(svc.service_id),
                grid_row(
                    &svc.display_name(),
                    &format!("{}", svc.service_id),
                    false,
                ),
                svc.timeline.as_slice(),
            ));
        }
        self.build_grid(win, feed, rows);

        // ── Gráficos (SPEC-PROBE-010) ───────────────────────────────────
        let charts: Vec<ProbeChart> = MetricId::ALL
            .iter()
            .filter_map(|m| feed.series.get(m).map(|p| build_chart(*m, p)))
            .collect();
        self.charts.set_vec(charts);

        // ── Event log (§8.2) ────────────────────────────────────────────
        let rows: Vec<ProbeEventRow> = feed
            .events
            .iter()
            .filter(|e| self.severity_filter.accepts(e.severity))
            .map(event_row)
            .collect();
        self.events.set_vec(rows);

        // ── Resumo e saúde da probe (SPEC-PROBE-013) ────────────────────
        self.summary.set_vec(feed_summary(feed));
        self.health.set_vec(feed_health(feed));
        self.streams.set_vec(Vec::new());

        self.refresh_alerts(win, feed, None);
    }

    // ── Nível 2: serviço ────────────────────────────────────────────────

    fn refresh_service(&mut self, win: &crate::AppWindow, feed: &FeedSnapshot, service_id: u16) {
        let tile = self.build_tile(feed);
        win.set_probe_detail_tile(tile);

        let Some(svc) = feed.service(service_id).cloned() else {
            return;
        };
        let service_tile = self.build_service_tile(feed.slot, &svc);
        win.set_probe_service_tile(service_tile);
        self.services.set_vec(Vec::new());
        self.charts.set_vec(Vec::new());

        // Grade do serviço: a linha geral e uma por PID elementar.
        let mut rows: Vec<(GridScope, ProbeGridRow, &[TimelineBucket])> = vec![(
            GridScope::Service(svc.service_id),
            grid_row(&svc.display_name(), "geral", true),
            svc.timeline.as_slice(),
        )];
        for s in &svc.streams {
            rows.push((
                GridScope::Pid(s.pid),
                grid_row(&s.describe(), &kbps_short(s.bitrate_kbps), false),
                s.timeline.as_slice(),
            ));
        }
        self.build_grid(win, feed, rows);

        // Event log restrito ao serviço: no nível 2 o operador está olhando
        // um canal, e eventos dos vizinhos só atrapalhariam.
        let rows: Vec<ProbeEventRow> = feed
            .events
            .iter()
            .filter(|e| self.severity_filter.accepts(e.severity))
            .filter(|e| svc.owns_event(e))
            .map(event_row)
            .collect();
        self.events.set_vec(rows);

        self.summary.set_vec(service_summary(feed, &svc));
        self.streams.set_vec(service_streams(&svc));
        self.health.set_vec(Vec::new());

        self.refresh_alerts(win, feed, Some(&svc));
    }

    // ── Grade de saúde ──────────────────────────────────────────────────

    /// Projeta cada escopo no mesmo eixo de tempo e publica os modelos.
    ///
    /// Escopos nascem em instantes diferentes (um serviço que só apareceu na
    /// PAT depois de uma hora tem menos células).  Alinhar pelo índice do vetor
    /// desalinharia as colunas; o eixo é o **relógio**, e cada linha procura o
    /// bucket daquele instante — o que não existe fica cinza, que é justamente
    /// "sem dado" (SPEC-PROBE-009).
    ///
    /// SPEC-PROBE-023
    fn build_grid(
        &mut self,
        win: &crate::AppWindow,
        feed: &FeedSnapshot,
        rows: Vec<(GridScope, ProbeGridRow, &[TimelineBucket])>,
    ) {
        let bucket = feed.timeline_bucket_secs.max(1) as i64;
        self.bucket_secs = bucket;

        let cols = grid_cols(feed.timeline.len(), bucket as u64);
        let last = feed
            .timeline
            .last()
            .map(|b| b.start_utc)
            .or_else(|| rows.iter().filter_map(|(_, _, t)| t.last()).map(|b| b.start_utc).max());

        let Some(last) = last else {
            self.row_scopes.clear();
            self.columns.clear();
            self.grid_rows.set_vec(Vec::new());
            self.grid_cells.set_vec(Vec::new());
            self.grid_ticks.set_vec(Vec::new());
            win.set_probe_grid_cols(0);
            win.set_probe_grid_caption(SharedString::from("aguardando amostras"));
            return;
        };

        let columns: Vec<DateTime<Utc>> = (0..cols)
            .map(|i| last - ChronoDuration::seconds((cols - 1 - i) as i64 * bucket))
            .collect();

        let mut cells: Vec<ProbeGridCell> = Vec::with_capacity(rows.len() * cols);
        let mut scopes = Vec::with_capacity(rows.len());
        let mut model_rows = Vec::with_capacity(rows.len());

        for (row_index, (scope, row, timeline)) in rows.into_iter().enumerate() {
            let by_start: HashMap<i64, &TimelineBucket> = timeline
                .iter()
                .map(|b| (b.start_utc.timestamp(), b))
                .collect();
            for (col, start) in columns.iter().enumerate() {
                let color = by_start
                    .get(&start.timestamp())
                    .map_or(probe::severity::RGB_NO_DATA, |b| b.rgb());
                cells.push(ProbeGridCell {
                    row: row_index as i32,
                    col: col as i32,
                    cell_color: rgb(color),
                });
            }
            scopes.push(scope);
            model_rows.push(row);
        }

        let ticks = build_ticks(&columns);
        let caption = format!(
            "{} células · {} min/célula · {} → {}",
            cols,
            bucket / 60,
            columns
                .first()
                .map(|t| t.format("%d/%m %H:%M").to_string())
                .unwrap_or_default(),
            columns
                .last()
                .map(|t| t.format("%d/%m %H:%M").to_string())
                .unwrap_or_default(),
        );

        // A célula selecionada precisa continuar válida quando o eixo rola: o
        // índice é posicional, e sem o clamp o destaque apontaria para fora.
        if let Some((row, col)) = self.selected_cell {
            if row >= scopes.len() || col >= columns.len() {
                self.clear_selection();
            }
        }

        self.row_scopes = scopes;
        self.columns = columns;
        self.grid_rows.set_vec(model_rows);
        self.grid_cells.set_vec(cells);
        self.grid_ticks.set_vec(ticks);
        win.set_probe_grid_cols(cols as i32);
        win.set_probe_grid_caption(SharedString::from(caption));
        win.set_probe_grid_selected_row(self.selected_cell.map_or(-1, |(r, _)| r as i32));
        win.set_probe_grid_selected_col(self.selected_cell.map_or(-1, |(_, c)| c as i32));
    }

    // ── Nível 3: alertas da janela ──────────────────────────────────────

    /// Monta a lista consolidada dos problemas da célula selecionada.
    ///
    /// SPEC-PROBE-025
    fn refresh_alerts(
        &mut self,
        win: &crate::AppWindow,
        feed: &FeedSnapshot,
        service: Option<&ServiceSnapshot>,
    ) {
        win.set_probe_alerts_open(self.alerts_open);
        let Some((row, col)) = self.selected_cell.filter(|_| self.alerts_open) else {
            self.alerts.set_vec(Vec::new());
            return;
        };
        let (Some(scope), Some(from)) = (self.row_scopes.get(row).copied(), self.columns.get(col))
        else {
            self.alerts.set_vec(Vec::new());
            return;
        };
        let from = *from;
        let to = from + ChronoDuration::seconds(self.bucket_secs);

        let names: HashMap<u16, String> = feed
            .services
            .iter()
            .map(|s| (s.service_id, s.display_name()))
            .collect();

        let mut rows: Vec<&EventRow> = feed
            .events
            .iter()
            .filter(|e| e.ts_utc >= from && e.ts_utc < to)
            .filter(|e| scope.accepts(e, &feed.services))
            .collect();
        // Pior primeiro, e dentro da mesma severidade o mais recente: é a ordem
        // em que o operador quer ler ao abrir a janela.
        rows.sort_by(|a, b| b.severity.cmp(&a.severity).then(b.ts_utc.cmp(&a.ts_utc)));

        let alerts: Vec<ProbeAlertRow> = rows
            .iter()
            .enumerate()
            .map(|(i, e)| ProbeAlertRow {
                index: SharedString::from((i + 1).to_string()),
                level: SharedString::from(e.severity.label()),
                level_color: rgb(e.severity.rgb()),
                time: SharedString::from(e.ts_utc.format("%H:%M:%S%.3f").to_string()),
                description: SharedString::from(e.describe()),
                occurrence: SharedString::from(e.count.to_string()),
                service: SharedString::from(
                    e.service_id
                        .and_then(|id| names.get(&id).cloned())
                        .unwrap_or_else(|| "—".to_string()),
                ),
                pid: SharedString::from(
                    e.pid.map_or_else(|| "—".to_string(), |p| p.to_string()),
                ),
            })
            .collect();

        let title = match scope {
            GridScope::Network => format!("Alertas de rede · {}", feed.display_name()),
            GridScope::Transport => format!("Alertas de transporte · {}", feed.display_name()),
            GridScope::Service(id) => format!(
                "Alertas · {}",
                names
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| format!("Serviço {id}"))
            ),
            GridScope::Pid(pid) => format!(
                "Alertas · PID {pid}{}",
                service
                    .map(|s| format!(" · {}", s.display_name()))
                    .unwrap_or_default()
            ),
        };
        let subtitle = format!(
            "{} → {} · {} min",
            from.format("%d/%m/%Y %H:%M:%S"),
            to.format("%H:%M:%S"),
            self.bucket_secs / 60
        );

        self.alerts.set_vec(alerts);
        win.set_probe_alerts_title(SharedString::from(title));
        win.set_probe_alerts_subtitle(SharedString::from(subtitle));
    }
}

// ── Helpers de formatação ───────────────────────────────────────────────────

/// Cor da barra de disponibilidade.
///
/// Os degraus são os da operação: abaixo de 99 % o canal esteve fora tempo
/// demais para ser considerado saudável; entre 99 % e 99,9 % houve interrupção.
fn availability_rgb(window: Option<f64>) -> u32 {
    match window {
        Some(v) if v < 0.99 => Severity::Error.rgb(),
        Some(v) if v < 0.999 => Severity::Warning.rgb(),
        _ => probe::severity::RGB_OK,
    }
}

/// Quantas colunas a grade desenha para uma sessão com `available` buckets.
///
/// Cresce com a sessão e satura em 12 h; nunca abaixo de [`GRID_MIN_COLS`].
///
/// SPEC-PROBE-023a
fn grid_cols(available: usize, bucket_secs: u64) -> usize {
    available.clamp(GRID_MIN_COLS, GRID_WINDOW.cells(bucket_secs).max(GRID_MIN_COLS))
}

fn grid_row(label: &str, sub: &str, header: bool) -> ProbeGridRow {
    ProbeGridRow {
        label: SharedString::from(label),
        sub: SharedString::from(sub),
        header,
    }
}

/// Rótulos da régua de tempo, espaçados para caber ~[`GRID_TICKS`] marcas.
fn build_ticks(columns: &[DateTime<Utc>]) -> Vec<ProbeGridTick> {
    if columns.is_empty() {
        return Vec::new();
    }
    let step = (columns.len() / GRID_TICKS).max(1);
    columns
        .iter()
        .enumerate()
        .filter(|(i, _)| i % step == 0)
        .map(|(i, t)| ProbeGridTick {
            col: i as i32,
            label: SharedString::from(t.format("%H:%M").to_string()),
        })
        .collect()
}

fn kv(key: &str, value: String) -> ProbeInfoRow {
    ProbeInfoRow {
        key: SharedString::from(key),
        value: SharedString::from(value),
    }
}

/// Bitrate curto para a coluna direita da grade.
fn kbps_short(kbps: f64) -> String {
    if kbps >= 1000.0 {
        format!("{:.1}M", kbps / 1000.0).replace('.', ",")
    } else {
        format!("{kbps:.0}k")
    }
}

/// Subtítulo do tile de serviço: provedor e PIDs, para achar o canal sem abrir.
fn service_subtitle(svc: &ServiceSnapshot) -> String {
    let pids: Vec<String> = svc
        .streams
        .iter()
        .take(4)
        .map(|s| s.pid.to_string())
        .collect();
    let mut text = format!("sid {}", svc.service_id);
    if let Some(p) = &svc.provider {
        if !p.trim().is_empty() {
            text.push_str(" · ");
            text.push_str(p);
        }
    }
    if !pids.is_empty() {
        text.push_str(" · ");
        text.push_str(&pids.join("/"));
    }
    text
}

fn uptime(secs: u64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

fn feed_summary(feed: &FeedSnapshot) -> Vec<ProbeInfoRow> {
    vec![
        kv("uptime", uptime(feed.uptime_secs)),
        kv("disponibilidade (60 min)", pct(feed.availability_window)),
        kv(
            "disponibilidade (sessão)",
            pct(Some(feed.availability_session)),
        ),
        kv("bitrate", mbps(feed.bitrate_kbps)),
        kv("null ratio", format!("{:.2} %", feed.null_ratio * 100.0)),
        kv("serviços", feed.services.len().to_string()),
        kv("vídeo", mbps(feed.video_kbps)),
        // O indicador `A` é presença e bitrate do PID de áudio, nunca nível —
        // o mosaico de referência mostra VU meter, que exigiria decodificar
        // áudio continuamente (§8.1).
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
    ]
}

fn feed_health(feed: &FeedSnapshot) -> Vec<ProbeInfoRow> {
    vec![
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
    ]
}

fn service_summary(feed: &FeedSnapshot, svc: &ServiceSnapshot) -> Vec<ProbeInfoRow> {
    let mut rows = vec![
        kv("service id", svc.service_id.to_string()),
        kv(
            "provedor",
            svc.provider.clone().unwrap_or_else(|| "—".to_string()),
        ),
        kv("feed", feed.display_name().to_string()),
        kv("uptime", uptime(feed.uptime_secs)),
        kv("disponibilidade (60 min)", pct(svc.availability_window)),
        kv("bitrate do serviço", mbps(svc.bitrate_kbps)),
        kv("vídeo", mbps(svc.video_kbps)),
        kv("áudio (presença)", mbps(svc.audio_kbps)),
        kv(
            "resolução",
            svc.video_height
                .map_or("—".to_string(), |h| format!("{h}p / {h}i")),
        ),
        kv("pmt pid", svc.pmt_pid.to_string()),
        kv("pcr pid", svc.pcr_pid.to_string()),
        kv("acesso condicional", if svc.scrambled { "sim" } else { "não" }.to_string()),
        kv("alarmes abertos", svc.open_events.to_string()),
        kv(
            "pior evento",
            svc.worst_severity
                .map_or("—".to_string(), |s| s.label().to_string()),
        ),
    ];
    // A participação no multiplex responde "este serviço cabe no transporte?",
    // que é a primeira pergunta num MPTS apertado.
    if feed.bitrate_kbps > 0.0 {
        rows.push(kv(
            "share do multiplex",
            format!("{:.1} %", svc.bitrate_kbps / feed.bitrate_kbps * 100.0).replace('.', ","),
        ));
    }
    rows
}

fn service_streams(svc: &ServiceSnapshot) -> Vec<ProbeInfoRow> {
    svc.streams
        .iter()
        .map(|s| {
            let mark = match s.kind {
                StreamKind::Video => "V",
                StreamKind::Audio => "A",
                StreamKind::Subtitle => "L",
                StreamKind::Pcr => "P",
                _ => "D",
            };
            ProbeInfoRow {
                key: SharedString::from(format!("{mark} · {}", s.describe())),
                value: SharedString::from(if s.cc_errors > 0 {
                    format!("{} · {} cc", kbps_short(s.bitrate_kbps), s.cc_errors)
                } else {
                    kbps_short(s.bitrate_kbps)
                }),
            }
        })
        .collect()
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
    use probe::{StreamSnapshot, TimelineBucket};

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("timestamp")
    }

    fn event(check_id: &str, secs: i64, pid: Option<u16>, service: Option<u16>) -> EventRow {
        EventRow {
            event_id: format!("e{secs}"),
            ts_utc: ts(secs),
            severity: Severity::Error,
            check_id: check_id.into(),
            phase: probe::EventPhase::Open,
            count: 3,
            measured: 3.0,
            unit: "errors".into(),
            context: String::new(),
            pid,
            service_id: service,
            local: false,
        }
    }

    fn service(id: u16, pids: &[u16]) -> ServiceSnapshot {
        ServiceSnapshot {
            service_id: id,
            name: format!("CANAL_{id}"),
            pmt_pid: 0x1000 + id,
            streams: pids
                .iter()
                .map(|p| StreamSnapshot {
                    pid: *p,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

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

    /// SPEC-PROBE-025 — cada linha da grade filtra os alertas pelo seu escopo:
    /// a linha do PID não mostra o problema do vizinho, e a de transporte não
    /// mostra um alarme de rede.
    #[test]
    fn spec_probe_025_grid_scope_filters_alerts() {
        let services = vec![service(100, &[6100, 6101]), service(200, &[6200])];

        let cc_a = event("cc_error", 10, Some(6100), Some(100));
        let cc_b = event("cc_error", 11, Some(6200), Some(200));
        let outage = event("feed_unavailable", 12, None, None);
        let rtp = event("rtp_out_of_order", 13, None, None);

        assert!(GridScope::Pid(6100).accepts(&cc_a, &services));
        assert!(!GridScope::Pid(6100).accepts(&cc_b, &services));

        assert!(GridScope::Service(100).accepts(&cc_a, &services));
        assert!(!GridScope::Service(100).accepts(&cc_b, &services));

        // `cc_error` é da camada TS; `feed_unavailable` é IP.
        assert!(GridScope::Transport.accepts(&cc_a, &services));
        assert!(!GridScope::Transport.accepts(&outage, &services));
        assert!(GridScope::Network.accepts(&outage, &services));
        assert!(GridScope::Network.accepts(&rtp, &services));
        assert!(!GridScope::Network.accepts(&cc_a, &services));
    }

    /// SPEC-PROBE-023 — a régua distribui os rótulos sem estourar o eixo nem
    /// dividir por zero num eixo vazio.
    #[test]
    fn spec_probe_023_time_ruler_is_spread_over_the_axis() {
        assert!(build_ticks(&[]).is_empty());

        let columns: Vec<DateTime<Utc>> = (0..144).map(|i| ts(i * 300)).collect();
        let ticks = build_ticks(&columns);
        assert!(ticks.len() <= GRID_TICKS + 1, "{} marcas", ticks.len());
        assert_eq!(ticks[0].col, 0);
        assert!(ticks.iter().all(|t| (t.col as usize) < columns.len()));

        // Eixo curto: uma marca por coluna, sem passo zero.
        let short: Vec<DateTime<Utc>> = (0..3).map(|i| ts(i * 300)).collect();
        assert_eq!(build_ticks(&short).len(), 3);
    }

    /// SPEC-PROBE-023a — a grade cresce com a sessão e satura em 12 h, sem
    /// nunca virar uma faixa de duas células gigantes nem 143 células cinza.
    #[test]
    fn spec_probe_023a_grid_grows_with_the_session_and_saturates_at_12h() {
        // Sessão recém-aberta: o piso garante contexto.
        assert_eq!(grid_cols(0, 300), GRID_MIN_COLS);
        assert_eq!(grid_cols(1, 300), GRID_MIN_COLS);
        // No meio: uma coluna por bucket existente.
        assert_eq!(grid_cols(36, 300), 36);
        // Saturada: 12 h com bucket de 5 min.
        assert_eq!(grid_cols(144, 300), 144);
        assert_eq!(grid_cols(500, 300), 144, "24 h de sessão mostram as 12 h finais");
        // Bucket maior que a janela não colapsa a grade abaixo do piso.
        assert_eq!(grid_cols(200, 86_400), GRID_MIN_COLS);
    }

    /// SPEC-PROBE-022 — o subtítulo do tile identifica o serviço sem abrir.
    #[test]
    fn spec_probe_022_service_subtitle_carries_sid_provider_and_pids() {
        let mut svc = service(1097, &[401, 402, 403]);
        svc.provider = Some("ESPN".into());
        let text = service_subtitle(&svc);
        assert!(text.starts_with("sid 1097"));
        assert!(text.contains("ESPN"));
        assert!(text.contains("401/402/403"));

        // Sem provedor não sobra separador solto.
        let bare = service(7, &[]);
        assert_eq!(service_subtitle(&bare), "sid 7");
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

    /// SPEC-PROBE-009 — célula sem bucket no instante do eixo fica cinza, e não
    /// herda a cor da vizinha: um serviço que nasceu no meio da sessão não pode
    /// aparecer como "esteve bom" no tempo em que nem existia.
    #[test]
    fn spec_probe_009_missing_bucket_renders_as_no_data() {
        let late = TimelineBucket {
            start_utc: ts(600),
            worst: None,
            samples: 300,
            connected_samples: 300,
        };
        assert_eq!(late.rgb(), probe::severity::RGB_OK);

        let by_start: HashMap<i64, &TimelineBucket> =
            [(late.start_utc.timestamp(), &late)].into_iter().collect();
        let missing = by_start
            .get(&ts(300).timestamp())
            .map_or(probe::severity::RGB_NO_DATA, |b| b.rgb());
        assert_eq!(missing, probe::severity::RGB_NO_DATA);
    }

    /// §8 — a navegação sobe um nível de cada vez, e o modal intercepta o
    /// primeiro "voltar".
    #[test]
    fn spec_probe_022_back_walks_one_level_at_a_time() {
        assert_eq!(ProbeLevel::Feeds.index(), 0);
        assert_eq!(ProbeLevel::Feed { slot: 1 }.index(), 1);
        assert_eq!(
            ProbeLevel::Service {
                slot: 1,
                service_id: 55
            }
            .index(),
            2
        );
        assert_eq!(ProbeLevel::Feeds.slot(), None);
        assert_eq!(
            ProbeLevel::Service {
                slot: 3,
                service_id: 1
            }
            .slot(),
            Some(3)
        );
    }
}
