//! Discriminating coverage for [`TransportError`] variants flowing through
//! the pipeline shell errors.
//!
//! Each test asserts on the **specific `ShellErrorKind` AND inner-variant
//! pair** via `matches!` — not on `is_err()`, which any error satisfies.
//!
//! ## Coverage
//!
//! 5 `TransportError` → `ShellErrorKind` routings exercised (per
//! `shell_error.rs` `kind_from_transport`):
//!
//! | `TransportError` variant | Shell | `ShellErrorKind` | Test |
//! |--------------------------|-------|------------------|------|
//! | `Backpressure(_)` | `RawSender` | `Backpressure` | `backpressure_propagates_to_shell_as_backpressure_kind` |
//! | `Broken(_)` | `RawSender` | `TransportBroken` | `broken_propagates_to_shell_as_transport_broken_kind` |
//! | `Closed` (recv) | `Receiver` | `EndOfStream` | `closed_propagates_as_end_of_stream_on_receiver` |
//! | `ExplicitClose` | `RawSender` | `Closed` | `explicit_close_propagates_as_closed` |
//! | `TooLarge { .. }` | `RawSender` | `InputMalformed` | `too_large_propagates_as_input_malformed` |
//!
//! ## Design notes
//!
//! - All tests use `RawSender<MockTransport>` for send-side variants because
//!   it is the thinnest send shell (no muxer, no framing). The mock is either
//!   `tst_test_helpers::MockTransport` (for `Backpressure`, `Broken`,
//!   `TooLarge`) or a minimal local struct for `ExplicitClose` (which
//!   `MockTransport` does not support as a `FailMode`).
//! - The receiver-side test uses a minimal local `ClosedRecv` that immediately
//!   returns `TransportError::Closed`, driving `Receiver::next_packet` to
//!   `ReceiverError { kind: EndOfStream, source: Transport(Closed) }`.
//! - Asserting on **both** `err.kind` and `err.source` matters: `err.kind` is
//!   what binding authors (`tst-c`, `tst-jni`, `tst-uniffi`) use for retry /
//!   error-code decisions; `err.source` gives power users the inner-variant
//!   discrimination needed to route logs and telemetry.

use tst_core::transport::{RecvTransport, Transport, TransportError};
use tst_pipeline::{
    RawSender, RawSenderConfig, RawSenderErrorSource, Receiver, ReceiverConfig,
    ReceiverErrorSource, ShellErrorKind,
};
use tst_test_helpers::mock_transport::{FailMode, MockTransport};

// ---------------------------------------------------------------------------
// Test 1: Backpressure
// ---------------------------------------------------------------------------

/// `TransportError::Backpressure` from the underlying send transport must
/// propagate as `ShellErrorKind::Backpressure` + `RawSenderErrorSource::Transport(Backpressure(_))`.
///
/// `Backpressure` is the caller's signal to back off and retry the same
/// payload — distinguishable from `Broken` (which means the transport is
/// permanently dead) by the `ShellErrorKind` alone. The inner variant
/// confirms the transport (not the muxer or framing layer) triggered it.
#[test]
fn backpressure_propagates_to_shell_as_backpressure_kind() {
    let mock = MockTransport::new(1316);
    // Program the mock to return Backpressure on the next send.
    *mock.fail_handle().lock().unwrap() = FailMode::BackpressureForN(1);

    let mut sender = RawSender::new(mock, RawSenderConfig::default());
    let err = sender.send(&[0u8; 100]).expect_err("must backpressure");

    assert_eq!(
        err.kind,
        ShellErrorKind::Backpressure,
        "kind must be Backpressure, got: {:?}",
        err.kind
    );
    assert!(
        matches!(
            err.source,
            RawSenderErrorSource::Transport(TransportError::Backpressure { .. })
        ),
        "source must be Transport(Backpressure(_)), got: {:?}",
        err.source
    );
}

// ---------------------------------------------------------------------------
// Test 2: Broken
// ---------------------------------------------------------------------------

/// `TransportError::Broken` from the underlying send transport must propagate
/// as `ShellErrorKind::TransportBroken` + `RawSenderErrorSource::Transport(Broken(_))`.
///
/// `TransportBroken` signals to binding authors that the handle is permanently
/// dead; the inner `Broken(_)` message carries a human-readable diagnosis for
/// logging. The two-field assertion ensures both the retry decision (kind) and
/// the diagnostic path (source variant) are correct.
#[test]
fn broken_propagates_to_shell_as_transport_broken_kind() {
    let mock = MockTransport::new(1316);
    // Program the mock to return Broken on the next send.
    *mock.fail_handle().lock().unwrap() = FailMode::BrokenForN(1);

    let mut sender = RawSender::new(mock, RawSenderConfig::default());
    let err = sender.send(&[0u8; 100]).expect_err("must break");

    assert_eq!(
        err.kind,
        ShellErrorKind::TransportBroken,
        "kind must be TransportBroken, got: {:?}",
        err.kind
    );
    assert!(
        matches!(
            err.source,
            RawSenderErrorSource::Transport(TransportError::Broken { .. })
        ),
        "source must be Transport(Broken(_)), got: {:?}",
        err.source
    );
}

