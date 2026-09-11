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

use tst_core::error::{DemuxError, MuxError};
use tst_core::transport::TransportError;

use crate::sender::TsFramingError;

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
    /// Maps to `TST_E_BUFFER_FULL` (-4) in `tst-c`. Both transport-side
    /// (`TransportError::Backpressure`) and muxer-internal
    /// (`MuxError::BufferFull`) backpressure fold to this single code.
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

/// Direction parameter for `kind_from_transport` — `TransportError::Closed`
/// has different shell-kind disposition on senders (`Closed`) vs receivers
/// (`EndOfStream`). See `kind_from_transport` for the routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Send,
    // Only constructed by the receiver-side shells, which are gated behind the
    // `std` feature. Under `no_std` (sender-only) the variant is never built,
    // but `kind_from_transport` still matches it exhaustively.
    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    Recv,
}

/// Compute the shell-kind for a `MuxError`. Used by `MuxSender`'s
/// `From<MuxError>` impl. **Every MuxError variant is matched explicitly**
/// — the CI ratchet `scripts/check/rust/pipeline-kind-classification.sh`
/// (Task 10) will enforce no variant escapes through a wildcard.
pub(crate) fn kind_from_mux(e: &MuxError) -> ShellErrorKind {
    use ShellErrorKind::*;
    match e {
        MuxError::InvalidConfig(_) => ConfigInvalid,
        MuxError::ConfigInvalid { .. } => ConfigInvalid,
        MuxError::InvalidNal => InputMalformed,
        MuxError::InvalidAv1Obu => InputMalformed,
        MuxError::BufferFull { .. } => Backpressure,
        MuxError::KlvTooLarge { .. } => InputMalformed,
        MuxError::AudioTooLarge { .. } => InputMalformed,
        MuxError::InvalidStreamHandle { .. } => ConfigInvalid,
        MuxError::AmbiguousTarget { .. } => ConfigInvalid,
        MuxError::NoKlvStreamsConfigured => ConfigInvalid,
        MuxError::NoAudioStreamsConfigured => ConfigInvalid,
        MuxError::NoSubtitleStreamsConfigured => ConfigInvalid,
        MuxError::NoDataStreamsConfigured => ConfigInvalid,
        MuxError::TooManyVideoStreams { .. } => ConfigInvalid,
        MuxError::TooManyKlvStreams { .. } => ConfigInvalid,
        MuxError::TooManyAudioStreams { .. } => ConfigInvalid,
        MuxError::TooManySubtitleStreams { .. } => ConfigInvalid,
        MuxError::TooManyDataStreams { .. } => ConfigInvalid,
        MuxError::SubtitleTooLarge { .. } => InputMalformed,
        MuxError::DataTooLarge { .. } => InputMalformed,
        MuxError::SubtitlePidUsedAsPcrPid { .. } => ConfigInvalid,
        MuxError::KlvPidUsedAsPcrPid { .. } => ConfigInvalid,
        MuxError::DataPidUsedAsPcrPid { .. } => ConfigInvalid,
        MuxError::InvalidLanguageCode { .. } => ConfigInvalid,
        MuxError::InvalidTeletextField { .. } => ConfigInvalid,
        MuxError::PmtTooLarge { .. } => ConfigInvalid,
        MuxError::MalformedDescriptor { .. } => ConfigInvalid,
        MuxError::TooManyPrograms { .. } => ConfigInvalid,
        MuxError::EmptyProgram { .. } => ConfigInvalid,
        MuxError::DuplicateProgramNumber { .. } => ConfigInvalid,
        MuxError::DuplicatePmtPid { .. } => ConfigInvalid,
        MuxError::DuplicatePidAcrossPrograms { .. } => ConfigInvalid,
        MuxError::ProgramNotFound { .. } => ConfigInvalid,
        MuxError::PmtPidConflictsWithStream { .. } => ConfigInvalid,
        MuxError::NoPcrEligibleStream { .. } => ConfigInvalid,
        MuxError::DescriptorIndexOutOfRange { .. } => ConfigInvalid,
        MuxError::AbsIndexOutOfRange { .. } => ConfigInvalid,
        MuxError::MispTime(_) => InputMalformed,
        // Required by #[non_exhaustive]. CI ratchet
        // scripts/check/rust/pipeline-kind-classification.sh enforces every
        // upstream MuxError variant is matched above before this arm.
        // If this arm fires, the ratchet failed or was bypassed; the
        // shell error reports ConfigInvalid as a safe-default category
        // because the most common cause of an unmatched MuxError would
        // be a new config-validate failure.
        _ => ConfigInvalid,
    }
}

/// Compute the shell-kind for a `TransportError`. The `direction`
/// parameter distinguishes sender-shell-side (`Closed` -> `Closed` kind,
/// caller-initiated) from receiver-shell-side (`Closed` -> `EndOfStream`
/// kind, peer-initiated). `ExplicitClose` always maps to `Closed` kind
/// regardless of direction.
pub(crate) fn kind_from_transport(e: &TransportError, direction: Direction) -> ShellErrorKind {
    use ShellErrorKind::*;
    match e {
        TransportError::Backpressure { .. } => Backpressure,
        TransportError::Broken { .. } => TransportBroken,
        TransportError::Closed => match direction {
            Direction::Send => Closed,
            Direction::Recv => EndOfStream,
        },
        TransportError::TooLarge { .. } => InputMalformed,
        TransportError::ExplicitClose => Closed,
        // Required by #[non_exhaustive]. CI ratchet enforces every variant
        // matched above. Safe default: TransportBroken (most common
        // category for a new transport-layer failure).
        _ => TransportBroken,
    }
}

