//! `From<…> for tst_pipeline::binding::BindingError` for every tst-srt error
//! type — the SRT rows of the one kind table. Lives here, not in
//! tst-pipeline, because tst-pipeline must not depend on transport crates;
//! the orphan rule allows it (the source type is local).
//!
//! SRT keeps its eight-name classification (`CONFIG_INVALID`,
//! `CONNECT_FAILED`, `ACCEPT_FAILED`, `TIMEOUT`, `CLOSED`, `BROKEN`,
//! `BACKPRESSURE`, `IO`): `SrtError` is an umbrella over seven sub-enums whose
//! leaf names (`System`, `Other`, `TimedOut` ×4) are not kinds. `Backpressure`
//! is `BACKPRESSURE` (not `WOULD_BLOCK`) and `TooLarge` is `TOO_LARGE` (not
//! `CONFIG_INVALID`). A refused connection's reject reason travels in
//! `Diagnostics::reject_code`.

use crate::error::{
    AcceptError, BindError, IoError, OptionError, RecvError, SendError, SrtConnectError, SrtError,
};
use crate::url::SrtUrlError;
use tst_pipeline::binding::{BindingError, BindingErrorKind as K, Diagnostics};

/// A binding error carrying the reject reason code of a refused connection.
fn rejected(kind: K, detail: String, reject_code: i32) -> BindingError {
    let mut diagnostics = Diagnostics::default();
    diagnostics.reject_code = Some(reject_code);
    BindingError::new(kind, detail).with_diagnostics(diagnostics)
}

/// Every `SrtUrlError` variant is caller misconfiguration (no match: the enum
/// is `#[non_exhaustive]` and there is nothing to discriminate).
impl From<SrtUrlError> for BindingError {
    fn from(e: SrtUrlError) -> Self {
        BindingError::new(K::ConfigInvalid, e.to_string())
    }
}

impl From<OptionError> for BindingError {
    fn from(e: OptionError) -> Self {
        let kind = match &e {
            OptionError::InvalidState
            | OptionError::OutOfRange(_)
            | OptionError::InvalidPassphrase(_)
            | OptionError::InvalidStreamId(_)
            | OptionError::InvalidPacketFilter(_)
            | OptionError::Other { .. } => K::ConfigInvalid,
        };
        BindingError::new(kind, e.to_string())
    }
}

impl From<SrtConnectError> for BindingError {
    fn from(e: SrtConnectError) -> Self {
        let kind = match &e {
            SrtConnectError::InvalidAddress(_) | SrtConnectError::InvalidOption(_) => {
                K::ConfigInvalid
            }
            SrtConnectError::TimedOut => K::SrtTimeout,
            SrtConnectError::Refused
            | SrtConnectError::BadEncryption { .. }
            | SrtConnectError::Rejected { .. }
            | SrtConnectError::System(_)
            | SrtConnectError::Other { .. } => K::SrtConnectFailed,
        };
        if let SrtConnectError::Rejected { reason, .. } = &e {
            return rejected(kind, e.to_string(), reason.raw());
        }
        BindingError::new(kind, e.to_string())
    }
}

impl From<BindError> for BindingError {
    fn from(e: BindError) -> Self {
        let kind = match &e {
            BindError::InvalidAddress(_) | BindError::InvalidOption(_) => K::ConfigInvalid,
            BindError::AddressInUse
            | BindError::PermissionDenied
            | BindError::System(_)
            | BindError::Other { .. } => K::SrtConnectFailed,
        };
        BindingError::new(kind, e.to_string())
    }
}

impl From<AcceptError> for BindingError {
    fn from(e: AcceptError) -> Self {
        let kind = match &e {
            AcceptError::TimedOut => K::SrtTimeout,
            AcceptError::ListenerClosed => K::Closed,
            AcceptError::PeerRejected { .. }
            | AcceptError::System(_)
            | AcceptError::Other { .. } => K::SrtAcceptFailed,
        };
        if let AcceptError::PeerRejected { reason, .. } = &e {
            return rejected(kind, e.to_string(), reason.raw());
        }
        BindingError::new(kind, e.to_string())
    }
}

