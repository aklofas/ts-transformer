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

use crate::config::{SrtListenerConfig, SrtSocketConfig};
use crate::error::{OptionError, SrtError};
use crate::listener::SrtListener;
use crate::options::{
    Congestion, KeyLength, MaxBandwidth, PacketFilter, Passphrase, Role, StreamId,
};
use crate::socket::SrtSocket;
use crate::transport::SrtTransport;
use std::time::Duration;
use tst_core::cancel::CancelSlot;
use tst_core::url::common::{UrlError as CoreUrlError, decode_ipv6_zone, parse_url};
use tst_core::url::query::{KeyDoc, QueryKey, lookup};

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

/// Group 3: libsrt-URL keys we recognize but don't yet expose.
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

/// Parsed `srt://host:port?...` URL: connection target + the recognized
/// query parameters as a [`SrtSocketConfig`].
///
/// [`config`](Self::config) holds exactly the keys the URL carried (every
/// other field is unset), so it layers over a config built from other
/// sources with [`SrtSocketConfig::merge_from`] and the URL wins on conflict.
/// [`socket_config`](Self::socket_config) and
/// [`listener_config`](Self::listener_config) are the ready-to-open forms.
#[must_use]
#[derive(Debug, Clone)]
pub struct SrtUrl {
    pub host: String,
    pub port: u16,
    pub mode: Mode,
    pub config: SrtSocketConfig,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SrtUrlError {
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
    /// sequences (`%XX` only — `+` is NOT decoded as space). Every occurrence
    /// of a repeated key is validated and the last valid one wins, so an
    /// invalid occurrence anywhere is an error.
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
    /// assert_eq!(u.config.latency, Some(Duration::from_millis(200)));
    /// ```
    pub fn parse(s: &str) -> Result<Self, SrtUrlError> {
        // The common parser accepts empty-host URLs (e.g. srt://:7000?mode=listener)
        // directly — it returns Ok with host="" rather than an error. We check
        // mode after parsing query parameters to decide whether an empty host
        // is acceptable (listener) or an error (caller).
        let parsed = parse_url(s)?;

        if parsed.scheme != "srt" {
            return Err(SrtUrlError::WrongScheme {
                got: parsed.scheme.to_string(),
            });
        }
        if parsed.username.is_some() || parsed.password.is_some() {
            return Err(SrtUrlError::UserinfoNotSupported);
        }

        // The common parser strips IPv6 brackets, so host is always the
        // raw address string (e.g. "::1" not "[::1]"); an RFC 6874 "%25" zone
        // delimiter is decoded to the "%" libsrt's resolver expects.
        let host = decode_ipv6_zone(parsed.host).into_owned();
        let port = parsed.port.ok_or(SrtUrlError::MissingPort)?;

        let mut config = SrtSocketConfig::default();
        let mut mode = Mode::Caller;
        // Every occurrence is applied (and validated) in order, so the last
        // valid one wins and an invalid one anywhere is an error.
        for (key, value) in &parsed.query {
            // mode is consumed at the SrtUrl level (not in the config) since
            // it decides whether the caller dials or accepts.
            if key.as_ref() == "mode" {
                mode = match value.as_ref() {
                    "caller" => Mode::Caller,
                    "listener" => Mode::Listener,
                    other => {
                        return Err(SrtUrlError::UnsupportedMode {
                            mode: other.to_string(),
                        });
                    }
                };
                continue;
            }
            match lookup(URL_KEYS, key.as_ref()) {
                Some(row) => (row.apply)(&mut config, key.as_ref(), value.as_ref())?,
                None => return Err(unrecognized_key(key.as_ref())),
            }
        }

        // Caller mode requires a non-empty host (need somewhere to connect).
        // Listener mode allows empty host (bind-to-wildcard).
        if host.is_empty() && mode != Mode::Listener {
            return Err(SrtUrlError::MissingHost);
        }

        Ok(Self {
            host,
            port,
            mode,
            config,
        })
    }

    /// The caller-side [`SrtSocketConfig`] to dial with: a default config
    /// with this URL's keys merged in, plus, for [`Role::Sender`], the
    /// sender preset ([`SrtSocketConfig::merge_sender_defaults`]). That one
    /// role is the entire difference between [`Self::connect`] and
    /// [`Self::connect_recv`]. A receiver gets no preset.
    pub fn socket_config(&self, role: Role) -> SrtSocketConfig {
        let mut cfg = SrtSocketConfig::default();
        cfg.merge_from(&self.config);
        if role == Role::Sender {
            cfg.merge_sender_defaults();
        }
        cfg
    }

    /// The listener-side [`SrtListenerConfig`]: the defaults with this URL's
    /// keys as the socket options. The listener applies the subset of
    /// them that is meaningful on a passive socket.
    pub fn listener_config(&self) -> SrtListenerConfig {
        SrtListenerConfig {
            socket: self.config.clone(),
            ..Default::default()
        }
    }

    /// Shared dial tail: an IPv6-safe `host:port` join
    /// ([`crate::addr::join_host_port`]) → [`SrtSocket::connect_with`], which
    /// walks every resolved address → [`SrtTransport::new`].
    fn dial(&self, cfg: &SrtSocketConfig) -> Result<SrtTransport, SrtError> {
        let addr = crate::addr::join_host_port(&self.host, self.port);
        let socket = SrtSocket::connect_with(cfg, addr.as_str())?;
        Ok(SrtTransport::new(socket))
    }

    /// Open this URL as a **caller** for the SENDER path: dial `host:port`
    /// with the URL's keys applied and the sender preset merged underneath
    /// them, and return the connected transport.
    ///
    /// The composition: [`SrtUrl::socket_config`] for [`Role::Sender`] (the URL's keys on a
    /// default [`SrtSocketConfig`], then [`SrtSocketConfig::merge_sender_defaults`]:
    /// 15 s connect timeout, 5 s linger, `Role::Sender`, each only where the
    /// URL left it unset, so the URL wins) → the shared dial tail.
    ///
    /// # Which one to call
    ///
    /// - `connect` — applies the sender defaults where the URL left them
    ///   unset. The C plain opens (`tst_sender_open`, `tst_raw_sender_open`,
    ///   `tst_mux_sender_open`, the caller-mode receivers) and every managed caller open in all
    ///   three bindings use it, so a managed caller-mode receiver also
    ///   gets the 5 s linger and `Role::Sender`.
    /// - [`connect_recv`](Self::connect_recv) — the plain/receive-side
    ///   open path: the URL's keys only, no preset. The Python and JVM plain
    ///   caller opens use it — senders included, so a plain Python/JVM
    ///   sender keeps libsrt's linger-off default and no 15 s connect
    ///   timeout.
    ///
    /// [`mode`](Self::mode) is not consulted: the caller chooses the
    /// direction by calling this or [`accept_one`](Self::accept_one)
    /// (`tst_srt::shells` dispatches on `mode` for you).
    ///
    /// # Errors
    ///
    /// [`SrtError::Connect`] carrying the typed [`SrtConnectError`]
    /// (resolution, handshake reject, timeout, refused, option refused).
    ///
    /// [`SrtConnectError`]: crate::SrtConnectError
    pub fn connect(&self) -> Result<SrtTransport, SrtError> {
        self.dial(&self.socket_config(Role::Sender))
    }

    /// Open this URL as a **caller** for the plain/receive path: exactly
    /// [`connect`](Self::connect) minus
    /// [`SrtSocketConfig::merge_sender_defaults`] — the URL's keys are merged
    /// into a default [`SrtSocketConfig`] and nothing else is, so the socket keeps
    /// libsrt's own connect timeout and linger and stays
    /// [`Role::Receiver`].
    ///
    /// That is what the Python and JVM plain caller opens compose,
    /// and why the split exists: routing them through
    /// [`connect`](Self::connect) would silently promote every plain
    /// receiver to `SRTO_SENDER=1` with a 5 s close linger. See
    /// [`connect`](Self::connect)'s "Which one to call".
    ///
    /// # Errors
    ///
    /// Identical to [`connect`](Self::connect): [`SrtError::Connect`]
    /// carrying the typed [`SrtConnectError`](crate::SrtConnectError).
    pub fn connect_recv(&self) -> Result<SrtTransport, SrtError> {
        self.dial(&self.socket_config(Role::Receiver))
    }

    /// Open this URL as a **listener**: bind `host:port` (an empty host
    /// binds the wildcard `0.0.0.0`), accept **one** peer, and return it
    /// as a transport — with the accept reachable by `cancel.cancel()`
    /// from another thread.
    ///
    /// The bind → install-into-slot → accept → clear → classify sequence
    /// is [`SrtListener::accept_one_cancellable`]; this method only builds
    /// the [`SrtListenerConfig`] ([`SrtUrl::listener_config`]) and renders the bind address
    /// (an empty host becomes `0.0.0.0`). Single-accept: the
    /// listener is dropped on return. Share the same `cancel` slot with
    /// a managed transport's `tst_pipeline::FactoryCancel` — a type alias
    /// for this very [`CancelSlot`] — so a
    /// re-accept parked with no peer in sight can be woken by the
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
        let cfg = self.listener_config();
        let bind_host = if self.host.is_empty() {
            "0.0.0.0"
        } else {
            self.host.as_str()
        };
        let addr = crate::addr::join_host_port(bind_host, self.port);
        Ok(SrtListener::accept_one_cancellable(&cfg, &addr, cancel)?)
    }
}

