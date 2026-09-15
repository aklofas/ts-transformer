//! Builders for [`RistTransport`] and [`RistRecvTransport`].

use std::time::Duration;

use crate::config::{EncryptionKey, RistConfig, RistProfile};
use crate::error::RistError;
use crate::recv::RistRecvTransport;
use crate::transport::RistTransport;
use crate::url::RistUrl;

/// Fluent builder for [`RistTransport`].
///
/// ```no_run
/// use tst_rist::{RistTransportBuilder, RistProfile, EncryptionKey};
/// use std::time::Duration;
///
/// let sender = RistTransportBuilder::new("rist://1.2.3.4:8000")?
///     .profile(RistProfile::Main)
///     .buffer(Duration::from_millis(500))
///     .encryption(EncryptionKey::aes256("psk"))
///     .cname("uav-12")
///     .connect()?;
/// # Ok::<(), tst_rist::RistError>(())
/// ```
pub struct RistTransportBuilder {
    url: RistUrl,
    config: RistConfig,
}

impl RistTransportBuilder {
    /// Parse a `rist://` URL and start a builder. URL params seed config
    /// (matching [`RistConfig::merge_from_url`] semantics).
    pub fn new(url: &str) -> Result<Self, RistError> {
        let parsed = RistUrl::parse(url)?;
        if parsed.is_recv_bind {
            return Err(RistError::InvalidConfig(
                "URL has '@' prefix — use RistRecvTransportBuilder".into(),
            ));
        }
        let mut config = RistConfig::default();
        config.merge_from_url(&parsed);
        Ok(Self {
            url: parsed,
            config,
        })
    }

    /// Override profile. Forced to [`RistProfile::Main`] if encryption is set.
    pub fn profile(mut self, profile: RistProfile) -> Self {
        self.config.profile = profile;
        self
    }

    /// Recovery buffer duration.
    pub fn buffer(mut self, buffer: Duration) -> Self {
        self.config.buffer = buffer;
        self
    }

    /// Alias of [`Self::recovery_maxbitrate_kbps`] (same librist field);
    /// setting both to different values fails at `connect()`.
    pub fn bandwidth_kbps(mut self, kbps: u32) -> Self {
        self.config.bandwidth_kbps = Some(kbps);
        self
    }

    /// Retransmit bandwidth cap (kbps, librist `recovery_maxbitrate`).
    pub fn recovery_maxbitrate_kbps(mut self, kbps: u32) -> Self {
        self.config.recovery_maxbitrate_kbps = Some(kbps);
        self
    }

    /// Enable AES encryption. Forces profile to [`RistProfile::Main`].
    pub fn encryption(mut self, key: EncryptionKey) -> Self {
        self.config.encryption = Some(key);
        self.config.profile = RistProfile::Main;
        self
    }

    /// RTCP CNAME.
    pub fn cname(mut self, cname: impl Into<String>) -> Self {
        self.config.cname = Some(cname.into());
        self
    }

    /// Session timeout (ms).
    pub fn session_timeout(mut self, timeout: Duration) -> Self {
        self.config.session_timeout = Some(timeout);
        self
    }

    /// Enable NULL-packet deletion / compression.
    pub fn compression(mut self, enabled: bool) -> Self {
        self.config.compression = enabled;
        self
    }

    /// Per-send-call payload cap.
    pub fn pkt_size(mut self, n: usize) -> Self {
        self.config.pkt_size = n;
        self
    }

    /// Borrow the accumulated config (for inspection or further mutation).
    pub fn config(&self) -> &RistConfig {
        &self.config
    }

    /// Construct the transport.
    pub fn connect(self) -> Result<RistTransport, RistError> {
        RistTransport::connect_with_config(&self.url, &self.config)
    }
}

/// Fluent builder for [`RistRecvTransport`].
///
/// Mirrors [`RistTransportBuilder`] but consumes a bind URL
/// (`rist://@host:port`).
///
/// ```no_run
/// use tst_rist::{RistRecvTransportBuilder, RistProfile};
///
/// let recv = RistRecvTransportBuilder::new("rist://@0.0.0.0:8000")?
///     .profile(RistProfile::Main)
///     .listen()?;
/// # Ok::<(), tst_rist::RistError>(())
/// ```
pub struct RistRecvTransportBuilder {
    url: RistUrl,
    config: RistConfig,
}

