#![doc = include_str!("../README.md")]
//!
//! The server's closing contract is documented under the `# Closing`
//! heading of the server type.
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
pub use mount::MountHandle;
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
