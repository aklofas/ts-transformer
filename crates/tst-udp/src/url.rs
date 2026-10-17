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

use tst_core::url::UrlError;
use tst_core::url::common::{parse_int, parse_size, parse_url};
use tst_core::url::query::{QueryKey, UnknownKeys, apply_query};

use crate::config::UdpConfig;

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
    /// The socket knobs the URL's query carried; every other field is
    /// `None`. `pkt_size` is rejected on a receive-side URL by the
    /// receiver's entry point.
    pub config: UdpConfig,
}

type Applied = Result<(), UrlError>;

fn set_iface(c: &mut UdpConfig, _key: &str, value: &str) -> Applied {
    c.iface = Some(value.to_string());
    Ok(())
}

fn set_ttl(c: &mut UdpConfig, key: &str, value: &str) -> Applied {
    c.ttl = Some(parse_int(key, value)?);
    Ok(())
}

fn set_tos(c: &mut UdpConfig, key: &str, value: &str) -> Applied {
    let parsed = if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u8::from_str_radix(hex, 16)
    } else {
        value.parse::<u8>()
    };
    c.tos = Some(parsed.map_err(|e| UrlError::BadQueryValue {
        key: key.to_string(),
        value: value.to_string(),
        detail: e.to_string(),
    })?);
    Ok(())
}

fn set_rcvbuf(c: &mut UdpConfig, key: &str, value: &str) -> Applied {
    c.rcvbuf = Some(parse_size(key, value)?);
    Ok(())
}

fn set_sndbuf(c: &mut UdpConfig, key: &str, value: &str) -> Applied {
    c.sndbuf = Some(parse_size(key, value)?);
    Ok(())
}

fn set_pkt_size(c: &mut UdpConfig, key: &str, value: &str) -> Applied {
    c.pkt_size = Some(parse_size(key, value)?);
    Ok(())
}

fn set_localaddr(c: &mut UdpConfig, key: &str, value: &str) -> Applied {
    let addr: IpAddr =
        value
            .parse()
            .map_err(|e: std::net::AddrParseError| UrlError::BadQueryValue {
                key: key.to_string(),
                value: value.to_string(),
                detail: e.to_string(),
            })?;
    c.localaddr = Some(addr);
    Ok(())
}

/// The query keys a `udp://` URL accepts; the parser and the generated
/// key documentation both read this table.
#[doc(hidden)]
pub const URL_KEYS: &[QueryKey<UdpConfig>] = &[
    QueryKey::new(
        "iface",
        &[],
        "Network interface for multicast and egress",
        "interface name or literal IP address",
        set_iface,
    ),
    QueryKey::new(
        "localaddr",
        &[],
        "Local address to bind",
        "IP address",
        set_localaddr,
    ),
    QueryKey::new(
        "ttl",
        &[],
        "Multicast hop limit",
        "decimal 0 to 255",
        set_ttl,
    ),
    QueryKey::new(
        "tos",
        &[],
        "IP type of service",
        "decimal, or hex with a `0x` prefix, 0 to 255",
        set_tos,
    ),
    QueryKey::new(
        "rcvbuf",
        &[],
        "Kernel socket buffer size",
        "bytes; `K` and `M` suffixes are 1024-based; at most 256 MiB",
        set_rcvbuf,
    ),
    QueryKey::new(
        "sndbuf",
        &[],
        "Kernel socket buffer size",
        "bytes; `K` and `M` suffixes are 1024-based; at most 256 MiB",
        set_sndbuf,
    ),
    QueryKey::new(
        "pkt_size",
        &[],
        "Datagram payload size",
        "bytes with the `K`/`M` forms",
        set_pkt_size,
    ),
];

