//! Error types for the `tst-rist` crate.

use thiserror::Error;

/// All errors returned from `tst-rist` operations. The bindings map them
/// through `BindingError` (see `binding_kind.rs`).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RistError {
    #[error("URL parse failed: {0}")]
    Url(#[from] tst_core::url::UrlError),

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_error_carries_code_and_function() {
        let e = RistError::Ffi {
            code: -3,
            function: "rist_peer_create",
        };
        assert!(matches!(e, RistError::Ffi { .. }));
        assert!(e.to_string().contains("-3"));
        assert!(e.to_string().contains("rist_peer_create"));
    }
}
