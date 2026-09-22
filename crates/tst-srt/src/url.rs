//! `srt://host:port?key=value&...` URL parser.
//!
//! Vocabulary: libsrt-URL keys (strict — recognizes libsrt's key set,
//! rejects any key for which this library doesn't yet expose a builder
//! setter, with a clear "known but unsupported" error).
//!
//! Plus two `tst-c`-flavor extensions: `x-recvtimeout` / `x-sendtimeout`
//! (no libsrt-URL precedent; `SRTO_RCVTIMEO` / `SRTO_SNDTIMEO`).
//!
//! See `/docs/guides/srt.md`'s URL keys section for the full user-facing key
//! reference.

use crate::config::{ListenerConfig, SocketConfig};
use crate::error::{OptionError, SrtError};
use crate::listener::Listener;
use crate::options::{Congestion, KeyLength, MaxBandwidth, PacketFilter, Passphrase, StreamId};
use crate::socket::Socket;
use crate::transport::SrtTransport;
use std::time::Duration;
use tst_core::cancel::CancelSlot;
use tst_core::url::common::{UrlError as CoreUrlError, parse_url};

/// `?latency=N` is parsed as N milliseconds (libsrt-URL canonical), but
/// ffmpeg's URL parses it as microseconds. A user copying an ffmpeg URL
/// gets 1000x too much receiver buffer with no error. This threshold
/// (10s) is well above any realistic live-streaming latency, so any
/// value at or above it is almost certainly a unit-misunderstanding.
const SUSPICIOUS_LATENCY_MS: i32 = 10_000;

fn warn_if_suspicious_latency(key: &str, ms: i32) {
    if ms >= SUSPICIOUS_LATENCY_MS {
        tracing::warn!(
            url_key = key,
            value_ms = ms,
            "SRT URL '{}={}' parses to {}ms ({}s) of buffer - \
             unusually high. Note: ffmpeg uses microseconds while ts-transformer \
             (libsrt-URL canonical) uses milliseconds. If you copied this \
             from an ffmpeg URL, divide by 1000.",
            key,
            ms,
            ms,
            ms / 1000,
        );
    }
}

/// Group 3 (spec §4.3): libsrt-URL keys we recognize but don't yet expose.
/// Each entry maps the URL key to its `SRTO_*` name for error messages.
const GROUP3_REJECTED: &[(&str, &str)] = &[
    ("bindtodevice", "SRTO_BINDTODEVICE"),
    ("cryptomode", "SRTO_CRYPTOMODE"),
    ("drifttracer", "SRTO_DRIFTTRACER"),
    ("enforcedencryption", "SRTO_ENFORCEDENCRYPTION"),
    ("groupconnect", "SRTO_GROUPCONNECT"),
    ("groupminstabletimeo", "SRTO_GROUPMINSTABLETIMEO"),
    ("iptos", "SRTO_IPTOS"),
    ("ipttl", "SRTO_IPTTL"),
    ("ipv6only", "SRTO_IPV6ONLY"),
    ("kmpreannounce", "SRTO_KMPREANNOUNCE"),
    ("kmrefreshrate", "SRTO_KMREFRESHRATE"),
    ("maxrexmitbw", "SRTO_MAXREXMITBW"),
    ("messageapi", "SRTO_MESSAGEAPI"),
    ("mininputbw", "SRTO_MININPUTBW"),
    ("minversion", "SRTO_MINVERSION"),
    ("nakreport", "SRTO_NAKREPORT"),
    ("peeridletimeo", "SRTO_PEERIDLETIMEO"),
    ("rcvbuf", "SRTO_RCVBUF"),
    ("retransmitalgo", "SRTO_RETRANSMITALGO"),
    ("sndbuf", "SRTO_SNDBUF"),
    ("snddropdelay", "SRTO_SNDDROPDELAY"),
    ("transtype", "SRTO_TRANSTYPE"),
    ("tsbpdmode", "SRTO_TSBPDMODE"),
];

fn group3_lookup(key: &str) -> Option<&'static str> {
    GROUP3_REJECTED
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, srto)| *srto)
}

/// Connection mode for an SRT endpoint. Driven by the `?mode=` URL key
/// (default: `caller`). Determines whether the endpoint connects out
/// (caller) or binds-and-accepts (listener).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Connect out to a peer's listener. Default when `?mode=` is absent
    /// or set to `caller`.
    #[default]
    Caller,
    /// Bind a local port and accept the first incoming connection.
    /// Requires `?mode=listener` in the URL. Allows an empty host
    /// (e.g. `srt://:7000`) — empty host binds to the platform's default
    /// wildcard address (typically `0.0.0.0`); a non-empty host binds to
    /// that specific interface.
    Listener,
}

/// Parsed `srt://host:port?...` URL: connection target + a typed overlay
/// of the recognized query parameters.
#[must_use]
#[derive(Debug, Clone)]
pub struct SrtUrl {
    pub host: String,
    pub port: u16,
    pub overlay: UrlOverlay,
    pub mode: Mode,
}

