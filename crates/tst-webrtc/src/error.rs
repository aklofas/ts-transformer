//! Errors returned by the WHEP server and by a mount's push calls.

use tst_core::url::UrlError;

/// A server-level failure: binding, TLS, mount management, or a call after
/// shutdown.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WhepServerError {
    /// The `whep://` / `wheps://` bind URL did not parse.
    #[error("WHEP bind URL error: {0}")]
    InvalidUrl(#[from] UrlError),
    /// The listening socket could not be bound.
    #[error("bind failed: {0:?}")]
    Bind(std::io::ErrorKind),
    /// The certificate or key could not be loaded, or TLS setup failed.
    #[error("TLS error: {0}")]
    Tls(String),
    /// A mount already exists at this path.
    #[error("duplicate mount path '{path}'")]
    MountExists {
        /// The path that is already mounted.
        path: String,
    },
    /// No mount exists at this path.
    #[error("no mount at path '{path}'")]
    MountNotFound {
        /// The path that was looked up.
        path: String,
    },
    /// The mount path is not a valid path.
    #[error("invalid mount path: {detail}")]
    InvalidMountPath {
        /// Which rule the path broke.
        detail: String,
    },
    /// The mount configuration is out of range or unsupported.
    #[error("invalid mount config: {detail}")]
    InvalidConfig {
        /// Which field was rejected and why.
        detail: String,
    },
    /// The server has been shut down.
    #[error("WHEP server has been shut down")]
    Shutdown,
}

/// A failure pushing media into a mount.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PushError {
    /// The mount was removed or the server shut down.
    #[error("mount closed")]
    Closed,
    /// The push call does not match the mount's kind (elementary or MP2T).
    #[error("wrong push family for this mount kind")]
    WrongMountKind,
    /// The access unit could not be used.
    #[error("invalid access unit: {detail}")]
    InvalidAccessUnit {
        /// What was wrong with it.
        detail: String,
    },
    /// The audio frame does not fit the mount's audio policy.
    #[error("invalid audio frame for the mount's audio policy")]
    InvalidAudioFrame,
    /// A KLV record is larger than one data-channel message may be.
    #[error("KLV record of {len} bytes exceeds the {limit}-byte data-channel limit")]
    KlvTooLarge {
        /// The record's length in bytes.
        len: u64,
        /// The largest record a data-channel message carries.
        limit: u64,
    },
}
