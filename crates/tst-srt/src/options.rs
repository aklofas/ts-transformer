//! Typed wrappers around libsrt SRTO_* options.
//!
//! Each wrapper validates its input at construction so the runtime layer
//! never has to re-validate. Bindings present these as their underlying
//! representations (string, u32, etc.).

use crate::error::{OptionError, PacketFilterError, PassphraseError, StreamIdError};
use secrecy::{ExposeSecret, SecretString};
use std::path::Path;

// ============================================================================
// Passphrase
// ============================================================================

/// SRT passphrase. Backed by `secrecy::SecretString` — zeroes on drop, redacts in Debug.
#[must_use]
#[derive(Clone)]
pub struct Passphrase(SecretString);

impl Passphrase {
    /// Construct from any string. Validates length (10–79) and ASCII-printable charset.
    ///
    /// # Errors
    /// - [`PassphraseError::InvalidLength`] if the character count is
    ///   outside the SRT-mandated 10..=79 range.
    /// - [`PassphraseError::InvalidCharset`] if any byte is outside
    ///   printable ASCII (`0x20..=0x7E`).
    pub fn new(s: impl Into<String>) -> Result<Self, PassphraseError> {
        let s = s.into();
        Self::validate(&s)?;
        Ok(Self(SecretString::from(s)))
    }

    /// Read from environment variable. Errors if unset or empty.
    ///
    /// # Errors
    /// - [`PassphraseError::EnvUnset`] if the named variable is unset
    ///   or empty.
    /// - Length / charset errors from [`Self::new`] propagate as
    ///   [`PassphraseError::InvalidLength`] / [`PassphraseError::InvalidCharset`].
    pub fn from_env(var: &str) -> Result<Self, PassphraseError> {
        let val = std::env::var(var).map_err(|_| PassphraseError::EnvUnset(var.to_string()))?;
        if val.is_empty() {
            return Err(PassphraseError::EnvUnset(var.to_string()));
        }
        Self::new(val)
    }

    /// Read from a file (one line; trailing newline stripped).
    ///
    /// # Errors
    /// - [`PassphraseError::Io`] if the file cannot be read.
    /// - Length / charset errors from [`Self::new`] propagate as
    ///   [`PassphraseError::InvalidLength`] / [`PassphraseError::InvalidCharset`].
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, PassphraseError> {
        let s = std::fs::read_to_string(path.as_ref())?;
        let s = s.trim_end_matches(['\n', '\r']).to_string();
        Self::new(s)
    }

    /// Expose the passphrase as a string slice. Prefer not to log the result.
    pub fn as_str(&self) -> &str {
        self.0.expose_secret()
    }

    /// Internal: hand the secret to libsrt's `srt_setsockflag`.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.expose_secret().as_bytes()
    }

    fn validate(s: &str) -> Result<(), PassphraseError> {
        let len = s.chars().count();
        if !(10..=79).contains(&len) {
            return Err(PassphraseError::InvalidLength(len));
        }
        if !s.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
            return Err(PassphraseError::InvalidCharset);
        }
        Ok(())
    }
}

impl std::fmt::Debug for Passphrase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Passphrase(<redacted>)")
    }
}

// ============================================================================
// KeyLength
// ============================================================================

/// AES key length for SRT encryption (`SRTO_PBKEYLEN`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyLength {
    #[default]
    Aes128,
    Aes192,
    Aes256,
}

impl KeyLength {
    pub(crate) fn as_bytes(self) -> i32 {
        match self {
            KeyLength::Aes128 => 16,
            KeyLength::Aes192 => 24,
            KeyLength::Aes256 => 32,
        }
    }

    /// Construct from libsrt-compatible byte count (16, 24, or 32).
    pub fn from_bytes(n: i32) -> Result<Self, OptionError> {
        match n {
            16 => Ok(KeyLength::Aes128),
            24 => Ok(KeyLength::Aes192),
            32 => Ok(KeyLength::Aes256),
            other => Err(OptionError::OutOfRange(format!(
                "pbkeylen must be 16, 24, or 32, got {other}"
            ))),
        }
    }
}

// ============================================================================
// MaxBandwidth
// ============================================================================

