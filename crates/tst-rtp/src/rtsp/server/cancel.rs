//! Cancel handle for an [`RtspServer`](crate::RtspServer).
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

/// Cancel handle for an `RtspServer`.
///
/// `cancel()` is the HARD cancel — equivalent to a SIGKILL on the
/// server's tokio Runtime tasks: the listener stops accepting new
/// connections, per-session tasks abort at their next poll, and no
/// RTSP TEARDOWN-style notification is sent to connected clients
/// (clients will see TCP RST or a half-closed connection).
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
