//! Classification of failed socket *sends* by [`std::io::ErrorKind`].
//!
//! Shared by the TCP and UDP transports so both agree on which kinds mean
//! "the send deadline ticked over, nothing is wrong with the socket" and
//! which are terminal. The transports set `SO_SNDTIMEO` to the cancel-poll
//! interval ([`crate::cancel::CANCEL_POLL_INTERVAL`]) so a parked send
//! wakes to re-check its cancel flag; `std` spells that expiry as
//! `WouldBlock` on Linux/macOS (`EAGAIN`) and as `TimedOut` on Windows
//! (`WSAETIMEDOUT`). The receive paths already treat both as the poll tick;
//! a send path matching only `WouldBlock` would latch the transport dead on
//! a Windows send stall.

use std::io;

/// What a send path does with a failed `write` / `send_to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendClass {
    /// The send deadline expired (`WouldBlock` / `TimedOut`) or a signal
    /// interrupted the call (`Interrupted`): the socket is fine. Zero
    /// progress is reported as `Backpressure` (the slice is intact); a
    /// stream transport that already committed a prefix keeps writing.
    Transient,
    /// Anything else: the socket is unusable — report `Broken` and latch
    /// the transport dead.
    Fatal,
}

/// Classify a failed send by its [`io::ErrorKind`].
///
/// `Interrupted` (`EINTR`) is transient for the same reason the receive
/// classifiers retry it: a signal landed on the parked thread and the call
/// must be resumed, not turned into a dead transport.
pub fn classify_send_error(err: &io::Error) -> SendClass {
    match err.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted => {
            SendClass::Transient
        }
        _ => SendClass::Fatal,
    }
}

/// What a failed `recv` should do next.
///
/// Split out as a pure function so the transient/fatal decision is unit
/// testable: `EINTR` in particular cannot be provoked deterministically
/// from a plain socket, and the arm it feeds latches the transport
/// dead, so the classification is worth pinning directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvAction {
    /// Transient — poll again; the transport stays alive.
    Retry,
    /// Terminal — report `Broken` and latch the transport dead.
    Fatal,
}

/// Classify a failed `recv` by its [`std::io::ErrorKind`].
///
/// `WouldBlock` / `TimedOut` are the `SO_RCVTIMEO` cancel-poll tick.
/// `Interrupted` is `EINTR` — a signal landed on the thread parked in
/// `recv`, which is exactly what the SIGINT-handler shutdown pattern
/// delivers. The send path already treats it as transient
/// ([`classify_send_error`]) and every receive path retries it for the same reason: a fatal receive latches
/// `alive = false`, so letting a handled signal through to that
/// arm would leave the receiver permanently dead with every later call
/// reporting `Closed`. Everything else is terminal.
pub fn classify_recv_error(kind: io::ErrorKind) -> RecvAction {
    match kind {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted => {
            RecvAction::Retry
        }
        _ => RecvAction::Fatal,
    }
}

#[cfg(test)]
mod tests {
    use super::{SendClass, classify_send_error};
    use std::io::{Error, ErrorKind};

    /// The synthetic Windows spelling of a send-deadline expiry.
    /// There is no Linux RED for this (Linux reports `WouldBlock`), so this
    /// unit test is the pin.
    #[test]
    fn timed_out_is_transient() {
        assert_eq!(
            classify_send_error(&Error::from(ErrorKind::TimedOut)),
            SendClass::Transient
        );
    }

    #[test]
    fn wouldblock_and_interrupted_are_transient() {
        assert_eq!(
            classify_send_error(&Error::from(ErrorKind::WouldBlock)),
            SendClass::Transient
        );
        assert_eq!(
            classify_send_error(&Error::from(ErrorKind::Interrupted)),
            SendClass::Transient
        );
    }

    #[test]
    fn genuine_failures_are_fatal() {
        for kind in [
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::NotConnected,
            ErrorKind::ConnectionRefused,
        ] {
            assert_eq!(
                classify_send_error(&Error::from(kind)),
                SendClass::Fatal,
                "{kind:?}"
            );
        }
    }
}

#[cfg(test)]
mod recv_classify_tests {
    use super::{RecvAction, classify_recv_error};
    use std::io::ErrorKind;

    #[test]
    fn wouldblock_and_timedout_retry() {
        // The SO_RCVTIMEO cancel-poll tick: transient by construction.
        assert_eq!(
            classify_recv_error(ErrorKind::WouldBlock),
            RecvAction::Retry
        );
        assert_eq!(classify_recv_error(ErrorKind::TimedOut), RecvAction::Retry);
    }

    #[test]
    fn interrupted_retries_like_the_send_path() {
        // EINTR: a signal landed on the thread parked in `recv`. The send
        // path classifies it Transient; treating it as fatal here would
        // latch `alive = false` and kill the receiver for good
        // on a signal that was meant to be handled and resumed.
        assert_eq!(
            classify_recv_error(ErrorKind::Interrupted),
            RecvAction::Retry
        );
    }

    #[test]
    fn genuine_failures_are_fatal() {
        for kind in [
            ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::NotConnected,
        ] {
            assert_eq!(classify_recv_error(kind), RecvAction::Fatal, "{kind:?}");
        }
    }
}
