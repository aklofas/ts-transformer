//! Parsing of `udp://...` URLs.
//!
//! The format mirrors what ffmpeg + VLC accept:
//!
//! - `udp://host:port` — unicast send/recv (IP literal or DNS hostname)
//! - `udp://@group:port` — multicast recv (the `@` prefix is the ffmpeg convention)
//! - `udp://group:port` (group in 224.0.0.0/4 or ff00::/8) — multicast send
//!
//! Query parameters: `iface`, `ttl`, `tos`, `rcvbuf`, `sndbuf`, `pkt_size` (send-only), `localaddr`.

use std::net::IpAddr;

use thiserror::Error;

/// Parsed UDP URL.
#[derive(Debug, Clone)]
pub struct UdpUrl {
    /// Destination address (for send) or bind address (for recv).
    /// Hostnames in the URL are resolved at parse time; this always
    /// holds the resolved literal.
    pub addr: IpAddr,
    /// Port (always required).
    pub port: u16,
    /// Whether the URL had an `@` prefix → "this is a receive-side bind".
    pub recv_bind: bool,
    /// Multicast outgoing interface (literal IPv4/IPv6 addr or interface name).
    pub iface: Option<String>,
    /// IPv4 multicast TTL or IPv6 hop limit.
    pub ttl: Option<u8>,
    /// IP TOS / DSCP byte.
    pub tos: Option<u8>,
    /// SO_RCVBUF size in bytes.
    pub rcvbuf: Option<usize>,
    /// SO_SNDBUF size in bytes.
    pub sndbuf: Option<usize>,
    /// Send-side datagram size. Rejected on a recv-side URL.
    pub pkt_size: Option<usize>,
    /// Local bind address override (for sending from a specific NIC).
    pub localaddr: Option<IpAddr>,
}

/// Errors from `UdpUrl::parse`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum UdpUrlError {
    #[error("URL scheme must be 'udp', got '{0}'")]
    BadScheme(String),
    #[error("URL must include a port")]
    MissingPort,
    #[error("could not resolve host '{host}': {detail}")]
    HostResolve { host: String, detail: String },
    /// A recv-bind (`@`-prefixed) URL was passed to a send-side entry
    /// point, or vice versa. The message names the right entry point.
    #[error("{0}")]
    SendRecvMismatch(String),
    /// The peer address and `?localaddr=` are different IP families. A
    /// socket bound to one family cannot send to the other — the failure
    /// would otherwise surface only at the first send as an opaque OS
    /// error, so it is rejected at parse time.
    #[error("peer address {peer} and ?localaddr={local} are different IP families")]
    FamilyMismatch { peer: IpAddr, local: IpAddr },
    #[error("query param '{key}' has invalid value '{value}': {detail}")]
    BadQueryValue {
        key: String,
        value: String,
        detail: String,
    },
    /// `?pkt_size=` supplied on a receive-side URL. Send-side only since
    /// the recv-ceiling change: the receive buffer always accepts any
    /// legal datagram (65535 ceiling).
    #[error(
        "pkt_size is a send-side knob; receive buffers size to the transport's deliverable ceiling automatically — remove ?pkt_size= from receiver URLs"
    )]
    RecvPktSize,
    #[error("URL parse failed: {0}")]
    Parse(#[from] tst_core::url::common::UrlError),
}

