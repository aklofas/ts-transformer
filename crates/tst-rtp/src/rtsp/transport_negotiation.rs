//! UDP-first SETUP attempt with auto-fallback to TCP-interleaved on 461.
//!
//! Trigger for fallback: server response code 461 Unsupported Transport
//! (RFC 7826 §17.4.6). Any other 4xx/5xx surfaces as
//! `RtspError::Protocol { code, reason }`.
//!
//! `?transport=udp` in the URL skips fallback; `?transport=tcp` skips
//! the UDP attempt entirely.

use std::net::{IpAddr, SocketAddr, UdpSocket};

use crate::error::RtspError;
use crate::url::RtspTransportPref;

/// What kind of transport was negotiated by SETUP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RtspTransportKind {
    /// Plain UDP unicast: RTP on one even port, RTCP on the next odd port.
    Udp,
    /// TCP-interleaved framing per RFC 7826 §14 over the RTSP control
    /// connection itself.
    TcpInterleaved,
}

/// Parsed `Transport:` response header.
#[derive(Debug, Clone)]
pub(crate) struct TransportResponse {
    /// Which transport the server actually accepted.
    pub kind: RtspTransportKind,
    /// For UDP: client's RTP+RTCP port range from `client_port=lo-hi`.
    /// Server-side parses this from the client's SETUP request; client-side
    /// parsers leave this as `None` (the client knew its own ports).
    #[cfg(feature = "rtsp-server")]
    pub client_port: Option<(u16, u16)>,
    /// For TCP-interleaved: channel range from `interleaved=lo-hi`.
    pub interleaved: Option<(u8, u8)>,
    /// `mode=` parameter, lowercased and with surrounding quotes
    /// stripped (ffmpeg sends `mode=record`; RFC 2326 shows
    /// `mode="PLAY"`). `None` when absent — a reader's SETUP omits it.
    #[cfg(feature = "rtsp-server")]
    pub mode: Option<String>,
}

/// Parse a `Transport:` header value (server response OR client SETUP
/// request — the syntax is shared per RFC 7826 §18.54).
///
/// Recognized keys: `client_port=`, `server_port=` (validated, not kept),
/// `interleaved=`, `mode=`. The kind is inferred from the presence of
/// `RTP/AVP/TCP` or `interleaved=`.
///
/// Parsing is **strict** and parameter names are matched
/// **case-insensitively** (RFC 7826 §18.54: Transport parameter names are
/// case-insensitive). A present-but-malformed port/channel pair —
/// non-numeric, out-of-range, reversed (`hi < lo`), or a single value
/// whose fabricated companion would overflow — is rejected with
/// [`RtspError::UnsupportedTransport`] rather than silently mapped to `0`
/// or wrapped. A *missing* `client_port`/`server_port`/`interleaved` key
/// is left as `None` (the caller decides whether it is required for the
/// chosen transport).
///
/// # Errors
///
/// [`RtspError::UnsupportedTransport`] if any present port/channel pair is
/// malformed.
pub(crate) fn parse_transport_response(header_value: &str) -> Result<TransportResponse, RtspError> {
    let lower = header_value.to_ascii_lowercase();
    let kind = if lower.contains("rtp/avp/tcp") || lower.contains("interleaved=") {
        RtspTransportKind::TcpInterleaved
    } else {
        RtspTransportKind::Udp
    };
    #[cfg(feature = "rtsp-server")]
    let mut client_port = None;
    let mut interleaved = None;
    #[cfg(feature = "rtsp-server")]
    let mut mode = None;
    for part in header_value.split(';') {
        let part = part.trim();
        // Split the `key=value` once; match the key case-insensitively
        // per RFC 7826 §18.54 while preserving the original value casing.
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            // Validated (a malformed pair rejects the SETUP) but not kept:
            // nothing reads the server's ports.
            "server_port" => {
                parse_u16_pair(value)?;
            }
            // Validated on every build; kept only where the server reads it.
            "client_port" => {
                let _pair = parse_u16_pair(value)?;
                #[cfg(feature = "rtsp-server")]
                {
                    client_port = Some(_pair);
                }
            }
            "interleaved" => interleaved = Some(parse_u8_pair(value)?),
            // Quotes stripped (RFC 2326 shows `mode="PLAY"`; ffmpeg sends
            // the bare `mode=record`), lowercased so callers can compare
            // against the one case-normalized literal "record".
            #[cfg(feature = "rtsp-server")]
            "mode" => mode = Some(value.trim_matches('"').to_ascii_lowercase()),
            _ => {}
        }
    }
    Ok(TransportResponse {
        kind,
        #[cfg(feature = "rtsp-server")]
        client_port,
        interleaved,
        #[cfg(feature = "rtsp-server")]
        mode,
    })
}

