//! `RtspClient::setup` (explicit media) + `setup_mp2t_auto` (picks the
//! unique PT=33 m-line) + `setup_h264_auto` (picks the unique H.264
//! rtpmap m-line). Implements UDP-first attempt with 461 →
//! TCP-interleaved auto-fallback per RFC 7826 §17.4.6.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex, mpsc};

use crate::error::RtspError;
use crate::h264::depacketizer::H264DepayConfig;
use crate::rtcp::stats::RtcpStats;
use crate::rtsp::client::RtspClient;
use crate::rtsp::client::driver::{DATA_QUEUE_BOUND, RouteSpec, RtcpSink};
use crate::rtsp::client::engine::{RequestSpec, RouteId, RouteKind, SetupRoute};
use crate::rtsp::client::session::RtspSession;
use crate::rtsp::message::RtspMethod;
use crate::rtsp::transport_negotiation::{bind_udp_pair, build_transport_request};
use crate::sdp::pick::{pick_h264, pick_mp2t};
use crate::sdp::{Sdp, SdpMedia};
use crate::url::RtspTransportPref;

impl RtspClient {
    /// Setup the unique MP2T (PT=33) media line in `sdp`.
    ///
    /// # Errors
    ///
    /// - [`RtspError::NoMp2tMedia`] / [`RtspError::MultipleMp2tMedia`] if
    ///   the SDP does not contain exactly one PT=33 m-line.
    /// - Any error from [`Self::setup`].
    pub fn setup_mp2t_auto(&mut self, sdp: &Sdp) -> Result<RtspSession, RtspError> {
        let media = pick_mp2t(sdp)?;
        self.setup(media)
    }

    /// Setup the unique H.264 (RFC 6184) media line in `sdp`, returning a
    /// session and the negotiated [`H264DepayConfig`] ready for
    /// [`crate::rtsp::client::session::RtspSession::into_h264_receiver`].
    ///
    /// The returned tuple gives you everything you need to wire up the
    /// receiver: the session drives PLAY/PAUSE/TEARDOWN, and the config
    /// carries the negotiated payload type and any out-of-band SPS/PPS
    /// NALUs from `a=fmtp sprop-parameter-sets=`.
    ///
    /// # Errors
    ///
    /// - [`RtspError::NoH264Media`] / [`RtspError::MultipleH264Media`] if
    ///   the SDP does not contain exactly one H.264 rtpmap m-line.
    /// - [`RtspError::UnsupportedPacketizationMode`]`(2)` if the H.264
    ///   media advertises packetization-mode 2 (interleaved; not implemented).
    ///   Modes 0 and 1 proceed normally.
    /// - Any error from [`Self::setup`].
    pub fn setup_h264_auto(
        &mut self,
        sdp: &Sdp,
    ) -> Result<(RtspSession, H264DepayConfig), RtspError> {
        let h = pick_h264(sdp)?;
        if h.fmtp.packetization_mode == 2 {
            return Err(RtspError::UnsupportedPacketizationMode(2));
        }
        let session = self.setup(h.media)?;
        let config = H264DepayConfig {
            payload_type: h.payload_type,
            initial_parameter_sets: h.fmtp.sprop_parameter_sets,
            ..H264DepayConfig::default()
        };
        Ok((session, config))
    }

    /// Setup an explicit SDP media line.
    ///
    /// Tries UDP first when the URL's transport preference is `PreferUdp`
    /// or `ForceUdp`, falling back to TCP-interleaved on a 461 response
    /// only when the preference is `PreferUdp`. Any other 4xx/5xx
    /// surfaces immediately as [`RtspError::Protocol`].
    ///
    /// # Errors
    ///
    /// - [`RtspError::Io`] on socket-level failure.
    /// - [`RtspError::Protocol`] on non-200 server response (after
    ///   exhausting auto-fallback).
    /// - [`RtspError::BadResponse`] on malformed SETUP response (missing
    ///   `Transport:` or `Session:` headers).
    pub fn setup(&mut self, media: &SdpMedia) -> Result<RtspSession, RtspError> {
        let pref = self.url.transport_preference;
        // Use control URL from media's a=control: attribute if present;
        // otherwise fall back to URL-with-path.
        let setup_uri = match &media.control {
            Some(c) if c.starts_with("rtsp://") || c.starts_with("rtsps://") => c.clone(),
            Some(c) => format!(
                "{}/{}",
                self.url.render_no_credentials().trim_end_matches('/'),
                c.trim_start_matches('/')
            ),
            None => self.url.render_no_credentials(),
        };

        // First attempt
        match self.attempt_setup(&setup_uri, pref) {
            Ok(session) => Ok(session),
            Err(RtspError::Protocol { code: 461, .. }) if pref == RtspTransportPref::PreferUdp => {
                // Auto-fallback to TCP-interleaved
                self.attempt_setup(&setup_uri, RtspTransportPref::ForceTcp)
            }
            Err(e) => Err(e),
        }
    }

