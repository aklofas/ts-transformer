//! `rtp://host:port?key=value&...` URL parsing, plus `rtsp://` and
//! `rtsps://` URL parsing for the Phase 2 RTSP client.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Built on the scheme-neutral [`tst_core::url::common`] helpers from
//! Plan #97. The `rtp://` shape is parsed by [`RtpUrl`]; the
//! `rtsp://` / `rtsps://` shapes are parsed by [`RtspUrl`] (consumed by
//! the Phase 2 RTSP client; see `crate::rtsp`).
//!
//! Supported `rtp://` query keys:
//!
//! | Key | Value | Default |
//! |---|---|---|
//! | `ttl` | 1..=255 | 1 (unicast); 8 (multicast send) |
//! | `iface` | interface name (`eth0`) or IPv4 literal | OS default |
//! | `pkt_size` | positive multiple of 188 (send URLs only; rejected on receive URLs) | 1316 |
//! | `ssrc` | u32 decimal or `0x`-prefixed hex | random |
//! | `pt` | 1..=127, 33 rejected (MPEG-TS) | absent — required by `H264Receiver::listen` |
//! | `recv_timeout` | positive integer milliseconds | absent — no deadline |
//!
//! Supported `rtsp[s]://` query keys:
//!
//! | Key | Value | Default |
//! |---|---|---|
//! | `transport` | `tcp` or `udp` | absent → prefer-UDP with TCP fallback |
//! | `rtsp_version` | `1.0` or `2.0` | `1.0` |
//! | `tcp_keepalive` | `SO_KEEPALIVE` idle seconds for the control TCP socket | absent → OS keepalive off |
//! | `recv_timeout` | positive integer milliseconds | absent — no deadline |
//!
//! `ttl` is wire-format-shared between IPv4 (`IP_MULTICAST_TTL`) and
//! IPv6 (`IPV6_MULTICAST_HOPS`). The transport applies the right
//! setsockopt based on the destination address family.

use std::net::IpAddr;
use std::time::Duration;

use secrecy::SecretString;
use thiserror::Error;
use tst_core::url::common::{ParsedUrl, UrlError as CoreUrlError, parse_url, percent_decode};

/// Default UDP payload size: 7 MPEG-TS packets per RTP packet = 1316 B
/// (RFC 2250 §2 mandates an integral multiple of 188; 7×188 matches
/// libsrt live-mode payload, and the SocketStats / transport `max_payload`
/// shape we cascade through `Transport`).
pub const DEFAULT_PKT_SIZE: usize = 1316;

/// Maximum value we accept for `ttl` / `pkt_size` etc; just `u8::MAX`
/// for now.
const MAX_TTL: u32 = 255;

/// MPEG-TS packet size (RFC 2250 §2).
const TS_PACKET_SIZE: usize = 188;

/// Parsed `rtp://` URL. Construction validates the scheme and query keys
/// but does NOT touch the network — interface names, host resolvability,
/// and bind permissions are checked by the `transport` layer at
/// `connect` / `listen` time.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpUrl {
    /// Destination/source host as written in the URL (literal IP or
    /// domain). Parsed to `IpAddr` lazily by the transport when needed.
    pub host: String,
    /// Port.
    pub port: u16,
    /// `?ttl=N` query key, or `None` to use the default.
    pub ttl: Option<u8>,
    /// `?iface=eth0` or `?iface=192.168.1.50` — empty when absent.
    pub iface: Option<String>,
    /// `?pkt_size=N` — bytes per UDP datagram (188-multiple). `None` when
    /// the query key is absent; the send path resolves
    /// [`DEFAULT_PKT_SIZE`] at build time. Receive-side entry points
    /// reject `Some` with [`UrlError::RecvPktSize`] — the knob is
    /// send-side only since the recv-ceiling change.
    pub pkt_size: Option<usize>,
    /// `?ssrc=N` — explicit SSRC; `None` means "randomize at construct".
    pub ssrc: Option<u32>,
    /// `?pt=N` — RTP payload type for elementary-stream (H.264) ingest.
    ///
    /// Range 1..=127; value 33 is rejected ("payload type 33 is MPEG-TS —
    /// use the MP2T receiver (no ?pt=)"). Absent for standard MP2T URLs.
    /// Required by [`crate::h264::H264Receiver::listen`]; rejected by the
    /// MP2T constructors (`RtpTransport::connect*` /
    /// `RtpRecvTransport::listen*`).
    pub pt: Option<u8>,
    /// Configured receive deadline (`?recv_timeout=<ms>`, milliseconds).
    /// Applied at transport construction; expiry surfaces as retryable
    /// `Backpressure`. Absent — the default — means no deadline.
    pub recv_timeout: Option<Duration>,
}