/// Integers are bare canonical decimals: libsrt-URL tools parse base-0, where
/// `010` is 8 and `+5` is accepted, so a form whose meaning differs is refused.
fn check_decimal(key: &str, value: &str) -> Result<(), SrtUrlError> {
    let canonical = value == "0"
        || (!value.starts_with('0')
            && !value.is_empty()
            && value.bytes().all(|b| b.is_ascii_digit()));
    if canonical {
        Ok(())
    } else {
        Err(SrtUrlError::InvalidValue {
            key: key.to_string(),
            detail: format!(
                "expected a decimal integer without sign or leading zeros, got '{value}'"
            ),
        })
    }
}

fn parse_int_nonneg<T>(key: &str, value: &str) -> Result<T, SrtUrlError>
where
    T: std::str::FromStr<Err = std::num::ParseIntError>,
{
    // Strict-A: bare decimal, no suffix, non-negative. We rely on T's
    // from_str to enforce range. Use this for unsigned T (u8/u16/u32/u64);
    // signed callers go through parse_i32_nonneg, which adds the sign check.
    check_decimal(key, value)?;
    value.parse::<T>().map_err(|e| SrtUrlError::InvalidValue {
        key: key.to_string(),
        detail: format!("expected non-negative decimal integer in range, got '{value}': {e}"),
    })
}

