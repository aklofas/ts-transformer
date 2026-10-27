//! Unified shell-layer error vocabulary for binding authors.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Every pipeline shell (`MuxSender`, `Sender`, `RawSender`, `DemuxReceiver`,
//! `Receiver`, `RawReceiver`) exposes a `struct { kind, source }` error
//! type. Bindings categorize failures by matching on `err.kind()` (6
//! variants, 1:1 with TST_E codes); power users `match err.source` for the
//! full inner-error variant set.

use tst_core::transport::TransportError;

/// Lifetime span field shared by the pipeline shells (the sender/receiver
/// families plus [`crate::MuxPublisher`] and the managed shells).
///
/// The inner [`tracing::Span`] holds a `Mutex`, which would flip the shell
/// type from `UnwindSafe`/`RefUnwindSafe` to their negations. Wrapping in
/// [`core::panic::AssertUnwindSafe`] is correct here because `_span` is
/// only entered during construction and `Drop`, never on hot push/recv
/// paths — a panic while it is entered can only escape from a constructor
/// (before the shell is usable) or from `Drop` (as the shell is being
/// discarded), so no caller can observe a shell in a broken state through
/// an unwind that crossed the span.
pub(crate) type ShellSpan = core::panic::AssertUnwindSafe<tracing::Span>;

/// Categorical reason for a shell-layer failure.
///
/// Bindings (`tst-c`, `tst-jni`, `tst-uniffi`) map each kind directly to
/// a language-native error code or exception. Per-shell applicability:
///
/// | Kind | MuxSender | Sender | RawSender | DemuxReceiver | Receiver | RawReceiver |
/// |------|:---------:|:------:|:---------:|:-------------:|:--------:|:-----------:|
/// | `ConfigInvalid` | ✓ | — | — | — | — | — |
/// | `InputMalformed` | ✓ | ✓ | ✓ | ✓ | — | — |
/// | `Backpressure` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
/// | `TransportBroken` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
/// | `Closed` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
/// | `EndOfStream` | — | — | — | ✓ | ✓ | ✓ |
///
/// Kinds marked "—" cannot be produced by that shell. Bindings can
/// document the unreachable arms with `unreachable!()` or
/// `debug_assert!(false)`; the runtime never reaches them.
///
/// `ShellErrorKind` is a projection of the binding-wide
/// `tst_pipeline::binding::BindingErrorKind` table (`impl From<ShellErrorKind>
/// for BindingErrorKind`, exhaustive); the bindings resolve kinds through that
/// table since 0.7.0.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShellErrorKind {
    /// Configuration is invalid — either the construction-time
    /// `*Config` failed `validate()`, or a runtime push referenced an
    /// invalid handle / ambiguous target.
    ///
    /// Maps to `TST_E_INVALID_CONFIG` (-1) in `tst-c`.
    ConfigInvalid,

    /// Input bytes don't conform to the expected shape (Annex-B NAL,
    /// well-formed PSI / PES, valid KLV LS bytes, TS sync alignment,
    /// payload size within PES caps).
    ///
    /// Maps to `TST_E_INVALID_TS` (-3) in `tst-c`.
    InputMalformed,

    /// Transport is alive but momentarily refused (libsrt `SRT_EASYNCSND`,
    /// muxer internal buffer full and waiting for drain). Caller can retry
    /// the same input after backing off.
    ///
    /// Maps to `TST_E_BUFFER_FULL` (-4) in `tst-c` — on **every** path since
    /// 0.7.0: the shell path always did, and the raw connect/listen path
    /// (which emitted `TST_E_TRANSPORT` (-8) for a
    /// `TransportError::Backpressure` before the kind table) now projects
    /// through `tst_pipeline::binding::BindingErrorKind::Backpressure` (-4)
    /// too. Both transport-side (`TransportError::Backpressure`) and
    /// muxer-internal (`MuxError::BufferFull`) backpressure fold to this
    /// single code.
    Backpressure,

    /// Transport-layer failure — socket broken, libsrt error, factory
    /// closure errored during managed reconnect.
    ///
    /// Maps to `TST_E_TRANSPORT` (-8) in `tst-c`.
    TransportBroken,

    /// Caller invoked `close()` / `cancel()` on this shell (or on its
    /// underlying transport). Subsequent calls on this handle return
    /// the same kind.
    ///
    /// Maps to `TST_E_CLOSED` (-7) in `tst-c`.
    Closed,

    /// Peer closed the connection cleanly — receiver shells only.
    /// The shell's recv loop reached end-of-stream. The handle is dead;
    /// subsequent calls return `Closed`.
    ///
    /// Maps to `TST_E_END_OF_STREAM` (-12) in `tst-c`.
    EndOfStream,
}