    fn attempt_setup(
        &mut self,
        uri: &str,
        pref: RtspTransportPref,
    ) -> Result<RtspSession, RtspError> {
        // For UDP we need a local port pair before SETUP.
        let local_udp = if matches!(
            pref,
            RtspTransportPref::PreferUdp | RtspTransportPref::ForceUdp
        ) {
            let (rtp, rtcp, port) = bind_udp_pair(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 5004)?;
            Some((rtp, rtcp, port))
        } else {
            None
        };
        let local_port = local_udp.as_ref().map(|(_, _, p)| *p).unwrap_or(0);
        let transport_hdr = build_transport_request(pref, local_port)?;
        // The receiving side's one route. Its sinks go with the request,
        // whichever transport it asks for: the client's thread installs
        // them when it accepts an interleaved reply, so `$` frames a server
        // sends right behind that reply are not missed, and forgets them
        // otherwise.
        let route = RouteId(0);
        let (data_tx, data_rx) = mpsc::sync_channel(DATA_QUEUE_BOUND);
        let rtcp_stats = Arc::new(Mutex::new(RtcpStats::default()));
        let ssrc = crate::transport::random_u32();
        // SETUP authenticates like every method, hashing the digest against
        // the control URI. gortsplib/MediaMTX require auth on SETUP even
        // after an authenticated DESCRIBE.
        let deadline = self.request_deadline();
        let outcome = self.request_routed(
            RequestSpec {
                method: RtspMethod::Setup,
                uri: uri.to_string(),
                headers: vec![("transport", transport_hdr)],
                body: None,
                setup: Some(SetupRoute {
                    id: route,
                    kind: RouteKind::Pull,
                }),
            },
            Some(RouteSpec {
                id: route,
                inbound_media: Some(data_tx),
                inbound_rtcp: RtcpSink {
                    stats: rtcp_stats.clone(),
                    ssrc,
                },
            }),
            deadline,
            None,
        )?;
        let resp = outcome.response;
        self.expect_ok(&resp)?;
        let sid = resp
            .session_id()
            .ok_or(RtspError::BadResponse {
                detail: "SETUP 200 missing Session: header",
            })?
            .to_string();
        // The client's thread learned the session from this reply too: its
        // pings carry the id and follow the advertised timeout.
        self.session_id = Some(sid.clone());

        // The transport the server's rewritten `Transport:` negotiated: the
        // client's thread read it, and installed the channel pair of an
        // interleaved one (the request asks for `interleaved=0-1`; the
        // server may name another pair).
        let session = match outcome.setup {
            None => {
                let (rtp, rtcp, _port) = local_udp.ok_or(RtspError::BadResponse {
                    detail: "server replied UDP but we sent TCP-interleaved",
                })?;
                RtspSession::new_udp(
                    sid,
                    rtp,
                    rtcp,
                    self.url.recv_timeout,
                    self.end_reason.clone(),
                )
            }
            Some(_) => RtspSession::new_interleaved(
                sid,
                data_rx,
                rtcp_stats,
                ssrc,
                self.url.recv_timeout,
                self.end_reason.clone(),
            ),
        };
        Ok(session)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use tst_core::transport::RecvTransport;
    use tst_test_helpers::timing::scaled;

    use crate::rtcp::{ReceiverReport, ReportBlock};
    use crate::rtsp::client::RtspClient;
    use crate::sdp::Sdp;

    const SDP: &[u8] = b"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=x\r\nt=0 0\r\nm=video 0 RTP/AVP 33\r\na=control:trackID=0\r\n";

    /// A receiver report on the interleaved RTCP channel whose block names
    /// the session's SSRC reaches the stats of the transport the session
    /// becomes: the client's thread matches against the SSRC the transport
    /// reports, and counts into the cell the transport reads.
    #[test]
    fn a_receiver_report_for_the_session_ssrc_reaches_the_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (ssrc_tx, ssrc_rx) = mpsc::channel::<u32>();
        let peer = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
            let mut head = Vec::new();
            let mut chunk = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => head.extend_from_slice(&chunk[..n]),
                }
            }
            let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
            let cseq = head
                .lines()
                .find_map(|l| l.strip_prefix("cseq:"))
                .map(str::trim)
                .unwrap_or("1")
                .to_owned();
            sock.write_all(
                format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: S1;timeout=60\r\n\
                     Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
                )
                .as_bytes(),
            )
            .unwrap();
            let Ok(ssrc) = ssrc_rx.recv_timeout(Duration::from_secs(10)) else {
                return;
            };
            let rr = ReceiverReport {
                ssrc: 0xDEAD_BEEF,
                report_blocks: vec![ReportBlock {
                    ssrc,
                    fraction_lost: 0,
                    cumulative_lost: 2024,
                    extended_highest_seq: 0,
                    jitter: 0,
                    last_sr: 0,
                    delay_since_last_sr: 0,
                }],
            }
            .encode()
            .unwrap();
            let mut frame = vec![b'$', 1];
            frame.extend_from_slice(&(rr.len() as u16).to_be_bytes());
            frame.extend_from_slice(&rr);
            sock.write_all(&frame).unwrap();
            // Hold the connection until the client hangs up.
            let _ = sock.read_to_end(&mut Vec::new());
        });

        let mut client =
            RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/x?transport=tcp")).unwrap();
        let sdp = Sdp::parse(SDP).unwrap();
        let session = client.setup_mp2t_auto(&sdp).unwrap();
        let ssrc = session.rtcp.as_ref().expect("an interleaved session").1;
        let transport = session.into_recv_transport();
        ssrc_tx.send(ssrc).unwrap();

        let give_up = Instant::now() + scaled(Duration::from_secs(5));
        loop {
            let lost = transport
                .socket_stats()
                .expect("a live transport reports stats")
                .packets_lost_send;
            if lost == 2024 {
                break;
            }
            assert!(
                Instant::now() < give_up,
                "the report for the session's SSRC never reached the transport (lost = {lost})"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        drop(transport);
        drop(client);
        peer.join().unwrap();
    }
}