impl RistRecvTransportBuilder {
    /// Parse a `rist://@host:port` URL and start a builder.
    pub fn new(url: &str) -> Result<Self, RistError> {
        let parsed = RistUrl::parse(url)?;
        if !parsed.is_recv_bind {
            return Err(RistError::InvalidConfig(
                "URL missing '@' prefix — use rist://@host:port for receivers".into(),
            ));
        }
        let mut config = RistConfig::default();
        config.merge_from_url(&parsed);
        Ok(Self {
            url: parsed,
            config,
        })
    }

    /// Override profile. Forced to [`RistProfile::Main`] if encryption is set.
    pub fn profile(mut self, profile: RistProfile) -> Self {
        self.config.profile = profile;
        self
    }

    /// Recovery buffer duration.
    pub fn buffer(mut self, buffer: Duration) -> Self {
        self.config.buffer = buffer;
        self
    }

    /// Enable AES encryption. Forces profile to [`RistProfile::Main`].
    pub fn encryption(mut self, key: EncryptionKey) -> Self {
        self.config.encryption = Some(key);
        self.config.profile = RistProfile::Main;
        self
    }

    /// RTCP CNAME.
    pub fn cname(mut self, cname: impl Into<String>) -> Self {
        self.config.cname = Some(cname.into());
        self
    }

    /// Session timeout.
    pub fn session_timeout(mut self, timeout: Duration) -> Self {
        self.config.session_timeout = Some(timeout);
        self
    }

    /// Borrow the accumulated config.
    pub fn config(&self) -> &RistConfig {
        &self.config
    }

    /// Construct the receiver.
    pub fn listen(self) -> Result<RistRecvTransport, RistError> {
        RistRecvTransport::listen_with_config(&self.url, &self.config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_builder_chains_config() {
        let b = RistTransportBuilder::new("rist://1.2.3.4:8000")
            .unwrap()
            .profile(RistProfile::Main)
            .buffer(Duration::from_millis(500))
            .bandwidth_kbps(10_000)
            .cname("test")
            .compression(true);
        let cfg = b.config();
        assert_eq!(cfg.profile, RistProfile::Main);
        assert_eq!(cfg.buffer, Duration::from_millis(500));
        assert_eq!(cfg.bandwidth_kbps, Some(10_000));
        assert_eq!(cfg.cname.as_deref(), Some("test"));
        assert!(cfg.compression);
    }

    #[test]
    fn sender_builder_encryption_forces_main_profile() {
        let b = RistTransportBuilder::new("rist://1.2.3.4:8000")
            .unwrap()
            .profile(RistProfile::Simple)
            .encryption(EncryptionKey::aes256("psk"));
        assert_eq!(b.config().profile, RistProfile::Main);
        assert!(b.config().encryption.is_some());
    }

    #[test]
    fn sender_builder_rejects_recv_bind_url() {
        let r = RistTransportBuilder::new("rist://@0.0.0.0:8000");
        assert!(matches!(r, Err(RistError::InvalidConfig(_))));
    }

    #[test]
    fn recv_builder_rejects_non_bind_url() {
        let r = RistRecvTransportBuilder::new("rist://1.2.3.4:8000");
        assert!(matches!(r, Err(RistError::InvalidConfig(_))));
    }

    #[test]
    fn recv_builder_chains_config() {
        let b = RistRecvTransportBuilder::new("rist://@0.0.0.0:8000")
            .unwrap()
            .profile(RistProfile::Main)
            .buffer(Duration::from_millis(300))
            .cname("recv-1");
        assert_eq!(b.config().profile, RistProfile::Main);
        assert_eq!(b.config().buffer, Duration::from_millis(300));
        assert_eq!(b.config().cname.as_deref(), Some("recv-1"));
    }
}