/// Parse a strict `lo-hi` `u16` port range. Rejects non-numeric,
/// out-of-range, reversed (`hi < lo`), and single-value forms whose
/// fabricated companion (`lo+1`) would overflow.
///
/// The caller-facing error stays the bare [`RtspError::UnsupportedTransport`]
/// (wire behavior is a plain rejection); the specific failed check is
/// reported via a `debug!`-level log line so a rejected SETUP can be
/// diagnosed without a packet capture.
fn parse_u16_pair(value: &str) -> Result<(u16, u16), RtspError> {
    parse_u16_pair_detail(value).map_err(|reason| {
        // Debug-capture (`?value`): the range comes off the wire, so
        // control characters must be escaped, not passed through to logs.
        tracing::debug!(
            target: "tst_rtp::rtsp::transport_negotiation",
            ?value,
            reason,
            "rejecting Transport port range"
        );
        RtspError::UnsupportedTransport
    })
}

/// [`parse_u16_pair`] with the failed check named in the error.
fn parse_u16_pair_detail(value: &str) -> Result<(u16, u16), &'static str> {
    let mut it = value.split('-');
    let lo: u16 = it
        .next()
        .and_then(|s| s.trim().parse().ok())
        .ok_or("port is not an integer in 0-65535")?;
    let hi: u16 = match it.next() {
        Some(s) => s
            .trim()
            .parse()
            .map_err(|_| "port is not an integer in 0-65535")?,
        // No explicit hi: derive the RTCP companion, rejecting overflow.
        None => lo
            .checked_add(1)
            .ok_or("single-port form has no valid companion (port+1 overflows 65535)")?,
    };
    if it.next().is_some() {
        return Err("more than two ports in range");
    }
    if hi < lo {
        return Err("pair must be ascending (lo-hi)");
    }
    Ok((lo, hi))
}

/// Parse a strict `lo-hi` `u8` interleaved-channel range. Same rejection
/// rules (and logging) as [`parse_u16_pair`], scoped to `u8`.
fn parse_u8_pair(value: &str) -> Result<(u8, u8), RtspError> {
    parse_u8_pair_detail(value).map_err(|reason| {
        tracing::debug!(
            target: "tst_rtp::rtsp::transport_negotiation",
            ?value,
            reason,
            "rejecting Transport channel range"
        );
        RtspError::UnsupportedTransport
    })
}

/// [`parse_u8_pair`] with the failed check named in the error.
fn parse_u8_pair_detail(value: &str) -> Result<(u8, u8), &'static str> {
    let mut it = value.split('-');
    let lo: u8 = it
        .next()
        .and_then(|s| s.trim().parse().ok())
        .ok_or("channel is not an integer in 0-255")?;
    let hi: u8 = match it.next() {
        Some(s) => s
            .trim()
            .parse()
            .map_err(|_| "channel is not an integer in 0-255")?,
        None => lo
            .checked_add(1)
            .ok_or("single-channel form has no valid companion (channel+1 overflows 255)")?,
    };
    if it.next().is_some() {
        return Err("more than two channels in range");
    }
    if hi < lo {
        return Err("pair must be ascending (lo-hi)");
    }
    Ok((lo, hi))
}

/// The RTP port attempt `attempt` asks for: port 0 (a fresh kernel pick)
/// every time when `start` is 0, else `start + 2 * attempt`, or `None` once
/// that would overflow `u16`.
fn candidate_port(start: u16, attempt: u16) -> Option<u16> {
    if start == 0 {
        return Some(0);
    }
    attempt
        .checked_mul(2)
        .and_then(|off| start.checked_add(off))
}