fn parse_i32_nonneg(key: &str, value: &str) -> Result<i32, SrtUrlError> {
    check_decimal(key, value)?;
    let n: i32 = value.parse().map_err(|e| SrtUrlError::InvalidValue {
        key: key.to_string(),
        detail: format!("expected i32 decimal, got '{value}': {e}"),
    })?;
    if n < 0 {
        return Err(SrtUrlError::InvalidValue {
            key: key.to_string(),
            detail: format!("must be non-negative, got {n}"),
        });
    }
    Ok(n)
}

fn parse_oheadbw(value: &str) -> Result<u8, SrtUrlError> {
    let n: u32 = parse_int_nonneg("oheadbw", value)?;
    crate::options::validate_overhead_bandwidth_pct(n).map_err(|detail| {
        SrtUrlError::InvalidValue {
            key: "oheadbw".into(),
            detail,
        }
    })?;
    Ok(n as u8)
}

fn parse_bool_strict(key: &str, value: &str) -> Result<bool, SrtUrlError> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        other => Err(SrtUrlError::InvalidValue {
            key: key.to_string(),
            detail: format!("expected '0' or '1', got '{other}'"),
        }),
    }
}

type Applied = Result<(), SrtUrlError>;

fn set_congestion(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    // libsrt-URL canonical key is `congestion` (renamed from `smoother`
    // in libsrt 1.4.1); `smoother` is the pre-rename ffmpeg-style alias.
    c.congestion = Some(Congestion::from_str_strict(value).map_err(|source| {
        SrtUrlError::OptionValidation {
            key: "congestion".into(),
            source,
        }
    })?);
    Ok(())
}

