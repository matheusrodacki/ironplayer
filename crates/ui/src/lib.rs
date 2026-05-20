//! Crate `ui` — Interface egui do IronPlayer.
//!
//! SPEC-UI-001 a SPEC-UI-006

pub mod panels;
pub mod state;
pub mod status_bar;

pub use state::{AppCommand, AppState, ConnectionState, TableEvent, TablesSnapshot};

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;

use crate::panels::metrics::MetricsPanel;
use crate::panels::tables::TablesPanel;
use crate::panels::video::VideoPanel;
use crate::status_bar::StatusBar;

// ---------------------------------------------------------------------------
// IronPlayerApp
// ---------------------------------------------------------------------------

/// Aplicação principal do IronPlayer.
///
/// Implementa `eframe::App` com layout de 3 colunas:
/// - Esquerda:  `VideoPanel` (≈40%)
/// - Centro:    análise — PIDs / Tables / Serviços (≈35%)
/// - Direita:   `MetricsPanel` (≈25%)
/// - Topo:      barra URL + Conectar / Desconectar
/// - Rodapé:    `StatusBar`
///
/// SPEC-UI-001
pub struct IronPlayerApp {
    /// Estado completo da UI — snapshot imutável atualizado a cada frame.
    state: AppState,
    /// Sender para enviar comandos ao backend via canal MPSC bounded.
    cmd_tx: Sender<AppCommand>,
    /// Conteúdo atual do campo de texto de URL.
    url_input: String,
    /// Painel central com abas PIDs / Tables / Serviços.
    tables_panel: TablesPanel,
    /// Painel direito com gráficos e log de erros.
    metrics_panel: MetricsPanel,
    /// Receptor de snapshots do pipeline (opcional).
    ///
    /// SPEC-UI-008
    snapshot_rx: Option<ts::aggregator::SnapshotReceiver>,
    /// Estado de conexão compartilhado com o command handler do pipeline.
    connection_rx: Option<Arc<RwLock<ConnectionState>>>,
    /// Eventos incrementais de tabelas PSI/SI vindos do `TableDispatcher`.
    table_events_rx: Option<Receiver<TableEvent>>,
}

impl IronPlayerApp {
    /// Cria um novo `IronPlayerApp`.
    ///
    /// `snapshot_rx`: receptor de métricas do pipeline; `None` quando o
    /// pipeline ainda não foi iniciado (modo stand-alone / testes).
    ///
    /// SPEC-UI-001 · SPEC-UI-008
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        cmd_tx: Sender<AppCommand>,
        snapshot_rx: Option<ts::aggregator::SnapshotReceiver>,
        connection_rx: Option<Arc<RwLock<ConnectionState>>>,
        table_events_rx: Option<Receiver<TableEvent>>,
    ) -> Self {
        Self {
            state: AppState::default(),
            cmd_tx,
            url_input: String::new(),
            tables_panel: TablesPanel::new(),
            metrics_panel: MetricsPanel::new(),
            snapshot_rx,
            connection_rx,
            table_events_rx,
        }
    }

    /// Retorna uma referência imutável ao estado atual da UI.
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Lê o `SnapshotReceiver` (se disponível) e atualiza `AppState`.
    ///
    /// Chamado no início de cada frame em `update()`.
    ///
    /// SPEC-UI-008
    fn poll_snapshot(&mut self) {
        let rx = match &self.snapshot_rx {
            Some(rx) => rx.clone(),
            None => return,
        };
        let snapshot = rx.borrow();
        let now = Instant::now();
        let cutoff = now - Duration::from_secs(60);

        // Histórico de bitrate total (janela 60 s).
        self.state
            .bitrate_history
            .push_back((now, snapshot.total_bitrate_kbps));
        while self
            .state
            .bitrate_history
            .front()
            .is_some_and(|(t, _)| *t < cutoff)
        {
            self.state.bitrate_history.pop_front();
        }

        // Histórico de jitter PCR por PID (janela 60 s).
        for record in &snapshot.errors.pcr_jitter_events {
            let history = self.state.pcr_history.entry(record.pid).or_default();
            history.push_back(record.clone());
        }
        for history in self.state.pcr_history.values_mut() {
            while history.front().is_some_and(|r| r.timestamp < cutoff) {
                history.pop_front();
            }
        }

        // Atualiza o snapshot de métricas.
        self.state.metrics = snapshot;

        // Atualiza o estado de conexão a partir do command handler.
        if let Some(conn_rx) = &self.connection_rx {
            if let Ok(state) = conn_rx.read() {
                self.state.connection = state.clone();
            }
        }
    }

    /// Drena eventos de tabela sem bloquear o frame da UI.
    ///
    /// SPEC-UI-008
    fn poll_table_events(&mut self) {
        let Some(rx) = self.table_events_rx.as_ref().cloned() else {
            return;
        };

        for event in rx.try_iter().take(512) {
            self.state.apply_table_event(event);
        }
    }
}

