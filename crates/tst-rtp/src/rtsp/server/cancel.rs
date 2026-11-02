//! Cancel handle for an [`RtspServer`](crate::RtspServer).
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

/// Cancel handle for an `RtspServer`.
///
/// `cancel()` is the HARD cancel. The listener observes the flag within
/// 100 ms, stops accepting, fires the session cancel token so parked
/// sessions end, performs the same shutdown bookkeeping as
/// `RtspServer::stop` (a parked `next_publisher` returns `Shutdown`,
/// publish mounts' application transports read `Closed`, registration and
/// rule edits are refused) without the Notice 5402, and drains sessions for
/// `graceful_shutdown_drain`. Clients see the connection close with no
/// RTSP TEARDOWN-style notification.
///
/// For graceful shutdown with session-end notification + bounded drain,
/// use `RtspServer::stop` instead.
///
/// It is a [`tst_core::cancel::FlagCancel`]: `cancel()` is one atomic store,
/// safe from a signal handler, and the handle implements `TransportCancel`.
///
/// Requires the `rtsp-server` feature — this handle only exists paired
/// with a real `RtspServer`.
///
/// Obtain it from the owner's `cancel_handle()`; a handle built with
/// `new()` or `Default` is read by nothing.
pub type RtspServerCancelHandle = tst_core::cancel::FlagCancel;

#[cfg(test)]
mod tests {
    use tst_core::transport::TransportCancel;

    use super::*;

    #[test]
    fn server_handle_is_a_transport_cancel() {
        fn assert_tc<T: TransportCancel + Clone + Send + Sync + 'static>() {}
        assert_tc::<RtspServerCancelHandle>();
    }
}