/// Typed overlay of query-parameter values. Apply via `apply_to_socket`
/// or `apply_to_listener`. URL wins on conflict (Q4-A precedence rule).
#[derive(Debug, Default, Clone)]
pub struct UrlOverlay {
    // Group 1 — libsrt-URL honored keys.
    pub passphrase: Option<Passphrase>,
    pub key_length: Option<KeyLength>,
    pub latency: Option<Duration>,
    pub recv_latency: Option<Duration>,
    pub peer_latency: Option<Duration>,
    pub mss: Option<u16>,
    pub payload_size: Option<u16>,
    pub max_bandwidth: Option<MaxBandwidth>,
    pub input_bandwidth: Option<u64>,
    pub overhead_bandwidth_pct: Option<u8>,
    pub stream_id: Option<StreamId>,
    pub loss_max_ttl: Option<u32>,
    pub too_late_packet_drop: Option<bool>,
    pub flow_window_packets: Option<u32>,
    pub packet_filter: Option<PacketFilter>,
    pub congestion: Option<Congestion>,

    // Group 2 — `tst-c` extension keys.
    pub recv_timeout: Option<Duration>,
    pub send_timeout: Option<Duration>,

    // Group 1 — connect timeout. Honored from libsrt URL vocabulary
    // (`conntimeo`); also accepts ffmpeg-style alias `connect_timeout`.
    pub connect_timeout: Option<Duration>,

    // Group 1 — `SRTO_LINGER` close-grace period (seconds, matching ffmpeg).
    pub linger: Option<Duration>,

    // Group 1 — kernel UDP socket buffer sizes. Honored from libsrt URL
    // vocabulary (`udprcvbuf`/`udpsndbuf`); also accepts ffmpeg-style
    // aliases `recv_buffer_size`/`send_buffer_size`.
    pub udp_recv_buffer_bytes: Option<u32>,
    pub udp_send_buffer_bytes: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UrlError {
    #[error("URL parse failed: {0}")]
    Syntax(#[from] CoreUrlError),

    #[error("scheme must be 'srt', got '{got}'")]
    WrongScheme { got: String },

    #[error("URL must include port (e.g. srt://host:9000), got no port")]
    MissingPort,

    #[error("URL must include host")]
    MissingHost,

    #[error("userinfo (user:pass@) is not supported in SRT URLs; use ?passphrase=... instead")]
    UserinfoNotSupported,

    #[error("unsupported mode '{mode}'; only mode=caller and mode=listener are accepted")]
    UnsupportedMode { mode: String },

    #[error(
        "option '{key}' (libsrt {srto}) is recognized but not yet exposed by this library; see deferred-features.md"
    )]
    UnsupportedKey { key: String, srto: &'static str },

    #[error("ffmpeg URL alias '{key}' ({canonical}) is not exposed by this library; {suggestion}")]
    FfmpegAliasNotExposed {
        key: String,
        canonical: &'static str,
        suggestion: &'static str,
    },

    #[error("unknown URL key '{key}'")]
    UnknownKey { key: String },

    #[error("invalid value for '{key}': {detail}")]
    InvalidValue { key: String, detail: String },

    #[error("option validation failed for '{key}': {source}")]
    OptionValidation {
        key: String,
        #[source]
        source: OptionError,
    },
}

impl SrtUrl {
    /// Parse `srt://host:port?key=value&...` into validated parts.
    ///
    /// Strict ASCII canonical forms (decimal-only integers, `0`/`1` for
    /// bools, lowercase enums); the common parser URL-decodes percent
    /// sequences (`%XX` only — `+` is NOT decoded as space). Last-occurrence
    /// wins on duplicate keys.
    ///
    /// # Example
    ///
    /// ```
    /// use tst_srt::SrtUrl;
    /// use std::time::Duration;
    ///
    /// let u = SrtUrl::parse(
    ///     "srt://camera.local:9000?streamid=front&latency=200",
    /// ).unwrap();
    /// assert_eq!(u.host, "camera.local");
    /// assert_eq!(u.port, 9000);
    /// assert_eq!(u.overlay.latency, Some(Duration::from_millis(200)));
    /// ```
    pub fn parse(s: &str) -> Result<Self, UrlError> {
        // The common parser accepts empty-host URLs (e.g. srt://:7000?mode=listener)
        // directly — it returns Ok with host="" rather than an error. We check
        // mode after parsing query parameters to decide whether an empty host
        // is acceptable (listener) or an error (caller).
        let parsed = parse_url(s)?;

        if parsed.scheme != "srt" {
            return Err(UrlError::WrongScheme {
                got: parsed.scheme.to_string(),
            });
        }
        if parsed.username.is_some() || parsed.password.is_some() {
            return Err(UrlError::UserinfoNotSupported);
        }

        // The common parser strips IPv6 brackets, so host is always the
        // raw address string (e.g. "::1" not "[::1]").
        let host = parsed.host.to_string();
        let port = parsed.port.ok_or(UrlError::MissingPort)?;

        let mut overlay = UrlOverlay::default();
        let mut mode = Mode::Caller;
        // Last-occurrence wins (Q4-A): we just overwrite as we go.
        for (key, value) in &parsed.query {
            // mode is consumed at the SrtUrl level (not in overlay) since it
            // determines which apply_to_* method the caller should use.
            if key.as_ref() == "mode" {
                mode = match value.as_ref() {
                    "caller" => Mode::Caller,
                    "listener" => Mode::Listener,
                    other => {
                        return Err(UrlError::UnsupportedMode {
                            mode: other.to_string(),
                        });
                    }
                };
                continue;
            }
            apply_query_pair(&mut overlay, key.as_ref(), value.as_ref())?;
        }

        // Caller mode requires a non-empty host (need somewhere to connect).
        // Listener mode allows empty host (bind-to-wildcard).
        if host.is_empty() && mode != Mode::Listener {
            return Err(UrlError::MissingHost);
        }

        Ok(Self {
            host,
            port,
            overlay,
            mode,
        })
    }

