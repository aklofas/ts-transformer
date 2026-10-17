//! Error types for the `tst-udp` crate.

use std::io;

use thiserror::Error;

/// All errors that can be returned from `tst-udp` operations. The bindings
/// map them through `BindingError` (see `binding_kind.rs`).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum UdpError {
    #[error("URL parse failed: {0}")]
    Url(#[from] tst_core::url::UrlError),

    #[error("UDP socket I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_wraps() {
        let io_err = io::Error::new(io::ErrorKind::TimedOut, "tick");
        let u: UdpError = io_err.into();
        assert!(matches!(u, UdpError::Io(_)));
        assert!(u.to_string().contains("UDP socket I/O error"));
    }
}
