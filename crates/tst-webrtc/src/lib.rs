#![doc = include_str!("../README.md")]
//!
//! The server's closing contract is documented under the `# Closing`
//! heading of the server type.
#![forbid(unsafe_code)]

pub mod config;
pub mod error;

#[cfg(feature = "tls")]
mod tls;

pub use config::{AudioPolicy, KlvCarriage, MountConfig, MountKind, VideoCodec};
pub use error::{PushError, WhepServerError};

/// Installs str0m's process-wide crypto provider. The first install wins and
/// later calls are ignored, so calling this on every bind is safe.
#[expect(
    dead_code,
    reason = "called by the server's bind, which is not in the crate yet"
)]
pub(crate) fn ensure_crypto() {
    str0m::crypto::from_feature_flags().install_process_default();
}
