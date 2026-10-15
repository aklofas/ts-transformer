//! Error types for the `tst-tcp` crate.

use std::io;

use thiserror::Error;

/// All errors that can be returned from `tst-tcp` operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TcpError {
    #[error("URL parse failed: {0}")]
    Url(#[from] crate::url::TcpUrlError),

    #[error("TCP I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("transport closed by caller")]
    Closed,

    #[error("connection timed out after {seconds}s")]
    ConnectTimeout { seconds: u64 },

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("TLS error: {0}")]
    #[cfg(feature = "tls")]
    Tls(String),

    #[error("TLS feature disabled but URL uses 'tcps://'")]
    TlsDisabled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_wraps() {
        let io_err = io::Error::new(io::ErrorKind::ConnectionRefused, "no");
        let t: TcpError = io_err.into();
        assert!(matches!(t, TcpError::Io(_)));
        assert!(t.to_string().contains("TCP I/O error"));
    }
}