    /// Open this URL as a **caller**: dial `host:port` with the overlay
    /// applied and the sender preset merged underneath it, and return the
    /// connected transport.
    ///
    /// This is the one composition every binding used to carry a private
    /// copy of (tst-c's `connect_srt`, the Python and JVM mirrors):
    /// [`UrlOverlay::apply_to_socket`] on a default [`SocketConfig`] →
    /// [`SocketConfig::merge_sender_defaults`] (15 s connect timeout,
    /// 5 s linger, `Role::Sender` — each only where the overlay left it
    /// unset, so the URL wins) → an IPv6-safe `host:port` join
    /// ([`crate::addr::join_host_port`]) → [`Socket::connect_with`], which
    /// walks every resolved address. The bindings open caller-mode
    /// *receivers* through this same preset today; that is preserved.
    ///
    /// [`mode`](Self::mode) is not consulted: the caller chooses the
    /// direction by calling this or [`accept_one`](Self::accept_one)
    /// (`tst_srt::shells` dispatches on `mode` for you).
    ///
    /// # Errors
    ///
    /// [`SrtError::Connect`] carrying the typed [`ConnectError`]
    /// (resolution, handshake reject, timeout, refused, option refused).
    ///
    /// [`ConnectError`]: crate::ConnectError
    pub fn connect(&self) -> Result<SrtTransport, SrtError> {
        let mut cfg = SocketConfig::default();
        self.overlay.apply_to_socket(&mut cfg);
        cfg.merge_sender_defaults();
        let addr = crate::addr::join_host_port(&self.host, self.port);
        let socket = Socket::connect_with(&cfg, addr.as_str())?;
        Ok(SrtTransport::new(socket))
    }

    /// Open this URL as a **listener**: bind `host:port` (an empty host
    /// binds the wildcard `0.0.0.0`), accept **one** peer, and return it
    /// as a transport — with the accept reachable by `cancel.cancel()`
    /// from another thread.
    ///
    /// The bind → install-into-slot → accept → clear → classify sequence
    /// is [`Listener::accept_one_cancellable`]; this method only builds
    /// the [`ListenerConfig`] from the overlay
    /// ([`UrlOverlay::apply_to_listener`]) and renders the bind address
    /// the way the bindings' `listen_srt` did. Single-accept: the
    /// listener is dropped on return. Share the same `cancel` slot with
    /// a managed transport's [`FactoryCancel`](tst_core::cancel::CancelSlot)
    /// so a re-accept parked with no peer in sight can be woken by the
    /// transport's cancel handle — `tst_srt::shells` wires that.
    ///
    /// # Errors
    ///
    /// - [`SrtError::Transport`]`(`[`TransportError::ExplicitClose`]`)` —
    ///   `cancel` was cancelled before or during the accept.
    /// - [`SrtError::Transport`]`(`[`TransportError::Broken`]`)` — bind or
    ///   accept fault; the message carries `bind: …` / `accept: …`.
    ///
    /// [`TransportError::ExplicitClose`]: tst_core::transport::TransportError::ExplicitClose
    /// [`TransportError::Broken`]: tst_core::transport::TransportError::Broken
    pub fn accept_one(&self, cancel: &CancelSlot) -> Result<SrtTransport, SrtError> {
        let mut cfg = ListenerConfig::default();
        self.overlay.apply_to_listener(&mut cfg);
        let bind_host = if self.host.is_empty() {
            "0.0.0.0"
        } else {
            self.host.as_str()
        };
        let addr = crate::addr::join_host_port(bind_host, self.port);
        Ok(Listener::accept_one_cancellable(&cfg, &addr, cancel)?)
    }
}

fn parse_int_nonneg<T>(key: &str, value: &str) -> Result<T, UrlError>
where
    T: std::str::FromStr<Err = std::num::ParseIntError>,
{
    // Strict-A: bare decimal, no suffix, non-negative. We rely on T's
    // from_str to enforce range. Use this for unsigned T (u8/u16/u32/u64);
    // signed callers go through parse_i32_nonneg, which adds the sign check.
    value.parse::<T>().map_err(|e| UrlError::InvalidValue {
        key: key.to_string(),
        detail: format!("expected non-negative decimal integer in range, got '{value}': {e}"),
    })
}

fn parse_i32_nonneg(key: &str, value: &str) -> Result<i32, UrlError> {
    let n: i32 = value.parse().map_err(|e| UrlError::InvalidValue {
        key: key.to_string(),
        detail: format!("expected i32 decimal, got '{value}': {e}"),
    })?;
    if n < 0 {
        return Err(UrlError::InvalidValue {
            key: key.to_string(),
            detail: format!("must be non-negative, got {n}"),
        });
    }
    Ok(n)
}

fn parse_oheadbw(value: &str) -> Result<u8, UrlError> {
    let n: u32 = parse_int_nonneg("oheadbw", value)?;
    crate::options::validate_overhead_bandwidth_pct(n).map_err(|detail| {
        UrlError::InvalidValue {
            key: "oheadbw".into(),
            detail,
        }
    })?;
    Ok(n as u8)
}

fn parse_bool_strict(key: &str, value: &str) -> Result<bool, UrlError> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        other => Err(UrlError::InvalidValue {
            key: key.to_string(),
            detail: format!("expected '0' or '1', got '{other}'"),
        }),
    }
}

