//! `From<…> for BindingError` — the WebRTC rows of the kind table; see
//! `tst-srt/src/binding_kind.rs` for the why.
//!
//! Both matches are exhaustive and wildcard-free, so a new variant is a
//! compile error here until it has a kind.

use crate::error::{PushError, WhepServerError};
use tst_pipeline::binding::{BindingError, BindingErrorKind as K};

/// The mount-management variants share one kind; a call after `stop` is
/// the one-cancel-outcome kind `Closed`.
impl From<WhepServerError> for BindingError {
    fn from(e: WhepServerError) -> Self {
        let kind = match &e {
            WhepServerError::InvalidUrl(_) => K::WebrtcUrl,
            WhepServerError::Bind(_) => K::WebrtcIo,
            WhepServerError::Tls(_) => K::WebrtcTls,
            WhepServerError::MountExists { .. }
            | WhepServerError::MountNotFound { .. }
            | WhepServerError::InvalidMountPath { .. }
            | WhepServerError::InvalidConfig { .. } => K::WebrtcMount,
            WhepServerError::Shutdown => K::Closed,
        };
        BindingError::new(kind, e.to_string())
    }
}

/// A push into a closed mount is `Closed`; every refused input is `Push`.
impl From<PushError> for BindingError {
    fn from(e: PushError) -> Self {
        let kind = match &e {
            PushError::Closed => K::Closed,
            PushError::WrongMountKind
            | PushError::InvalidAccessUnit { .. }
            | PushError::InvalidAudioFrame
            | PushError::KlvTooLarge { .. } => K::WebrtcPush,
        };
        BindingError::new(kind, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use crate::error::{PushError, WhepServerError};
    use std::io;
    use tst_core::url::UrlError;
    use tst_pipeline::binding::{BindingError, BindingErrorKind as K};

    /// The kind, its C projection, and that the message is the error's
    /// display text.
    fn check(e: impl Into<BindingError> + ToString, kind: K, c: i32) {
        let text = e.to_string();
        let b: BindingError = e.into();
        assert_eq!(b.kind, kind, "{text}");
        assert_eq!(b.kind.c_projection(), c, "{text}");
        assert_eq!(b.detail, text);
    }

    #[test]
    fn server_error_kinds() {
        check(
            WhepServerError::InvalidUrl(UrlError::MissingSchemeSeparator),
            K::WebrtcUrl,
            -35,
        );
        check(
            WhepServerError::Bind(io::ErrorKind::AddrInUse),
            K::WebrtcIo,
            -34,
        );
        check(WhepServerError::Tls("no key".into()), K::WebrtcTls, -37);
        check(
            WhepServerError::MountExists { path: "/a".into() },
            K::WebrtcMount,
            -35,
        );
        check(
            WhepServerError::MountNotFound { path: "/a".into() },
            K::WebrtcMount,
            -35,
        );
        check(
            WhepServerError::InvalidMountPath {
                detail: "//".into(),
            },
            K::WebrtcMount,
            -35,
        );
        check(
            WhepServerError::InvalidConfig { detail: "x".into() },
            K::WebrtcMount,
            -35,
        );
        check(WhepServerError::Shutdown, K::Closed, -7);
    }

    #[test]
    fn push_error_kinds() {
        check(PushError::Closed, K::Closed, -7);
        check(PushError::WrongMountKind, K::WebrtcPush, -35);
        check(
            PushError::InvalidAccessUnit {
                detail: "empty".into(),
            },
            K::WebrtcPush,
            -35,
        );
        check(PushError::InvalidAudioFrame, K::WebrtcPush, -35);
        check(
            PushError::KlvTooLarge {
                len: 300_000,
                limit: 262_144,
            },
            K::WebrtcPush,
            -35,
        );
    }

    /// The kinds without a variant mapping here keep the projection their
    /// rows declare.
    #[test]
    fn server_kind_projects_like_hls_bind_failed() {
        assert_eq!(
            K::WebrtcServer.c_projection(),
            K::HlsBindFailed.c_projection()
        );
    }
}