impl UdpUrl {
    /// Parse a `udp://...` URL into the structured form.
    ///
    /// The host may be an IP literal or a DNS hostname. Non-literal
    /// hosts are resolved here via the system resolver
    /// ([`std::net::ToSocketAddrs`]); among multiple results, IPv4 is
    /// preferred among the candidates that probe clean (see the internal
    /// `resolve_host` doc comment for the full tiebreak rationale). The
    /// `?localaddr=` and IPv4 `?iface=` values stay literal-only
    /// (resolving a local NIC selector through DNS is meaningless).
    pub fn parse(s: &str) -> Result<Self, UdpUrlError> {
        use tst_core::url::common::parse_url;

        let parsed = parse_url(s)?;
        if parsed.scheme != "udp" {
            return Err(UdpUrlError::BadScheme(parsed.scheme.to_string()));
        }
        let port = parsed.port.ok_or(UdpUrlError::MissingPort)?;

        // ffmpeg `@` prefix on host means "bind on this address (recv-side)".
        // `parse_url` treats `@` as the userinfo separator, so `udp://@host:port`
        // arrives here as `username = Some("")` + `host = "host"`. The presence
        // of a (possibly empty) username signals recv-bind intent.
        let recv_bind = parsed.username.is_some();
        let host_str = parsed.host;

        let mut iface = None;
        let mut ttl = None;
        let mut tos = None;
        let mut rcvbuf = None;
        let mut sndbuf = None;
        let mut pkt_size = None;
        let mut localaddr = None;

        for (k, v) in &parsed.query {
            let key = k.as_ref();
            let value = v.as_ref();
            match key {
                "iface" => iface = Some(value.to_string()),
                "ttl" => ttl = Some(parse_u8_dec(key, value)?),
                "tos" => tos = Some(parse_u8_hex_or_dec(key, value)?),
                "rcvbuf" => rcvbuf = Some(parse_byte_size(key, value)?),
                "sndbuf" => sndbuf = Some(parse_byte_size(key, value)?),
                "pkt_size" => pkt_size = Some(parse_byte_size(key, value)?),
                "localaddr" => {
                    let a: IpAddr = value.parse().map_err(|e: std::net::AddrParseError| {
                        UdpUrlError::BadQueryValue {
                            key: key.to_string(),
                            value: value.to_string(),
                            detail: e.to_string(),
                        }
                    })?;
                    localaddr = Some(a);
                }
                _ => { /* unknown params silently ignored — matches ffmpeg behavior */ }
            }
        }

        // IP literals stay a pure parse; anything else is treated as a
        // hostname and resolved via the system resolver — matching
        // tst-srt and tst-tcp, which both accept hostnames (containerized
        // consumers address peers by service name). Multicast
        // classification runs on the resolved address either way; group
        // addresses are conventionally written as literals.
        //
        // Resolution runs AFTER the query loop on purpose: an explicit
        // `?localaddr=` constrains which address family a resolved
        // candidate may have (a socket bound to one family cannot send to
        // the other), and a literal peer of the wrong family is rejected
        // here rather than failing the first send with an opaque OS error.
        let addr: IpAddr = match host_str.parse::<IpAddr>() {
            Ok(a) => {
                if let Some(la) = localaddr {
                    if a.is_ipv4() != la.is_ipv4() {
                        return Err(UdpUrlError::FamilyMismatch { peer: a, local: la });
                    }
                }
                a
            }
            Err(_) => resolve_host(host_str, port, localaddr.map(|a| a.is_ipv4()))?,
        };

        Ok(Self {
            addr,
            port,
            recv_bind,
            iface,
            ttl,
            tos,
            rcvbuf,
            sndbuf,
            pkt_size,
            localaddr,
        })
    }

    /// True if `addr` is in 224.0.0.0/4 or ff00::/8.
    pub fn is_multicast(&self) -> bool {
        match self.addr {
            IpAddr::V4(v4) => v4.is_multicast(),
            IpAddr::V6(v6) => v6.is_multicast(),
        }
    }
}

