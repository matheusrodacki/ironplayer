//! Crate `net` — recepção UDP/RTP multicast e primitivas da camada 1.
//!
//! Duas responsabilidades, deliberadamente separadas:
//!
//! - **Aquisição** ([`PacketSource`], [`SocketSource`], [`UdpReceiver`]) — pôr
//!   o datagrama para dentro, com o instante de chegada e o endereço de origem.
//! - **Parsing e contagem** ([`RtpHeader`], [`RtpSeqState`], [`FecHeader`],
//!   [`timing`]) — puro, sem socket e sem estado global, para que a análise
//!   inteira seja testável com fixtures sintéticas.
//!
//! Quem compõe as duas coisas num diagnóstico é o crate `probe`; aqui não há
//! nenhum tipo de check, alarme ou severidade.
//!
//! SPEC-NET-001 · SPEC-NET-002 · SPEC-NET-003 · SPEC-PROBE-IP-001 …
//! SPEC-PROBE-IP-032

mod error;
pub mod fec;
mod receiver;
mod rtp;
mod rtp_seq;
pub mod source;
mod stop;
pub mod timing;
mod url;

pub use error::{NetError, NetEvent, RtpEvent};
pub use fec::{FecAxis, FecHeader, FecMatrix, FecMatrixHint, FEC_HEADER_LEN};
pub use receiver::{fec_ports, ReceiverConfig, UdpReceiver, UNSPECIFIED_SOURCE};
pub use rtp::{
    ts_payload_shape, RtpHeader, RtpStripper, TsPayloadShape, RTP_FIXED_HEADER_LEN, RTP_PT_MPEGTS,
    TS_PACKET_LEN, TS_SYNC_BYTE,
};
pub use rtp_seq::{RtpSeqCounters, RtpSeqState, SeqOutcome, MAX_DROPOUT, MAX_MISORDER};
pub use source::{
    Datagram, PacketSource, SocketSource, SocketSourceConfig, SourceBinding, SourceCapabilities,
};
pub use stop::{StopHandle, StopToken};
pub use timing::{
    burstiness, expected_iat_us, IatSummary, IatWindow, LogHistogram, NoiseCalibration,
    Rfc3550Jitter, Welford, HIST_BUCKETS,
};
pub use url::StreamUrl;
