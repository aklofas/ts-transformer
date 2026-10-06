//! Socket configuration knobs for TCP transports.

use std::time::Duration;

use crate::url::TcpUrl;

/// Per-transport socket knobs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SocketConfig {
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
    /// Default: 10 s (see [`SocketConfig::connect_timeout_or_default`]).
    pub connect_timeout: Option<Duration>,
    /// Send-side payload chunk size; per-call max for `send_bytes`.
    /// Default 64 KiB (matches reasonable TCP send-buffer sizing).
    pub pkt_size: Option<usize>,
}

impl Default for SocketConfig {
    fn default() -> SocketConfig {
        Self {
            // Default TCP_NODELAY to true: live-video transports are latency-
            // sensitive; Nagle coalescing adds measurable delay on small writes.
            nodelay: Some(true),
            keepalive: None,
            rcvbuf: None,
            sndbuf: None,
            connect_timeout: None,
            pkt_size: None,
        }
    }
}

impl SocketConfig {
    /// Default per-call payload limit. TCP is a bytestream, so this is
    /// just an upper bound on how much we accept per `send_bytes` call.
    pub const DEFAULT_PKT_SIZE: usize = 64 * 1024;

    pub fn pkt_size_or_default(&self) -> usize {
        self.pkt_size.unwrap_or(Self::DEFAULT_PKT_SIZE)
    }

    pub fn connect_timeout_or_default(&self) -> Duration {
        self.connect_timeout.unwrap_or(Duration::from_secs(10))
    }

    pub fn merge_from_url(&mut self, url: &TcpUrl) {
        if let Some(v) = url.nodelay {
            self.nodelay = Some(v);
        }
        if let Some(v) = url.keepalive {
            self.keepalive = Some(v);
        }
        if let Some(v) = url.rcvbuf {
            self.rcvbuf = Some(v);
        }
        if let Some(v) = url.sndbuf {
            self.sndbuf = Some(v);
        }
        if let Some(v) = url.connect_timeout {
            self.connect_timeout = Some(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::url::TcpUrl;

    #[test]
    fn defaults_are_sensible() {
        let cfg = SocketConfig::default();
        assert_eq!(cfg.pkt_size_or_default(), 64 * 1024);
        assert_eq!(cfg.connect_timeout_or_default(), Duration::from_secs(10));
        // TCP_NODELAY is on by default for live-latency paths.
        assert_eq!(
            cfg.nodelay,
            Some(true),
            "nodelay must default to Some(true)"
        );
    }

    /// The URL `nodelay=0` override must still turn Nagle back on even
    /// though the default is `Some(true)`.
    #[test]
    fn nodelay_url_override_disables_nagle() {
        let mut cfg = SocketConfig::default();
        assert_eq!(cfg.nodelay, Some(true));
        let u = TcpUrl::parse("tcp://1.2.3.4:7001?nodelay=0").unwrap();
        cfg.merge_from_url(&u);
        assert_eq!(
            cfg.nodelay,
            Some(false),
            "nodelay=0 must override the default to Some(false)"
        );
    }

    #[test]
    fn merge_from_url_applies_set_fields_only() {
        let mut cfg = SocketConfig {
            pkt_size: Some(8192),
            ..Default::default()
        };
        let u = TcpUrl::parse("tcp://1.2.3.4:7001?nodelay=1&keepalive=30").unwrap();
        cfg.merge_from_url(&u);
        assert_eq!(cfg.nodelay, Some(true));
        assert_eq!(cfg.keepalive, Some(Duration::from_secs(30)));
        assert_eq!(cfg.pkt_size, Some(8192));
    }
}