/// Errors specific to parsing the `rtp://` URL form.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum UrlError {
    /// Structural URL parse failure (delegated to `tst_core::url`).
    #[error(transparent)]
    Syntax(#[from] CoreUrlError),
    /// Scheme was not `rtp` (e.g., user passed an `srt://` URL by
    /// mistake). `rtsp://` / `rtsps://` rejected here too — they are
    /// parsed by a separate Phase 2 module.
    #[error("expected 'rtp' scheme, got '{got}'")]
    WrongScheme { got: String },
    /// `rtp://` requires a port. We do not pick a default.
    #[error("rtp:// URL requires :port")]
    MissingPort,
    /// Host failed validation for the parser's use case (e.g., DNS name
    /// supplied where an IP literal is required for a server bind, or
    /// non-multicast address for a multicast group).
    #[error("bad host: {detail}")]
    BadHost { detail: String },
    /// `?ttl=` failed validation (out of `1..=255` range or non-numeric).
    #[error("invalid ttl '{got}': {detail}")]
    BadTtl { got: String, detail: String },
    /// `?pkt_size=` not a positive multiple of 188.
    #[error("invalid pkt_size '{got}': {detail}")]
    BadPktSize { got: String, detail: String },
    /// `?pkt_size=` supplied to a receive-side entry point. The knob is
    /// send-side only since the recv-ceiling change (PR #97): receive
    /// buffers size to the transport's deliverable ceiling automatically.
    #[error(
        "pkt_size is a send-side knob; receive buffers size to the transport's deliverable ceiling automatically — remove ?pkt_size= from receiver URLs"
    )]
    RecvPktSize,
    /// `?ssrc=` couldn't be parsed as decimal or `0x`-prefixed hex.
    #[error("invalid ssrc '{got}': {detail}")]
    BadSsrc { got: String, detail: String },
    /// `?iface=` value couldn't be interpreted as either a non-empty
    /// interface name or a literal IPv4 address.
    #[error("invalid iface '{got}'")]
    BadIface { got: String },
    /// `?pt=` value is out of range or is the reserved MPEG-TS type (33).
    #[error("invalid pt '{got}': {detail}")]
    BadPayloadType { got: String, detail: String },
    /// `?recv_timeout=` failed validation (non-numeric or zero).
    #[error("invalid recv_timeout '{got}': {detail}")]
    BadRecvTimeout { got: String, detail: String },
    /// Query key wasn't recognized. (The full set is documented in the
    /// module rustdoc table.) We reject unknown keys hard so typos don't
    /// silently get ignored.
    #[error("unknown rtp:// URL query key '{got}'")]
    UnknownKey { got: String },
    /// URL scheme is not one of the supported set for the parser invoked.
    /// Distinct from [`UrlError::WrongScheme`] which is reserved for the
    /// `rtp://`-only [`RtpUrl`] parser; [`RtspUrl`] uses this variant
    /// when given anything other than `rtsp://` / `rtsps://`.
    #[error("unsupported scheme: {scheme}")]
    BadScheme { scheme: String },
    /// Query parameter key is recognized but its value is invalid
    /// (e.g., `?transport=quic` on an `rtsp://` URL).
    #[error("bad value for query parameter {key}: {value}")]
    BadQuery { key: String, value: String },
    /// Query parameter key is not recognized. Mirrors
    /// [`UrlError::UnknownKey`] but uses RFC-3986 terminology and is the
    /// variant emitted by [`RtspUrl::parse`].
    #[error("unknown query parameter: {key}")]
    UnknownQueryKey { key: String },
}

impl RtpUrl {
    /// Parse `rtp://host:port?key=value&...`.
    ///
    /// On success, all fields are validated independently — the
    /// transport layer doesn't need to re-check ranges. The `host`
    /// field is preserved as-written (no DNS resolution at this stage).
    pub fn parse(s: &str) -> Result<Self, UrlError> {
        let ParsedUrl {
            scheme,
            host,
            port,
            query,
            ..
        } = parse_url(s)?;
        if !scheme.eq_ignore_ascii_case("rtp") {
            return Err(UrlError::WrongScheme {
                got: scheme.to_string(),
            });
        }
        let port = port.ok_or(UrlError::MissingPort)?;
        let mut ttl = None;
        let mut iface = None;
        let mut pkt_size = None;
        let mut ssrc = None;
        let mut pt = None;
        let mut recv_timeout = None;
        for (k, v) in query.iter() {
            match k.as_ref() {
                "ttl" => ttl = Some(parse_ttl(v.as_ref())?),
                "iface" => iface = Some(parse_iface(v.as_ref())?),
                "pkt_size" => pkt_size = Some(parse_pkt_size(v.as_ref())?),
                "ssrc" => ssrc = Some(parse_ssrc(v.as_ref())?),
                "pt" => pt = Some(parse_pt(v.as_ref())?),
                "recv_timeout" => recv_timeout = Some(parse_recv_timeout(v.as_ref())?),
                other => {
                    return Err(UrlError::UnknownKey {
                        got: other.to_string(),
                    });
                }
            }
        }
        Ok(Self {
            host: host.to_string(),
            port,
            ttl,
            iface,
            pkt_size,
            ssrc,
            pt,
            recv_timeout,
        })
    }
}

fn parse_ttl(v: &str) -> Result<u8, UrlError> {
    let n: u32 = v
        .parse()
        .map_err(|e: std::num::ParseIntError| UrlError::BadTtl {
            got: v.to_string(),
            detail: e.to_string(),
        })?;
    if !(1..=MAX_TTL).contains(&n) {
        return Err(UrlError::BadTtl {
            got: v.to_string(),
            detail: format!("must be in 1..={MAX_TTL}"),
        });
    }
    Ok(n as u8)
}