fn set_connect_timeout(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    // libsrt-URL canonical key is `conntimeo` (milliseconds);
    // `connect_timeout` is the ffmpeg-style alias.
    let n = parse_i32_nonneg("conntimeo", value)?;
    c.connect_timeout = Some(Duration::from_millis(n as u64));
    Ok(())
}

fn set_flow_window(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    // libsrt-URL canonical key is `fc` (flow control window); `ffs` is
    // the ffmpeg-style alias (flight flag size).
    c.flow_window_packets = Some(parse_int_nonneg("fc", value)?);
    Ok(())
}

fn set_input_bandwidth(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    // The socket option is an i64, so the accepted range is 0..=i64::MAX.
    check_decimal("inputbw", value)?;
    let n: i64 = value.parse().map_err(|e| SrtUrlError::InvalidValue {
        key: "inputbw".to_string(),
        detail: format!(
            "expected non-negative decimal integer at most {}, got '{value}': {e}",
            i64::MAX
        ),
    })?;
    c.input_bandwidth = Some(n as u64);
    Ok(())
}

/// Parses a latency value in milliseconds under the name used in errors
/// and the high-latency warning.
fn latency_value(name: &'static str, value: &str) -> Result<Duration, SrtUrlError> {
    // `latency`'s ffmpeg-style alias is `tsbpddelay` (the `SRTO_*` constant
    // for SRT's TSBPD mechanism is `SRTO_LATENCY`).
    let n = parse_i32_nonneg(name, value)?;
    warn_if_suspicious_latency(name, n);
    // n is a non-negative i32; widening to u64 is lossless.
    Ok(Duration::from_millis(n as u64))
}

fn set_latency(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    c.latency = Some(latency_value("latency", value)?);
    Ok(())
}

fn set_peer_latency(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    c.peer_latency = Some(latency_value("peerlatency", value)?);
    Ok(())
}

fn set_recv_latency(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    c.recv_latency = Some(latency_value("rcvlatency", value)?);
    Ok(())
}

fn set_linger(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    // SRTO_LINGER value is in seconds (matches ffmpeg's URL).
    let n = parse_i32_nonneg("linger", value)?;
    c.linger = Some(Duration::from_secs(n as u64));
    Ok(())
}

fn set_loss_max_ttl(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    c.loss_max_ttl = Some(parse_int_nonneg("lossmaxttl", value)?);
    Ok(())
}

fn set_max_bandwidth(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    // SRTO_MAXBW is i64; we expose non-negative as Limited(u64).
    // The `-1` sentinel (`MaxBandwidth::Infinite`, libsrt's infinite) is not
    // URL-settable; `?maxbw=0` sends 0 (relative to the input rate).
    let n = parse_int_nonneg::<u64>("maxbw", value)?;
    c.max_bandwidth = Some(MaxBandwidth::Limited(n));
    Ok(())
}

fn set_mss(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    c.mss = Some(parse_int_nonneg::<u16>("mss", value)?);
    Ok(())
}

fn set_overhead_bandwidth(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    c.overhead_bandwidth_pct = Some(parse_oheadbw(value)?);
    Ok(())
}

