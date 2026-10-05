//! UDP-first SETUP attempt with auto-fallback to TCP-interleaved on 461.
//!
//! Trigger for fallback: server response code 461 Unsupported Transport
//! (RFC 7826 §17.4.6). Any other 4xx/5xx surfaces as
//! `RtspError::Protocol { code, reason }`.
//!
//! `?transport=udp` in the URL skips fallback; `?transport=tcp` skips
//! the UDP attempt entirely.

use std::net::UdpSocket;

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
pub struct TransportResponse {
    /// Which transport the server actually accepted.
    pub kind: RtspTransportKind,
    /// For UDP: server's RTP+RTCP port range from `server_port=lo-hi`.
    pub server_port: Option<(u16, u16)>,
    /// For UDP: client's RTP+RTCP port range from `client_port=lo-hi`.
    /// Server-side parses this from the client's SETUP request; client-side
    /// parsers leave this as `None` (the client knew its own ports).
    pub client_port: Option<(u16, u16)>,
    /// For TCP-interleaved: channel range from `interleaved=lo-hi`.
    pub interleaved: Option<(u8, u8)>,
    /// `ssrc=` parameter from the Transport header, if present.
    pub ssrc: Option<u32>,
}

/// Parse a `Transport:` header value (server response OR client SETUP
/// request — the syntax is shared per RFC 7826 §18.54).
///
/// Recognized keys: `client_port=`, `server_port=`, `interleaved=`,
/// `ssrc=`. The kind is inferred from the presence of `RTP/AVP/TCP` or
/// `interleaved=`.
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
pub fn parse_transport_response(header_value: &str) -> Result<TransportResponse, RtspError> {
    let lower = header_value.to_ascii_lowercase();
    let kind = if lower.contains("rtp/avp/tcp") || lower.contains("interleaved=") {
        RtspTransportKind::TcpInterleaved
    } else {
        RtspTransportKind::Udp
    };
    let mut server_port = None;
    let mut client_port = None;
    let mut interleaved = None;
    let mut ssrc = None;
    for part in header_value.split(';') {
        let part = part.trim();
        // Split the `key=value` once; match the key case-insensitively
        // per RFC 7826 §18.54 while preserving the original value casing
        // (hex ssrc, etc.).
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "server_port" => server_port = Some(parse_u16_pair(value)?),
            "client_port" => client_port = Some(parse_u16_pair(value)?),
            "interleaved" => interleaved = Some(parse_u8_pair(value)?),
            "ssrc" => ssrc = u32::from_str_radix(value, 16).ok(),
            _ => {}
        }
    }
    Ok(TransportResponse {
        kind,
        server_port,
        client_port,
        interleaved,
        ssrc,
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

/// Bind a UDP RTP+RTCP port pair on `0.0.0.0`.
///
/// Returns `(rtp_socket, rtcp_socket, rtp_port)`. Tries up to 8 candidate
/// even-port pairs starting from `start` — the first pair where both ports
/// bind cleanly is returned.
///
/// # Errors
///
/// [`RtspError::UnsupportedTransport`] if `start` is so close to
/// `u16::MAX` that no candidate even/odd pair fits without overflowing
/// (the companion RTCP port is always RTP+1).
/// [`RtspError::Io`] with `AddrInUse` if all candidate pairs fail to bind.
pub fn bind_udp_pair(start: u16) -> Result<(UdpSocket, UdpSocket, u16), RtspError> {
    let mut bound_overflow = false;
    for attempt in 0..8u16 {
        // start + attempt*2, then companion port+1 — both checked so a
        // `start` near 65535 returns a typed error instead of wrapping.
        let Some(port) = attempt
            .checked_mul(2)
            .and_then(|off| start.checked_add(off))
        else {
            bound_overflow = true;
            break;
        };
        let Some(rtcp_port) = port.checked_add(1) else {
            bound_overflow = true;
            break;
        };
        if let Ok(rtp) = UdpSocket::bind(("0.0.0.0", port)) {
            if let Ok(rtcp) = UdpSocket::bind(("0.0.0.0", rtcp_port)) {
                return Ok((rtp, rtcp, port));
            }
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
pub fn build_transport_request(
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
    use super::*;

    #[test]
    fn parse_udp_transport_response() {
        let h = "RTP/AVP;unicast;client_port=5004-5005;server_port=6970-6971;ssrc=0DEADBEEF";
        let t = parse_transport_response(h).unwrap();
        assert_eq!(t.kind, RtspTransportKind::Udp);
        assert_eq!(t.server_port, Some((6970, 6971)));
        assert_eq!(t.client_port, Some((5004, 5005)));
    }

    #[test]
    fn parse_interleaved_transport_response() {
        let h = "RTP/AVP/TCP;unicast;interleaved=0-1";
        let t = parse_transport_response(h).unwrap();
        assert_eq!(t.kind, RtspTransportKind::TcpInterleaved);
        assert_eq!(t.interleaved, Some((0, 1)));
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

    #[test]
    fn bind_udp_pair_rejects_start_at_max_port() {
        // start == 65535 → the very first companion (port+1) overflows.
        // Must Err cleanly instead of wrapping/panicking.
        let r = bind_udp_pair(65535);
        assert!(r.is_err(), "bind_udp_pair(65535) must Err, not wrap");
    }

    #[test]
    fn bind_udp_pair_at_65534_does_not_wrap() {
        // start == 65534: attempt 0 gives port 65534 / companion 65535
        // (both valid u16, may bind); attempt 1 would be 65536 which must
        // NOT wrap to 0 — the loop breaks cleanly. So this returns either
        // Ok (bound 65534) or Err (AddrInUse / overflow), NEVER a panic or
        // a wrapped port-0 binding.
        let r = bind_udp_pair(65534);
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
        assert_eq!(t.client_port, Some((5004, 5005)));
        assert_eq!(t.server_port, Some((6970, 6971)));
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
        assert_eq!(t.client_port, Some((5004, 5005)));
    }
}