fn apply_query_pair(overlay: &mut UrlOverlay, key: &str, value: &str) -> Result<(), UrlError> {
    match key {
        "congestion" | "smoother" => apply_congestion(overlay, value),
        "conntimeo" | "connect_timeout" => apply_connect_timeout(overlay, value),
        // libsrt-URL canonical key is `fc` (flow control window); `ffs` is
        // the ffmpeg-style alias (flight flag size).
        "fc" | "ffs" => {
            overlay.flow_window_packets = Some(parse_int_nonneg("fc", value)?);
            Ok(())
        }
        "inputbw" => {
            overlay.input_bandwidth = Some(parse_int_nonneg("inputbw", value)?);
            Ok(())
        }
        "latency" | "tsbpddelay" => apply_latency(overlay, "latency", value),
        "linger" => {
            // SRTO_LINGER value is in seconds (matches ffmpeg's URL).
            let n = parse_i32_nonneg("linger", value)?;
            overlay.linger = Some(Duration::from_secs(n as u64));
            Ok(())
        }
        "lossmaxttl" => {
            overlay.loss_max_ttl = Some(parse_int_nonneg("lossmaxttl", value)?);
            Ok(())
        }
        "maxbw" => apply_maxbw(overlay, value),
        "mss" => {
            overlay.mss = Some(parse_int_nonneg::<u16>("mss", value)?);
            Ok(())
        }
        "oheadbw" => apply_oheadbw(overlay, value),
        "packetfilter" => apply_packetfilter(overlay, value),
        "passphrase" => apply_passphrase(overlay, value),
        "payloadsize" | "pkt_size" | "payload_size" => apply_payloadsize(overlay, value),
        "pbkeylen" => apply_pbkeylen(overlay, value),
        "peerlatency" => apply_latency(overlay, "peerlatency", value),
        "rcvlatency" => apply_latency(overlay, "rcvlatency", value),
        "streamid" | "srt_streamid" => apply_streamid(overlay, value),
        "tlpktdrop" => apply_tlpktdrop(overlay, value),
        // libsrt-URL canonical key is `udprcvbuf`; `recv_buffer_size` /
        // `send_buffer_size` are ffmpeg-style aliases.
        "udprcvbuf" | "recv_buffer_size" => {
            overlay.udp_recv_buffer_bytes = Some(parse_int_nonneg("udprcvbuf", value)?);
            Ok(())
        }
        "udpsndbuf" | "send_buffer_size" => {
            overlay.udp_send_buffer_bytes = Some(parse_int_nonneg("udpsndbuf", value)?);
            Ok(())
        }
        "x-recvtimeout" => {
            let n = parse_i32_nonneg("x-recvtimeout", value)?;
            overlay.recv_timeout = Some(Duration::from_millis(n as u64));
            Ok(())
        }
        "x-sendtimeout" => {
            let n = parse_i32_nonneg("x-sendtimeout", value)?;
            overlay.send_timeout = Some(Duration::from_millis(n as u64));
            Ok(())
        }
        "timeout" | "listen_timeout" | "tsbpd" => Err(ffmpeg_alias_not_exposed(key)),
        other => fallback_unknown(other),
    }
}

// ── per-parameter-family helpers ─────────────────────────────────────────────

fn apply_congestion(overlay: &mut UrlOverlay, value: &str) -> Result<(), UrlError> {
    // libsrt-URL canonical key is `congestion` (renamed from `smoother`
    // in libsrt 1.4.1); `smoother` is the pre-rename ffmpeg-style alias.
    overlay.congestion =
        Some(
            Congestion::from_str_strict(value).map_err(|source| UrlError::OptionValidation {
                key: "congestion".into(),
                source,
            })?,
        );
    Ok(())
}

fn apply_connect_timeout(overlay: &mut UrlOverlay, value: &str) -> Result<(), UrlError> {
    // libsrt-URL canonical key is `conntimeo` (milliseconds);
    // `connect_timeout` is the ffmpeg-style alias.
    let n = parse_i32_nonneg("conntimeo", value)?;
    overlay.connect_timeout = Some(Duration::from_millis(n as u64));
    Ok(())
}