fn set_packet_filter(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    c.packet_filter =
        Some(
            PacketFilter::new(value.to_string()).map_err(|e| SrtUrlError::OptionValidation {
                key: "packetfilter".into(),
                source: OptionError::from(e),
            })?,
        );
    Ok(())
}

fn set_passphrase(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    c.passphrase =
        Some(
            Passphrase::new(value.to_string()).map_err(|e| SrtUrlError::OptionValidation {
                key: "passphrase".into(),
                source: OptionError::from(e),
            })?,
        );
    Ok(())
}

fn set_payload_size(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
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
        return Err(SrtUrlError::InvalidValue {
            key: "payloadsize".into(),
            detail: format!("must be <= 1456 (libsrt SRT_LIVE_MAX_PLSIZE, live-mode cap), got {n}"),
        });
    }
    c.payload_size = Some(n);
    Ok(())
}

fn set_key_length(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    let n = parse_i32_nonneg("pbkeylen", value)?;
    c.key_length =
        Some(
            KeyLength::from_bytes(n).map_err(|source| SrtUrlError::OptionValidation {
                key: "pbkeylen".into(),
                source,
            })?,
        );
    Ok(())
}

fn set_stream_id(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    // libsrt-URL canonical key is `streamid`; `srt_streamid` is the
    // ffmpeg-style alias.
    c.stream_id =
        Some(
            StreamId::new(value.to_string()).map_err(|e| SrtUrlError::OptionValidation {
                key: "streamid".into(),
                source: OptionError::from(e),
            })?,
        );
    Ok(())
}

fn set_too_late_drop(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    c.too_late_packet_drop = Some(parse_bool_strict("tlpktdrop", value)?);
    Ok(())
}

fn set_udp_recv_buffer(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    // libsrt-URL canonical key is `udprcvbuf`; `recv_buffer_size` is the
    // ffmpeg-style alias.
    c.udp_recv_buffer_bytes = Some(parse_i32_nonneg("udprcvbuf", value)? as u32);
    Ok(())
}

fn set_udp_send_buffer(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    c.udp_send_buffer_bytes = Some(parse_i32_nonneg("udpsndbuf", value)? as u32);
    Ok(())
}

fn set_recv_timeout(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    let n = parse_i32_nonneg("x-recvtimeout", value)?;
    c.recv_timeout = Some(Duration::from_millis(n as u64));
    Ok(())
}

fn set_send_timeout(c: &mut SrtSocketConfig, _key: &str, value: &str) -> Applied {
    let n = parse_i32_nonneg("x-sendtimeout", value)?;
    c.send_timeout = Some(Duration::from_millis(n as u64));
    Ok(())
}

/// The keys an `srt://` URL consumes at the URL level, outside the config
/// table below, as the generated key documentation lists them. The
/// `print-url-keys --check` comparison cannot see a key consumed outside a
/// table, so a new URL-level key must be added here by hand.
#[doc(hidden)]
pub const URL_LEVEL_KEYS: &[KeyDoc] = &[KeyDoc {
    scheme: "srt",
    canonical: "mode",
    aliases: &[],
    meaning: "Connection role",
    unit: "`caller` or `listener`; default `caller`",
}];