/// The refusal for a receive-side URL that names `?pkt_size=`.
pub(crate) fn recv_pkt_size_rejection() -> UrlError {
    UrlError::Rejected(
        "pkt_size is a send-side knob; receive buffers size to the transport's deliverable ceiling automatically — remove ?pkt_size= from receiver URLs".into(),
    )
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
    pub fn parse(s: &str) -> Result<Self, UrlError> {
        let parsed =
            parse_url(s).map_err(|e| UrlError::Rejected(format!("URL parse failed: {e}")))?;
        if parsed.scheme != "udp" {
            return Err(UrlError::BadScheme {
                expected: "'udp'",
                got: parsed.scheme.to_string(),
            });
        }
        let port = parsed.port.ok_or(UrlError::MissingPort)?;

        // ffmpeg `@` prefix on host means "bind on this address (recv-side)".
        // `parse_url` treats `@` as the userinfo separator, so `udp://@host:port`
        // arrives here as `username = Some("")` + `host = "host"`. The presence
        // of a (possibly empty) username signals recv-bind intent.
        let recv_bind = parsed.username.is_some();
        let host_str = parsed.host;

        // Unknown params are silently ignored — matches ffmpeg behavior.
        let mut config = UdpConfig::default();
        apply_query(URL_KEYS, &mut config, &parsed.query, UnknownKeys::Ignore)?;
        let localaddr = config.localaddr;

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
                        return Err(UrlError::Rejected(format!(
                            "peer address {a} and ?localaddr={la} are different IP families"
                        )));
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
            config,
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
) -> Result<IpAddr, UrlError> {
    use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};

    let mut candidates: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| UrlError::HostResolve {
            host: host.to_string(),
            detail: e.to_string(),
        })?
        .collect();
    if let Some(v4) = required_v4 {
        let unfiltered = candidates.len();
        candidates.retain(|sa| sa.is_ipv4() == v4);
        if candidates.is_empty() && unfiltered > 0 {
            return Err(UrlError::HostResolve {
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
        .ok_or_else(|| UrlError::HostResolve {
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
        assert!(
            matches!(&e, UrlError::Rejected(m) if m.contains("different IP families")),
            "got {e:?}"
        );
        let e = UdpUrl::parse("udp://[ff02::1]:5000?localaddr=127.0.0.1").unwrap_err();
        assert!(
            matches!(&e, UrlError::Rejected(m) if m.contains("different IP families")),
            "got {e:?}"
        );
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
            Err(UrlError::HostResolve { detail, .. }) => {
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
        assert_eq!(u.config.iface.as_deref(), Some("eth0"));
        assert_eq!(u.config.ttl, Some(8));
    }

    #[test]
    fn knobs_pkt_size_tos_bufs_hex_suffix() {
        let u = UdpUrl::parse("udp://239.10.0.1:5004?pkt_size=1316&tos=0xb8&sndbuf=2M&rcvbuf=8M")
            .unwrap();
        assert_eq!(u.config.pkt_size, Some(1316));
        assert_eq!(u.config.tos, Some(0xb8));
        assert_eq!(u.config.sndbuf, Some(2 * 1024 * 1024));
        assert_eq!(u.config.rcvbuf, Some(8 * 1024 * 1024));
    }

    #[test]
    fn byte_size_suffix_overflow_is_rejected_not_panic() {
        // The suffix multiply (n * 1024*1024 for "M") must not panic in debug
        // or wrap in release. A value this large overflows usize before any
        // ceiling check.
        let err = UdpUrl::parse("udp://239.10.0.1:5004?pkt_size=999999999999999999999M")
            .expect_err("overflowing pkt_size must be rejected");
        assert!(matches!(err, UrlError::BadQueryValue { .. }));
    }

    #[test]
    fn byte_size_enormous_but_valid_is_rejected_by_bound() {
        // 999999M ≈ 1 TiB: valid digits + recognized suffix, but absurd;
        // the transport upper bound (256 MiB) rejects it.
        let err = UdpUrl::parse("udp://239.10.0.1:5004?pkt_size=999999M")
            .expect_err("absurd pkt_size must exceed the byte-size ceiling");
        match err {
            UrlError::BadQueryValue { detail, .. } => {
                assert!(detail.contains("exceeds maximum"), "detail: {detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn byte_size_at_ceiling_is_accepted() {
        let u = UdpUrl::parse("udp://239.10.0.1:5004?rcvbuf=256M").unwrap();
        assert_eq!(u.config.rcvbuf, Some(256 * 1024 * 1024));
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
            UrlError::HostResolve { host, .. } => assert_eq!(host, "gimbal.invalid"),
            other => panic!("expected HostResolve, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_scheme() {
        assert!(matches!(
            UdpUrl::parse("https://host:5004"),
            Err(UrlError::BadScheme { .. })
        ));
    }

    #[test]
    fn rejects_missing_port() {
        assert!(matches!(
            UdpUrl::parse("udp://239.10.0.1"),
            Err(UrlError::MissingPort)
        ));
    }
}