/// Implemented by all 6 pipeline shell error types. Bindings use this
/// trait to categorize failures uniformly across shells.
///
/// # Example
///
/// ```
/// use tst_pipeline::{ShellError, ShellErrorKind, MuxSenderError};
/// use tst_core::error::MuxError;
///
/// let err = MuxSenderError::from(MuxError::InvalidConfig("test"));
/// assert_eq!(err.kind(), ShellErrorKind::ConfigInvalid);
/// ```
pub trait ShellError: core::error::Error {
    /// Categorical reason for this failure. See [`ShellErrorKind`] for
    /// the per-shell-applicability matrix.
    fn kind(&self) -> ShellErrorKind;

    /// Wire-level transport errno code when this failure originated in
    /// the underlying `TransportError::{Backpressure, Broken}` variants
    /// and the transport implementation supplied one (libsrt MJ_* major
    /// for `SrtTransport`).
    ///
    /// Returns `None` for non-transport failures (`MuxError`-derived,
    /// `DemuxError`-derived, framing errors, lock-poison shells) and for
    /// transport failures from transports that don't expose a numeric
    /// code (test mocks, in-memory channels, the `reconnect`
    /// orchestration layer's own shell-poison paths).
    ///
    /// Surfaced as a flat accessor on the canonical pipeline-error
    /// surface so JNI / UniFFI bindings can read the wire-level cause
    /// without drilling through the typed `source` enum tree. Rust
    /// callers that want full structured access still match on the
    /// inner `*ErrorSource::Transport(TransportError { errno_code })`.
    ///
    /// Default implementation returns `None` — error types whose inner
    /// source can't carry a transport errno override only when they
    /// can.
    fn errno_code(&self) -> Option<i32> {
        None
    }
}

/// Extract the transport errno_code from a `TransportError` when the
/// variant carries one. Used by the per-shell `errno_code()` impls on
/// the 6 ShellError types, each of which holds a typed source enum that
/// has a `Transport(TransportError)` variant.
///
/// Pulled into a helper here because all 6 impls are identical in shape:
/// reach into the source enum's Transport variant; otherwise None.
pub(crate) fn errno_code_from_transport(e: &TransportError) -> Option<i32> {
    match e {
        TransportError::Backpressure { errno_code, .. } => *errno_code,
        TransportError::Broken { errno_code, .. } => *errno_code,
        _ => None,
    }
}

/// Direction of a transport failure as a shell sees it: `TransportError::Closed`
/// is the caller's own close on a sender (`Closed`) and the peer's close on a
/// receiver (`EndOfStream`). See `binding::kind::kind_of_transport`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Send,
    Recv,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tst_core::transport::BrokenCause;

    // The shell kinds are pinned through every shell's `From` impl by
    // `tests/errors/shell_error_kind_derivation.rs`; this module keeps only what that
    // table cannot reach.

    /// `errno_code_from_transport` extracts the field from
    /// the carrying variants and returns `None` for non-carrying ones.
    #[test]
    fn errno_code_from_transport_extracts_from_carrying_variants() {
        let bp = TransportError::Backpressure {
            msg: "test".into(),
            errno_code: Some(6),
        };
        assert_eq!(errno_code_from_transport(&bp), Some(6));

        let bk = TransportError::Broken {
            msg: "test".into(),
            errno_code: Some(2),
            cause: BrokenCause::Unspecified,
        };
        assert_eq!(errno_code_from_transport(&bk), Some(2));

        // None field round-trips as None.
        let bp_none = TransportError::Backpressure {
            msg: "test".into(),
            errno_code: None,
        };
        assert_eq!(errno_code_from_transport(&bp_none), None);

        // Non-carrying variants return None.
        assert_eq!(errno_code_from_transport(&TransportError::Closed), None);
        assert_eq!(
            errno_code_from_transport(&TransportError::TooLarge { len: 1, max: 0 }),
            None
        );
        assert_eq!(
            errno_code_from_transport(&TransportError::ExplicitClose),
            None
        );
    }
}
