//! Thin wrapper around `sdp-types` for our DESCRIBE response parsing.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

use crate::error::RtspError;
use std::net::SocketAddr;

pub mod pick;

/// A parsed SDP document.
#[derive(Debug, Clone)]
pub struct Sdp {
    pub media: Vec<SdpMedia>,
    /// Session-level `c=` connection address, if present.
    pub session_connection: Option<String>,
    /// Session name from `s=` line.
    pub session_name: String,
}

/// A single `m=` media line from SDP.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SdpMedia {
    /// E.g., "video", "audio", "application".
    pub media: String,
    /// Port from the `m=` line.
    pub port: u16,
    /// Protocol — "RTP/AVP" for plain UDP, "RTP/AVP/TCP" if SDP advertises TCP-interleaved.
    pub protocol: String,
    /// Payload types from the `m=` line. For MP2T, this contains 33.
    pub payload_types: Vec<u8>,
    /// Media-level connection address (`c=` line under this m=), falls
    /// back to session-level `c=` if absent.
    pub connection: Option<String>,
    /// Per-media attributes (`a=` lines under this m=).
    pub attributes: Vec<(String, Option<String>)>,
    /// The `a=control:` URL for this media — used in SETUP request URI.
    pub control: Option<String>,
}

impl Sdp {
    /// Parse SDP bytes. Wraps `sdp-types` and surfaces any parse error
    /// as `RtspError::BadSdp`.
    ///
    /// The bytes come from a peer before anything about it is trusted (a
    /// publisher's ANNOUNCE body on the server, a DESCRIBE answer on the
    /// client), and the parser is an external crate. A panic inside it
    /// would let a peer end the server's session task or unwind through the
    /// caller's thread with a few bytes, so the call runs under
    /// [`std::panic::catch_unwind`] and a caught panic becomes `BadSdp` like
    /// any other malformed input. The guard cannot help a build with
    /// `panic = "abort"`: there the panic ends the process, and the real
    /// protection is a parser that does not panic (the `sdp-types` version
    /// this crate requires returns an error for the input that used to
    /// assert).
    pub fn parse(bytes: &[u8]) -> Result<Self, RtspError> {
        let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sdp_types::Session::parse(bytes)
        }))
        .map_err(|_| {
            tracing::debug!(
                target: "tst_rtp::sdp",
                len = bytes.len(),
                "sdp parser panicked on malformed input"
            );
            RtspError::BadSdp {
                detail: "sdp parser panicked on malformed input".to_owned(),
            }
        })?
        .map_err(|e| RtspError::BadSdp {
            detail: format!("{e:?}"),
        })?;
        let session_connection = parsed
            .connection
            .as_ref()
            .map(|c| c.connection_address.clone());
        let session_name = parsed.session_name.clone();
        let media: Vec<SdpMedia> = parsed
            .medias
            .iter()
            .map(|m| {
                let payload_types: Vec<u8> = m
                    .fmt
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                let connection = m.connections.first().map(|c| c.connection_address.clone());
                let attributes: Vec<(String, Option<String>)> = m
                    .attributes
                    .iter()
                    .map(|a| (a.attribute.clone(), a.value.clone()))
                    .collect();
                let control = attributes
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("control"))
                    .and_then(|(_, v)| v.clone());
                SdpMedia {
                    media: m.media.clone(),
                    port: m.port,
                    protocol: m.proto.clone(),
                    payload_types,
                    connection,
                    attributes,
                    control,
                }
            })
            .collect();
        Ok(Sdp {
            media,
            session_connection,
            session_name,
        })
    }

    /// Generate the SDP body for a mount, returned by the RTSP server's
    /// DESCRIBE handler.
    ///
    /// - For unicast mounts: `connection_addr` is the server's local IP;
    ///   the `c=` line carries it and the `m=` port is 0 (RFC 4566 §5.7
    ///   placeholder — the actual port comes from SETUP's `Transport:
    ///   server_port=...` response).
    /// - For multicast mounts: `connection_addr` is the multicast group
    ///   address + port; both `c=` and the `m=` port advertise it.
    ///
    /// `mount_path` is the registered mount path (e.g., `/live`).
    pub fn build_for_mount(
        mount_path: &str,
        connection_addr: SocketAddr,
        is_multicast: bool,
    ) -> bytes::Bytes {
        let ip_family = match connection_addr.ip() {
            std::net::IpAddr::V4(_) => "IP4",
            std::net::IpAddr::V6(_) => "IP6",
        };
        let host = connection_addr.ip().to_string();
        let m_port = if is_multicast {
            connection_addr.port()
        } else {
            0
        };
        // Strip leading slash from mount path for the session-name (s=) line.
        let session_name = mount_path.trim_start_matches('/');
        // RFC 2250 §2: MP2T uses `m=video` (not `m=application`).
        // RFC 7826 App. D: session-level `a=control:*` lets third-party
        // clients resolve the aggregate control URL correctly.
        let body = format!(
            "v=0\r\n\
             o=- 0 0 IN {family} {host}\r\n\
             s=tst-rtp {name}\r\n\
             t=0 0\r\n\
             c=IN {family} {host}\r\n\
             a=control:*\r\n\
             m=video {port} RTP/AVP 33\r\n\
             a=control:trackID=0\r\n",
            family = ip_family,
            host = host,
            name = session_name,
            port = m_port,
        );
        bytes::Bytes::from(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_SDP_SINGLE_MP2T: &[u8] = b"\
v=0\r\n\
o=- 0 0 IN IP4 192.0.2.1\r\n\
s=GimbalCam RTSP\r\n\
t=0 0\r\n\
m=application 0 RTP/AVP 33\r\n\
c=IN IP4 192.0.2.1\r\n\
a=control:trackID=0\r\n";

    const SAMPLE_SDP_MULTI: &[u8] = b"\
v=0\r\n\
o=- 0 0 IN IP4 192.0.2.1\r\n\
s=Camera with audio+video\r\n\
t=0 0\r\n\
m=video 0 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=control:trackID=0\r\n\
m=audio 0 RTP/AVP 97\r\n\
a=rtpmap:97 MPEG4-GENERIC/48000/2\r\n\
a=control:trackID=1\r\n";

    /// The 8-byte input on which `sdp-types` 0.1.8 trips an internal
    /// `assert_eq!`: a peer controls these bytes (ANNOUNCE body, DESCRIBE
    /// answer), so it must be an error, never a panic.
    #[test]
    fn malformed_sdp_is_a_bad_sdp_error_not_a_panic() {
        let res = Sdp::parse(b"v=0\n\xff\n\0=");
        assert!(
            matches!(res, Err(RtspError::BadSdp { .. })),
            "expected BadSdp, got {res:?}"
        );
    }

    #[test]
    fn parse_single_mp2t() {
        let sdp = Sdp::parse(SAMPLE_SDP_SINGLE_MP2T).unwrap();
        assert_eq!(sdp.media.len(), 1);
        assert_eq!(sdp.media[0].media, "application");
        assert_eq!(sdp.media[0].payload_types, vec![33]);
        assert_eq!(sdp.media[0].control.as_deref(), Some("trackID=0"));
    }

    #[test]
    fn parse_multi_no_mp2t() {
        let sdp = Sdp::parse(SAMPLE_SDP_MULTI).unwrap();
        assert_eq!(sdp.media.len(), 2);
        assert_eq!(sdp.media[0].payload_types, vec![96]);
        assert_eq!(sdp.media[1].payload_types, vec![97]);
    }

    #[test]
    fn parse_malformed() {
        let bad = b"v="; // no value, missing CRLF
        let e = Sdp::parse(bad).unwrap_err();
        assert!(matches!(e, RtspError::BadSdp { .. }));
    }
}

#[cfg(test)]
mod phase3_build_for_mount_tests {
    use super::*;

    #[test]
    fn unicast_sdp_round_trips_through_parse() {
        let addr: SocketAddr = "192.0.2.1:8554".parse().unwrap();
        let body = Sdp::build_for_mount("/live", addr, false);
        let parsed = Sdp::parse(&body).unwrap();
        assert_eq!(parsed.media.len(), 1);
        assert_eq!(parsed.media[0].payload_types, vec![33]);
        // Unicast: m= port is 0 (placeholder per RFC 4566 §5.7).
        assert_eq!(parsed.media[0].port, 0);
        assert_eq!(parsed.media[0].control.as_deref(), Some("trackID=0"));
    }

    #[test]
    fn multicast_sdp_advertises_group() {
        let addr: SocketAddr = "239.0.0.1:5004".parse().unwrap();
        let body = Sdp::build_for_mount("/mc", addr, true);
        let parsed = Sdp::parse(&body).unwrap();
        assert_eq!(parsed.media.len(), 1);
        // Multicast: m= port carries the group port.
        assert_eq!(parsed.media[0].port, 5004);
        // Session-level c= should be the multicast group.
        assert_eq!(parsed.session_connection.as_deref(), Some("239.0.0.1"));
    }

    #[test]
    fn sdp_session_name_strips_leading_slash() {
        let addr: SocketAddr = "127.0.0.1:8554".parse().unwrap();
        let body = Sdp::build_for_mount("/live", addr, false);
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("s=tst-rtp live\r\n"));
        assert!(!text.contains("s=tst-rtp /live\r\n"));
    }

    #[test]
    fn sdp_session_name_handles_no_leading_slash() {
        let addr: SocketAddr = "127.0.0.1:8554".parse().unwrap();
        let body = Sdp::build_for_mount("live", addr, false);
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("s=tst-rtp live\r\n"));
    }

    #[test]
    fn sdp_ipv6_unicast() {
        let addr: SocketAddr = "[::1]:8554".parse().unwrap();
        let body = Sdp::build_for_mount("/live", addr, false);
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("IN IP6"));
        assert!(text.contains("::1"));
    }

    #[test]
    fn sdp_multicast_ipv6() {
        let addr: SocketAddr = "[ff02::1]:5004".parse().unwrap();
        let body = Sdp::build_for_mount("/v6", addr, true);
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("IN IP6"));
        assert!(text.contains("ff02::1"));
        assert!(text.contains("m=video 5004 RTP/AVP 33"));
    }

    #[test]
    fn sdp_terminates_lines_with_crlf() {
        let addr: SocketAddr = "127.0.0.1:8554".parse().unwrap();
        let body = Sdp::build_for_mount("/live", addr, false);
        let text = std::str::from_utf8(&body).unwrap();
        // Each declarative line ends with CRLF per RFC 4566 §5.
        let crlf_count = text.matches("\r\n").count();
        // v= o= s= t= c= a=control:* m= a=control:trackID=0 → 8 CRLFs.
        assert_eq!(crlf_count, 8);
    }

    // --- Conventional SDP shape tests ---

    /// RFC 2250 §2 mandates `m=video` for MP2T streams; `m=application`
    /// is non-standard and breaks many third-party RTSP players.
    #[test]
    fn build_for_mount_emits_m_video() {
        let addr: SocketAddr = "127.0.0.1:8554".parse().unwrap();
        let body = Sdp::build_for_mount("/live", addr, false);
        let text = std::str::from_utf8(&body).unwrap();
        assert!(
            text.contains("m=video 0 RTP/AVP 33\r\n"),
            "expected m=video 0 RTP/AVP 33, got:\n{text}"
        );
    }

    /// RFC 7826 App. D requires a session-level `a=control:*` so third-party
    /// clients (VLC, ffplay) can resolve the aggregate control URL correctly.
    #[test]
    fn build_for_mount_has_session_level_control() {
        let addr: SocketAddr = "127.0.0.1:8554".parse().unwrap();
        let body = Sdp::build_for_mount("/live", addr, false);
        let text = std::str::from_utf8(&body).unwrap();
        // Must appear BEFORE the first m= line (i.e. at session level).
        let control_star_pos = text.find("a=control:*\r\n").expect("a=control:* missing");
        let media_line_pos = text.find("m=video").expect("m=video missing");
        assert!(
            control_star_pos < media_line_pos,
            "a=control:* must be at session level (before m=)"
        );
    }
}
