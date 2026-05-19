//! Crate `net` — recepção UDP/RTP multicast.
//!
//! SPEC-NET-001 · SPEC-NET-002 · SPEC-NET-003

mod error;
mod receiver;
mod rtp;
mod stop;
mod url;

pub use error::{NetError, NetEvent, RtpEvent};
pub use receiver::{ReceiverConfig, UdpReceiver};
pub use rtp::RtpStripper;
pub use stop::{StopHandle, StopToken};
pub use url::StreamUrl;