/// The query keys an `srt://` URL applies to a [`SrtSocketConfig`]; the
/// parser and the generated key documentation both read this table. The
/// `mode` key (see [`URL_LEVEL_KEYS`]) and the recognised-but-unexposed keys
/// are handled around it, not in it.
#[doc(hidden)]
pub const URL_KEYS: &[QueryKey<SrtSocketConfig, SrtUrlError>] = &[
    QueryKey::new(
        "congestion",
        &["smoother"],
        "Congestion-control algorithm",
        "`live` or `file`",
        set_congestion,
    ),
    QueryKey::new(
        "conntimeo",
        &["connect_timeout"],
        "Connect deadline",
        "milliseconds",
        set_connect_timeout,
    ),
    QueryKey::new(
        "fc",
        &["ffs"],
        "Flow-control window",
        "packets",
        set_flow_window,
    ),
    QueryKey::new(
        "inputbw",
        &[],
        "Input bandwidth estimate for the send budget",
        "bytes per second",
        set_input_bandwidth,
    ),
    QueryKey::new(
        "latency",
        &["tsbpddelay"],
        "Receiver buffering delay",
        "milliseconds",
        set_latency,
    ),
    QueryKey::new(
        "linger",
        &[],
        "Close grace period for unsent data",
        "seconds",
        set_linger,
    ),
    QueryKey::new(
        "lossmaxttl",
        &[],
        "Reorder tolerance before a loss is reported",
        "packets",
        set_loss_max_ttl,
    ),
    QueryKey::new(
        "maxbw",
        &[],
        "Maximum send bandwidth",
        "bytes per second; 0 is relative to the input rate",
        set_max_bandwidth,
    ),
    QueryKey::new(
        "mss",
        &[],
        "Maximum segment size",
        "bytes, 16 bits",
        set_mss,
    ),
    QueryKey::new(
        "oheadbw",
        &[],
        "Recovery bandwidth overhead",
        "percent, 5 to 100",
        set_overhead_bandwidth,
    ),
    QueryKey::new(
        "packetfilter",
        &[],
        "Packet filter (forward error correction) configuration",
        "libsrt spec string, for example `fec,cols:10,rows:5`",
        set_packet_filter,
    ),
    QueryKey::new(
        "passphrase",
        &[],
        "Encryption passphrase",
        "10 to 79 characters",
        set_passphrase,
    ),
    QueryKey::new(
        "payloadsize",
        &["pkt_size", "payload_size"],
        "Payload bytes per packet",
        "bytes, at most 1456",
        set_payload_size,
    ),
    QueryKey::new(
        "pbkeylen",
        &[],
        "Encryption key length",
        "16, 24 or 32 bytes",
        set_key_length,
    ),
    QueryKey::new(
        "peerlatency",
        &[],
        "Latency the peer should use when receiving",
        "milliseconds",
        set_peer_latency,
    ),
    QueryKey::new(
        "rcvlatency",
        &[],
        "Receiver buffering delay of this side",
        "milliseconds",
        set_recv_latency,
    ),
    QueryKey::new(
        "streamid",
        &["srt_streamid"],
        "Stream identifier sent in the handshake",
        "text up to 512 bytes",
        set_stream_id,
    ),
    QueryKey::new(
        "tlpktdrop",
        &[],
        "Drop packets that arrive too late",
        "`0` or `1`",
        set_too_late_drop,
    ),
    QueryKey::new(
        "udprcvbuf",
        &["recv_buffer_size"],
        "Kernel UDP receive buffer",
        "bytes",
        set_udp_recv_buffer,
    ),
    QueryKey::new(
        "udpsndbuf",
        &["send_buffer_size"],
        "Kernel UDP send buffer",
        "bytes",
        set_udp_send_buffer,
    ),
    QueryKey::new(
        "x-recvtimeout",
        &[],
        "Receive timeout",
        "milliseconds",
        set_recv_timeout,
    ),
    QueryKey::new(
        "x-sendtimeout",
        &[],
        "Send timeout",
        "milliseconds",
        set_send_timeout,
    ),
];

// ── keys the table does not hold ─────────────────────────────────────────────

