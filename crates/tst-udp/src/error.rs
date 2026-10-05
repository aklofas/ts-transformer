//! Error types for the `tst-udp` crate.

use std::io;

use thiserror::Error;

/// All errors that can be returned from `tst-udp` operations.
///
/// The `kind()` accessor projects to [`UdpErrorKind`]. The bindings do not
/// read it; they map `UdpError` through `BindingError` (see
/// `binding_kind.rs`).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum UdpError {
    #[error("URL parse failed: {0}")]
    Url(#[from] crate::url::UdpUrlError),

    #[error("UDP socket I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
}

/// Flat error-variant projection with a stable numeric code per variant.
/// The bindings do not read it; they map `UdpError` through
/// `BindingError` (see `binding_kind.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u32)]
pub enum UdpErrorKind {
    Url = 1,
    Io = 3,
    InvalidConfig = 7,
}

impl UdpError {
    /// Project to a flat variant for C-ABI mapping. Stable numeric codes.
    pub fn kind(&self) -> UdpErrorKind {
        match self {
            Self::Url(_) => UdpErrorKind::Url,
            Self::Io(_) => UdpErrorKind::Io,
            Self::InvalidConfig(_) => UdpErrorKind::InvalidConfig,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_is_stable() {
        // If anyone reorders the enum, this catches the numeric drift before
        // the bindings ratchet flags it.
        assert_eq!(UdpErrorKind::Url as u32, 1);
        assert_eq!(UdpErrorKind::InvalidConfig as u32, 7);
    }

    #[test]
    fn io_error_wraps() {
        let io_err = io::Error::new(io::ErrorKind::TimedOut, "tick");
        let u: UdpError = io_err.into();
        assert_eq!(u.kind(), UdpErrorKind::Io);
        assert!(u.to_string().contains("UDP socket I/O error"));
    }
}
