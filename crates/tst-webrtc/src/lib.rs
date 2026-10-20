#![doc = include_str!("../README.md")]
//!
//! The server's closing contract is documented under the `# Closing`
//! heading of [`WhepServer`].
//!
//! A KLV record travels on the data channel when it is at most
//! [`KLV_DC_LIMIT`] bytes (131 059, what the WebRTC stack can send in one
//! message); a larger one is refused by `push_klv`. Browsers state the
//! largest message they accept (`max-message-size`, 64 KiB when unstated):
//! a record between that size and the limit depends on the browser, and
//! when a viewer's data channel drops it the drop is counted in
//! [`MountStats::klv_dc_dropped_viewer`] and
//! [`SessionStats::dc_messages_dropped`]. The video is never held back for
//! the data channel.
#![forbid(unsafe_code)]

pub mod clock;
pub mod config;
pub mod error;
pub mod mount;
pub mod packetize;
pub mod sei;
pub mod stats;
pub mod url;

pub mod server;

mod binding_kind;

#[cfg(feature = "tls")]
mod tls;

#[doc(hidden)]
pub use clock::ManualClock;
pub use clock::{Clock, SystemClock};
pub use config::{AudioPolicy, KlvCarriage, MountConfig, MountKind, VideoCodec};
pub use error::{PushError, WhepServerError};
pub use mount::{KLV_DC_LIMIT, MountHandle};
pub use server::WhepServer;
pub use stats::{IceState, MountStats, ServerStats, SessionStats};

/// Installs str0m's process-wide crypto provider. The first install wins and
/// later calls are ignored, so calling this on every bind is safe.
pub(crate) fn ensure_crypto() {
    str0m::crypto::from_feature_flags().install_process_default();
}