/// Returns a `SrtUrlError::FfmpegAliasNotExposed` for the three ffmpeg-canonical
/// option short-aliases that don't share a libsrt URL name. Without these
/// arms, users porting ffmpeg URLs hit the generic "unknown URL key"
/// fallthrough — surface what they're trying to set + what to use instead
/// (or that we don't expose it). ffmpeg names: libsrt.c:103/104/118/149.
fn ffmpeg_alias_not_exposed(key: &str) -> SrtUrlError {
    match key {
        "timeout" => SrtUrlError::FfmpegAliasNotExposed {
            key: key.to_string(),
            canonical: "ffmpeg's read/write timeout, libsrt rw_timeout, microseconds",
            suggestion: "use x-recvtimeout / x-sendtimeout (milliseconds) for the closest equivalent",
        },
        "listen_timeout" => SrtUrlError::FfmpegAliasNotExposed {
            key: key.to_string(),
            canonical: "ffmpeg's listen_timeout (connection-awaiting timeout, microseconds)",
            suggestion: "not exposed by this library; see deferred-features.md",
        },
        "tsbpd" => SrtUrlError::FfmpegAliasNotExposed {
            key: key.to_string(),
            canonical: "ffmpeg short alias for SRTO_TSBPDMODE (also reachable via the longer 'tsbpdmode' libsrt URL key)",
            suggestion: "not exposed by this library; see deferred-features.md",
        },
        _ => unreachable!("ffmpeg_alias_not_exposed called with unexpected key '{key}'"),
    }
}

/// The error for a key the table does not list: an ffmpeg alias this
/// library does not expose, a libsrt key it recognises but does not expose,
/// or an unknown key.
fn unrecognized_key(key: &str) -> SrtUrlError {
    if matches!(key, "timeout" | "listen_timeout" | "tsbpd") {
        return ffmpeg_alias_not_exposed(key);
    }
    if let Some(srto) = group3_lookup(key) {
        return SrtUrlError::UnsupportedKey {
            key: key.to_string(),
            srto,
        };
    }
    SrtUrlError::UnknownKey {
        key: key.to_string(),
    }
}