fn parse_pkt_size(v: &str) -> Result<usize, UrlError> {
    let n: usize = v
        .parse()
        .map_err(|e: std::num::ParseIntError| UrlError::BadPktSize {
            got: v.to_string(),
            detail: e.to_string(),
        })?;
    if n == 0 || n % TS_PACKET_SIZE != 0 {
        return Err(UrlError::BadPktSize {
            got: v.to_string(),
            detail: format!("must be a positive multiple of {TS_PACKET_SIZE}"),
        });
    }
    Ok(n)
}

fn parse_ssrc(v: &str) -> Result<u32, UrlError> {
    let (radix, body) = if let Some(rest) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        (16, rest)
    } else {
        (10, v)
    };
    u32::from_str_radix(body, radix).map_err(|e| UrlError::BadSsrc {
        got: v.to_string(),
        detail: e.to_string(),
    })
}

fn parse_iface(v: &str) -> Result<String, UrlError> {
    if v.is_empty() {
        return Err(UrlError::BadIface { got: v.to_string() });
    }
    // Interface name OR literal IPv4 (libsrt-style: `?iface=192.168.1.50`).
    // We don't restrict ASCII here; OS interface names can be Unicode on
    // some platforms.
    if v.parse::<IpAddr>().is_ok() || !v.chars().any(|c| c.is_whitespace()) {
        Ok(v.to_string())
    } else {
        Err(UrlError::BadIface { got: v.to_string() })
    }
}

fn parse_pt(v: &str) -> Result<u8, UrlError> {
    let n: u32 = v
        .parse()
        .map_err(|e: std::num::ParseIntError| UrlError::BadPayloadType {
            got: v.to_string(),
            detail: e.to_string(),
        })?;
    if n == 33 {
        return Err(UrlError::BadPayloadType {
            got: v.to_string(),
            detail: "payload type 33 is MPEG-TS — use the MP2T receiver (no ?pt=)".to_string(),
        });
    }
    if !(1..=127).contains(&n) {
        return Err(UrlError::BadPayloadType {
            got: v.to_string(),
            detail: "must be in 1..=127 (excluding 33, which is MPEG-TS)".to_string(),
        });
    }
    Ok(n as u8)
}

fn parse_recv_timeout(v: &str) -> Result<Duration, UrlError> {
    let ms: u64 = v
        .parse()
        .map_err(|e: std::num::ParseIntError| UrlError::BadRecvTimeout {
            got: v.to_string(),
            detail: e.to_string(),
        })?;
    if ms == 0 {
        return Err(UrlError::BadRecvTimeout {
            got: v.to_string(),
            detail: "must be nonzero".to_string(),
        });
    }
    Ok(Duration::from_millis(ms))
}

/// RTSP URL scheme — distinguishes plain RTSP from RTSP-over-TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RtspScheme {
    /// Plain `rtsp://` — TCP control connection without TLS.
    Rtsp,
    /// `rtsps://` — TCP control connection wrapped in rustls.
    /// Requires cargo feature `tls`; URL parses fine without the
    /// feature but `RtspClient::connect` will reject it.
    Rtsps,
}

/// Wire-format version of outgoing RTSP request lines.
///
/// Most deployed IP cameras only understand `RTSP/1.0` (RFC 2326);
/// `RTSP/2.0` (RFC 7826) is wire-identical for the OPTIONS / DESCRIBE
/// / SETUP / PLAY / TEARDOWN subset we use (see RFC 7826 §1.3
/// "Backward Compatibility"). Default is `V1_0` for maximum interop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RtspVersion {
    /// `RTSP/1.0` per RFC 2326.
    V1_0,
    /// `RTSP/2.0` per RFC 7826.
    V2_0,
}

impl RtspVersion {
    /// Returns the wire-format string used on outgoing request lines
    /// (e.g., `OPTIONS rtsp://cam/h264 RTSP/1.0`).
    #[must_use]
    pub fn wire_str(self) -> &'static str {
        match self {
            RtspVersion::V1_0 => "RTSP/1.0",
            RtspVersion::V2_0 => "RTSP/2.0",
        }
    }
}

/// Per-URL transport preference — what to send in the SETUP `Transport:`
/// header and how to react to a server 461 Unsupported Transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RtspTransportPref {
    /// `?transport=` absent — try UDP first; on 461, auto-fall-back to
    /// TCP-interleaved.
    PreferUdp,
    /// `?transport=udp` — try UDP only; on 461, surface
    /// `RtspError::UnsupportedTransport` without TCP retry.
    ForceUdp,
    /// `?transport=tcp` — skip the UDP attempt; SETUP directly with
    /// `RTP/AVP/TCP;interleaved=0-1`.
    ForceTcp,
}

