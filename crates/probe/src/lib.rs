//! Crate `probe` — modo Probe: monitoração contínua sem player.
//!
//! Concentra motor de checks, estado de alarmes, séries temporais, escrita em
//! disco e exportação.  **Não** parseia TS nem toca em socket: consome as
//! APIs públicas de `ts` e `net` (§5.1).
//!
//! Direção de dependências (regra do AGENTS.md):
//!
//! ```text
//! probe → ts, net        (nunca o inverso)
//! ui-slint → probe, ts, av, net
//! av → ts
//! ```
//!
//! **Colisão de nomes:** já existem `ts::mediainfo::StreamProbe` e o exemplo
//! `crates/av/examples/vp_probe.rs`.  Os tipos deste crate usam o prefixo
//! `Probe*` (`ProbeEngine`, `ProbeSession`, `ProbeEvent`) para evitar
//! ambiguidade em `use` (§5.1).
//!
//! SPEC-PROBE-004 … SPEC-PROBE-016

pub mod check;
pub mod clock;
pub mod config;
pub mod degrade;
pub mod engine;
pub mod event;
pub mod power;
pub mod report;
pub mod retention;
pub mod sample;
pub mod series;
pub mod session;
pub mod severity;
pub mod snapshot;
pub mod writer;

pub use check::{CheckDef, CheckEngine, CheckProfile, Measurement};
pub use clock::{ProbeClock, SystemClock, TestClock};
pub use config::{FecMode, FeedConfig, ProbeConfig, MAX_FEEDS};
pub use degrade::{DegradeController, DegradePolicy, OverloadSignals};
pub use engine::{FeedIdentity, ProbeEngine, TickInput, AVAILABILITY_WINDOW_SECS};
pub use event::{EventContext, EventOrigin, EventPhase, ProbeEvent};
pub use power::KeepAwake;
pub use report::render_run_report;
pub use sample::{ProbeSample, CSV_HEADER, CSV_SCHEMA_VERSION};
pub use series::{MetricId, SeriesPoints, SeriesWindow, TimelineBucket, MAX_PLOT_POINTS};
pub use session::{Encapsulation, ProbeRun, RunMeta, SessionMeta, SessionSummary, SESSIONS_DIR};
pub use severity::{Layer, LayerHealth, Severity};
pub use snapshot::{
    DegradationStage, EventRow, FeedSnapshot, ProbeHealth, ProbeSnapshot, SnapshotState,
};
pub use writer::{writer_channel, ProbeWriter, WriteJob, WriterHandle};