/// `SRTO_MAXBW` value. Wraps libsrt's overloaded sentinel ints.
///
/// libsrt 1.5.7 reads `-1` as "infinite" (live mode caps at 1 Gbps;
/// `congctl.cpp` `setMaxBW`), `0` as "relative to the input rate"
/// (`SRTO_INPUTBW` × (1 + `SRTO_OHEADBW`/100), or in-buffer rate sampling
/// when `SRTO_INPUTBW` is also 0; `core.cpp` `updateCC`), and a positive
/// value as an absolute cap in bytes per second. Its default is `-1` in
/// every mode; any value below `-1` is rejected. Each variant's doc names
/// the value this crate sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxBandwidth {
    /// Sends `0`, which libsrt reads as "relative to the input rate"
    /// (`SRTO_INPUTBW` × (1 + `SRTO_OHEADBW`/100), or in-buffer sampling
    /// when `SRTO_INPUTBW` is 0) — not "no cap".
    Unlimited,
    /// Sends `-1`, which libsrt reads as "infinite" (live mode caps at
    /// 1 Gbps) — libsrt's own default, not an input-rate derivation.
    Auto,
    /// Sends the value as an absolute cap in bytes per second.
    /// `Limited(0)` (also what `?maxbw=0` produces) sends `0`, the same
    /// relative-to-input behaviour as [`Self::Unlimited`]. A value above
    /// `i64::MAX` is clamped to `i64::MAX` (the option is a signed 64-bit
    /// integer; a wrapped cast would turn `u64::MAX` into `-1` = [`Self::Auto`]).
    Limited(u64),
}

impl MaxBandwidth {
    pub(crate) fn as_libsrt_i64(self) -> i64 {
        match self {
            MaxBandwidth::Unlimited => 0,
            MaxBandwidth::Auto => -1,
            MaxBandwidth::Limited(bps) => i64::try_from(bps).unwrap_or(i64::MAX),
        }
    }
}

// ============================================================================
// Congestion
// ============================================================================

/// `SRTO_CONGESTION` controller name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Congestion {
    #[default]
    Live,
    File,
}

impl Congestion {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Congestion::Live => "live",
            Congestion::File => "file",
        }
    }

    /// Parse libsrt-compatible enum name (`"live"` or `"file"`, lowercase).
    pub fn from_str_strict(s: &str) -> Result<Self, OptionError> {
        match s {
            "live" => Ok(Congestion::Live),
            "file" => Ok(Congestion::File),
            other => Err(OptionError::OutOfRange(format!(
                "congestion must be 'live' or 'file' (lowercase), got '{other}'"
            ))),
        }
    }
}

// ============================================================================
// StreamId
// ============================================================================

/// `SRTO_STREAMID` — application-defined identifier sent during handshake.
#[must_use]
#[derive(Debug, Clone)]
pub struct StreamId(String);

