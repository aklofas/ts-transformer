//! `From<…> for BindingError` — the RIST rows of the kind table (spec §3.3);
//! see `tst-srt/src/binding_kind.rs` for the why.

use crate::error::RistError;
use crate::url::RistUrlError;
use tst_pipeline::binding::{BindingError, BindingErrorKind as K};

impl From<RistUrlError> for BindingError {
    fn from(e: RistUrlError) -> Self {
        BindingError::new(K::RistUrl, e.to_string())
    }
}

impl From<RistError> for BindingError {
    fn from(e: RistError) -> Self {
        let kind = match &e {
            RistError::Url(_) => K::RistUrl,
            RistError::Ffi { .. } => K::RistFfi,
            RistError::InvalidConfig(_) => K::RistInvalidConfig,
            RistError::EncryptionDisabled => K::RistEncryptionDisabled,
            RistError::ContextCreateFailed => K::RistContextCreateFailed,
            RistError::PeerCreateFailed => K::RistPeerCreateFailed,
        };
        BindingError::new(kind, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use crate::error::RistError;
    use tst_pipeline::binding::{BindingError, BindingErrorKind as K};

    #[test]
    fn rist_kinds() {
        assert_eq!(
            BindingError::from(RistError::Ffi {
                code: -3,
                function: "rist_peer_create"
            })
            .kind,
            K::RistFfi
        );
        assert_eq!(
            BindingError::from(RistError::InvalidConfig("x".into())).kind,
            K::RistInvalidConfig
        );
        assert_eq!(
            BindingError::from(RistError::EncryptionDisabled).kind,
            K::RistEncryptionDisabled
        );
        assert_eq!(
            BindingError::from(RistError::ContextCreateFailed).kind,
            K::RistContextCreateFailed
        );
        assert_eq!(
            BindingError::from(RistError::PeerCreateFailed).kind,
            K::RistPeerCreateFailed
        );
        assert_eq!(K::RistPeerCreateFailed.c_projection(), -38);
    }
}
