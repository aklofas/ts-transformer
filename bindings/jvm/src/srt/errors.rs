//! `org.tstrans.srt` Rust→Java error mapping.
//!
//! Since Arc 2 WP-B3 this file holds no mapping tables: every kind comes from
//! `tst_pipeline::binding` (A2's one table) and is raised through the single
//! [`crate::error::throw_binding`] path. What remains is the per-error-type
//! plumbing that names `Domain::Srt` and picks the right `From` impl.

use jni::JNIEnv;
use tst_core::transport::TransportError;
use tst_pipeline::ShellErrorKind;
use tst_pipeline::binding::{BindingError, BindingErrorKind};
use tst_pipeline::receiver::{ReceiverError, ReceiverErrorSource};
use tst_pipeline::sender::{SenderError, SenderErrorSource};
use tst_srt::{SrtError, UrlError};

use crate::error::{Domain, throw_binding};

/// `org.tstrans.SrtException(Kind.<kind.name()>, message)`.
pub(crate) fn throw_srt(env: &mut JNIEnv, kind: BindingErrorKind, message: &str) {
    throw_binding(
        env,
        Domain::Srt,
        &BindingError {
            kind,
            detail: message.to_owned(),
        },
    );
}

/// Any tst-srt per-category error (`ConnectError` / `BindError` /
/// `AcceptError` / `IoError` / `OptionError` are `#[from]` arms of `SrtError`,
/// `crates/tst-srt/src/error.rs`) through A2's one mapping:
/// `InvalidAddress` / `InvalidOption` / `OptionError::*` → `CONFIG_INVALID`,
/// `TimedOut` → `TIMEOUT`, `ListenerClosed` / `SocketClosed` → `CLOSED`, the
/// rest → `CONNECT_FAILED` / `ACCEPT_FAILED` / `IO` — today's buckets exactly.
pub(crate) fn srt_error(env: &mut JNIEnv, e: impl Into<SrtError>) {
    throw_binding(env, Domain::Srt, &BindingError::from(e.into()));
}

/// `SrtUrl::parse` failures are caller misconfiguration by definition;
/// `UrlError` is not an `SrtError` arm, so it is mapped here, once.
pub(crate) fn url_error(env: &mut JNIEnv, e: &UrlError) {
    throw_srt(env, BindingErrorKind::ConfigInvalid, &e.to_string());
}

/// `tst_core::transport::TransportError` (any shell op) through A2's
/// `From<TransportError>`: `Backpressure → BACKPRESSURE`, `Broken → BROKEN`,
/// `Closed → CLOSED`, `ExplicitClose → CLOSED` ("cancelled from another
/// thread"), `TooLarge → TOO_LARGE`. No wildcard here — A2's mapping is
/// exhaustive-before-wildcard and rail-pinned.
pub(crate) fn transport_error(env: &mut JNIEnv, e: &TransportError) {
    throw_binding(env, Domain::Srt, &BindingError::from(e.clone()));
}

/// `tst_pipeline::Sender` errors: the transport arm through
/// [`transport_error`]; a framing error (TS sync lost in the caller's bytes)
/// through A2's `From<TsFramingError>` = `INPUT_MALFORMED` — it was
/// `CONFIG_INVALID` before 0.7.0 (observed change, CHANGELOG).
///
/// Matched on `source` rather than calling A2's `From<SenderError>` only so
/// that it reads as one rule with its receiver twin [`throw_receiver_error`];
/// the two are equivalent on the send side (there is no direction-sensitive
/// `Closed` on a sender — that stays `CLOSED`).
pub(crate) fn throw_sender_error(env: &mut JNIEnv, e: &SenderError) {
    match &e.source {
        SenderErrorSource::Transport(t) => transport_error(env, t),
        SenderErrorSource::Framing(f) => {
            throw_binding(env, Domain::Srt, &BindingError::from(f.clone()));
        }
        // `SenderErrorSource` is #[non_exhaustive]; a future arm keeps its Display text.
        _ => throw_srt(env, BindingErrorKind::SrtIo, &e.to_string()),
    }
}

/// Receive-DIRECTION projection of a transport failure, shared by every srt
/// receiver shell.
///
/// The shell has already classified the error direction-sensitively:
/// `tst_pipeline::shell_error::kind_from_transport(.., Direction::Recv)` turns
/// a `TransportError::Closed` on a RECEIVER into
/// [`ShellErrorKind::EndOfStream`] — the peer hung up cleanly, which on SRT is
/// `srt_recv` returning 0 and the shell converting that to `Closed`
/// (`receiver/mod.rs`'s `n == 0` guard). A2's exhaustive
/// `From<ShellErrorKind> for BindingErrorKind` is the projection; it is how the
/// C ABI reaches `TST_E_END_OF_STREAM` (-12) on the same event, so the JVM
/// declares and raises the same kind instead of folding it into `CLOSED`.
///
/// Every other kind keeps the richer `From<TransportError>` detail (`msg` plus
/// the wire errno), so only the end-of-stream case is special-cased here.
fn throw_recv_transport(env: &mut JNIEnv, kind: ShellErrorKind, t: &TransportError) {
    if matches!(kind, ShellErrorKind::EndOfStream) {
        throw_srt(env, BindingErrorKind::from(kind), "peer closed the stream");
    } else {
        transport_error(env, t);
    }
}

/// `tst_pipeline::Receiver` errors — the transport arm through
/// [`throw_recv_transport`], so a peer's clean hang-up is `END_OF_STREAM`
/// rather than `CLOSED` (0.7.0 change; C has always reported it as -12).
pub(crate) fn throw_receiver_error(env: &mut JNIEnv, e: &ReceiverError) {
    match &e.source {
        ReceiverErrorSource::Transport(t) => throw_recv_transport(env, e.kind, t),
        // `ReceiverErrorSource` is #[non_exhaustive]; a future arm keeps its Display text.
        _ => throw_srt(env, BindingErrorKind::SrtIo, &e.to_string()),
    }
}
