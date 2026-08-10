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
pub mod ip;
pub mod power;
pub mod report;
pub mod retention;
pub mod sample;
pub mod series;
pub mod service;
pub mod session;
pub mod severity;
pub mod snapshot;
pub mod video;
pub mod writer;

pub use check::{
    health_row_of, layer_of, CheckDef, CheckEngine, CheckProfile, Measurement, OpenCheck,
};
pub use clock::{ProbeClock, SystemClock, TestClock};
pub use config::{
    FecMode, FecProfile, FeedConfig, ProbeConfig, TransportProfile, VideoProfileConfig,
    VideoServiceProfile, MAX_FEEDS,
};
pub use degrade::{DegradeController, DegradePolicy, OverloadSignals};
pub use engine::{FeedIdentity, ProbeEngine, PsiObservation, TickInput, AVAILABILITY_WINDOW_SECS};
pub use event::{EventContext, EventOrigin, EventPhase, ProbeEvent};
// Reexportado de `net` para que a UI monte o histograma sem depender do crate
// de aquisição (SPEC-PROBE-IP-047).
pub use ip::{
    iat_bucket_of, iat_bucket_upper_us, FecStatus, IpAnalyzer, IpAnalyzerConfig, IpTick,
    IpViolations, RtpDelta, IAT_HIST_BUCKETS,
};
pub use net::IatSummary;
pub use power::KeepAwake;
pub use report::render_run_report;
pub use sample::{ProbeSample, CSV_HEADER, CSV_SCHEMA_VERSION};
pub use series::{
    HealthScope, HealthTimeline, MetricId, SeriesPoints, SeriesWindow, TimelineBucket,
    MAX_PLOT_POINTS, MAX_TIMELINE_CELLS,
};
pub use service::{ServiceInfo, ServiceInventory, ServiceStream, ServiceVisual, StreamKind};
pub use session::{Encapsulation, ProbeRun, RunMeta, SessionMeta, SessionSummary, SESSIONS_DIR};
pub use severity::{HealthRow, Layer, LayerHealth, Severity};
pub use snapshot::{
    DegradationStage, EventRow, FeedSnapshot, ProbeHealth, ProbeSnapshot, ServiceSnapshot,
    SnapshotState, StreamSnapshot,
};
pub use video::{
    validate_elementary_stream, video_observation_channel, ActiveFormat, AspectRatio,
    DetectorResult, ElementaryStreamError, ElementaryStreamErrorKind, GopStats, HdrMetadata,
    HdrValue, MetadataChange, Rate, ScanType, VideoAnalysis, VideoAnalyzer, VideoAvailability,
    VideoCodec, VideoFrameObservation, VideoMetadataObservation, VideoObservationError,
    VideoObservationSender, VideoProfile,
};
pub use writer::{writer_channel, ProbeWriter, WriteJob, WriterHandle};