/// Resolve a non-literal host via the system resolver and pick a usable
/// address.
///
/// tst-tcp and tst-srt walk every resolved address and keep the first
/// successful *connection* — connection feedback UDP does not have. The
/// UDP equivalent: probe each candidate with a local socket
/// `bind` + `connect` (no packets are sent; a UDP `connect` only sets the
/// default destination), which fails fast for an address family that is
/// unconfigured or unroutable on this host (e.g. an AAAA record arriving
/// first while IPv6 is disabled). The probe only rejects unconfigured or
/// unroutable families though — it cannot detect an absent listener — so
/// among probe-clean candidates IPv4 is preferred (the dual-stack
/// `localhost` trap: `[::1, 127.0.0.1]` both probe clean, but picking
/// `::1` dies against an IPv4-only listener; this matches the dominant
/// TS-over-UDP tooling). If every probe fails, fall back to the first
/// resolved address so the real send path surfaces the OS error — never
/// worse than not probing at all.
fn resolve_host(
    host: &str,
    port: u16,
    // `Some(true)` = only IPv4 candidates are acceptable, `Some(false)` =
    // only IPv6 — set when `?localaddr=` pins the local family. `None` =
    // unconstrained (the documented IPv4-preference tiebreak applies).
    required_v4: Option<bool>,
) -> Result<IpAddr, UdpUrlError> {
    use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};

    let mut candidates: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| UdpUrlError::HostResolve {
            host: host.to_string(),
            detail: e.to_string(),
        })?
        .collect();
    if let Some(v4) = required_v4 {
        let unfiltered = candidates.len();
        candidates.retain(|sa| sa.is_ipv4() == v4);
        if candidates.is_empty() && unfiltered > 0 {
            return Err(UdpUrlError::HostResolve {
                host: host.to_string(),
                detail: format!(
                    "resolved to no {} addresses ({} candidate(s) of the \
                     other family) — required by the ?localaddr= family",
                    if v4 { "IPv4" } else { "IPv6" },
                    unfiltered
                ),
            });
        }
    }
    let first = candidates
        .first()
        .copied()
        .ok_or_else(|| UdpUrlError::HostResolve {
            host: host.to_string(),
            detail: "resolved to no addresses".to_string(),
        })?;
    let mut first_clean: Option<SocketAddr> = None;
    for sa in &candidates {
        let unspec: SocketAddr = if sa.is_ipv4() {
            (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
        };
        let Ok(probe) = UdpSocket::bind(unspec) else {
            continue;
        };
        if probe.connect(sa).is_ok() {
            if sa.is_ipv4() {
                return Ok(sa.ip()); // documented preference: first clean IPv4
            }
            first_clean.get_or_insert(*sa);
        }
    }
    if let Some(sa) = first_clean {
        return Ok(sa.ip());
    }
    Ok(first.ip())
}

fn parse_u8_hex_or_dec(key: &str, value: &str) -> Result<u8, UdpUrlError> {
    let v = if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u8::from_str_radix(hex, 16)
    } else {
        value.parse::<u8>()
    };
    v.map_err(|e| UdpUrlError::BadQueryValue {
        key: key.to_string(),
        value: value.to_string(),
        detail: e.to_string(),
    })
}

fn parse_u8_dec(key: &str, value: &str) -> Result<u8, UdpUrlError> {
    tst_core::url::common::parse_int_query(value).map_err(|detail| UdpUrlError::BadQueryValue {
        key: key.to_string(),
        value: value.to_string(),
        detail,
    })
}

fn parse_byte_size(key: &str, value: &str) -> Result<usize, UdpUrlError> {
    tst_core::url::common::parse_byte_size(value).map_err(|detail| UdpUrlError::BadQueryValue {
        key: key.to_string(),
        value: value.to_string(),
        detail,
    })
}

#[cfg(test)]
mod tests {
    /// `?localaddr=` and the peer must agree on IP family. Literal
    /// mismatches are typed parse errors; hostname resolution filters
    /// candidates to the local family (resolving IPv4-first before reading
    /// localaddr would bind an IPv6 socket that can never send to the
    /// chosen IPv4 peer).
    #[test]
    fn localaddr_family_mismatch_literals_rejected() {
        let e = UdpUrl::parse("udp://239.0.0.1:5000?localaddr=::1").unwrap_err();
        assert!(matches!(e, UdpUrlError::FamilyMismatch { .. }), "got {e:?}");
        let e = UdpUrl::parse("udp://[ff02::1]:5000?localaddr=127.0.0.1").unwrap_err();
        assert!(matches!(e, UdpUrlError::FamilyMismatch { .. }), "got {e:?}");
    }

