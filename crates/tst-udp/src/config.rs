//! Socket configuration knobs for UDP transports.

use std::net::IpAddr;

use crate::url::UdpUrl;

/// Per-transport socket knobs.
///
/// All fields have sane OS-default behavior when set to `None`.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SocketConfig {
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
    /// A receive-side URL carrying `?pkt_size=` is refused
    /// ([`UdpUrlError::RecvPktSize`](crate::url::UdpUrlError::RecvPktSize)); the
    /// receiver itself ignores this field.
    pub pkt_size: Option<usize>,
    /// Local bind address override (for sending from a specific NIC).
    pub localaddr: Option<IpAddr>,
}

impl SocketConfig {
    /// The standard 7×188 MPEG-TS datagram payload size.
    pub const DEFAULT_PKT_SIZE: usize = 7 * 188;

    /// Resolved pkt_size — caller's override or the default.
    pub fn pkt_size_or_default(&self) -> usize {
        self.pkt_size.unwrap_or(Self::DEFAULT_PKT_SIZE)
    }

    /// Merge URL query knobs into self. URL-level knobs win over previously-set
    /// builder knobs (matches how `RtpSocketBuilder::from_url` behaves).
    pub fn merge_from_url(&mut self, url: &UdpUrl) {
        if let Some(iface) = &url.iface {
            self.iface = Some(iface.clone());
        }
        if let Some(ttl) = url.ttl {
            self.ttl = Some(ttl);
        }
        if let Some(tos) = url.tos {
            self.tos = Some(tos);
        }
        if let Some(rcvbuf) = url.rcvbuf {
            self.rcvbuf = Some(rcvbuf);
        }
        if let Some(sndbuf) = url.sndbuf {
            self.sndbuf = Some(sndbuf);
        }
        if let Some(pkt_size) = url.pkt_size {
            self.pkt_size = Some(pkt_size);
        }
        if let Some(localaddr) = url.localaddr {
            self.localaddr = Some(localaddr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::url::UdpUrl;

    #[test]
    fn default_pkt_size_is_7x188() {
        let cfg = SocketConfig::default();
        assert_eq!(cfg.pkt_size_or_default(), 1316);
    }

    #[test]
    fn merge_from_url_overrides_set_fields_only() {
        let mut cfg = SocketConfig {
            pkt_size: Some(7654),
            ttl: Some(99),
            ..Default::default()
        };
        let url = UdpUrl::parse("udp://239.10.0.1:5004?ttl=8").unwrap();
        cfg.merge_from_url(&url);
        // URL overrode ttl
        assert_eq!(cfg.ttl, Some(8));
        // pkt_size NOT in URL → original kept
        assert_eq!(cfg.pkt_size, Some(7654));
    }
}
