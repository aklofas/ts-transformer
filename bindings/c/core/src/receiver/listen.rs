//! Shared `listen_srt` helper used by all `tst_*_open_listener` calls
//! (and by URL-driven `_open` calls when `mode=listener` is detected).
//!
//! Each `_open_listener` builds a `ListenerConfig` (typically from a
//! parsed URL overlay), binds the local address, blocks on `accept()`
//! until the first peer completes the SRT handshake, and returns the
//! accepted `SrtTransport`. Subsequent peer connections on the same
//! port are not handled — single-accept matches the connection-oriented
//! shape of every other entry point in `tst-c`.

use tst_pipeline::{FactoryCancel, TransportError};
use tst_srt::Listener;
use tst_srt::SrtTransport;
use tst_srt::config::ListenerConfig;

/// Build a `Listener` bound to `host:port`, block on `accept()`, and
/// return the accepted `SrtTransport`. The listener is dropped before
/// return — single-accept semantics.
///
/// `host` may be empty (binds wildcard `0.0.0.0`) or a specific bind
/// address. `cfg` is the `ListenerConfig` (receiver timeouts and other
/// listen-side settings are applied here). Accepted sockets inherit the
/// listen-side `send_timeout` and `recv_timeout` per libsrt's option
/// inheritance mechanism.
///
/// Returns `TransportError::Broken` on bind or accept failure for
/// unified surfacing through `record_transport_error`.
pub(crate) fn listen_srt(
    host: &str,
    port: u16,
    cfg: &ListenerConfig,
) -> Result<SrtTransport, TransportError> {
    let bind_host = if host.is_empty() { "0.0.0.0" } else { host };
    let addr = crate::srt_addr::join_host_port(bind_host, port);
    let mut listener = Listener::bind_with(cfg, addr.as_str()).map_err(|e| {
        // Bind/accept errors aren't libsrt MJ_* errnos in the typed
        // sense — pass None and let the message carry the detail.
        TransportError::Broken {
            msg: format!("bind: {e}"),
            errno_code: None,
        }
    })?;
    let (socket, _peer) = listener.accept().map_err(|e| TransportError::Broken {
        msg: format!("accept: {e}"),
        errno_code: None,
    })?;
    Ok(SrtTransport::new(socket))
}

/// [`listen_srt`] for the MANAGED listener factories: the same bind +
/// single accept, but reachable by `_cancel` while it blocks.
///
/// A managed receiver in listener mode re-runs its factory after a peer
/// disconnect, and that factory parks in `Listener::accept()` until the
/// next peer shows up — before this helper existed, `_cancel` could not
/// reach that listener and a SIGINT handler had nothing to wake it with
/// (ROADMAP "cancellable managed-listener re-accept"). The listener's
/// `cancel_handle()` is published into the shared [`FactoryCancel`] slot
/// around the accept; the managed transport's cancel handle fires the
/// slot, the accept returns, and this reports `ExplicitClose` so the
/// managed transport surfaces the caller-initiated close on its next turn.
///
/// The bind → install → accept → clear → classify sequence itself lives in
/// [`Listener::accept_one_cancellable`] (shared with the Python and JVM
/// managed listener factories, which need the identical lifecycle); this
/// wrapper only renders the bind address the way the C entry points do.
pub(crate) fn listen_srt_cancellable(
    host: &str,
    port: u16,
    cfg: &ListenerConfig,
    cancel: &FactoryCancel,
) -> Result<SrtTransport, TransportError> {
    let bind_host = if host.is_empty() { "0.0.0.0" } else { host };
    let addr = crate::srt_addr::join_host_port(bind_host, port);
    Listener::accept_one_cancellable(cfg, addr.as_str(), cancel)
}