// ---------------------------------------------------------------------------
// Test 3: Closed → EndOfStream (receiver side)
// ---------------------------------------------------------------------------

/// A `RecvTransport` that immediately returns `TransportError::Closed`
/// (peer-EOS) must cause `Receiver::next_packet` to return
/// `ShellErrorKind::EndOfStream` + `ReceiverErrorSource::Transport(Closed)`.
///
/// The `Closed` → `EndOfStream` mapping only applies when `kind_from_transport`
/// receives `Direction::Recv`. The same `Closed` variant on the send side maps
/// to `Closed` kind (a different route). This test pins the receiver-side
/// routing to catch any future swap.
struct ClosedRecv;

impl RecvTransport for ClosedRecv {
    fn recv_bytes(&mut self, _buf: &mut [u8]) -> Result<usize, TransportError> {
        Err(TransportError::Closed)
    }
    fn max_payload(&self) -> usize {
        1316
    }
    fn is_alive(&self) -> bool {
        false
    }
}

#[test]
fn closed_propagates_as_end_of_stream_on_receiver() {
    let mut rx = Receiver::new(ClosedRecv, ReceiverConfig::default());
    let err = rx.next_packet().expect_err("must fail with EOS");

    assert_eq!(
        err.kind,
        ShellErrorKind::EndOfStream,
        "kind must be EndOfStream (receiver-side Closed routing), got: {:?}",
        err.kind
    );
    assert!(
        matches!(
            err.source,
            ReceiverErrorSource::Transport(TransportError::Closed)
        ),
        "source must be Transport(Closed), got: {:?}",
        err.source
    );
}

// ---------------------------------------------------------------------------
// Test 4: ExplicitClose
// ---------------------------------------------------------------------------

/// `TransportError::ExplicitClose` from the underlying send transport must
/// propagate as `ShellErrorKind::Closed` + `RawSenderErrorSource::Transport(ExplicitClose)`.
///
/// `ExplicitClose` is the caller-initiated variant (the
/// `ManagedRecvTransport::cancel()` path). Unlike `Closed` (which on the
/// sender side also maps to `Closed` kind), `ExplicitClose` is distinguished
/// at the inner-source level. Binding authors don't differentiate the two at
/// the kind level (both are `Closed`), but power users inspecting `err.source`
/// can tell that the cancel signal fired rather than the remote peer closing.
///
/// `MockTransport` does not have an `ExplicitClose` `FailMode`, so this test
/// uses a minimal local mock that unconditionally returns `ExplicitClose`.
struct ExplicitCloseTransport;

impl Transport for ExplicitCloseTransport {
    fn send_bytes(&mut self, _msg: &[u8]) -> Result<(), TransportError> {
        Err(TransportError::ExplicitClose)
    }
    fn max_payload(&self) -> usize {
        1316
    }
    fn is_alive(&self) -> bool {
        false
    }
    fn close(&mut self) {}
}

#[test]
fn explicit_close_propagates_as_closed() {
    let mut sender = RawSender::new(ExplicitCloseTransport, RawSenderConfig::default());
    let err = sender
        .send(&[0u8; 100])
        .expect_err("must return ExplicitClose");

    assert_eq!(
        err.kind,
        ShellErrorKind::Closed,
        "kind must be Closed (ExplicitClose maps to Closed kind regardless of direction), \
         got: {:?}",
        err.kind
    );
    assert!(
        matches!(
            err.source,
            RawSenderErrorSource::Transport(TransportError::ExplicitClose)
        ),
        "source must be Transport(ExplicitClose), got: {:?}",
        err.source
    );
}

// ---------------------------------------------------------------------------
// Test 5: TooLarge
// ---------------------------------------------------------------------------

/// Sending a payload that exceeds `max_payload()` must return
/// `ShellErrorKind::InputMalformed` + `RawSenderErrorSource::Transport(TooLarge { .. })`.
///
/// `RawSender::send` pre-validates `bytes.len() <= transport.max_payload()` and
/// synthesizes `TransportError::TooLarge` before delegating to the transport.
/// This test confirms that pre-validation path is wired to the correct kind
/// and that the inner-source fields (`len`, `max`) can be discriminated.
///
/// `InputMalformed` is the correct kind: the caller supplied a payload larger
/// than the transport can carry in one SRT message. The fix is to split the
/// payload, not to retry.
#[test]
fn too_large_propagates_as_input_malformed() {
    // MockTransport with max_payload = 100. Sending 101 bytes must fail.
    let mock = MockTransport::new(100);
    let mut sender = RawSender::new(mock, RawSenderConfig::default());

    let err = sender
        .send(&[0u8; 101])
        .expect_err("must reject payload exceeding max_payload");

    assert_eq!(
        err.kind,
        ShellErrorKind::InputMalformed,
        "kind must be InputMalformed for TooLarge, got: {:?}",
        err.kind
    );
    assert!(
        matches!(
            err.source,
            RawSenderErrorSource::Transport(TransportError::TooLarge { len: 101, max: 100 })
        ),
        "source must be Transport(TooLarge {{ len: 101, max: 100 }}), got: {:?}",
        err.source
    );
}