/// Bind a UDP RTP+RTCP port pair on `bind_ip`; the RTCP socket sits on
/// RTP+1 (RFC 3550 §11).
///
/// Returns `(rtp_socket, rtcp_socket, rtp_port)`. With `start == 0` the
/// kernel picks the RTP port and up to 16 fresh picks are tried until the
/// neighbouring odd port is free (the server's allocation). With a
/// non-zero `start` up to 8 candidate even-port pairs `start`, `start + 2`,
/// … are tried and the first pair where both ports bind cleanly is returned
/// (the client's allocation).
///
/// # Errors
///
/// [`RtspError::UnsupportedTransport`] if `start` is so close to
/// `u16::MAX` that no candidate pair fits without overflowing (the
/// companion RTCP port is always RTP+1).
/// [`RtspError::Io`] with `AddrInUse` if every candidate pair fails to bind,
/// or the kernel-picked ports never left room for the companion.
pub(crate) fn bind_udp_pair(
    bind_ip: IpAddr,
    start: u16,
) -> Result<(UdpSocket, UdpSocket, u16), RtspError> {
    let kernel_picks = start == 0;
    let attempts: u16 = if kernel_picks { 16 } else { 8 };
    let mut bound_overflow = false;
    for attempt in 0..attempts {
        // Companion port+1 is checked below, and the candidate here, so a
        // `start` near 65535 returns a typed error instead of wrapping.
        let Some(want) = candidate_port(start, attempt) else {
            bound_overflow = true;
            break;
        };
        let Ok(rtp) = UdpSocket::bind(SocketAddr::new(bind_ip, want)) else {
            continue;
        };
        let Ok(port) = rtp.local_addr().map(|a| a.port()) else {
            continue;
        };
        let Some(rtcp_port) = port.checked_add(1) else {
            // A kernel pick can land on 65535; the next pick will not.
            // A fixed start has no further candidate that fits.
            if kernel_picks {
                continue;
            }
            bound_overflow = true;
            break;
        };
        if let Ok(rtcp) = UdpSocket::bind(SocketAddr::new(bind_ip, rtcp_port)) {
            return Ok((rtp, rtcp, port));
        }
    }
    if bound_overflow {
        return Err(RtspError::UnsupportedTransport);
    }
    Err(RtspError::Io(std::io::ErrorKind::AddrInUse))
}

