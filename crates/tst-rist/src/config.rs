//! [`RistConfig`] + [`RistProfile`] + encryption keys.

use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};

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
    /// Alias of [`Self::recovery_maxbitrate_kbps`] (same librist field,
    /// `recovery_maxbitrate`). Setting both to different values fails at
    /// connect/listen with [`crate::RistError::InvalidConfig`].
    pub bandwidth_kbps: Option<u32>,
    /// Recovery window, fixed: librist's minimum and maximum recovery length
    /// both take this value. Must be at least 1 ms.
    pub buffer: Duration,
    /// Encryption (None = unencrypted). The builders' `encryption` call
    /// and a URL `secret` also set [`Self::profile`] to Main; a config
    /// built by hand is used as given, and encryption with
    /// [`RistProfile::Simple`] fails at connect/listen.
    pub encryption: Option<EncryptionKey>,
    /// RTCP CNAME.
    pub cname: Option<String>,
    /// Retransmit bandwidth cap, kbps (librist `recovery_maxbitrate`).
    /// [`Self::bandwidth_kbps`] is an alias.
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
    fn encryption_key_builder() {
        let k = EncryptionKey::aes256("abc").rotation(1000);
        assert_eq!(k.size_bits, 256);
        assert_eq!(k.rotation, 1000);
    }
}