/// Parsed `rtsp://` or `rtsps://` URL.
///
/// Constructed by [`RtspUrl::parse`]; consumed by the Phase 2 RTSP
/// client (`RtspClient::connect_with`).
#[derive(Debug, Clone)]
pub struct RtspUrl {
    scheme: RtspScheme,
    /// Host as written in the URL (literal IP or domain). Parsed lazily
    /// by the RTSP client at connect time.
    pub host: String,
    /// Control-channel TCP port. Defaults to 554 for `rtsp://` and 322
    /// for `rtsps://` when the URL omits the port.
    pub port: u16,
    /// Path component including the leading `/` (empty string when the
    /// URL has no path). Used verbatim on the RTSP request line and for
    /// Digest `uri=`.
    pub path: String,
    /// `user` from `user[:password]@host`, if present.
    pub username: Option<String>,
    /// `password` from `user:password@host`. Wrapped in [`SecretString`]
    /// so it zeroes on drop and redacts in `Debug` output.
    pub password: Option<SecretString>,
    /// Effective transport preference; see [`RtspTransportPref`].
    pub transport_preference: RtspTransportPref,
    /// Effective RTSP wire-format version; see [`RtspVersion`].
    pub rtsp_version: RtspVersion,
    /// TCP-level `SO_KEEPALIVE` idle time for the control socket
    /// (`?tcp_keepalive=N` seconds), or `None` (default) to leave the OS
    /// keepalive off. Distinct from the RTSP-level OPTIONS keepalive
    /// cadence (`RtspClientBuilder::keepalive_interval`): this knob makes
    /// the kernel probe an idle connection so a peer that vanished
    /// without FIN/RST eventually errors the socket.
    pub tcp_keepalive: Option<Duration>,
    /// Configured receive deadline (`?recv_timeout=<ms>`, milliseconds).
    /// Applied at transport construction; expiry surfaces as retryable
    /// `Backpressure`. Client-local on rtsp URLs — never rendered to the
    /// wire.
    pub recv_timeout: Option<Duration>,
}

impl RtspUrl {
    /// Parse an `rtsp://` or `rtsps://` URL into the structured form.
    ///
    /// Accepted query parameters:
    /// - `transport=tcp|udp` — see [`RtspTransportPref`]; absent ==
    ///   `PreferUdp`.
    /// - `rtsp_version=1.0|2.0` — see [`RtspVersion`]; absent == `V1_0`.
    /// - `tcp_keepalive=N` — enable `SO_KEEPALIVE` on the control-channel
    ///   TCP socket with an `N`-second idle time; absent == OS default
    ///   (keepalive off).
    /// - `recv_timeout=N` — configured receive deadline in milliseconds
    ///   (must be nonzero); absent == no deadline.
    ///
    /// Unknown query keys cause [`UrlError::UnknownQueryKey`]; recognized
    /// keys with invalid values cause [`UrlError::BadQuery`]; non-rtsp
    /// schemes cause [`UrlError::BadScheme`].
    ///
    /// # Errors
    ///
    /// Returns [`UrlError`] when structural URL parsing fails, when the
    /// scheme is not `rtsp` / `rtsps`, when a query value is invalid, or
    /// when an unknown query key is present.
    pub fn parse(s: &str) -> Result<Self, UrlError> {
        let parsed = parse_url(s).map_err(UrlError::Syntax)?;
        let scheme = match parsed.scheme {
            "rtsp" => RtspScheme::Rtsp,
            "rtsps" => RtspScheme::Rtsps,
            other => {
                return Err(UrlError::BadScheme {
                    scheme: other.to_string(),
                });
            }
        };
        let default_port = match scheme {
            RtspScheme::Rtsp => 554,
            RtspScheme::Rtsps => 322,
        };
        let port = parsed.port.unwrap_or(default_port);

        let mut transport_preference = RtspTransportPref::PreferUdp;
        let mut rtsp_version = RtspVersion::V1_0;
        let mut tcp_keepalive = None;
        let mut recv_timeout = None;
        for (k, v) in &parsed.query {
            match k.as_ref() {
                "transport" => {
                    transport_preference = match v.as_ref() {
                        "tcp" => RtspTransportPref::ForceTcp,
                        "udp" => RtspTransportPref::ForceUdp,
                        other => {
                            return Err(UrlError::BadQuery {
                                key: "transport".to_string(),
                                value: other.to_string(),
                            });
                        }
                    };
                }
                "rtsp_version" => {
                    rtsp_version = match v.as_ref() {
                        "1.0" => RtspVersion::V1_0,
                        "2.0" => RtspVersion::V2_0,
                        other => {
                            return Err(UrlError::BadQuery {
                                key: "rtsp_version".to_string(),
                                value: other.to_string(),
                            });
                        }
                    };
                }
                "tcp_keepalive" => {
                    let secs: u64 =
                        tst_core::url::common::parse_int_query(v.as_ref()).map_err(|_| {
                            UrlError::BadQuery {
                                key: "tcp_keepalive".to_string(),
                                value: v.to_string(),
                            }
                        })?;
                    tcp_keepalive = Some(Duration::from_secs(secs));
                }
                "recv_timeout" => {
                    recv_timeout = Some(parse_recv_timeout(v.as_ref()).map_err(|e| {
                        let detail = match &e {
                            UrlError::BadRecvTimeout { detail, .. } => detail.clone(),
                            _ => e.to_string(),
                        };
                        UrlError::BadQuery {
                            key: "recv_timeout".to_string(),
                            value: format!("{v} ({detail})"),
                        }
                    })?);
                }
                other => {
                    return Err(UrlError::UnknownQueryKey {
                        key: other.to_string(),
                    });
                }
            }
        }

        Ok(RtspUrl {
            scheme,
            host: parsed.host.to_string(),
            port,
            path: parsed.path.to_string(),
            // Userinfo is percent-decoded (CORR-21): `parse_url` hands it
            // over verbatim; the encoded form is the only way to put
            // `@ : / ? #` in a credential and every other RTSP client decodes.
            username: parsed
                .username
                .map(percent_decode)
                .transpose()
                .map_err(UrlError::Syntax)?
                .map(|u| u.into_owned()),
            password: parsed
                .password
                .map(percent_decode)
                .transpose()
                .map_err(UrlError::Syntax)?
                .map(|p| SecretString::new(p.into_owned().into())),
            transport_preference,
            rtsp_version,
            tcp_keepalive,
            recv_timeout,
        })
    }

