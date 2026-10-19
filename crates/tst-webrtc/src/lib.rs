#![doc = include_str!("../README.md")]
//!
//! The server's closing contract is documented under the `# Closing`
//! heading of the server type.
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

/// The server's internals. Public only for the crate's integration tests
/// and fuzz targets; not a supported API.
#[doc(hidden)]
pub mod server {
    pub mod http;
    #[expect(
        dead_code,
        reason = "the server's bind spawns the media thread; until then only the waker is used"
    )]
    pub(crate) mod media;
    pub mod negotiate;
    #[expect(
        dead_code,
        reason = "the media thread's sessions; spawned by the server's bind"
    )]
    pub(crate) mod session;
}

#[cfg(feature = "tls")]
mod tls;

#[doc(hidden)]
pub use clock::ManualClock;
pub use clock::{Clock, SystemClock};
pub use config::{AudioPolicy, KlvCarriage, MountConfig, MountKind, VideoCodec};
pub use error::{PushError, WhepServerError};
pub use mount::{KLV_DC_LIMIT, MountHandle};
pub use stats::{IceState, MountStats, ServerStats, SessionStats};

/// Installs str0m's process-wide crypto provider. The first install wins and
/// later calls are ignored, so calling this on every bind is safe.
#[expect(
    dead_code,
    reason = "called by the server's bind, which is not in the crate yet"
)]
pub(crate) fn ensure_crypto() {
    str0m::crypto::from_feature_flags().install_process_default();
}