impl eframe::App for IronPlayerApp {
    /// Atualiza e renderiza a interface a cada frame.
    ///
    /// SPEC-UI-001 · SPEC-UI-008
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ── Poll de métricas do pipeline ──────────────────────────────────
        self.poll_snapshot();
        self.poll_table_events();

        // ── Header: URL + botões Conectar / Desconectar ──────────────────────
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("URL:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.url_input)
                        .hint_text("udp://@239.1.1.1:1234")
                        .desired_width(400.0),
                );

                let can_connect = matches!(
                    self.state.connection,
                    ConnectionState::Idle | ConnectionState::Error { .. }
                );
                if ui
                    .add_enabled(can_connect, egui::Button::new("Conectar"))
                    .clicked()
                    && !self.url_input.is_empty()
                {
                    let _ = self.cmd_tx.try_send(AppCommand::Connect {
                        url: self.url_input.clone(),
                        iface: None,
                    });
                }

                let can_disconnect = matches!(
                    self.state.connection,
                    ConnectionState::Connected { .. } | ConnectionState::Connecting { .. }
                );
                if ui
                    .add_enabled(can_disconnect, egui::Button::new("Desconectar"))
                    .clicked()
                {
                    let _ = self.cmd_tx.try_send(AppCommand::Disconnect);
                }
            });
        });

        // ── Rodapé: StatusBar ─────────────────────────────────────────────────
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            StatusBar::show(ui, &self.state);
        });

        // ── Painel esquerdo: VideoPanel (≈40%) ───────────────────────────────
        egui::SidePanel::left("video_panel")
            .resizable(true)
            .default_width(400.0)
            .show(ctx, |ui| {
                VideoPanel::show(ui, &self.state);
            });

        // ── Painel direito: MetricsPanel (≈25%) ──────────────────────────────
        egui::SidePanel::right("metrics_panel")
            .resizable(true)
            .default_width(280.0)
            .show(ctx, |ui| {
                self.metrics_panel.show(ui, &self.state, &self.cmd_tx);
            });

        // ── Painel central: PIDs / Tables / Serviços (≈35%) ──────────────────
        egui::CentralPanel::default().show(ctx, |ui| {
            self.tables_panel.show(ui, &self.state, &self.cmd_tx);
        });
    }
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

/// Inicia a janela principal do IronPlayer.
///
/// Cria um canal de comandos bounded, constrói `IronPlayerApp` e delega ao
/// `eframe::run_native`. Retorna `Err` se o subsistema gráfico falhar.
///
/// SPEC-UI-001
pub fn run(title: &str) -> eframe::Result {
    let (cmd_tx, _cmd_rx) = crossbeam_channel::bounded::<AppCommand>(64);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([1280.0, 720.0]),
        ..Default::default()
    };

    eframe::run_native(
        title,
        native_options,
        Box::new(move |cc| Ok(Box::new(IronPlayerApp::new(cc, cmd_tx, None, None, None)))),
    )
}
