//! `From<…> for BindingError` — the RTP / RTSP rows of the kind
//! table; see `tst-srt/src/binding_kind.rs` for the why.

use crate::error::{MountError, RtspError, RtspServerError};
use crate::transport::ConnectError;
use tst_pipeline::binding::{BindingError, BindingErrorKind as K};

/// Each of the six `ConnectError` variants is its own kind (C keeps −15
/// for all of them).
impl From<ConnectError> for BindingError {
    fn from(e: ConnectError) -> Self {
        let kind = match &e {
            ConnectError::PayloadTypeParam => K::RtpPayloadTypeParam,
            ConnectError::MissingPayloadTypeParam => K::RtpMissingPayloadTypeParam,
            ConnectError::Url(_) => K::RtpUrl,
            ConnectError::HostNotLiteral { .. } => K::RtpHostNotLiteral,
            ConnectError::Io(_) => K::RtpIo,
            ConnectError::IfaceUnsupported { .. } => K::RtpIfaceUnsupported,
        };
        BindingError::new(kind, e.to_string())
    }
}

/// RTSP keeps its ten-name classification, and the finer bucket wins:
/// 401/404 are `AUTH_REQUIRED`/`NOT_FOUND`, `AuthUnsupported` is
/// `AUTH_REQUIRED`, the four SDP-media variants are `NOT_FOUND`.
/// `LocalCancel` is the one-cancel-outcome kind
/// `Closed`: a cancelled connect/play/pause/teardown is
/// `TST_E_CLOSED` / `RtspError(CLOSED)` / `RtspException(CLOSED)`,
/// indistinguishable from no other RTSP failure.
impl From<RtspError> for BindingError {
    fn from(e: RtspError) -> Self {
        let kind = match &e {
            RtspError::Io(_) => K::RtspIo,
            RtspError::Tls(_) => K::RtspTls,
            RtspError::Protocol { code: 404, .. } => K::RtspNotFound,
            RtspError::Protocol { code: 401, .. } => K::RtspAuthRequired,
            RtspError::Protocol { .. } => K::RtspProtocol,
            RtspError::AuthFailed => K::RtspAuthFailed,
            RtspError::AuthUnsupported { .. } => K::RtspAuthRequired,
            RtspError::BadResponse { .. } => K::RtspProtocol,
            RtspError::BadSdp { .. } => K::RtspProtocol,
            RtspError::UnsupportedTransport => K::RtspUnsupportedTransport,
            RtspError::SessionExpired => K::RtspProtocol,
            RtspError::Timeout => K::RtspTimeout,
            RtspError::LocalCancel => K::Closed,
            RtspError::NoMp2tMedia => K::RtspNotFound,
            RtspError::MultipleMp2tMedia { .. } => K::RtspNotFound,
            RtspError::NoH264Media => K::RtspNotFound,
            RtspError::MultipleH264Media { .. } => K::RtspNotFound,
            RtspError::UnsupportedPacketizationMode(_) => K::RtspUnsupportedTransport,
            RtspError::InvalidHeader { .. } => K::RtspProtocol,
            RtspError::Url(_) => K::RtspProtocol,
        };
        BindingError::new(kind, e.to_string())
    }
}

/// Server errors take the client-side buckets where one applies.
impl From<RtspServerError> for BindingError {
    fn from(e: RtspServerError) -> Self {
        let kind = match &e {
            RtspServerError::Io(_) | RtspServerError::BindAddrInUse => K::RtspIo,
            RtspServerError::Tls(_) => K::RtspTls,
            RtspServerError::UrlParse(_) => K::RtspProtocol,
            RtspServerError::InvalidMountPath { .. }
            | RtspServerError::InvalidMulticastGroup { .. }
            | RtspServerError::DuplicateMount { .. }
            | RtspServerError::InvalidConfig { .. } => K::RtspMount,
            RtspServerError::AlreadyStarted
            | RtspServerError::NotStarted
            | RtspServerError::Shutdown => K::RtspServer,
            // Same one-cancel-outcome kind as `RtspError::LocalCancel`: a
            // publish mount's application transport was already taken by
            // another clone of the handle, not a wire failure.
            RtspServerError::TransportTaken => K::Closed,
        };
        BindingError::new(kind, e.to_string())
    }
}