fn apply_latency(overlay: &mut UrlOverlay, key: &'static str, value: &str) -> Result<(), UrlError> {
    // libsrt-URL canonical key is `latency`; `tsbpddelay` is the
    // ffmpeg-style alias (the `SRTO_*` constant for SRT's TSBPD
    // mechanism is `SRTO_LATENCY`).
    let n = parse_i32_nonneg(key, value)?;
    warn_if_suspicious_latency(key, n);
    // n is a non-negative i32; widening to u64 is lossless.
    let dur = Duration::from_millis(n as u64);
    match key {
        "latency" => overlay.latency = Some(dur),
        "peerlatency" => overlay.peer_latency = Some(dur),
        "rcvlatency" => overlay.recv_latency = Some(dur),
        _ => unreachable!("apply_latency called with non-latency key {key}"),
    }
    Ok(())
}

fn apply_maxbw(overlay: &mut UrlOverlay, value: &str) -> Result<(), UrlError> {
    // SRTO_MAXBW is i64; we expose non-negative as Limited(u64).
    // Negative-sentinel forms (Auto/Infinite) are not URL-settable
    // under strict-A.
    let n = parse_int_nonneg::<u64>("maxbw", value)?;
    overlay.max_bandwidth = Some(MaxBandwidth::Limited(n));
    Ok(())
}

fn apply_oheadbw(overlay: &mut UrlOverlay, value: &str) -> Result<(), UrlError> {
    overlay.overhead_bandwidth_pct = Some(parse_oheadbw(value)?);
    Ok(())
}

fn apply_packetfilter(overlay: &mut UrlOverlay, value: &str) -> Result<(), UrlError> {
    overlay.packet_filter =
        Some(
            PacketFilter::new(value.to_string()).map_err(|e| UrlError::OptionValidation {
                key: "packetfilter".into(),
                source: OptionError::from(e),
            })?,
        );
    Ok(())
}

fn apply_passphrase(overlay: &mut UrlOverlay, value: &str) -> Result<(), UrlError> {
    overlay.passphrase =
        Some(
            Passphrase::new(value.to_string()).map_err(|e| UrlError::OptionValidation {
                key: "passphrase".into(),
                source: OptionError::from(e),
            })?,
        );
    Ok(())
}

fn apply_payloadsize(overlay: &mut UrlOverlay, value: &str) -> Result<(), UrlError> {
    // libsrt-URL canonical key is `payloadsize`; `pkt_size` and
    // `payload_size` are ffmpeg-style aliases.
    //
    // Cap at libsrt's live-mode maximum SRT_LIVE_MAX_PLSIZE = 1456
    // (srt.h:297). ffmpeg clamps this via AVOption.max in
    // libsrt.c:107-108. Without a parse-time cap, accepting any u16
    // (up to 65535) defers the failure to apply_socket_config's
    // PRE setsockopt, which surfaces a generic libsrt error — much
    // less helpful than a clear "1456 cap" message at parse time.
    let n: u16 = parse_int_nonneg::<u16>("payloadsize", value)?;
    if n > 1456 {
        return Err(UrlError::InvalidValue {
            key: "payloadsize".into(),
            detail: format!("must be <= 1456 (libsrt SRT_LIVE_MAX_PLSIZE, live-mode cap), got {n}"),
        });
    }
    overlay.payload_size = Some(n);
    Ok(())
}

fn apply_pbkeylen(overlay: &mut UrlOverlay, value: &str) -> Result<(), UrlError> {
    let n = parse_i32_nonneg("pbkeylen", value)?;
    overlay.key_length =
        Some(
            KeyLength::from_bytes(n).map_err(|source| UrlError::OptionValidation {
                key: "pbkeylen".into(),
                source,
            })?,
        );
    Ok(())
}

fn apply_streamid(overlay: &mut UrlOverlay, value: &str) -> Result<(), UrlError> {
    // libsrt-URL canonical key is `streamid`; `srt_streamid` is the
    // ffmpeg-style alias.
    overlay.stream_id =
        Some(
            StreamId::new(value.to_string()).map_err(|e| UrlError::OptionValidation {
                key: "streamid".into(),
                source: OptionError::from(e),
            })?,
        );
    Ok(())
}

fn apply_tlpktdrop(overlay: &mut UrlOverlay, value: &str) -> Result<(), UrlError> {
    overlay.too_late_packet_drop = Some(parse_bool_strict("tlpktdrop", value)?);
    Ok(())
}

/// Returns a `UrlError::FfmpegAliasNotExposed` for the three ffmpeg-canonical
/// option short-aliases that don't share a libsrt URL name. Without these
/// arms, users porting ffmpeg URLs hit the generic "unknown URL key"
/// fallthrough — surface what they're trying to set + what to use instead
/// (or that we don't expose it). ffmpeg names: libsrt.c:103/104/118/149.
fn ffmpeg_alias_not_exposed(key: &str) -> UrlError {
    match key {
        "timeout" => UrlError::FfmpegAliasNotExposed {
            key: key.to_string(),
            canonical: "ffmpeg's read/write timeout, libsrt rw_timeout, microseconds",
            suggestion: "use x-recvtimeout / x-sendtimeout (milliseconds) for the closest equivalent",
        },
        "listen_timeout" => UrlError::FfmpegAliasNotExposed {
            key: key.to_string(),
            canonical: "ffmpeg's listen_timeout (connection-awaiting timeout, microseconds)",
            suggestion: "not exposed by this library; see deferred-features.md",
        },
        "tsbpd" => UrlError::FfmpegAliasNotExposed {
            key: key.to_string(),
            canonical: "ffmpeg short alias for SRTO_TSBPDMODE (also reachable via the longer 'tsbpdmode' libsrt URL key)",
            suggestion: "not exposed by this library; see deferred-features.md",
        },
        _ => unreachable!("ffmpeg_alias_not_exposed called with unexpected key '{key}'"),
    }
}

