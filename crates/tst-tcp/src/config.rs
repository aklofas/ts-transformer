//! Socket configuration knobs for TCP transports.

use std::path::PathBuf;
use std::time::Duration;

/// Per-transport socket and TLS settings.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TcpConfig {
    /// Whether to disable Nagle's algorithm (`TCP_NODELAY`).
    ///
    /// Defaults to `Some(true)` — Nagle is disabled by default because
    /// live-video transports are latency-sensitive and Nagle's coalescing
    /// adds measurable delay on small writes. Set to `Some(false)` or
    /// `nodelay=0` in the URL to re-enable Nagle when bulk throughput
    /// matters more than latency.
    pub nodelay: Option<bool>,
    /// SO_KEEPALIVE idle time. `None` = disabled.
    pub keepalive: Option<Duration>,
    /// SO_RCVBUF size in bytes.
    pub rcvbuf: Option<usize>,
    /// SO_SNDBUF size in bytes.
    pub sndbuf: Option<usize>,
    /// Connect timeout (caller-side only).
    ///
    /// Applied per resolved address — a hostname that resolves to multiple
    /// addresses may take up to N× this value before the connection attempt
    /// fails overall. For `tcps://` the TLS handshake runs inside the connect
    /// too, with its own budget of this value after the TCP connect; a
    /// handshake that does not complete in time is `ConnectTimeout`.
    /// Default: 10 s (see [`TcpConfig::connect_timeout_or_default`]).
    pub connect_timeout: Option<Duration>,
    /// Send-side payload chunk size; per-call max for `send_bytes`.
    /// Default 64 KiB (matches reasonable TCP send-buffer sizing).
    pub pkt_size: Option<usize>,
    /// Server certificate, PEM (`tcps://` listener only).
    pub cert: Option<PathBuf>,
    /// Server private key, PEM (`tcps://` listener only).
    pub key: Option<PathBuf>,
    /// Trust anchors, PEM (`tcps://` caller only). `None` uses the system
    /// roots.
    pub ca: Option<PathBuf>,
}

impl Default for TcpConfig {
    fn default() -> TcpConfig {
        Self {
            // Default TCP_NODELAY to true: live-video transports are latency-
            // sensitive; Nagle coalescing adds measurable delay on small writes.
            nodelay: Some(true),
            keepalive: None,
            rcvbuf: None,
            sndbuf: None,
            connect_timeout: None,
            pkt_size: None,
            cert: None,
            key: None,
            ca: None,
        }
    }
}

impl TcpConfig {
    /// Default per-call payload limit. TCP is a bytestream, so this is
    /// just an upper bound on how much we accept per `send_bytes` call.
    pub const DEFAULT_PKT_SIZE: usize = 64 * 1024;

    pub fn pkt_size_or_default(&self) -> usize {
        self.pkt_size.unwrap_or(Self::DEFAULT_PKT_SIZE)
    }

    pub fn connect_timeout_or_default(&self) -> Duration {
        self.connect_timeout.unwrap_or(Duration::from_secs(10))
    }
}

/// The name this type had before it was renamed.
#[deprecated(since = "0.9.0", note = "renamed to `TcpConfig`")]
pub type SocketConfig = TcpConfig;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let cfg = TcpConfig::default();
        assert_eq!(cfg.pkt_size_or_default(), 64 * 1024);
        assert_eq!(cfg.connect_timeout_or_default(), Duration::from_secs(10));
        // TCP_NODELAY is on by default for live-latency paths.
        assert_eq!(
            cfg.nodelay,
            Some(true),
            "nodelay must default to Some(true)"
        );
    }
}
