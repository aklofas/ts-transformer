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
/// let mut builder = RistTransportBuilder::new("rist://1.2.3.4:8000")?;
/// builder
///     .profile(RistProfile::Main)
///     .buffer(Duration::from_millis(500))
///     .encryption(EncryptionKey::aes256("psk"))
///     .cname("uav-12");
/// let sender = builder.connect()?;
/// # Ok::<(), tst_rist::RistError>(())
/// ```
pub struct RistTransportBuilder {
    url: RistUrl,
    config: RistConfig,
}

impl RistTransportBuilder {
    /// Parse a `rist://` URL and start a builder. URL params seed config
    /// (the same keys as every `rist://` entry point).
    pub fn new(url: &str) -> Result<Self, RistError> {
        let parsed = RistUrl::parse(url)?;
        if parsed.is_recv_bind {
            return Err(RistError::InvalidConfig(
                "URL has '@' prefix — use RistRecvTransportBuilder".into(),
            ));
        }
        let config = parsed.config.clone();
        Ok(Self {
            url: parsed,
            config,
        })
    }

    /// Override profile. [`Self::encryption`] sets [`RistProfile::Main`];
    /// the later of the two calls wins, and a later `profile(Simple)` makes
    /// `connect`/`listen` fail while a key is set.
    pub fn profile(&mut self, profile: RistProfile) -> &mut Self {
        self.config.profile = profile;
        self
    }

    /// Recovery window, fixed (librist's minimum and maximum recovery length).
    pub fn buffer(&mut self, buffer: Duration) -> &mut Self {
        self.config.buffer = buffer;
        self
    }

    /// Alias of [`Self::recovery_maxbitrate_kbps`] (same librist field);
    /// setting both to different values fails at `connect()`.
    pub fn bandwidth_kbps(&mut self, kbps: u32) -> &mut Self {
        self.config.bandwidth_kbps = Some(kbps);
        self
    }

    /// Ceiling on payload plus retransmissions (kbps, librist
    /// `recovery_maxbitrate`).
    pub fn recovery_maxbitrate_kbps(&mut self, kbps: u32) -> &mut Self {
        self.config.recovery_maxbitrate_kbps = Some(kbps);
        self
    }

    /// Enable AES encryption. Forces profile to [`RistProfile::Main`].
    pub fn encryption(&mut self, key: EncryptionKey) -> &mut Self {
        self.config.encryption = Some(key);
        self.config.profile = RistProfile::Main;
        self
    }

    /// RTCP CNAME.
    pub fn cname(&mut self, cname: impl Into<String>) -> &mut Self {
        self.config.cname = Some(cname.into());
        self
    }

    /// Session timeout (ms).
    pub fn session_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.config.session_timeout = Some(timeout);
        self
    }

    /// Enable NULL-packet deletion / compression.
    pub fn compression(&mut self, enabled: bool) -> &mut Self {
        self.config.compression = enabled;
        self
    }

    /// Per-send-call payload cap, `1..=`[`RistConfig::MAX_PKT_SIZE`]; a sender
    /// refuses anything else at connect.
    pub fn pkt_size(&mut self, n: usize) -> &mut Self {
        self.config.pkt_size = n;
        self
    }

    /// Borrow the accumulated config (for inspection or further mutation).
    pub fn config(&self) -> &RistConfig {
        &self.config
    }

    /// Construct the transport.
    pub fn connect(&self) -> Result<RistTransport, RistError> {
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
/// let mut builder = RistRecvTransportBuilder::new("rist://@0.0.0.0:8000")?;
/// builder.profile(RistProfile::Main);
/// let recv = builder.listen()?;
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
        let config = parsed.config.clone();
        Ok(Self {
            url: parsed,
            config,
        })
    }

    /// Override profile. [`Self::encryption`] sets [`RistProfile::Main`];
    /// the later of the two calls wins, and a later `profile(Simple)` makes
    /// `connect`/`listen` fail while a key is set.
    pub fn profile(&mut self, profile: RistProfile) -> &mut Self {
        self.config.profile = profile;
        self
    }

    /// Recovery window, fixed (librist's minimum and maximum recovery length).
    pub fn buffer(&mut self, buffer: Duration) -> &mut Self {
        self.config.buffer = buffer;
        self
    }

    /// Enable AES encryption. Forces profile to [`RistProfile::Main`].
    pub fn encryption(&mut self, key: EncryptionKey) -> &mut Self {
        self.config.encryption = Some(key);
        self.config.profile = RistProfile::Main;
        self
    }

    /// RTCP CNAME.
    pub fn cname(&mut self, cname: impl Into<String>) -> &mut Self {
        self.config.cname = Some(cname.into());
        self
    }

    /// Session timeout.
    pub fn session_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.config.session_timeout = Some(timeout);
        self
    }

    /// Borrow the accumulated config.
    pub fn config(&self) -> &RistConfig {
        &self.config
    }

    /// Construct the receiver.
    pub fn listen(&self) -> Result<RistRecvTransport, RistError> {
        RistRecvTransport::listen_with_config(&self.url, &self.config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_builder_chains_config() {
        let mut b = RistTransportBuilder::new("rist://1.2.3.4:8000").unwrap();
        b.profile(RistProfile::Main)
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
        let mut b = RistTransportBuilder::new("rist://1.2.3.4:8000").unwrap();
        b.profile(RistProfile::Simple)
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
        let mut b = RistRecvTransportBuilder::new("rist://@0.0.0.0:8000").unwrap();
        b.profile(RistProfile::Main)
            .buffer(Duration::from_millis(300))
            .cname("recv-1");
        assert_eq!(b.config().profile, RistProfile::Main);
        assert_eq!(b.config().buffer, Duration::from_millis(300));
        assert_eq!(b.config().cname.as_deref(), Some("recv-1"));
    }

    /// librist udp.c: the Simple profile sends RTP straight to UDP, so a key
    /// would be silently unused.
    #[cfg(feature = "mbedtls")]
    #[test]
    fn encryption_then_simple_is_refused_at_connect() {
        let mut b = RistTransportBuilder::new("rist://127.0.0.1:19010").unwrap();
        b.encryption(EncryptionKey::aes256("example-secret"))
            .profile(RistProfile::Simple);
        assert!(matches!(b.connect(), Err(RistError::InvalidConfig(ref m)) if m.contains("Main")));
    }

    #[cfg(feature = "mbedtls")]
    #[test]
    fn receiver_encryption_then_simple_is_refused_at_listen() {
        let mut b = RistRecvTransportBuilder::new("rist://@127.0.0.1:19012").unwrap();
        b.encryption(EncryptionKey::aes256("example-secret"))
            .profile(RistProfile::Simple);
        assert!(matches!(b.listen(), Err(RistError::InvalidConfig(ref m)) if m.contains("Main")));
    }

    #[cfg(feature = "mbedtls")]
    #[test]
    fn direct_config_simple_with_encryption_is_refused() {
        let url = crate::url::RistUrl::parse("rist://127.0.0.1:19014").unwrap();
        let cfg = RistConfig {
            profile: RistProfile::Simple,
            encryption: Some(EncryptionKey::aes128("example-secret")),
            ..RistConfig::default()
        };
        assert!(matches!(
            RistTransport::connect_with_config(&url, &cfg),
            Err(RistError::InvalidConfig(_))
        ));
    }
}
