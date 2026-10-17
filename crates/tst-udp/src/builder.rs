//! Builder types for [`UdpTransport`] and [`UdpRecvTransport`].

use std::net::IpAddr;

use crate::config::UdpConfig;
use crate::error::UdpError;
use crate::recv::UdpRecvTransport;
use crate::transport::UdpTransport;
use tst_core::url::UrlError;

use crate::url::UdpUrl;

/// Builder for [`UdpTransport`] (sender).
#[must_use]
#[derive(Debug, Clone)]
pub struct UdpTransportBuilder {
    url: UdpUrl,
    config: UdpConfig,
}

impl UdpTransportBuilder {
    /// Build from a `udp://` URL. Query-param knobs are applied automatically.
    pub fn from_url(url: &str) -> Result<Self, UrlError> {
        let url = UdpUrl::parse(url)?;
        if url.recv_bind {
            return Err(UrlError::Rejected(
                "URL has '@' prefix → use UdpRecvTransportBuilder instead".into(),
            ));
        }
        let config = url.config.clone();
        Ok(Self { url, config })
    }

    /// Multicast outgoing interface (literal IP or interface name).
    pub fn iface(&mut self, iface: impl Into<String>) -> &mut Self {
        self.config.iface = Some(iface.into());
        self
    }

    /// Multicast TTL / hop limit (1-255).
    pub fn ttl(&mut self, n: u8) -> &mut Self {
        self.config.ttl = Some(n);
        self
    }

    /// IP TOS / DSCP byte (e.g., 0xb8 for EF).
    pub fn tos(&mut self, n: u8) -> &mut Self {
        self.config.tos = Some(n);
        self
    }

    /// SO_SNDBUF size in bytes.
    pub fn sndbuf(&mut self, n: usize) -> &mut Self {
        self.config.sndbuf = Some(n);
        self
    }

    /// Datagram payload size (default 7×188 = 1316 bytes).
    pub fn pkt_size(&mut self, n: usize) -> &mut Self {
        self.config.pkt_size = Some(n);
        self
    }

    /// Local bind address override.
    pub fn localaddr(&mut self, addr: IpAddr) -> &mut Self {
        self.config.localaddr = Some(addr);
        self
    }

    /// Build the transport.
    pub fn build(&self) -> Result<UdpTransport, UdpError> {
        UdpTransport::with_config(&self.url, &self.config)
    }
}

/// Builder for [`UdpRecvTransport`] (receiver).
#[must_use]
#[derive(Debug, Clone)]
pub struct UdpRecvTransportBuilder {
    url: UdpUrl,
    config: UdpConfig,
}

impl UdpRecvTransportBuilder {
    pub fn from_url(url: &str) -> Result<Self, UrlError> {
        let url = UdpUrl::parse(url)?;
        if url.config.pkt_size.is_some() {
            return Err(crate::url::recv_pkt_size_rejection());
        }
        let config = url.config.clone();
        Ok(Self { url, config })
    }

    pub fn iface(&mut self, iface: impl Into<String>) -> &mut Self {
        self.config.iface = Some(iface.into());
        self
    }

    pub fn rcvbuf(&mut self, n: usize) -> &mut Self {
        self.config.rcvbuf = Some(n);
        self
    }

    pub fn build(&self) -> Result<UdpRecvTransport, UdpError> {
        UdpRecvTransport::with_config(&self.url, &self.config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_builder_chains() {
        let _b = UdpTransportBuilder::from_url("udp://239.10.0.1:5004")
            .unwrap()
            .iface("127.0.0.1")
            .ttl(8)
            .tos(0xb8)
            .pkt_size(1316);
    }

    #[test]
    fn recv_builder_chains() {
        let _b = UdpRecvTransportBuilder::from_url("udp://@127.0.0.1:0")
            .unwrap()
            .rcvbuf(8 * 1024 * 1024);
    }

    #[test]
    fn sender_rejects_at_prefix() {
        assert!(UdpTransportBuilder::from_url("udp://@239.10.0.1:5004").is_err());
    }

    #[test]
    fn recv_from_url_rejects_pkt_size() {
        let err = UdpRecvTransportBuilder::from_url("udp://@127.0.0.1:0?pkt_size=1316")
            .expect_err("recv URL with ?pkt_size= must be rejected");
        assert!(matches!(err, UrlError::Rejected(_)), "got: {err:?}");
        assert!(err.to_string().contains("send-side knob"));
    }

    #[test]
    fn send_from_url_pkt_size_still_accepted() {
        let b = UdpTransportBuilder::from_url("udp://239.10.0.1:5004?pkt_size=1316")
            .expect("send URL keeps ?pkt_size=");
        assert_eq!(b.config.pkt_size, Some(1316));
    }
}
