#![doc = include_str!("../README.md")]
//!
//! WebRTC playback for tstrans: a WHEP server with RTSP-style mounts.
//! Browsers pull a mount over WebRTC; video passes through untouched and
//! KLV rides frame-locked in SEI and on a `klv` data channel.
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

/// Installs str0m's process-wide crypto provider once. str0m panics on a
/// second install, so every caller goes through this `Once`. The server calls
/// it when it binds.
#[allow(dead_code)] // the server that calls it is not in the crate yet
pub(crate) fn ensure_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        str0m::crypto::from_feature_flags().install_process_default();
    });
}