/// The name this type had before it was renamed.
#[deprecated(since = "0.9.0", note = "renamed to `SrtUrlError`")]
pub type UrlError = SrtUrlError;

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
        assert_eq!(u.config.connect_timeout, Some(Duration::from_millis(10000)));
    }

    #[test]
    fn url_connect_timeout_alias_parses() {
        let u = SrtUrl::parse("srt://h:9000?connect_timeout=10000").unwrap();
        assert_eq!(u.config.connect_timeout, Some(Duration::from_millis(10000)));
    }

    #[test]
    fn url_linger_parses() {
        let u = SrtUrl::parse("srt://h:9000?linger=5").unwrap();
        assert_eq!(u.config.linger, Some(Duration::from_secs(5)));
    }

    #[test]
    fn url_udprcvbuf_parses() {
        let u = SrtUrl::parse("srt://h:9000?udprcvbuf=2000000").unwrap();
        assert_eq!(u.config.udp_recv_buffer_bytes, Some(2_000_000));
    }

    #[test]
    fn url_udpsndbuf_parses() {
        let u = SrtUrl::parse("srt://h:9000?udpsndbuf=2000000").unwrap();
        assert_eq!(u.config.udp_send_buffer_bytes, Some(2_000_000));
    }

    #[test]
    fn url_recv_buffer_size_alias_parses() {
        let u = SrtUrl::parse("srt://h:9000?recv_buffer_size=2000000").unwrap();
        assert_eq!(u.config.udp_recv_buffer_bytes, Some(2_000_000));
    }

    #[test]
    fn url_send_buffer_size_alias_parses() {
        let u = SrtUrl::parse("srt://h:9000?send_buffer_size=2000000").unwrap();
        assert_eq!(u.config.udp_send_buffer_bytes, Some(2_000_000));
    }

    #[test]
    fn url_alias_pkt_size_matches_payloadsize() {
        let canonical = SrtUrl::parse("srt://h:9000?payloadsize=1316").unwrap();
        let alias = SrtUrl::parse("srt://h:9000?pkt_size=1316").unwrap();
        assert_eq!(canonical.config.payload_size, alias.config.payload_size);
        assert_eq!(alias.config.payload_size, Some(1316));
    }

    #[test]
    fn url_alias_payload_size_matches_payloadsize() {
        let alias = SrtUrl::parse("srt://h:9000?payload_size=1316").unwrap();
        assert_eq!(alias.config.payload_size, Some(1316));
    }

    #[test]
    fn url_alias_srt_streamid_matches_streamid() {
        let canonical = SrtUrl::parse("srt://h:9000?streamid=abc").unwrap();
        let alias = SrtUrl::parse("srt://h:9000?srt_streamid=abc").unwrap();
        assert_eq!(
            canonical.config.stream_id.as_ref().map(|s| s.as_str()),
            alias.config.stream_id.as_ref().map(|s| s.as_str()),
        );
        assert_eq!(
            alias.config.stream_id.as_ref().map(|s| s.as_str()),
            Some("abc")
        );
    }

    #[test]
    fn url_alias_tsbpddelay_matches_latency() {
        let alias = SrtUrl::parse("srt://h:9000?tsbpddelay=200").unwrap();
        assert_eq!(alias.config.latency, Some(Duration::from_millis(200)));
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
        assert_eq!(canonical.config.congestion, alias.config.congestion);
    }

    #[test]
    fn url_alias_ffs_matches_fc() {
        let canonical = SrtUrl::parse("srt://h:9000?fc=25600").unwrap();
        let alias = SrtUrl::parse("srt://h:9000?ffs=25600").unwrap();
        assert_eq!(
            canonical.config.flow_window_packets,
            alias.config.flow_window_packets
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
        assert!(matches!(err, SrtUrlError::MissingHost));
    }

    #[test]
    fn mode_rendezvous_unsupported() {
        let err = SrtUrl::parse("srt://peer:9000?mode=rendezvous").unwrap_err();
        assert!(matches!(err, SrtUrlError::UnsupportedMode { .. }));
    }

    /// The one difference between [`SrtUrl::connect`] and
    /// [`SrtUrl::connect_recv`]: the sender preset. `connect` is the C
    /// sender open path (15 s connect timeout, 5 s linger, `Role::Sender`
    /// filled in where the URL left them unset); `connect_recv` is the
    /// plain/receive-side open the Python and JVM bindings do today, which
    /// applies the URL's keys and nothing else. Asserted on the config both
    /// methods dial with, since a live socket exposes neither value back.
    #[test]
    fn connect_applies_the_sender_preset_and_connect_recv_does_not() {
        let url = SrtUrl::parse("srt://camera.local:9000?latency=120").expect("parse");

        let sender = url.socket_config(Role::Sender);
        assert_eq!(sender.connect_timeout, Some(Duration::from_secs(15)));
        assert_eq!(sender.linger, Some(Duration::from_secs(5)));
        assert_eq!(sender.role, crate::options::Role::Sender);

        let recv = url.socket_config(Role::Receiver);
        assert_eq!(recv.connect_timeout, None, "no sender connect-timeout");
        assert_eq!(recv.linger, None, "no sender linger");
        assert_eq!(
            recv.role,
            crate::options::Role::Receiver,
            "connect_recv must not promote the socket to Role::Sender"
        );

        // Both apply the URL's keys identically.
        assert_eq!(sender.latency, Some(Duration::from_millis(120)));
        assert_eq!(recv.latency, Some(Duration::from_millis(120)));
    }

    /// The preset only fills what the URL left unset, so a URL that names
    /// `conntimeo` / `linger` wins on BOTH paths (Q4-A precedence).
    #[test]
    fn the_url_wins_over_the_sender_preset() {
        let url = SrtUrl::parse("srt://camera.local:9000?conntimeo=2000&linger=1").expect("parse");
        for cfg in [
            url.socket_config(Role::Sender),
            url.socket_config(Role::Receiver),
        ] {
            assert_eq!(cfg.connect_timeout, Some(Duration::from_millis(2000)));
            assert_eq!(cfg.linger, Some(Duration::from_secs(1)));
        }
    }
}
