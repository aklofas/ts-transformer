//! Error types for the `hls` module.

use std::io;

use thiserror::Error;

/// All errors returned by the HLS publisher. A URL error is `HlsUrlError`,
/// returned by `HlsPublisherBuilder::from_url`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HlsError {
    #[error("filesystem I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("HTTP server bind failed: {0}")]
    BindFailed(String),

    #[error("HLS configuration invalid: {0}")]
    InvalidConfig(String),

    #[error("payload must be a multiple of 188 bytes; got {len}")]
    UnalignedPushTs { len: usize },

    #[error("publisher already finished")]
    Finished,

    #[error("TLS feature disabled but `hls+tls` requested")]
    TlsDisabled,

    #[error("TLS setup error: {0}")]
    #[cfg(feature = "tls")]
    Tls(String),

    #[error("internal HTTP server error: {0}")]
    Internal(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unaligned_push_carries_len() {
        let e = HlsError::UnalignedPushTs { len: 187 };
        assert!(matches!(e, HlsError::UnalignedPushTs { len: 187 }));
        assert!(e.to_string().contains("187"));
    }
}
