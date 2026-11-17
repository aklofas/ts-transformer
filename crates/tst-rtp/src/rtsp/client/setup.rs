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
use crate::rtsp::client::driver::{Command, DATA_QUEUE_BOUND, RtcpSink};
use crate::rtsp::client::engine::InterleavedChannels;
use crate::rtsp::client::session::RtspSession;
use crate::rtsp::message::RtspMethod;
use crate::rtsp::transport_negotiation::{
    RtspTransportKind, bind_udp_pair, build_transport_request, parse_transport_response,
};
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
        // SETUP authenticates like every method, hashing the digest against
        // the control URI. gortsplib/MediaMTX require auth on SETUP even
        // after an authenticated DESCRIBE.
        let resp = self.exchange(
            RtspMethod::Setup,
            uri.to_string(),
            vec![("transport", transport_hdr)],
        )?;
        self.expect_ok(&resp)?;
        // Server-rewritten Transport: tells us what was actually negotiated.
        let server_transport = resp
            .headers
            .get("transport")
            .ok_or(RtspError::BadResponse {
                detail: "SETUP 200 missing Transport: header",
            })?;
        let transport = parse_transport_response(server_transport)?;
        let sid = resp
            .session_id()
            .ok_or(RtspError::BadResponse {
                detail: "SETUP 200 missing Session: header",
            })?
            .to_string();
        // The client's thread learned the session from this reply too: its
        // pings carry the id and follow the advertised timeout.
        self.session_id = Some(sid.clone());

        // Construct RtspSession with the negotiated transport.
        let session = match transport.kind {
            RtspTransportKind::Udp => {
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
            RtspTransportKind::TcpInterleaved => {
                // The client's thread routes `$` frames from here on, before
                // PLAY, so the leading frames a server pushes right after
                // its PLAY reply are not missed.
                //
                // Channels: the SETUP request hard-codes `interleaved=0-1`
                // in `build_transport_request`. The server may echo a
                // different pair via `Transport: interleaved=N-M` — use
                // the parsed response when present, else fall back to 0-1.
                let (rtp_ch, rtcp_ch) = transport.interleaved.unwrap_or((0, 1));
                let channels = InterleavedChannels {
                    rtp: rtp_ch,
                    rtcp: rtcp_ch,
                };
                let (data_tx, data_rx) = mpsc::sync_channel(DATA_QUEUE_BOUND);
                let rtcp_stats = Arc::new(Mutex::new(RtcpStats::default()));
                let ssrc = crate::transport::random_u32();
                self.driver.send(Command::SetInterleaved {
                    channels,
                    data_tx,
                    rtcp: RtcpSink {
                        stats: rtcp_stats.clone(),
                        ssrc,
                    },
                })?;
                RtspSession::new_interleaved(
                    sid,
                    data_rx,
                    rtcp_stats,
                    ssrc,
                    self.url.recv_timeout,
                    self.end_reason.clone(),
                )
            }
        };
        Ok(session)
    }
}
