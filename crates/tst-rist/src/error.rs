//! Error types for the `tst-rist` crate.

use thiserror::Error;

/// All errors returned from `tst-rist` operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RistError {
    #[error("URL parse failed: {0}")]
    Url(#[from] crate::url::RistUrlError),

    #[error("librist FFI error: code={code}, fn={function}")]
    Ffi { code: i32, function: &'static str },

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("encryption requested but `mbedtls` feature is disabled")]
    EncryptionDisabled,

    #[error("librist context creation failed")]
    ContextCreateFailed,

    #[error("librist peer creation failed")]
    PeerCreateFailed,
}

/// Flat error-kind projection with a stable numeric code per variant.
/// The bindings do not read it; they map `RistError` through
/// `BindingError` (see `binding_kind.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u32)]
pub enum RistErrorKind {
    Url = 1,
    Ffi = 2,
    InvalidConfig = 5,
    EncryptionDisabled = 6,
    ContextCreateFailed = 7,
    PeerCreateFailed = 8,
}

impl RistError {
    pub fn kind(&self) -> RistErrorKind {
        match self {
            Self::Url(_) => RistErrorKind::Url,
            Self::Ffi { .. } => RistErrorKind::Ffi,
            Self::InvalidConfig(_) => RistErrorKind::InvalidConfig,
            Self::EncryptionDisabled => RistErrorKind::EncryptionDisabled,
            Self::ContextCreateFailed => RistErrorKind::ContextCreateFailed,
            Self::PeerCreateFailed => RistErrorKind::PeerCreateFailed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_is_stable() {
        assert_eq!(RistErrorKind::Url as u32, 1);
        assert_eq!(RistErrorKind::PeerCreateFailed as u32, 8);
    }

    #[test]
    fn ffi_error_carries_code_and_function() {
        let e = RistError::Ffi {
            code: -3,
            function: "rist_peer_create",
        };
        assert_eq!(e.kind(), RistErrorKind::Ffi);
        assert!(e.to_string().contains("-3"));
        assert!(e.to_string().contains("rist_peer_create"));
    }
}
