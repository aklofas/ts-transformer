//! Socket configuration knobs for UDP transports.

use std::net::IpAddr;

/// Per-transport socket knobs.
///
/// All fields have sane OS-default behavior when set to `None`.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct UdpConfig {
    /// Multicast outgoing interface (literal IP addr or interface name).
    pub iface: Option<String>,
    /// IPv4 multicast TTL / IPv6 multicast hop limit. 1-255. OS default usually 1.
    pub ttl: Option<u8>,
    /// IP TOS / DSCP byte. Common values: 0xb8 (EF), 0x68 (AF31).
    pub tos: Option<u8>,
    /// SO_RCVBUF in bytes. Higher = less kernel-side drop under bursts.
    pub rcvbuf: Option<usize>,
    /// SO_SNDBUF in bytes.
    pub sndbuf: Option<usize>,
    /// Send-side datagram size. Default 7×188 = 1316 bytes (STANAG 4609 op default).
    /// A receive-side URL carrying `?pkt_size=` is refused; the
    /// receiver itself ignores this field.
    pub pkt_size: Option<usize>,
    /// Local bind address override (for sending from a specific NIC).
    pub localaddr: Option<IpAddr>,
}

impl UdpConfig {
    /// The standard 7×188 MPEG-TS datagram payload size.
    pub const DEFAULT_PKT_SIZE: usize = 7 * 188;

    /// Resolved pkt_size — caller's override or the default.
    pub fn pkt_size_or_default(&self) -> usize {
        self.pkt_size.unwrap_or(Self::DEFAULT_PKT_SIZE)
    }
}

/// The name this type had before it was renamed.
#[deprecated(since = "0.9.0", note = "renamed to `UdpConfig`")]
pub type SocketConfig = UdpConfig;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::url::UdpUrl;

    #[test]
    fn default_pkt_size_is_7x188() {
        let cfg = UdpConfig::default();
        assert_eq!(cfg.pkt_size_or_default(), 1316);
    }

    #[test]
    fn url_config_carries_only_the_keys_the_url_names() {
        let url = UdpUrl::parse("udp://239.10.0.1:5004?ttl=8").unwrap();
        assert_eq!(url.config.ttl, Some(8));
        assert_eq!(url.config.pkt_size, None);
    }
}
