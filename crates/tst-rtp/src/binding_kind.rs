//! `From<…> for BindingError` — the RTP / RTSP rows of the kind
//! table; see `tst-srt/src/binding_kind.rs` for the why.

use crate::error::{MountError, RtspError, RtspServerError};
use crate::transport::RtpConnectError;
use tst_pipeline::binding::{BindingError, BindingErrorKind as K};

/// Each of the six `RtpConnectError` variants is its own kind (C keeps −15
/// for all of them).
impl From<RtpConnectError> for BindingError {
    fn from(e: RtpConnectError) -> Self {
        let kind = match &e {
            RtpConnectError::PayloadTypeParam => K::RtpPayloadTypeParam,
            RtpConnectError::MissingPayloadTypeParam => K::RtpMissingPayloadTypeParam,
            RtpConnectError::Url(_) => K::RtpUrl,
            RtpConnectError::HostNotLiteral { .. } => K::RtpHostNotLiteral,
            RtpConnectError::Io(_) => K::RtpIo,
            RtpConnectError::IfaceUnsupported { .. } => K::RtpIfaceUnsupported,
        };
        BindingError::new(kind, e.to_string())
    }
}

/// RTSP keeps its ten-name classification, and the finer bucket wins:
/// 401/404 are `AUTH_REQUIRED`/`NOT_FOUND`, `AuthUnsupported` is
/// `AUTH_REQUIRED`, the four SDP-media variants are `NOT_FOUND`.
/// `LocalCancel` is the one-cancel-outcome kind
/// `Closed`: a cancelled request (OPTIONS, DESCRIBE, ANNOUNCE, SETUP, PLAY,
/// RECORD, PAUSE, TEARDOWN) is `TST_E_CLOSED` / `RtspError(CLOSED)` /
/// `RtspException(CLOSED)`, distinguishable from every other `RtspError`.
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
            RtspError::NotAllowed { .. } => K::RtspProtocol,
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
            | RtspServerError::MountNotFound { .. }
            | RtspServerError::InvalidConfig { .. } => K::RtspMount,
            // The server's own configuration was refused and nothing was
            // built: the cross-domain configuration kind, as a refused URL
            // key or muxer config reads.
            RtspServerError::InvalidServerConfig { .. } => K::ConfigInvalid,
            RtspServerError::AlreadyStarted | RtspServerError::NotStarted => K::RtspServer,
            // An ended server (stopped, hard-cancelled or dropped) is the
            // closed-handle outcome every binding reports for a call after
            // close or cancel.
            RtspServerError::Shutdown => K::Closed,
            // Same one-cancel-outcome kind as `RtspError::LocalCancel`: a
            // publish mount's application transport was already taken by
            // another clone of the handle, not a wire failure.
            RtspServerError::TransportTaken => K::Closed,
        };
        BindingError::new(kind, e.to_string())
    }
}

#[cfg(feature = "rtsp-server")]
impl From<crate::rtsp::server::mount_rules::MountRuleError> for BindingError {
    fn from(e: crate::rtsp::server::mount_rules::MountRuleError) -> Self {
        use crate::rtsp::server::mount_rules::MountRuleError as E;
        let kind = match &e {
            E::InvalidMountPath { .. }
            | E::EmptyCredentials
            | E::DuplicateUsername { .. }
            | E::InvalidRealm { .. } => K::RtspMount,
            E::Closed => K::Closed,
        };
        BindingError::new(kind, e.to_string())
    }
}

impl From<MountError> for BindingError {
    fn from(e: MountError) -> Self {
        let kind = match &e {
            MountError::Mux(_) => K::RtspMount,
            // The server ended (or the mount's lock is poisoned): the
            // terminal outcome, as a WHEP push after the server stopped.
            MountError::Closed => K::Closed,
        };
        BindingError::new(kind, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use crate::error::{MountError, RtspError, RtspServerError};
    use crate::transport::RtpConnectError;
    use std::io;
    use tst_pipeline::binding::{BindingError, BindingErrorKind as K};

    #[test]
    fn rtp_connect_precision() {
        assert_eq!(
            BindingError::from(RtpConnectError::PayloadTypeParam).kind,
            K::RtpPayloadTypeParam
        );
        assert_eq!(
            BindingError::from(RtpConnectError::MissingPayloadTypeParam).kind,
            K::RtpMissingPayloadTypeParam
        );
        assert_eq!(
            BindingError::from(RtpConnectError::HostNotLiteral {
                host: "h".into(),
                detail: "d".into()
            })
            .kind,
            K::RtpHostNotLiteral
        );
        assert_eq!(
            BindingError::from(RtpConnectError::Io(io::Error::new(
                io::ErrorKind::AddrInUse,
                "x"
            )))
            .kind,
            K::RtpIo
        );
        assert_eq!(
            BindingError::from(RtpConnectError::IfaceUnsupported {
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
    fn not_allowed_maps_to_rtsp_protocol() {
        assert_eq!(
            BindingError::from(RtspError::NotAllowed {
                detail: "RECORD while recording"
            })
            .kind,
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
            BindingError::from(RtspServerError::MountNotFound { path: "/a".into() }).kind,
            K::RtspMount
        );
        assert_eq!(
            BindingError::from(RtspServerError::AlreadyStarted).kind,
            K::RtspServer
        );
        assert_eq!(
            BindingError::from(RtspServerError::InvalidConfig { detail: "x".into() }).kind,
            K::RtspMount
        );
        assert_eq!(
            BindingError::from(RtspServerError::TransportTaken).kind,
            K::Closed
        );
        assert_eq!(BindingError::from(MountError::Closed).kind, K::Closed);
        assert_eq!(
            BindingError::from(MountError::Mux(tst_core::error::MuxError::InvalidNal)).kind,
            K::RtspMount
        );
    }

    /// A refused server configuration is the configuration kind every
    /// binding already has, with a text that names the server.
    #[test]
    fn a_refused_server_configuration_is_the_configuration_kind() {
        let e = BindingError::from(RtspServerError::InvalidServerConfig {
            detail: "bind URL".into(),
        });
        assert_eq!(e.kind, K::ConfigInvalid);
        assert_eq!(e.detail, "invalid server config: bind URL");
        assert_eq!(K::ConfigInvalid.c_projection(), -1);
    }

    #[test]
    fn an_ended_server_and_its_mount_rules_are_closed() {
        assert_eq!(
            BindingError::from(RtspServerError::Shutdown).kind,
            K::Closed
        );
        assert_eq!(
            BindingError::from(RtspServerError::NotStarted).kind,
            K::RtspServer
        );
        #[cfg(feature = "rtsp-server")]
        {
            use crate::rtsp::server::mount_rules::MountRuleError;
            assert_eq!(BindingError::from(MountRuleError::Closed).kind, K::Closed);
            assert_eq!(
                BindingError::from(MountRuleError::EmptyCredentials).kind,
                K::RtspMount
            );
        }
    }
}
