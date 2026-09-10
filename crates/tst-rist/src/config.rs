//! [`RistConfig`] + [`RistProfile`] + encryption keys.

use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};

use crate::url::RistUrl;

/// AES pre-shared key. Backed by `secrecy::SecretString` — zeroes the
/// plaintext on drop and redacts in `Debug`, so it never leaks through a
/// `format!("{:?}", ..)` / `tracing::debug!(?..)` of an enclosing struct
/// nor lingers in freed heap.
///
/// Mirrors the `Passphrase` pattern in `tst-srt`. The plaintext is exposed
/// only via [`RistSecret::expose`], which is called exactly at the librist
/// FFI hand-off site — keep the result un-logged.
#[derive(Clone)]
pub struct RistSecret(SecretString);

impl RistSecret {
    /// Wrap a plaintext PSK.
    pub fn new(s: impl Into<String>) -> Self {
        Self(SecretString::from(s.into()))
    }

    /// Expose the plaintext PSK. Prefer not to log the result.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl std::fmt::Debug for RistSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RistSecret(<redacted>)")
    }
}

/// RIST profile. See VSF TR-06-1 (Simple) and TR-06-2 (Main).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RistProfile {
    /// Simple Profile — basic ARQ + multiplex. TR-06-1.
    Simple,
    /// Main Profile — adds encryption, RTCP, tunneling. TR-06-2.
    Main,
}

/// AES PSK with explicit key size.
#[derive(Debug, Clone)]
pub struct EncryptionKey {
    pub size_bits: u32,
    /// AES PSK. Redacting newtype — its plaintext never appears in `Debug`.
    pub secret: RistSecret,
    /// Optional key-rotation interval (librist `key_rotation` field, in packet count).
    /// 0 = no rotation.
    pub rotation: u32,
}

impl EncryptionKey {
    /// AES-128 PSK.
    pub fn aes128(secret: impl Into<String>) -> Self {
        Self {
            size_bits: 128,
            secret: RistSecret::new(secret),
            rotation: 0,
        }
    }
    /// AES-192 PSK.
    pub fn aes192(secret: impl Into<String>) -> Self {
        Self {
            size_bits: 192,
            secret: RistSecret::new(secret),
            rotation: 0,
        }
    }
    /// AES-256 PSK.
    pub fn aes256(secret: impl Into<String>) -> Self {
        Self {
            size_bits: 256,
            secret: RistSecret::new(secret),
            rotation: 0,
        }
    }
    /// Set the key-rotation packet count.
    pub fn rotation(mut self, count: u32) -> Self {
        self.rotation = count;
        self
    }
}

/// Per-transport librist configuration.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RistConfig {
    pub profile: RistProfile,
    /// Sender bandwidth cap, kbps.
    pub bandwidth_kbps: Option<u32>,
    /// Recovery buffer.
    pub buffer: Duration,
    /// Encryption (None = unencrypted). Forces Main profile when Some.
    pub encryption: Option<EncryptionKey>,
    /// RTCP CNAME.
    pub cname: Option<String>,
    /// Retransmit bandwidth cap, kbps.
    pub recovery_maxbitrate_kbps: Option<u32>,
    /// Receiver session timeout.
    pub session_timeout: Option<Duration>,
    /// NULL-packet deletion / compression.
    pub compression: bool,
    /// Per-send-call payload cap. Default 1316 (7 × 188, matches ffmpeg).
    pub pkt_size: usize,
}

impl Default for RistConfig {
    fn default() -> Self {
        Self {
            profile: RistProfile::Main,
            bandwidth_kbps: None,
            buffer: Duration::from_millis(200),
            encryption: None,
            cname: None,
            recovery_maxbitrate_kbps: None,
            session_timeout: None,
            compression: false,
            pkt_size: 7 * 188,
        }
    }
}

impl RistConfig {
    /// Default per-send-call payload cap (1316 bytes; 7 × 188).
    pub const DEFAULT_PKT_SIZE: usize = 7 * 188;

    /// Overlay URL-derived values on top of an existing config.
    /// Setting any encryption param promotes profile to Main.
    pub fn merge_from_url(&mut self, url: &RistUrl) {
        if let Some(p) = url.profile {
            self.profile = p;
        }
        if let Some(b) = url.bandwidth_kbps {
            self.bandwidth_kbps = Some(b);
        }
        if let Some(d) = url.buffer_ms {
            self.buffer = d;
        }
        if let (Some(bits), Some(secret)) = (url.aes_type, &url.secret) {
            self.encryption = Some(EncryptionKey {
                size_bits: bits,
                secret: secret.clone(),
                rotation: 0,
            });
            self.profile = RistProfile::Main;
        }
        if let Some(c) = &url.cname {
            self.cname = Some(c.clone());
        }
        if let Some(b) = url.recovery_maxbitrate_kbps {
            self.recovery_maxbitrate_kbps = Some(b);
        }
        if let Some(ms) = url.session_timeout_ms {
            self.session_timeout = Some(Duration::from_millis(ms as u64));
        }
        if let Some(c) = url.compression {
            self.compression = c;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_main_profile() {
        let cfg = RistConfig::default();
        assert_eq!(cfg.profile, RistProfile::Main);
        assert_eq!(cfg.pkt_size, RistConfig::DEFAULT_PKT_SIZE);
    }

    #[test]
    fn merge_from_url_promotes_to_main_on_encryption() {
        // RistConfig is #[non_exhaustive]; struct expression with
        // ..RistConfig::default() works because we're INSIDE the defining
        // crate (Rust RFC 2008).
        let mut cfg = RistConfig {
            profile: RistProfile::Simple,
            ..RistConfig::default()
        };
        let u = RistUrl::parse("rist://1.2.3.4:8000?aes-type=256&secret=s").unwrap();
        cfg.merge_from_url(&u);
        assert_eq!(cfg.profile, RistProfile::Main);
        assert!(cfg.encryption.is_some());
    }

    #[test]
    fn merge_from_url_secret_alone_yields_aes256_encryption() {
        let u = RistUrl::parse("rist://1.2.3.4:8000?secret=s").unwrap();
        let mut cfg = RistConfig::default();
        cfg.merge_from_url(&u);
        let k = cfg
            .encryption
            .as_ref()
            .expect("secret alone must configure encryption");
        assert_eq!(k.size_bits, 256);
        assert_eq!(cfg.profile, RistProfile::Main);
    }

    #[test]
    fn encryption_key_builder() {
        let k = EncryptionKey::aes256("abc").rotation(1000);
        assert_eq!(k.size_bits, 256);
        assert_eq!(k.rotation, 1000);
    }
}