impl StreamId {
    pub fn new(s: impl Into<String>) -> Result<Self, StreamIdError> {
        let s = s.into();
        if s.len() > 512 {
            return Err(StreamIdError::TooLong(s.len()));
        }
        if !s.is_ascii() {
            return Err(StreamIdError::NonAscii);
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for StreamId {
    type Error = StreamIdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl TryFrom<&str> for StreamId {
    type Error = StreamIdError;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::new(s.to_string())
    }
}

// ============================================================================
// Role
// ============================================================================

/// Direction this socket is opened for. Drives `SRTO_SENDER` for HSv4-peer
/// latency-negotiation compatibility (mostly informational under HSv5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Role {
    /// Don't set `SRTO_SENDER`. libsrt defaults to 0 (= receiver under HSv4
    /// negotiation). Use for receiver pipelines.
    #[default]
    Receiver,
    /// Set `SRTO_SENDER=1`. Required for HSv4 peers (older Teradek/Makito
    /// gear, cable-industry hardware); harmless under HSv5.
    Sender,
}

// ============================================================================
// PacketFilter
// ============================================================================

/// `SRTO_PACKETFILTER` spec string (e.g. "fec,cols:10,rows:5,arq:onreq").
#[must_use]
#[derive(Debug, Clone)]
pub struct PacketFilter(String);

impl PacketFilter {
    pub fn new(spec: impl Into<String>) -> Result<Self, PacketFilterError> {
        let spec = spec.into();
        if spec.len() > 512 {
            return Err(PacketFilterError::TooLong);
        }
        let allowed =
            |c: char| c.is_ascii_alphanumeric() || matches!(c, ',' | ':' | '/' | '_' | '-');
        if !spec.chars().all(allowed) {
            return Err(PacketFilterError::InvalidCharset);
        }
        Ok(Self(spec))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ============================================================================
// Shared validation
// ============================================================================

/// `SRTO_OHEADBW`'s libsrt-mandated range. Shared by every site that would
/// otherwise duplicate this check: socket/listener option application and
/// the `oheadbw` URL key parser.
pub(crate) fn validate_overhead_bandwidth_pct(pct: u32) -> Result<(), String> {
    if !(5..=100).contains(&pct) {
        return Err(format!("overhead_bandwidth_pct must be 5..=100, got {pct}"));
    }
    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passphrase_valid() {
        let p = Passphrase::new("0123456789abcdef").unwrap();
        assert_eq!(p.as_bytes(), b"0123456789abcdef");
        // Debug is redacted.
        assert!(format!("{:?}", p).contains("redacted"));
    }

    #[test]
    fn passphrase_too_short() {
        assert!(matches!(
            Passphrase::new("short").unwrap_err(),
            PassphraseError::InvalidLength(5)
        ));
    }

    #[test]
    fn passphrase_too_long() {
        let s = "a".repeat(80);
        assert!(matches!(
            Passphrase::new(s).unwrap_err(),
            PassphraseError::InvalidLength(80)
        ));
    }

    #[test]
    fn passphrase_non_ascii() {
        // 10+ chars, but contains non-printable
        let s = "0123\x01abcdef";
        assert!(matches!(
            Passphrase::new(s).unwrap_err(),
            PassphraseError::InvalidCharset
        ));
    }

    #[test]
    fn passphrase_from_env_unset() {
        let var = "_SRT_CORE_TEST_NEVER_SET_";
        // SAFETY: removing an env var is process-wide; this var name is unique.
        unsafe {
            std::env::remove_var(var);
        }
        assert!(matches!(
            Passphrase::from_env(var).unwrap_err(),
            PassphraseError::EnvUnset(_)
        ));
    }

    #[test]
    fn key_length_bytes() {
        assert_eq!(KeyLength::Aes128.as_bytes(), 16);
        assert_eq!(KeyLength::Aes192.as_bytes(), 24);
        assert_eq!(KeyLength::Aes256.as_bytes(), 32);
    }

    #[test]
    fn max_bandwidth_libsrt_repr() {
        assert_eq!(MaxBandwidth::Unlimited.as_libsrt_i64(), 0);
        assert_eq!(MaxBandwidth::Auto.as_libsrt_i64(), -1);
        assert_eq!(MaxBandwidth::Limited(1_000_000).as_libsrt_i64(), 1_000_000);
    }

    /// A cap above `i64::MAX` must clamp, never wrap: `Limited(u64::MAX)`
    /// cast straight to `i64` is `-1`, which libsrt reads as `Auto`
    /// ("infinite"), and any other wrapped value is negative and rejected
    /// by `srt_setsockopt`. A caller asking for an absurdly large cap gets
    /// the largest cap the option can carry.
    #[test]
    fn max_bandwidth_limited_above_i64_max_clamps() {
        assert_eq!(MaxBandwidth::Limited(u64::MAX).as_libsrt_i64(), i64::MAX);
        assert_eq!(
            MaxBandwidth::Limited(i64::MAX as u64 + 1).as_libsrt_i64(),
            i64::MAX
        );
        assert_eq!(
            MaxBandwidth::Limited(i64::MAX as u64).as_libsrt_i64(),
            i64::MAX
        );
    }

    #[test]
    fn overhead_bandwidth_pct_range() {
        assert!(validate_overhead_bandwidth_pct(4).is_err());
        assert!(validate_overhead_bandwidth_pct(5).is_ok());
        assert!(validate_overhead_bandwidth_pct(100).is_ok());
        assert!(validate_overhead_bandwidth_pct(101).is_err());
    }

    #[test]
    fn congestion_str() {
        assert_eq!(Congestion::Live.as_str(), "live");
        assert_eq!(Congestion::File.as_str(), "file");
    }

    #[test]
    fn stream_id_valid() {
        let id = StreamId::new("publish:cam1").unwrap();
        assert_eq!(id.as_str(), "publish:cam1");
    }

    #[test]
    fn stream_id_too_long() {
        let s = "a".repeat(513);
        assert!(matches!(
            StreamId::new(s).unwrap_err(),
            StreamIdError::TooLong(513)
        ));
    }

    #[test]
    fn stream_id_non_ascii() {
        assert!(matches!(
            StreamId::new("café").unwrap_err(),
            StreamIdError::NonAscii
        ));
    }

    #[test]
    fn packet_filter_valid() {
        let f = PacketFilter::new("fec,cols:10,rows:5,arq:onreq").unwrap();
        assert_eq!(f.as_str(), "fec,cols:10,rows:5,arq:onreq");
    }

    #[test]
    fn packet_filter_too_long() {
        let s = "a".repeat(513);
        assert!(matches!(
            PacketFilter::new(s).unwrap_err(),
            PacketFilterError::TooLong
        ));
    }

    #[test]
    fn packet_filter_invalid_charset() {
        assert!(matches!(
            PacketFilter::new("fec,@invalid").unwrap_err(),
            PacketFilterError::InvalidCharset
        ));
    }
}