fn fallback_unknown(other: &str) -> Result<(), UrlError> {
    if let Some(srto) = group3_lookup(other) {
        return Err(UrlError::UnsupportedKey {
            key: other.to_string(),
            srto,
        });
    }
    Err(UrlError::UnknownKey {
        key: other.to_string(),
    })
}

impl UrlOverlay {
    /// Write `Some(_)` fields through to `cfg`. URL wins on conflict.
    pub fn apply_to_socket(&self, cfg: &mut SocketConfig) {
        if let Some(v) = self.passphrase.as_ref() {
            cfg.passphrase = Some(v.clone());
        }
        if let Some(v) = self.key_length {
            cfg.key_length = v;
        }
        if let Some(v) = self.latency {
            cfg.latency = Some(v);
        }
        if let Some(v) = self.recv_latency {
            cfg.recv_latency = Some(v);
        }
        if let Some(v) = self.peer_latency {
            cfg.peer_latency = Some(v);
        }
        if let Some(v) = self.mss {
            cfg.mss = Some(v);
        }
        if let Some(v) = self.payload_size {
            cfg.payload_size = Some(v);
        }
        if let Some(v) = self.max_bandwidth {
            cfg.max_bandwidth = Some(v);
        }
        if let Some(v) = self.input_bandwidth {
            cfg.input_bandwidth = Some(v);
        }
        if let Some(v) = self.overhead_bandwidth_pct {
            cfg.overhead_bandwidth_pct = Some(v);
        }
        if let Some(v) = self.stream_id.as_ref() {
            cfg.stream_id = Some(v.clone());
        }
        if let Some(v) = self.loss_max_ttl {
            cfg.loss_max_ttl = Some(v);
        }
        if let Some(v) = self.too_late_packet_drop {
            cfg.too_late_packet_drop = Some(v);
        }
        if let Some(v) = self.flow_window_packets {
            cfg.flow_window_packets = Some(v);
        }
        if let Some(v) = self.packet_filter.as_ref() {
            cfg.packet_filter = Some(v.clone());
        }
        if let Some(v) = self.congestion {
            cfg.congestion = Some(v);
        }
        if let Some(v) = self.recv_timeout {
            cfg.recv_timeout = Some(v);
        }
        if let Some(v) = self.send_timeout {
            cfg.send_timeout = Some(v);
        }
        if let Some(v) = self.connect_timeout {
            cfg.connect_timeout = Some(v);
        }
        if let Some(v) = self.linger {
            cfg.linger = Some(v);
        }
        if let Some(v) = self.udp_recv_buffer_bytes {
            cfg.udp_recv_buffer_bytes = Some(v);
        }
        if let Some(v) = self.udp_send_buffer_bytes {
            cfg.udp_send_buffer_bytes = Some(v);
        }
    }