    #[test]
    fn localaddr_v4_constrains_hostname_resolution_to_v4() {
        // localhost always resolves 127.0.0.1; with an IPv4 localaddr the
        // chosen peer MUST be IPv4 regardless of AAAA ordering.
        let u = UdpUrl::parse("udp://localhost:5000?localaddr=127.0.0.1").unwrap();
        assert!(u.addr.is_ipv4(), "got {:?}", u.addr);
    }

    #[test]
    fn localaddr_v6_never_yields_v4_peer() {
        // Environment-tolerant: hosts without an ::1 mapping for
        // localhost legitimately fail resolution with the
        // no-candidates-of-family detail — what must NEVER happen is the
        // old bug's outcome, an IPv4 peer paired with an IPv6 localaddr.
        match UdpUrl::parse("udp://localhost:5000?localaddr=::1") {
            Ok(u) => assert!(
                !u.addr.is_ipv4(),
                "IPv4 peer with IPv6 localaddr: {:?}",
                u.addr
            ),
            Err(UdpUrlError::HostResolve { detail, .. }) => {
                assert!(detail.contains("IPv6"), "unexpected detail: {detail}");
            }
            Err(e) => panic!("unexpected error kind: {e:?}"),
        }
    }

    use super::*;

    #[test]
    fn unicast_send() {
        let u = UdpUrl::parse("udp://192.168.1.5:5004").unwrap();
        assert!(!u.recv_bind);
        assert!(!u.is_multicast());
        assert_eq!(u.port, 5004);
    }

    #[test]
    fn multicast_send_v4() {
        let u = UdpUrl::parse("udp://239.10.0.1:5004").unwrap();
        assert!(!u.recv_bind);
        assert!(u.is_multicast());
        assert_eq!(u.port, 5004);
    }

    #[test]
    fn multicast_recv_v4_with_at_prefix() {
        let u = UdpUrl::parse("udp://@239.10.0.1:5004").unwrap();
        assert!(u.recv_bind);
        assert!(u.is_multicast());
    }

    #[test]
    fn unicast_recv_with_at_prefix() {
        let u = UdpUrl::parse("udp://@0.0.0.0:5004").unwrap();
        assert!(u.recv_bind);
        assert!(!u.is_multicast());
    }

    #[test]
    fn multicast_v6_with_query() {
        let u = UdpUrl::parse("udp://[ff0e::1]:5004?iface=eth0&ttl=8").unwrap();
        assert!(u.is_multicast());
        assert_eq!(u.iface.as_deref(), Some("eth0"));
        assert_eq!(u.ttl, Some(8));
    }

    #[test]
    fn knobs_pkt_size_tos_bufs_hex_suffix() {
        let u = UdpUrl::parse("udp://239.10.0.1:5004?pkt_size=1316&tos=0xb8&sndbuf=2M&rcvbuf=8M")
            .unwrap();
        assert_eq!(u.pkt_size, Some(1316));
        assert_eq!(u.tos, Some(0xb8));
        assert_eq!(u.sndbuf, Some(2 * 1024 * 1024));
        assert_eq!(u.rcvbuf, Some(8 * 1024 * 1024));
    }

    #[test]
    fn byte_size_suffix_overflow_is_rejected_not_panic() {
        // The suffix multiply (n * 1024*1024 for "M") must not panic in debug
        // or wrap in release. A value this large overflows usize before any
        // ceiling check.
        let err = UdpUrl::parse("udp://239.10.0.1:5004?pkt_size=999999999999999999999M")
            .expect_err("overflowing pkt_size must be rejected");
        assert!(matches!(err, UdpUrlError::BadQueryValue { .. }));
    }