impl From<SendError> for BindingError {
    fn from(e: SendError) -> Self {
        let kind = match &e {
            SendError::TimedOut => K::SrtTimeout,
            SendError::ConnectionBroken => K::Broken,
            SendError::PayloadTooLarge { .. } => K::TooLarge,
            SendError::QueueFull => K::Backpressure,
            SendError::System(_) | SendError::Other { .. } => K::SrtIo,
        };
        BindingError::new(kind, e.to_string())
    }
}

impl From<RecvError> for BindingError {
    fn from(e: RecvError) -> Self {
        let kind = match &e {
            RecvError::TimedOut => K::SrtTimeout,
            RecvError::ConnectionBroken => K::Broken,
            RecvError::BufferTooSmall { .. } => K::TooLarge,
            RecvError::System(_) | RecvError::Other { .. } => K::SrtIo,
        };
        BindingError::new(kind, e.to_string())
    }
}

impl From<IoError> for BindingError {
    fn from(e: IoError) -> Self {
        let kind = match &e {
            IoError::SocketClosed => K::Closed,
            IoError::System(_) | IoError::Other { .. } => K::SrtIo,
        };
        BindingError::new(kind, e.to_string())
    }
}

/// The umbrella delegates every arm; the tst-core arms reuse tst-pipeline's
/// impls so the SRT crate never restates a mux/demux/KLV bucket.
impl From<SrtError> for BindingError {
    fn from(e: SrtError) -> Self {
        match e {
            SrtError::Connect(e) => e.into(),
            SrtError::Bind(e) => e.into(),
            SrtError::Accept(e) => e.into(),
            SrtError::Send(e) => e.into(),
            SrtError::Recv(e) => e.into(),
            SrtError::Option(e) => e.into(),
            SrtError::Io(e) => e.into(),
            SrtError::KlvDecode(e) => e.into(),
            SrtError::KlvEncode(e) => e.into(),
            SrtError::KlvField(e) => e.into(),
            SrtError::Mux(e) => e.into(),
            SrtError::Demux(e) => e.into(),
            SrtError::Transport(e) => e.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::error::{
        AcceptError, BindError, IoError, RecvError, RejectReason, SendError, SrtConnectError,
        SrtError,
    };
    use std::io;
    use tst_core::transport::TransportError;
    use tst_pipeline::binding::{BindingError, BindingErrorKind as K};

    fn sys() -> io::Error {
        io::Error::other("boom")
    }

    #[test]
    fn connect_bind_accept_classification() {
        assert_eq!(
            BindingError::from(SrtConnectError::TimedOut).kind,
            K::SrtTimeout
        );
        assert_eq!(
            BindingError::from(SrtConnectError::Refused).kind,
            K::SrtConnectFailed
        );
        assert_eq!(
            BindingError::from(SrtConnectError::System(sys())).kind,
            K::SrtConnectFailed
        );
        assert_eq!(
            BindingError::from(BindError::AddressInUse).kind,
            K::SrtConnectFailed
        );
        assert_eq!(
            BindingError::from(AcceptError::TimedOut).kind,
            K::SrtTimeout
        );
        assert_eq!(
            BindingError::from(AcceptError::ListenerClosed).kind,
            K::Closed
        );
        assert_eq!(
            BindingError::from(AcceptError::System(sys())).kind,
            K::SrtAcceptFailed
        );
    }

    #[test]
    fn io_send_recv_classification() {
        assert_eq!(BindingError::from(IoError::SocketClosed).kind, K::Closed);
        assert_eq!(BindingError::from(IoError::System(sys())).kind, K::SrtIo);
        assert_eq!(
            BindingError::from(SendError::QueueFull).kind,
            K::Backpressure
        );
        assert_eq!(
            BindingError::from(SendError::PayloadTooLarge {
                actual: 2000,
                limit: 1316
            })
            .kind,
            K::TooLarge
        );
        assert_eq!(
            BindingError::from(SendError::ConnectionBroken).kind,
            K::Broken
        );
        assert_eq!(
            BindingError::from(RecvError::BufferTooSmall {
                buf_len: 1,
                message_len: 2
            })
            .kind,
            K::TooLarge
        );
        assert_eq!(BindingError::from(RecvError::TimedOut).kind, K::SrtTimeout);
    }

    #[test]
    fn umbrella_delegates() {
        let e: BindingError = SrtError::from(TransportError::ExplicitClose).into();
        assert_eq!(
            (e.kind, e.detail.as_str()),
            (K::Closed, "cancelled from another thread")
        );
        let e: BindingError = SrtError::from(tst_core::error::MuxError::InvalidNal).into();
        assert_eq!(e.kind, K::InvalidNal);
        let e: BindingError = SrtError::from(SrtConnectError::Refused).into();
        assert_eq!(e.kind, K::SrtConnectFailed);
        assert_eq!(K::SrtConnectFailed.name(), "CONNECT_FAILED");
        assert_eq!(K::SrtConnectFailed.variant_name(), "SRT_CONNECT_FAILED");
        assert_eq!(K::SrtConnectFailed.c_projection(), -8);
    }

    fn rejected(reason: RejectReason) -> SrtConnectError {
        SrtConnectError::Rejected {
            reason,
            detail: "test".into(),
        }
    }

    #[test]
    fn a_rejected_connect_carries_its_reject_code() {
        let e = BindingError::from(rejected(RejectReason::Forbidden));
        assert_eq!(e.kind, K::SrtConnectFailed);
        assert_eq!(e.diagnostics.reject_code, Some(1403));
        // Through the umbrella too: the plain shell open's path.
        let e: BindingError = SrtError::from(rejected(RejectReason::Forbidden)).into();
        assert_eq!(e.diagnostics.reject_code, Some(1403));
    }

    #[test]
    fn a_user_defined_reject_code_passes_through() {
        let e = BindingError::from(rejected(RejectReason::Other(4242)));
        assert_eq!(e.diagnostics.reject_code, Some(4242));
    }

    #[test]
    fn an_internal_reject_code_passes_through() {
        let e = BindingError::from(rejected(RejectReason::BadSecret));
        assert_eq!(e.diagnostics.reject_code, Some(10));
    }

    #[test]
    fn a_peer_rejected_accept_carries_its_code() {
        let e = BindingError::from(AcceptError::PeerRejected {
            reason: RejectReason::Forbidden,
            detail: "test".into(),
        });
        assert_eq!(e.kind, K::SrtAcceptFailed);
        assert_eq!(e.diagnostics.reject_code, Some(1403));
        let e: BindingError = SrtError::from(AcceptError::PeerRejected {
            reason: RejectReason::BadSecret,
            detail: "test".into(),
        })
        .into();
        assert_eq!(e.diagnostics.reject_code, Some(10));
    }

    #[test]
    fn other_errors_carry_none() {
        for e in [
            BindingError::from(SrtConnectError::Refused),
            BindingError::from(SrtConnectError::TimedOut),
            BindingError::from(SrtConnectError::BadEncryption {
                detail: "test".into(),
            }),
            BindingError::from(AcceptError::TimedOut),
            BindingError::from(AcceptError::ListenerClosed),
            BindingError::from(BindError::AddressInUse),
            BindingError::from(SendError::ConnectionBroken),
            BindingError::from(IoError::SocketClosed),
            SrtError::from(TransportError::ExplicitClose).into(),
        ] {
            assert_eq!(e.diagnostics.reject_code, None, "{e:?}");
        }
    }
}