    /// Same shape for `ListenerConfig` (for symmetry with future
    /// listener-side URL support; v1 has no listener-side _open in tst-c).
    pub fn apply_to_listener(&self, cfg: &mut ListenerConfig) {
        if let Some(v) = self.passphrase.as_ref() {
            cfg.passphrase = Some(v.clone());
        }
        if let Some(v) = self.key_length {
            cfg.key_length = v;
        }
        if let Some(v) = self.latency {
            cfg.latency = Some(v);
        }
        if let Some(v) = self.recv_latency {
            cfg.recv_latency = Some(v);
        }
        if let Some(v) = self.mss {
            cfg.mss = Some(v);
        }
        if let Some(v) = self.payload_size {
            cfg.payload_size = Some(v);
        }
        if let Some(v) = self.max_bandwidth {
            cfg.max_bandwidth = Some(v);
        }
        if let Some(v) = self.overhead_bandwidth_pct {
            cfg.overhead_bandwidth_pct = Some(v);
        }
        if let Some(v) = self.loss_max_ttl {
            cfg.loss_max_ttl = Some(v);
        }
        if let Some(v) = self.too_late_packet_drop {
            cfg.too_late_packet_drop = Some(v);
        }
        if let Some(v) = self.flow_window_packets {
            cfg.flow_window_packets = Some(v);
        }
        if let Some(v) = self.packet_filter.as_ref() {
            cfg.packet_filter = Some(v.clone());
        }
        if let Some(v) = self.congestion {
            cfg.congestion = Some(v);
        }
        if let Some(v) = self.recv_timeout {
            cfg.recv_timeout = Some(v);
        }
        if let Some(v) = self.send_timeout {
            cfg.send_timeout = Some(v);
        }
        if let Some(v) = self.linger {
            cfg.linger = Some(v);
        }
        if let Some(v) = self.udp_recv_buffer_bytes {
            cfg.udp_recv_buffer_bytes = Some(v);
        }
        // udp_send_buffer_bytes is sender-only — no listener field.
        // ListenerConfig has no peer_latency, stream_id, or input_bandwidth.
        // peer_latency is a caller-side option (libsrt allows it to be set
        // on listeners but it has no effect there).
        // stream_id is set by the caller during handshake; the listener
        // reads it via Socket::stream_id on accepted sockets.
        // input_bandwidth (SRTO_INPUTBW) is sender-side bandwidth budgeting.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_test::traced_test;

    #[test]
    #[traced_test]
    fn warns_on_high_latency_likely_ffmpeg_units() {
        // 120000 ms = 120s; ffmpeg URLs use µs, so this is a likely paste-from-ffmpeg.
        let _ = SrtUrl::parse("srt://h:9000?latency=120000").unwrap();
        assert!(
            logs_contain("ffmpeg uses microseconds while ts-transformer"),
            "expected µs/ms warning to fire for latency=120000",
        );
    }

    #[test]
    #[traced_test]
    fn no_warn_on_realistic_latency() {
        let _ = SrtUrl::parse("srt://h:9000?latency=200").unwrap();
        assert!(
            !logs_contain("ffmpeg uses microseconds"),
            "should not warn on realistic 200ms latency",
        );
        let _ = SrtUrl::parse("srt://h:9000?latency=4000").unwrap();
        assert!(
            !logs_contain("ffmpeg uses microseconds"),
            "should not warn on 4000ms latency (still realistic)",
        );
    }

    #[test]
    #[traced_test]
    fn warns_on_high_rcvlatency() {
        let _ = SrtUrl::parse("srt://h:9000?rcvlatency=15000").unwrap();
        assert!(logs_contain("ffmpeg uses microseconds"));
    }

    #[test]
    #[traced_test]
    fn warns_on_high_peerlatency() {
        let _ = SrtUrl::parse("srt://h:9000?peerlatency=15000").unwrap();
        assert!(logs_contain("ffmpeg uses microseconds"));
    }

    #[test]
    fn url_conntimeo_parses() {
        let u = SrtUrl::parse("srt://h:9000?conntimeo=10000").unwrap();
        assert_eq!(
            u.overlay.connect_timeout,
            Some(Duration::from_millis(10000))
        );
    }

    #[test]
    fn url_connect_timeout_alias_parses() {
        let u = SrtUrl::parse("srt://h:9000?connect_timeout=10000").unwrap();
        assert_eq!(
            u.overlay.connect_timeout,
            Some(Duration::from_millis(10000))
        );
    }

    #[test]
    fn url_linger_parses() {
        let u = SrtUrl::parse("srt://h:9000?linger=5").unwrap();
        assert_eq!(u.overlay.linger, Some(Duration::from_secs(5)));
    }

    #[test]
    fn url_udprcvbuf_parses() {
        let u = SrtUrl::parse("srt://h:9000?udprcvbuf=2000000").unwrap();
        assert_eq!(u.overlay.udp_recv_buffer_bytes, Some(2_000_000));
    }

    #[test]
    fn url_udpsndbuf_parses() {
        let u = SrtUrl::parse("srt://h:9000?udpsndbuf=2000000").unwrap();
        assert_eq!(u.overlay.udp_send_buffer_bytes, Some(2_000_000));
    }

    #[test]
    fn url_recv_buffer_size_alias_parses() {
        let u = SrtUrl::parse("srt://h:9000?recv_buffer_size=2000000").unwrap();
        assert_eq!(u.overlay.udp_recv_buffer_bytes, Some(2_000_000));
    }

    #[test]
    fn url_send_buffer_size_alias_parses() {
        let u = SrtUrl::parse("srt://h:9000?send_buffer_size=2000000").unwrap();
        assert_eq!(u.overlay.udp_send_buffer_bytes, Some(2_000_000));
    }

    #[test]
    fn url_alias_pkt_size_matches_payloadsize() {
        let canonical = SrtUrl::parse("srt://h:9000?payloadsize=1316").unwrap();
        let alias = SrtUrl::parse("srt://h:9000?pkt_size=1316").unwrap();
        assert_eq!(canonical.overlay.payload_size, alias.overlay.payload_size);
        assert_eq!(alias.overlay.payload_size, Some(1316));
    }

    #[test]
    fn url_alias_payload_size_matches_payloadsize() {
        let alias = SrtUrl::parse("srt://h:9000?payload_size=1316").unwrap();
        assert_eq!(alias.overlay.payload_size, Some(1316));
    }

    #[test]
    fn url_alias_srt_streamid_matches_streamid() {
        let canonical = SrtUrl::parse("srt://h:9000?streamid=abc").unwrap();
        let alias = SrtUrl::parse("srt://h:9000?srt_streamid=abc").unwrap();
        assert_eq!(
            canonical.overlay.stream_id.as_ref().map(|s| s.as_str()),
            alias.overlay.stream_id.as_ref().map(|s| s.as_str()),
        );
        assert_eq!(
            alias.overlay.stream_id.as_ref().map(|s| s.as_str()),
            Some("abc")
        );
    }

    #[test]
    fn url_alias_tsbpddelay_matches_latency() {
        let alias = SrtUrl::parse("srt://h:9000?tsbpddelay=200").unwrap();
        assert_eq!(alias.overlay.latency, Some(Duration::from_millis(200)));
    }

    #[test]
    #[traced_test]
    fn url_alias_tsbpddelay_high_value_warns() {
        let _ = SrtUrl::parse("srt://h:9000?tsbpddelay=120000").unwrap();
        assert!(
            logs_contain("ffmpeg uses microseconds while ts-transformer"),
            "the µs/ms warning should fire through the tsbpddelay alias too",
        );
    }

    #[test]
    fn url_alias_smoother_matches_congestion() {
        let canonical = SrtUrl::parse("srt://h:9000?congestion=live").unwrap();
        let alias = SrtUrl::parse("srt://h:9000?smoother=live").unwrap();
        assert_eq!(canonical.overlay.congestion, alias.overlay.congestion);
    }

    #[test]
    fn url_alias_ffs_matches_fc() {
        let canonical = SrtUrl::parse("srt://h:9000?fc=25600").unwrap();
        let alias = SrtUrl::parse("srt://h:9000?ffs=25600").unwrap();
        assert_eq!(
            canonical.overlay.flow_window_packets,
            alias.overlay.flow_window_packets
        );
    }

    #[test]
    fn ffmpeg_url_aliases_emit_friendly_errors_not_unknown_key() {
        // Users porting ffmpeg URLs paste these all the time. Today they see
        // "unknown URL key 'timeout'" — not helpful. Surface what they're trying
        // to set + what to use instead (or that we don't support it).
        for (url, expected_in_msg) in [
            (
                "srt://h:9000?timeout=5000000",
                &["rw_timeout", "x-recvtimeout"][..],
            ),
            (
                "srt://h:9000?listen_timeout=5000000",
                &["listen_timeout"][..],
            ),
            ("srt://h:9000?tsbpd=1", &["tsbpdmode"][..]),
            ("srt://h:9000?snddropdelay=100", &["snddropdelay"][..]),
        ] {
            let err = SrtUrl::parse(url).expect_err("should reject");
            let s = format!("{err}");
            for needle in expected_in_msg {
                assert!(
                    s.contains(needle),
                    "URL {url} produced unhelpful error {s:?} (expected to contain {needle})"
                );
            }
        }
    }

    #[test]
    fn payloadsize_above_live_max_is_rejected_at_parse_time() {
        // libsrt SRT_LIVE_MAX_PLSIZE = 1456 (srt.h:297). ffmpeg clamps via
        // AVOption.max in libsrt.c:107-108. Without a parse-time cap, libsrt's
        // PRE setsockopt fails inside apply_socket_config with a generic error;
        // catching it here gives the user a clear message.
        let result = SrtUrl::parse("srt://1.2.3.4:9000?payloadsize=2000");
        assert!(result.is_err(), "expected error, got {result:?}");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("1456") || err_msg.to_lowercase().contains("live"),
            "error should mention 1456 cap; got: {err_msg}"
        );
    }