    #[test]
    fn byte_size_enormous_but_valid_is_rejected_by_bound() {
        // 999999M ≈ 1 TiB: valid digits + recognized suffix, but absurd;
        // the transport upper bound (256 MiB) rejects it.
        let err = UdpUrl::parse("udp://239.10.0.1:5004?pkt_size=999999M")
            .expect_err("absurd pkt_size must exceed the byte-size ceiling");
        match err {
            UdpUrlError::BadQueryValue { detail, .. } => {
                assert!(detail.contains("exceeds maximum"), "detail: {detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn byte_size_at_ceiling_is_accepted() {
        let u = UdpUrl::parse("udp://239.10.0.1:5004?rcvbuf=256M").unwrap();
        assert_eq!(u.rcvbuf, Some(256 * 1024 * 1024));
    }

    #[test]
    fn cited_adversarial_input_returns_err() {
        // The exact adversarial fixture: `999999999999G`. `G` is not a
        // recognized suffix, so this is rejected at digit-parse — the key
        // property is Err, never panic/wrap.
        assert!(UdpUrl::parse("udp://239.10.0.1:5004?pkt_size=999999999999G").is_err());
    }

    #[test]
    fn hostname_resolves_via_system_resolver() {
        // `localhost` resolves from the hosts file — hermetic, no DNS
        // traffic — and must yield a loopback address whichever family
        // the resolver prefers.
        let u = UdpUrl::parse("udp://localhost:5004").unwrap();
        assert!(
            u.addr.is_loopback(),
            "resolved localhost must be loopback, got {}",
            u.addr
        );
        assert_eq!(u.port, 5004);
        assert!(!u.recv_bind);
    }

    /// `localhost` resolves `[::1, 127.0.0.1]` on dual-stack
    /// hosts and a UDP connect-probe cannot detect an absent listener, so
    /// without an explicit preference the sender picks `::1` and dies
    /// against an IPv4-only listener. Documented preference: IPv4 first
    /// among probe-clean candidates.
    #[test]
    fn hostname_resolution_prefers_ipv4() {
        // IPv6-only hosts have no IPv4 localhost record to prefer — the
        // contract is "IPv4 first when present", not "IPv4 required".
        use std::net::ToSocketAddrs;
        let has_ipv4 = ("localhost", 5004u16)
            .to_socket_addrs()
            .map(|mut addrs| addrs.any(|a| a.is_ipv4()))
            .unwrap_or(false);
        if !has_ipv4 {
            eprintln!(
                "skipping hostname_resolution_prefers_ipv4: host has no IPv4 localhost record"
            );
            return;
        }

        let u = UdpUrl::parse("udp://localhost:5004").unwrap();
        assert!(
            u.addr.is_ipv4(),
            "expected IPv4-first for localhost, got {}",
            u.addr
        );
    }

    #[test]
    fn hostname_recv_bind_resolves() {
        let u = UdpUrl::parse("udp://@localhost:5004").unwrap();
        assert!(u.recv_bind);
        assert!(u.addr.is_loopback());
    }

    #[test]
    fn unresolvable_hostname_reports_resolve_error() {
        // RFC 6761 reserves `.invalid` — guaranteed non-resolvable, so
        // this exercises the resolution-failure path deterministically.
        let err = UdpUrl::parse("udp://gimbal.invalid:5004").expect_err("must not resolve");
        match err {
            UdpUrlError::HostResolve { host, .. } => assert_eq!(host, "gimbal.invalid"),
            other => panic!("expected HostResolve, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_scheme() {
        assert!(matches!(
            UdpUrl::parse("https://host:5004"),
            Err(UdpUrlError::BadScheme(_))
        ));
    }

    #[test]
    fn rejects_missing_port() {
        assert!(matches!(
            UdpUrl::parse("udp://239.10.0.1"),
            Err(UdpUrlError::MissingPort)
        ));
    }
}