    /// The URL scheme (`rtsp` or `rtsps`).
    #[must_use]
    pub fn scheme(&self) -> RtspScheme {
        self.scheme
    }

    /// Render the URL string suitable for the Digest `uri=` parameter
    /// and the RTSP request line. Includes scheme, host, port, and path;
    /// does NOT include user credentials.
    ///
    /// IPv6 hosts are stored bracket-stripped (e.g., `::1`); this method
    /// re-brackets them per RFC 3986 §3.2.2 so the rendered URI is valid
    /// (e.g., `rtsp://[::1]:554/live`).
    #[must_use]
    pub fn render_no_credentials(&self) -> String {
        let scheme = match self.scheme {
            RtspScheme::Rtsp => "rtsp",
            RtspScheme::Rtsps => "rtsps",
        };
        // An IPv6 literal contains ':'; IPv4 addresses and DNS names do not.
        let host_str = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("{}://{}:{}{}", scheme, host_str, self.port, self.path)
    }

    /// Validate the URL is appropriate for `RtspServer::bind(url)`.
    /// Rules: host must parse as an IP literal (DNS not resolved
    /// server-side); port is permitted to be 0 (kernel-pick).
    ///
    /// # Errors
    ///
    /// Returns [`UrlError::BadHost`] when the URL's host is not an IP
    /// literal (e.g., a DNS name like `example.com`).
    pub fn validate_for_server_bind(&self) -> Result<(), UrlError> {
        if self.host.parse::<std::net::IpAddr>().is_err() {
            return Err(UrlError::BadHost {
                detail: format!("server bind requires an IP literal; got '{}'", self.host),
            });
        }
        Ok(())
    }
}

/// Parsed multicast group URL used by
/// `crate::rtsp::server::RtspServer::add_multicast_mount`.
///
/// Form: `rtp://<mcast-ip>:<port>?ttl=N&iface=ethN`.
#[derive(Debug, Clone)]
pub struct MulticastGroup {
    /// Multicast destination address + port. IPv4 in `224.0.0.0/4` or
    /// IPv6 in `ff00::/8`.
    pub addr: std::net::SocketAddr,
    /// `?ttl=N` from the URL; defaults to 8 (multicast send default per
    /// the [`RtpUrl`] table).
    pub ttl: u8,
    /// `?iface=eth0` or IPv4 literal; `None` to let the OS pick.
    pub iface: Option<String>,
}