    #[test]
    fn payloadsize_at_or_below_live_max_is_accepted() {
        // Spot-check the boundary: 1456 OK, 1316 OK (common ffmpeg default,
        // a multiple of 188), 1 OK, 0 OK (libsrt default sentinel).
        for v in [0u16, 1, 1316, 1456] {
            let url = format!("srt://1.2.3.4:9000?payloadsize={v}");
            let result = SrtUrl::parse(&url);
            assert!(result.is_ok(), "expected Ok for {v}; got {result:?}");
        }
    }

    #[test]
    fn mode_listener_parses_with_empty_host() {
        let u = SrtUrl::parse("srt://:7000?mode=listener").unwrap();
        assert_eq!(u.host, "");
        assert_eq!(u.port, 7000);
        assert_eq!(u.mode, Mode::Listener);
    }

    #[test]
    fn mode_listener_parses_with_wildcard_host() {
        let u = SrtUrl::parse("srt://0.0.0.0:7000?mode=listener").unwrap();
        assert_eq!(u.host, "0.0.0.0");
        assert_eq!(u.mode, Mode::Listener);
    }

    #[test]
    fn mode_caller_remains_default() {
        let u = SrtUrl::parse("srt://peer:9000").unwrap();
        assert_eq!(u.mode, Mode::Caller);
    }

    #[test]
    fn mode_caller_explicit_is_accepted() {
        let u = SrtUrl::parse("srt://peer:9000?mode=caller").unwrap();
        assert_eq!(u.mode, Mode::Caller);
    }

    #[test]
    fn empty_host_without_listener_mode_rejects() {
        let err = SrtUrl::parse("srt://:7000").unwrap_err();
        assert!(matches!(err, UrlError::MissingHost));
    }

    #[test]
    fn mode_rendezvous_unsupported() {
        let err = SrtUrl::parse("srt://peer:9000?mode=rendezvous").unwrap_err();
        assert!(matches!(err, UrlError::UnsupportedMode { .. }));
    }
}
