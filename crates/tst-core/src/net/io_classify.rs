//! Classification of failed socket *sends* by [`std::io::ErrorKind`].
//!
//! Shared by the TCP and UDP transports so both agree on which kinds mean
//! "the send deadline ticked over, nothing is wrong with the socket" and
//! which are terminal. The transports set `SO_SNDTIMEO` to the cancel-poll
//! interval ([`super::udp_socket::CANCEL_POLL_INTERVAL`]) so a parked send
//! wakes to re-check its cancel flag; `std` spells that expiry as
//! `WouldBlock` on Linux/macOS (`EAGAIN`) and as `TimedOut` on Windows
//! (`WSAETIMEDOUT`). The receive paths already treat both as the poll tick;
//! before this module the send paths matched only `WouldBlock`, so a Windows
//! send stall latched the transport dead (CORR-24).

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

#[cfg(test)]
mod tests {
    use super::{SendClass, classify_send_error};
    use std::io::{Error, ErrorKind};

    /// CORR-24: the synthetic Windows spelling of a send-deadline expiry.
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