impl MulticastGroup {
    /// Parse a multicast group URL. Returns an error if the host is not
    /// in the IPv4 multicast range (224.0.0.0/4) or IPv6 multicast range
    /// (ff00::/8), or if the URL syntax is malformed.
    ///
    /// # Errors
    ///
    /// Returns [`UrlError`] propagated from [`RtpUrl::parse`] for
    /// structural errors, [`UrlError::BadHost`] when the host is not a
    /// valid IP literal or is not in a multicast range.
    pub fn parse(url: &str) -> Result<Self, UrlError> {
        let rtp = RtpUrl::parse(url)?;
        let ip: std::net::IpAddr =
            rtp.host
                .parse()
                .map_err(|e: std::net::AddrParseError| UrlError::BadHost {
                    detail: e.to_string(),
                })?;
        let is_mcast = match ip {
            std::net::IpAddr::V4(v) => v.is_multicast(),
            std::net::IpAddr::V6(v) => v.is_multicast(),
        };
        if !is_mcast {
            return Err(UrlError::BadHost {
                detail: format!("address '{}' is not multicast", rtp.host),
            });
        }
        Ok(MulticastGroup {
            addr: std::net::SocketAddr::new(ip, rtp.port),
            ttl: rtp.ttl.unwrap_or(8),
            iface: rtp.iface,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_unicast_url() {
        let u = RtpUrl::parse("rtp://10.0.0.5:5004").unwrap();
        assert_eq!(u.host, "10.0.0.5");
        assert_eq!(u.port, 5004);
        assert!(u.ttl.is_none());
        assert!(u.iface.is_none());
        assert_eq!(u.pkt_size, None);
        assert!(u.ssrc.is_none());
    }

    #[test]
    fn parse_multicast_url_with_ttl_iface() {
        let u = RtpUrl::parse("rtp://239.10.10.1:5004?ttl=8&iface=eth0").unwrap();
        assert_eq!(u.host, "239.10.10.1");
        assert_eq!(u.ttl, Some(8));
        assert_eq!(u.iface, Some("eth0".to_string()));
    }

    #[test]
    fn parse_url_with_pkt_size_and_ssrc() {
        let u = RtpUrl::parse("rtp://239.10.10.1:5004?pkt_size=1316&ssrc=0xCAFEBABE").unwrap();
        assert_eq!(u.pkt_size, Some(1316));
        assert_eq!(u.ssrc, Some(0xCAFE_BABE));
    }

    #[test]
    fn parse_ssrc_decimal() {
        let u = RtpUrl::parse("rtp://h:5004?ssrc=4242").unwrap();
        assert_eq!(u.ssrc, Some(4242));
    }

    #[test]
    fn rejects_non_rtp_scheme() {
        let err = RtpUrl::parse("srt://h:9000").unwrap_err();
        assert!(matches!(err, UrlError::WrongScheme { .. }));
    }

    #[test]
    fn rejects_rtsp_scheme() {
        // RTSP is a separate Phase 2 surface, not parsed by RtpUrl.
        let err = RtpUrl::parse("rtsp://camera.lan/path").unwrap_err();
        assert!(matches!(err, UrlError::WrongScheme { .. }));
    }

    #[test]
    fn rejects_missing_port() {
        let err = RtpUrl::parse("rtp://239.10.10.1").unwrap_err();
        assert!(matches!(err, UrlError::MissingPort));
    }

    #[test]
    fn rejects_zero_ttl() {
        let err = RtpUrl::parse("rtp://h:5004?ttl=0").unwrap_err();
        assert!(matches!(err, UrlError::BadTtl { .. }));
    }

    #[test]
    fn rejects_oob_ttl() {
        let err = RtpUrl::parse("rtp://h:5004?ttl=256").unwrap_err();
        assert!(matches!(err, UrlError::BadTtl { .. }));
    }

    #[test]
    fn rejects_pkt_size_not_188_multiple() {
        let err = RtpUrl::parse("rtp://h:5004?pkt_size=1500").unwrap_err();
        assert!(matches!(err, UrlError::BadPktSize { .. }));
    }

    #[test]
    fn rejects_pkt_size_zero() {
        let err = RtpUrl::parse("rtp://h:5004?pkt_size=0").unwrap_err();
        assert!(matches!(err, UrlError::BadPktSize { .. }));
    }

    #[test]
    fn rejects_unknown_query_key() {
        // `latency` makes sense for srt://; for rtp:// it's a typo.
        let err = RtpUrl::parse("rtp://h:5004?latency=200").unwrap_err();
        assert!(matches!(err, UrlError::UnknownKey { .. }));
    }

    #[test]
    fn rejects_bad_ssrc() {
        let err = RtpUrl::parse("rtp://h:5004?ssrc=notanint").unwrap_err();
        assert!(matches!(err, UrlError::BadSsrc { .. }));
    }

    // ── ?pt= query key tests ──────────────────────────────────────────────

    #[test]
    fn pt_96_parses_ok() {
        let u = RtpUrl::parse("rtp://127.0.0.1:5004?pt=96").unwrap();
        assert_eq!(u.pt, Some(96));
    }

    #[test]
    fn pt_1_parses_ok() {
        let u = RtpUrl::parse("rtp://127.0.0.1:5004?pt=1").unwrap();
        assert_eq!(u.pt, Some(1));
    }

    #[test]
    fn pt_127_parses_ok() {
        let u = RtpUrl::parse("rtp://127.0.0.1:5004?pt=127").unwrap();
        assert_eq!(u.pt, Some(127));
    }

    #[test]
    fn pt_absent_gives_none() {
        let u = RtpUrl::parse("rtp://127.0.0.1:5004").unwrap();
        assert_eq!(u.pt, None);
    }

    #[test]
    fn pt_33_rejected_as_mp2t() {
        let err = RtpUrl::parse("rtp://h:5004?pt=33").unwrap_err();
        assert!(
            matches!(err, UrlError::BadPayloadType { .. }),
            "expected BadPayloadType for pt=33, got {err:?}"
        );
        if let UrlError::BadPayloadType { detail, .. } = err {
            assert!(
                detail.contains("MPEG-TS"),
                "error detail must mention MPEG-TS"
            );
        }
    }

    #[test]
    fn pt_200_rejected_out_of_range() {
        let err = RtpUrl::parse("rtp://h:5004?pt=200").unwrap_err();
        assert!(matches!(err, UrlError::BadPayloadType { .. }));
    }

    #[test]
    fn pt_0_rejected_out_of_range() {
        let err = RtpUrl::parse("rtp://h:5004?pt=0").unwrap_err();
        assert!(matches!(err, UrlError::BadPayloadType { .. }));
    }

    #[test]
    fn pt_non_numeric_rejected() {
        let err = RtpUrl::parse("rtp://h:5004?pt=h264").unwrap_err();
        assert!(matches!(err, UrlError::BadPayloadType { .. }));
    }

    // ── ?recv_timeout= query key tests ──────────────────────────────────────

    #[test]
    fn rtp_url_recv_timeout_query() {
        let u = RtpUrl::parse("rtp://127.0.0.1:5004?recv_timeout=15000").unwrap();
        assert_eq!(
            u.recv_timeout,
            Some(std::time::Duration::from_millis(15000))
        );
    }

    #[test]
    fn rtp_url_recv_timeout_default_none() {
        let u = RtpUrl::parse("rtp://127.0.0.1:5004").unwrap();
        assert_eq!(u.recv_timeout, None);
    }

    #[test]
    fn rtp_url_recv_timeout_zero_rejected() {
        let err = RtpUrl::parse("rtp://127.0.0.1:5004?recv_timeout=0").unwrap_err();
        assert!(matches!(err, UrlError::BadRecvTimeout { .. }));
    }

    #[test]
    fn rtp_url_recv_timeout_bad_value_rejected() {
        let err = RtpUrl::parse("rtp://127.0.0.1:5004?recv_timeout=forever").unwrap_err();
        assert!(matches!(err, UrlError::BadRecvTimeout { .. }));
    }
}

#[cfg(test)]
mod rtsp_tests {
    use super::*;

    #[test]
    fn rtsp_url_basic_no_query() {
        let u = RtspUrl::parse("rtsp://cam.lan:554/h264").unwrap();
        assert_eq!(u.scheme(), RtspScheme::Rtsp);
        assert_eq!(u.host, "cam.lan");
        assert_eq!(u.port, 554);
        assert_eq!(u.path, "/h264");
        assert_eq!(u.transport_preference, RtspTransportPref::PreferUdp);
        assert_eq!(u.rtsp_version, RtspVersion::V1_0);
        assert!(u.username.is_none());
        assert!(u.password.is_none());
    }

    #[test]
    fn rtsps_url_default_port_322() {
        let u = RtspUrl::parse("rtsps://cam.lan/h264").unwrap();
        assert_eq!(u.scheme(), RtspScheme::Rtsps);
        assert_eq!(u.port, 322);
    }

    #[test]
    fn rtsp_url_default_port_554() {
        let u = RtspUrl::parse("rtsp://cam.lan/h264").unwrap();
        assert_eq!(u.port, 554);
    }

    #[test]
    fn rtsp_url_transport_tcp_query() {
        let u = RtspUrl::parse("rtsp://cam.lan/h264?transport=tcp").unwrap();
        assert_eq!(u.transport_preference, RtspTransportPref::ForceTcp);
    }

    #[test]
    fn rtsp_url_transport_udp_query() {
        let u = RtspUrl::parse("rtsp://cam.lan/h264?transport=udp").unwrap();
        assert_eq!(u.transport_preference, RtspTransportPref::ForceUdp);
    }

    #[test]
    fn rtsp_url_transport_bad_value_rejected() {
        let e = RtspUrl::parse("rtsp://cam.lan/h264?transport=quic").unwrap_err();
        assert!(matches!(e, UrlError::BadQuery { .. }));
    }

    #[test]
    fn rtsp_url_rtsp_version_2_0() {
        let u = RtspUrl::parse("rtsp://cam.lan/h264?rtsp_version=2.0").unwrap();
        assert_eq!(u.rtsp_version, RtspVersion::V2_0);
    }

    #[test]
    fn rtsp_url_combined_query() {
        let u = RtspUrl::parse("rtsp://cam.lan/h264?transport=tcp&rtsp_version=2.0").unwrap();
        assert_eq!(u.transport_preference, RtspTransportPref::ForceTcp);
        assert_eq!(u.rtsp_version, RtspVersion::V2_0);
    }

    #[test]
    fn rtsp_url_credentials_extracted() {
        let u = RtspUrl::parse("rtsp://admin:s3cret@cam.lan/h264").unwrap();
        assert_eq!(u.username.as_deref(), Some("admin"));
        // Password is wrapped in Secret; we only check it exists.
        assert!(u.password.is_some());
    }

    /// CORR-21: userinfo is percent-decoded like every other RTSP client
    /// (ffmpeg / VLC / GStreamer) — a password containing `@ : / ? #` can
    /// only be expressed encoded, and the encoded form used to be hashed
    /// verbatim into the Digest response → `AuthFailed` with no diagnostic.
    #[test]
    fn rtsp_url_credentials_are_percent_decoded() {
        use secrecy::ExposeSecret;
        let u = RtspUrl::parse("rtsp://u:p%40ss@h/x").unwrap();
        assert_eq!(u.username.as_deref(), Some("u"));
        assert_eq!(
            u.password.as_ref().map(|p| p.expose_secret().to_string()),
            Some("p@ss".to_string())
        );
        let u = RtspUrl::parse("rtsp://u%3Aser:p%2Fw%3F%23@h/x").unwrap();
        assert_eq!(u.username.as_deref(), Some("u:ser"));
        assert_eq!(
            u.password.as_ref().map(|p| p.expose_secret().to_string()),
            Some("p/w?#".to_string())
        );
    }

    #[test]
    fn rtsp_url_bad_percent_escape_in_password_is_rejected() {
        let e = RtspUrl::parse("rtsp://u:p%zz@h/x").unwrap_err();
        assert!(matches!(e, UrlError::Syntax(_)), "got {e:?}");
    }

    #[test]
    fn rtsp_url_unknown_query_key_rejected() {
        let e = RtspUrl::parse("rtsp://cam.lan/h264?bogus=1").unwrap_err();
        assert!(matches!(e, UrlError::UnknownQueryKey { .. }));
    }

    #[test]
    fn rtsp_url_tcp_keepalive_query() {
        let u = RtspUrl::parse("rtsp://cam.lan/h264?tcp_keepalive=30").unwrap();
        assert_eq!(u.tcp_keepalive, Some(std::time::Duration::from_secs(30)));
    }

    #[test]
    fn rtsp_url_tcp_keepalive_default_none() {
        let u = RtspUrl::parse("rtsp://cam.lan/h264").unwrap();
        assert_eq!(u.tcp_keepalive, None);
    }

    #[test]
    fn rtsp_url_tcp_keepalive_bad_value_rejected() {
        let e = RtspUrl::parse("rtsp://cam.lan/h264?tcp_keepalive=forever").unwrap_err();
        assert!(matches!(e, UrlError::BadQuery { .. }));
    }

    #[test]
    fn rtsp_url_recv_timeout_query() {
        let u = RtspUrl::parse("rtsp://cam.lan/h264?recv_timeout=15000").unwrap();
        assert_eq!(
            u.recv_timeout,
            Some(std::time::Duration::from_millis(15000))
        );
    }

    #[test]
    fn rtsp_url_recv_timeout_default_none() {
        assert_eq!(
            RtspUrl::parse("rtsp://cam.lan/h264").unwrap().recv_timeout,
            None
        );
    }

    #[test]
    fn rtsp_url_recv_timeout_zero_rejected() {
        let e = RtspUrl::parse("rtsp://cam.lan/h264?recv_timeout=0").unwrap_err();
        assert!(
            e.to_string().contains("must be nonzero"),
            "error message should explain why 0 is invalid, got: {e}"
        );
    }

    #[test]
    fn rtsp_url_recv_timeout_bad_value_rejected() {
        RtspUrl::parse("rtsp://cam.lan/h264?recv_timeout=forever").unwrap_err();
    }
}

#[cfg(test)]
mod phase3_url_tests {
    use super::*;

    #[test]
    fn server_bind_url_wildcard_ok() {
        let u = RtspUrl::parse("rtsp://0.0.0.0:8554").unwrap();
        u.validate_for_server_bind().unwrap();
    }

    #[test]
    fn server_bind_url_loopback_ok() {
        let u = RtspUrl::parse("rtsp://127.0.0.1:0").unwrap();
        u.validate_for_server_bind().unwrap();
    }

    #[test]
    fn server_bind_url_ipv6_loopback_ok() {
        let u = RtspUrl::parse("rtsp://[::1]:8554").unwrap();
        u.validate_for_server_bind().unwrap();
    }

    #[test]
    fn server_bind_url_dns_name_rejected() {
        // If RtspUrl::parse accepts DNS names, validate_for_server_bind
        // should reject them. If RtspUrl::parse itself rejects DNS, this
        // test asserts that earlier path.
        let res =
            RtspUrl::parse("rtsp://example.com:8554").and_then(|u| u.validate_for_server_bind());
        assert!(res.is_err(), "DNS hostname should not validate as bind");
    }

    #[test]
    fn multicast_group_ipv4_ok() {
        let g = MulticastGroup::parse("rtp://239.0.0.1:5004").unwrap();
        assert_eq!(g.addr.port(), 5004);
        assert_eq!(g.ttl, 8);
        assert!(g.iface.is_none());
    }

    #[test]
    fn multicast_group_ipv4_unicast_rejected() {
        let e = MulticastGroup::parse("rtp://10.0.0.1:5004").unwrap_err();
        assert!(matches!(e, UrlError::BadHost { .. }));
    }

    #[test]
    fn multicast_group_ipv6_ok() {
        let g = MulticastGroup::parse("rtp://[ff02::1]:5004").unwrap();
        assert!(g.addr.ip().is_multicast());
    }

    #[test]
    fn multicast_group_ttl_and_iface_extracted() {
        let g = MulticastGroup::parse("rtp://239.0.0.1:5004?ttl=4&iface=192.168.1.50").unwrap();
        assert_eq!(g.ttl, 4);
        assert_eq!(g.iface.as_deref(), Some("192.168.1.50"));
    }

    /// IPv6 hosts must be bracket-stripped on parse and re-bracketed on
    /// render so the rendered URI is valid per RFC 3986 §3.2.2.
    #[test]
    fn ipv6_host_render_round_trips_with_brackets() {
        let u = RtspUrl::parse("rtsp://[::1]:554/live").unwrap();
        // Stored bracket-stripped.
        assert_eq!(u.host, "::1", "host must be stored without brackets");
        // Rendered with brackets.
        let rendered = u.render_no_credentials();
        assert_eq!(rendered, "rtsp://[::1]:554/live");
        // Re-parsing the rendered form must succeed and produce the same URL.
        let u2 = RtspUrl::parse(&rendered).unwrap();
        assert_eq!(u2.host, "::1");
        assert_eq!(u2.port, 554);
        assert_eq!(u2.path, "/live");
    }

    /// IPv4 hosts must not gain spurious brackets.
    #[test]
    fn ipv4_host_render_has_no_brackets() {
        let u = RtspUrl::parse("rtsp://192.168.1.10:554/live").unwrap();
        let rendered = u.render_no_credentials();
        assert_eq!(rendered, "rtsp://192.168.1.10:554/live");
        assert!(
            !rendered.contains('['),
            "IPv4 rendered URL must not contain brackets"
        );
    }
}