/// Compute the shell-kind for a `DemuxError`. All 5 variants represent
/// "the input byte stream is malformed at this point" — all map to
/// `InputMalformed`. The exhaustive match documents that no variant has
/// a different disposition.
///
/// Only the receiver-side shells (gated behind `std`) call this; under
/// `no_std` (sender-only) it is dead.
#[cfg_attr(not(feature = "std"), allow(dead_code))]
pub(crate) fn kind_from_demux(e: &DemuxError) -> ShellErrorKind {
    use ShellErrorKind::*;
    match e {
        DemuxError::Unrecoverable { .. } => InputMalformed,
        DemuxError::StrictRejection(_) => InputMalformed,
        DemuxError::MalformedPsi { .. } => InputMalformed,
        DemuxError::MalformedPes { .. } => InputMalformed,
        DemuxError::SyncBufExhausted { .. } => InputMalformed,
        // Required by #[non_exhaustive]. Safe default: InputMalformed
        // (matches every existing variant's disposition).
        _ => InputMalformed,
    }
}

/// Compute the shell-kind for a `TsFramingError`. Both variants are TS
/// sync-loss failures — `InputMalformed`. Same exhaustive-match pattern.
///
/// No wildcard needed: TsFramingError is defined in the same crate as
/// this helper, so Rust treats the #[non_exhaustive] match as exhaustive
/// in-crate. (TsFramingError received #[non_exhaustive] in Wave 2.3.)
pub(crate) fn kind_from_framing(e: &TsFramingError) -> ShellErrorKind {
    use ShellErrorKind::*;
    match e {
        TsFramingError::SyncLost { .. } => InputMalformed,
        TsFramingError::NoSyncAfterLimit { .. } => InputMalformed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tst_core::transport::BrokenCause;

    #[test]
    fn kind_from_mux_invalid_config_is_config_invalid() {
        let kind = kind_from_mux(&MuxError::InvalidConfig("test"));
        assert_eq!(kind, ShellErrorKind::ConfigInvalid);
    }

    #[test]
    fn kind_from_mux_buffer_full_is_backpressure() {
        let kind = kind_from_mux(&MuxError::BufferFull {
            capacity_packets: 100,
        });
        assert_eq!(kind, ShellErrorKind::Backpressure);
    }

    #[test]
    fn kind_from_mux_invalid_nal_is_input_malformed() {
        let kind = kind_from_mux(&MuxError::InvalidNal);
        assert_eq!(kind, ShellErrorKind::InputMalformed);
    }

    #[test]
    fn kind_from_transport_closed_send_vs_recv() {
        assert_eq!(
            kind_from_transport(&TransportError::Closed, Direction::Send),
            ShellErrorKind::Closed
        );
        assert_eq!(
            kind_from_transport(&TransportError::Closed, Direction::Recv),
            ShellErrorKind::EndOfStream
        );
    }

    #[test]
    fn kind_from_transport_explicit_close_is_closed() {
        assert_eq!(
            kind_from_transport(&TransportError::ExplicitClose, Direction::Send),
            ShellErrorKind::Closed
        );
        assert_eq!(
            kind_from_transport(&TransportError::ExplicitClose, Direction::Recv),
            ShellErrorKind::Closed
        );
    }

    #[test]
    fn kind_from_transport_too_large_is_input_malformed() {
        assert_eq!(
            kind_from_transport(
                &TransportError::TooLarge {
                    len: 2000,
                    max: 1316
                },
                Direction::Send
            ),
            ShellErrorKind::InputMalformed
        );
    }

    #[test]
    fn kind_from_demux_all_variants_are_input_malformed() {
        for e in [
            DemuxError::Unrecoverable { after_bytes: 100 },
            DemuxError::StrictRejection("test".into()),
            DemuxError::MalformedPsi {
                pid: 0,
                reason: "test",
            },
            DemuxError::MalformedPes {
                pid: 0,
                reason: "test",
            },
            DemuxError::SyncBufExhausted {
                observed: 4096,
                max: 4096,
            },
        ] {
            assert_eq!(kind_from_demux(&e), ShellErrorKind::InputMalformed);
        }
    }

    #[test]
    fn kind_from_framing_all_variants_are_input_malformed() {
        assert_eq!(
            kind_from_framing(&TsFramingError::SyncLost { offset: 0 }),
            ShellErrorKind::InputMalformed
        );
        assert_eq!(
            kind_from_framing(&TsFramingError::NoSyncAfterLimit { max: 4096 }),
            ShellErrorKind::InputMalformed
        );
    }

    /// D5 follow-up: `errno_code_from_transport` extracts the field from
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
