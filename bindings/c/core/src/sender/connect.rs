//! Shared `connect_srt` helper used by all six `tst_*_open` calls.
//!
//! Each `_open` builds a fresh `SocketConfig` (typically from a parsed
//! URL overlay) and calls `connect_srt(host, port, &cfg)` to produce
//! an `SrtTransport`. The plain senders use the result directly; the
//! managed senders capture (host, port, cfg) in the reconnect closure.

use tst_pipeline::{BrokenCause, TransportError};
use tst_srt::SrtTransport;
use tst_srt::{Socket, SocketConfig};

/// Build a fresh `SrtTransport` connected to `host:port` using the
/// provided socket config (passphrase, latency, etc. set as captured
/// from the URL overlay). Returns `TransportError::Broken` on connect
/// failure for unified surfacing through `record_transport_error`.
///
/// Applies `SocketConfig::merge_sender_defaults` to the config in
/// place. User-set values are preserved (merge-if-default) — a future
/// URL `?role=...` key or a future C ABI role setter would survive
/// the merge unchanged. The defaults themselves
/// (`connect_timeout=15s`, `linger=5s`, `role=Sender`) live in
/// `tst-srt::config`; see `SocketConfig::sender_defaults` for rustdoc
/// on each field's rationale.
pub(crate) fn connect_srt(
    host: &str,
    port: u16,
    cfg: &SocketConfig,
) -> Result<SrtTransport, TransportError> {
    let mut cfg = cfg.clone();
    cfg.merge_sender_defaults();
    let addr = crate::srt_addr::join_host_port(host, port);
    let socket = Socket::connect_with(&cfg, addr.as_str()).map_err(|e| {
        // ConnectError is a typed enum but doesn't expose a libsrt
        // errno on every variant; pass None and let the message carry
        // the diagnostic.
        TransportError::Broken {
            msg: format!("connect: {e}"),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        }
    })?;
    Ok(SrtTransport::new(socket))
}
