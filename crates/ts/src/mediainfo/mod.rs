//! Análise de cabeçalhos de codec elementares para relatório Media Info.
//!
//! SPEC-MI-001 · SPEC-MI-002

pub mod aac;
pub mod ac3;
pub mod avc;
pub mod bitreader;
pub mod error;
pub mod hevc;
pub mod model;
pub mod mpeg2video;
pub mod mpegaudio;
pub mod probe;
pub mod report;

pub use model::{ElementaryCodecInfo, MediaInfoCodecSnapshot, StreamKind};
pub use probe::{ProbeStreamMeta, StreamProbe};
pub use report::{
    build_elementary_stream_fields, build_media_info_report, enrich_tables_ctx_from_descriptors,
    MediaInfoBuildInput, MediaInfoReport, MediaInfoSection, MediaInfoTablesCtx, ReportField,
};