/// Build the outgoing `Transport:` header value per the URL's
/// `transport_preference`.
///
/// `PreferUdp` and `ForceUdp` both emit a UDP request; the difference is
/// the fallback policy in [`super::setup`].
///
/// # Errors
///
/// [`RtspError::UnsupportedTransport`] for a UDP request when `rtp_port`
/// is `u16::MAX` (the RTCP companion port `rtp_port + 1` would overflow
/// `u16`).
pub(crate) fn build_transport_request(
    pref: RtspTransportPref,
    rtp_port: u16,
) -> Result<String, RtspError> {
    match pref {
        RtspTransportPref::PreferUdp | RtspTransportPref::ForceUdp => {
            let rtcp_port = rtp_port
                .checked_add(1)
                .ok_or(RtspError::UnsupportedTransport)?;
            Ok(format!(
                "RTP/AVP;unicast;client_port={rtp_port}-{rtcp_port}"
            ))
        }
        RtspTransportPref::ForceTcp => Ok("RTP/AVP/TCP;unicast;interleaved=0-1".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn parse_udp_transport_response() {
        let h = "RTP/AVP;unicast;client_port=5004-5005;server_port=6970-6971;ssrc=0DEADBEEF";
        let t = parse_transport_response(h).unwrap();
        assert_eq!(t.kind, RtspTransportKind::Udp);
        #[cfg(feature = "rtsp-server")]
        assert_eq!(t.client_port, Some((5004, 5005)));
    }

    #[test]
    fn parse_interleaved_transport_response() {
        let h = "RTP/AVP/TCP;unicast;interleaved=0-1";
        let t = parse_transport_response(h).unwrap();
        assert_eq!(t.kind, RtspTransportKind::TcpInterleaved);
        assert_eq!(t.interleaved, Some((0, 1)));
    }

    #[cfg(feature = "rtsp-server")]
    #[test]
    fn mode_record_is_parsed_case_insensitively() {
        let t =
            parse_transport_response("RTP/AVP/TCP;unicast;interleaved=0-1;MODE=RECORD").unwrap();
        assert_eq!(t.mode.as_deref(), Some("record"));
        // RFC 2326 §12.39 example form: quoted value.
        let t2 = parse_transport_response("RTP/AVP;unicast;client_port=1-2;mode=\"PLAY\"").unwrap();
        assert_eq!(t2.mode.as_deref(), Some("play"));
        // Absent — a reader's SETUP omits it.
        let t3 = parse_transport_response("RTP/AVP/TCP;unicast;interleaved=0-1").unwrap();
        assert_eq!(t3.mode, None);
    }

    #[test]
    fn build_udp_request_includes_port_pair() {
        let h = build_transport_request(RtspTransportPref::PreferUdp, 8000).unwrap();
        assert!(h.contains("client_port=8000-8001"));
    }

    #[test]
    fn build_tcp_request_uses_interleaved_0_1() {
        let h = build_transport_request(RtspTransportPref::ForceTcp, 0).unwrap();
        assert!(h.contains("interleaved=0-1"));
    }

    // --- Adversarial Transport endpoint parsing / port-overflow tests ---

    #[test]
    fn build_udp_request_rejects_max_port_companion_overflow() {
        // rtp_port == 65535 → companion 65536 overflows u16. Must Err,
        // not wrap to 0 or panic.
        let r = build_transport_request(RtspTransportPref::ForceUdp, 65535);
        assert!(r.is_err(), "65535 companion overflow must be an error");
    }

    /// A kernel-picked pair asks the kernel afresh on every attempt; it
    /// never walks fixed low ports. (Walking `0 + 2k` would bind ports 2, 4,
    /// ... instead of retrying.)
    #[test]
    fn a_kernel_picked_pair_retries_with_port_zero() {
        for attempt in 0..16 {
            assert_eq!(candidate_port(0, attempt), Some(0), "attempt {attempt}");
        }
        let (rtp, rtcp, port) = bind_udp_pair(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).unwrap();
        assert!(
            port >= 1024,
            "a fresh kernel pick, not a fixed low port: {port}"
        );
        assert_eq!(rtp.local_addr().unwrap().port(), port);
        assert_eq!(rtcp.local_addr().unwrap().port(), port + 1);
    }

    /// A fixed start walks `start + 2k`, skipping a pair whose RTP port is
    /// taken.
    #[test]
    fn a_fixed_start_walks_even_candidates() {
        assert_eq!(candidate_port(5004, 0), Some(5004));
        assert_eq!(candidate_port(5004, 3), Some(5010));
        assert_eq!(candidate_port(65534, 1), None);
        let any = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        let (a, b, first) = bind_udp_pair(any, 40_000).unwrap();
        // 40 000 is now taken (or was already): the next call moves on by 2k.
        let (_c, _d, second) = bind_udp_pair(any, first).unwrap();
        assert!(
            second >= first + 2 && (second - first) % 2 == 0,
            "{first} -> {second}"
        );
        drop((a, b));
    }

    #[test]
    fn bind_udp_pair_rejects_start_at_max_port() {
        // start == 65535 → the very first companion (port+1) overflows.
        // Must Err cleanly instead of wrapping/panicking.
        let r = bind_udp_pair(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 65535);
        assert!(
            r.is_err(),
            "bind_udp_pair(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 65535) must Err, not wrap"
        );
    }

    #[test]
    fn bind_udp_pair_at_65534_does_not_wrap() {
        // start == 65534: attempt 0 gives port 65534 / companion 65535
        // (both valid u16, may bind); attempt 1 would be 65536 which must
        // NOT wrap to 0 — the loop breaks cleanly. So this returns either
        // Ok (bound 65534) or Err (AddrInUse / overflow), NEVER a panic or
        // a wrapped port-0 binding.
        let r = bind_udp_pair(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 65534);
        if let Ok((_, _, port)) = r {
            assert_eq!(port, 65534, "must bind 65534, never a wrapped 0");
        }
    }

    #[test]
    fn parse_rejects_reversed_server_port_pair() {
        // hi < lo is a malformed range.
        let r = parse_transport_response("RTP/AVP;unicast;server_port=6971-6970");
        assert!(r.is_err(), "reversed server_port must be rejected");
    }

    #[test]
    fn parse_rejects_reversed_client_port_pair() {
        let r = parse_transport_response("RTP/AVP;unicast;client_port=5005-5004");
        assert!(r.is_err(), "reversed client_port must be rejected");
    }

    #[test]
    fn parse_rejects_invalid_port_value() {
        // Non-numeric port must not silently map to 0.
        let r = parse_transport_response("RTP/AVP;unicast;client_port=abc-def");
        assert!(r.is_err(), "non-numeric ports must be rejected");
    }

    #[test]
    fn parse_rejects_out_of_range_port() {
        // 70000 > u16::MAX must Err, not truncate/saturate.
        let r = parse_transport_response("RTP/AVP;unicast;client_port=70000-70001");
        assert!(r.is_err(), "out-of-range port must be rejected");
    }

    #[test]
    fn parse_rejects_missing_hi_port() {
        // A single-value range with no hi must not fabricate lo+1 (which
        // can also overflow at 65535).
        let r = parse_transport_response("RTP/AVP;unicast;client_port=65535");
        assert!(r.is_err(), "missing hi port must be rejected");
    }

    // --- failure-reason classification (server SETUP 400 diagnostics) ---
    // The wire behavior above must stay a bare rejection; these pin the
    // human-readable reason each failure mode reports so log lines can
    // name which Transport-header check failed.

    #[test]
    fn u16_pair_detail_accepts_valid_pair_and_single() {
        assert_eq!(parse_u16_pair_detail("5004-5005"), Ok((5004, 5005)));
        // Single-value form fabricates the RTCP companion.
        assert_eq!(parse_u16_pair_detail("5004"), Ok((5004, 5005)));
    }

    #[test]
    fn u16_pair_detail_names_reversed_pair() {
        let reason = parse_u16_pair_detail("5005-5004").unwrap_err();
        assert!(
            reason.contains("ascending"),
            "reversed pair must name the ascending-order check, got: {reason}"
        );
    }

    #[test]
    fn u16_pair_detail_names_non_integer_port() {
        let reason = parse_u16_pair_detail("abc-def").unwrap_err();
        assert!(
            reason.contains("not an integer"),
            "non-numeric port must name the integer check, got: {reason}"
        );
        // Out-of-range values fail the same u16 parse and share the reason.
        let reason = parse_u16_pair_detail("70000-70001").unwrap_err();
        assert!(
            reason.contains("not an integer"),
            "out-of-range port must name the integer check, got: {reason}"
        );
    }

    #[test]
    fn u16_pair_detail_names_companion_overflow() {
        let reason = parse_u16_pair_detail("65535").unwrap_err();
        assert!(
            reason.contains("companion"),
            "single 65535 must name the companion-port overflow, got: {reason}"
        );
    }

    #[test]
    fn u16_pair_detail_names_extra_tokens() {
        let reason = parse_u16_pair_detail("1-2-3").unwrap_err();
        assert!(
            reason.contains("more than two"),
            "triple range must name the token-count check, got: {reason}"
        );
    }

    #[test]
    fn u8_pair_detail_reasons_mirror_u16() {
        assert_eq!(parse_u8_pair_detail("0-1"), Ok((0, 1)));
        assert!(
            parse_u8_pair_detail("1-0")
                .unwrap_err()
                .contains("ascending"),
            "reversed channel pair must name the ascending-order check"
        );
        assert!(
            parse_u8_pair_detail("300-301")
                .unwrap_err()
                .contains("not an integer"),
            "out-of-range channel must name the integer check"
        );
        assert!(
            parse_u8_pair_detail("255")
                .unwrap_err()
                .contains("companion"),
            "single channel 255 must name the companion-channel overflow"
        );
    }

    #[test]
    fn parse_matches_keys_case_insensitively() {
        // RFC 7826: Transport header parameter names are case-insensitive.
        let t =
            parse_transport_response("RTP/AVP;UNICAST;Client_Port=5004-5005;SERVER_PORT=6970-6971")
                .unwrap();
        assert_eq!(t.kind, RtspTransportKind::Udp);
        #[cfg(feature = "rtsp-server")]
        assert_eq!(t.client_port, Some((5004, 5005)));
    }

    #[test]
    fn parse_interleaved_case_insensitive_keys() {
        let t = parse_transport_response("RTP/AVP/TCP;unicast;INTERLEAVED=0-1").unwrap();
        assert_eq!(t.kind, RtspTransportKind::TcpInterleaved);
        assert_eq!(t.interleaved, Some((0, 1)));
    }

    #[test]
    fn parse_rejects_reversed_interleaved_pair() {
        let r = parse_transport_response("RTP/AVP/TCP;unicast;interleaved=5-4");
        assert!(r.is_err(), "reversed interleaved channels must be rejected");
    }

    #[test]
    fn parse_rejects_interleaved_companion_overflow() {
        // Single channel 255 → fabricated companion 256 overflows u8.
        let r = parse_transport_response("RTP/AVP/TCP;unicast;interleaved=255");
        assert!(r.is_err(), "interleaved=255 companion overflow rejected");
    }

    #[test]
    fn parse_accepts_well_formed_lowercase_unchanged() {
        // Regression: don't break the happy path.
        let t = parse_transport_response("RTP/AVP;unicast;client_port=5004-5005").unwrap();
        assert_eq!(t.kind, RtspTransportKind::Udp);
        #[cfg(feature = "rtsp-server")]
        assert_eq!(t.client_port, Some((5004, 5005)));
    }
}
