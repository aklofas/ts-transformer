//! `From<…> for BindingError` — the HLS rows of the kind table (spec §3.3);
//! see `tst-srt/src/binding_kind.rs` for the why.

use crate::error::HlsError;
use tst_pipeline::binding::{BindingError, BindingErrorKind as K};

#[cfg(feature = "serve")]
impl From<crate::url::HlsUrlError> for BindingError {
    fn from(e: crate::url::HlsUrlError) -> Self {
        BindingError::new(K::HlsUrl, e.to_string())
    }
}

impl From<HlsError> for BindingError {
    fn from(e: HlsError) -> Self {
        let kind = match &e {
            #[cfg(feature = "serve")]
            HlsError::Url(_) => K::HlsUrl,
            HlsError::Io(_) => K::HlsIo,
            HlsError::BindFailed(_) => K::HlsBindFailed,
            HlsError::InvalidConfig(_) => K::HlsInvalidConfig,
            HlsError::UnalignedPushTs { .. } => K::HlsUnalignedPushTs,
            HlsError::Finished => K::HlsFinished,
            HlsError::TlsDisabled => K::HlsTlsDisabled,
            #[cfg(feature = "tls")]
            HlsError::Tls(_) => K::HlsTls,
            HlsError::Internal(_) => K::Internal,
        };
        BindingError::new(kind, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use crate::error::HlsError;
    use std::io;
    use tst_pipeline::binding::{BindingError, BindingErrorKind as K};

    #[test]
    fn hls_kinds() {
        assert_eq!(
            BindingError::from(HlsError::Io(io::Error::other("disk"))).kind,
            K::HlsIo
        );
        assert_eq!(
            BindingError::from(HlsError::BindFailed("in use".into())).kind,
            K::HlsBindFailed
        );
        assert_eq!(
            BindingError::from(HlsError::InvalidConfig("x".into())).kind,
            K::HlsInvalidConfig
        );
        assert_eq!(
            BindingError::from(HlsError::UnalignedPushTs { len: 187 }).kind,
            K::HlsUnalignedPushTs
        );
        assert_eq!(BindingError::from(HlsError::Finished).kind, K::HlsFinished);
        assert_eq!(
            BindingError::from(HlsError::TlsDisabled).kind,
            K::HlsTlsDisabled
        );
        assert_eq!(
            BindingError::from(HlsError::Internal("hyper".into())).kind,
            K::Internal
        );
        assert_eq!(K::HlsBindFailed.c_projection(), -34);
    }
}