impl From<MountError> for BindingError {
    fn from(e: MountError) -> Self {
        let kind = match &e {
            MountError::Mux(_) | MountError::Closed => K::RtspMount,
        };
        BindingError::new(kind, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use crate::error::{MountError, RtspError, RtspServerError};
    use crate::transport::ConnectError;
    use std::io;
    use tst_pipeline::binding::{BindingError, BindingErrorKind as K};

    #[test]
    fn rtp_connect_precision() {
        assert_eq!(
            BindingError::from(ConnectError::PayloadTypeParam).kind,
            K::RtpPayloadTypeParam
        );
        assert_eq!(
            BindingError::from(ConnectError::MissingPayloadTypeParam).kind,
            K::RtpMissingPayloadTypeParam
        );
        assert_eq!(
            BindingError::from(ConnectError::HostNotLiteral {
                host: "h".into(),
                detail: "d".into()
            })
            .kind,
            K::RtpHostNotLiteral
        );
        assert_eq!(
            BindingError::from(ConnectError::Io(io::Error::new(
                io::ErrorKind::AddrInUse,
                "x"
            )))
            .kind,
            K::RtpIo
        );
        assert_eq!(
            BindingError::from(ConnectError::IfaceUnsupported {
                iface: "eth0".into(),
                detail: "d".into()
            })
            .kind,
            K::RtpIfaceUnsupported
        );
        assert_eq!(K::RtpIo.c_projection(), -15);
    }

    #[test]
    fn rtsp_classification_with_status_splits() {
        assert_eq!(
            BindingError::from(RtspError::Io(io::ErrorKind::ConnectionReset)).kind,
            K::RtspIo
        );
        assert_eq!(
            BindingError::from(RtspError::Tls("cert".into())).kind,
            K::RtspTls
        );
        assert_eq!(
            BindingError::from(RtspError::Protocol {
                code: 404,
                reason: "Not Found".into()
            })
            .kind,
            K::RtspNotFound
        );
        assert_eq!(
            BindingError::from(RtspError::Protocol {
                code: 401,
                reason: "Unauthorized".into()
            })
            .kind,
            K::RtspAuthRequired
        );
        assert_eq!(
            BindingError::from(RtspError::Protocol {
                code: 500,
                reason: "x".into()
            })
            .kind,
            K::RtspProtocol
        );
        assert_eq!(
            BindingError::from(RtspError::AuthFailed).kind,
            K::RtspAuthFailed
        );
        assert_eq!(
            BindingError::from(RtspError::AuthUnsupported {
                scheme: "ntlm".into()
            })
            .kind,
            K::RtspAuthRequired
        );
        assert_eq!(
            BindingError::from(RtspError::UnsupportedTransport).kind,
            K::RtspUnsupportedTransport
        );
        assert_eq!(
            BindingError::from(RtspError::UnsupportedPacketizationMode(2)).kind,
            K::RtspUnsupportedTransport
        );
        assert_eq!(
            BindingError::from(RtspError::SessionExpired).kind,
            K::RtspProtocol
        );
        assert_eq!(BindingError::from(RtspError::Timeout).kind, K::RtspTimeout);
        assert_eq!(BindingError::from(RtspError::LocalCancel).kind, K::Closed);
        assert_eq!(
            BindingError::from(RtspError::NoMp2tMedia).kind,
            K::RtspNotFound
        );
        assert_eq!(
            BindingError::from(RtspError::MultipleH264Media { count: 2 }).kind,
            K::RtspNotFound
        );
        assert_eq!(
            BindingError::from(RtspError::InvalidHeader { detail: "CR" }).kind,
            K::RtspProtocol
        );
    }

    #[test]
    fn rtsp_server_and_mount() {
        assert_eq!(
            BindingError::from(RtspServerError::BindAddrInUse).kind,
            K::RtspIo
        );
        assert_eq!(
            BindingError::from(RtspServerError::Tls("x".into())).kind,
            K::RtspTls
        );
        assert_eq!(
            BindingError::from(RtspServerError::DuplicateMount { path: "/a".into() }).kind,
            K::RtspMount
        );
        assert_eq!(
            BindingError::from(RtspServerError::AlreadyStarted).kind,
            K::RtspServer
        );
        assert_eq!(
            BindingError::from(RtspServerError::Shutdown).kind,
            K::RtspServer
        );
        assert_eq!(
            BindingError::from(RtspServerError::TransportTaken).kind,
            K::Closed
        );
        assert_eq!(BindingError::from(MountError::Closed).kind, K::RtspMount);
        assert_eq!(
            BindingError::from(MountError::Mux(tst_core::error::MuxError::InvalidNal)).kind,
            K::RtspMount
        );
    }
}
