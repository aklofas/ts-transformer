//! The publish direction of [`RtspClient`]: ANNOUNCE a description, SETUP
//! each of its tracks for recording, RECORD, and send through the
//! [`RtspPush`] that RECORD returns.

mod session;
#[cfg(test)]
mod tests;

pub use session::RtspPush;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tst_core::transport::{EndReason, TransportError};

use crate::error::RtspError;
use crate::rtcp::sender::TrackState;
use crate::rtcp::stats::RtcpStats;
use crate::rtsp::client::driver::{RouteSpec, RtcpSink};
use crate::rtsp::client::engine::{
    RequestBody, RequestSpec, RouteId, RouteKind, SetupAccepted, SetupRoute,
};
use crate::rtsp::client::{CLOSE_JOIN_BOUND, CLOSE_TEARDOWN_BOUND, RtspClient};
use crate::rtsp::message::RtspMethod;
use crate::sdp::write::{
    PT_KLV, PT_MP2T, PT_VIDEO, SdpTracks, SdpVideo, h264_parameter_sets, h265_parameter_sets,
    write_announce,
};
use crate::transport::{random_u32, record_end};
use crate::url::RtspTransportPref;

/// The largest payload [`RtspPush::send_ts`] takes: seven TS packets.
pub const PUSH_MP2T_MAX_PAYLOAD: usize = 1316;

/// What a push announces.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum PushDescription {
    /// One MPEG-TS track (RFC 2250, payload type 33): send with
    /// [`RtspPush::send_ts`], or put a sender shell over the push.
    Mp2t,
    /// One video track and, with `klv`, one KLV track (RFC 6597): send
    /// with [`RtspPush::send_video`] and [`RtspPush::send_klv`].
    Elementary {
        /// The video codec and its parameter sets.
        video: VideoTrack,
        /// Announce a KLV track beside the video.
        klv: bool,
    },
}

/// The video track of an elementary push, with the parameter sets its
/// description carries. Each parameter set is one NAL unit with its header
/// and without a start code.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum VideoTrack {
    /// H.264 (RFC 6184), packetization mode 1.
    H264 {
        /// The stream's SPS and PPS NAL units: at least one of each. A NAL
        /// unit of another type is ignored.
        parameter_sets: Vec<Vec<u8>>,
    },
    /// H.265 (RFC 7798).
    H265 {
        /// The video parameter set.
        vps: Vec<u8>,
        /// The sequence parameter set.
        sps: Vec<u8>,
        /// The picture parameter set.
        pps: Vec<u8>,
    },
}

impl VideoTrack {
    /// An H.264 track described by every SPS and PPS of `annexb`, one
    /// access unit in Annex B form (a key frame as an encoder emits it).
    ///
    /// # Errors
    ///
    /// [`RtspError::NotAllowed`] when the access unit carries no SPS or no
    /// PPS.
    pub fn h264_from_key_frame(annexb: &[u8]) -> Result<Self, RtspError> {
        let parameter_sets = h264_parameter_sets(annexb).ok_or(RtspError::NotAllowed {
            detail: "the access unit carries no H.264 SPS and PPS",
        })?;
        Ok(Self::H264 { parameter_sets })
    }

    /// An H.265 track described by the first VPS, SPS and PPS of `annexb`,
    /// one access unit in Annex B form.
    ///
    /// # Errors
    ///
    /// [`RtspError::NotAllowed`] when the access unit lacks one of the
    /// three.
    pub fn h265_from_key_frame(annexb: &[u8]) -> Result<Self, RtspError> {
        let (vps, sps, pps) = h265_parameter_sets(annexb).ok_or(RtspError::NotAllowed {
            detail: "the access unit carries no H.265 VPS, SPS and PPS",
        })?;
        Ok(Self::H265 { vps, sps, pps })
    }
}

/// Why [`RtspPush::send_video`] or [`RtspPush::send_klv`] sent nothing, or
/// not everything.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PushError {
    /// The push is closed, cancelled or has ended: what
    /// [`RtspPush::send_ts`] reports.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// The call names a track the description does not carry: KLV on a
    /// push announced without it, video or KLV on an MPEG-TS push. Nothing
    /// was sent.
    #[error("this push has no {0} track")]
    NoSuchTrack(&'static str),
    /// The access unit holds no NAL unit, or the KLV unit is empty. Nothing
    /// was sent.
    #[error("empty {0}")]
    Empty(&'static str),
}

/// Counters of an [`RtspPush`], over all its tracks.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PushStats {
    /// RTP packets handed to the client's thread.
    pub packets_sent: u64,
    /// Bytes of those packets, RTP header included.
    pub bytes_sent: u64,
    /// RTCP sender reports handed to the client's thread.
    pub sender_reports_sent: u64,
    /// RTCP receiver reports the server sent. A server that sends none
    /// leaves this and the two loss figures at 0.
    pub receiver_reports: u64,
    /// The fraction of packets lost since the report before, in 1/256.
    /// This and [`Self::cumulative_lost`] are the figures of one track:
    /// the one whose RTCP channel carried the last receiver report, as
    /// the last report that named that track gave them. With two tracks
    /// reporting, the pair therefore changes between the tracks' values.
    pub fraction_lost_q8: u8,
    /// The packets lost since the start, for the same track.
    pub cumulative_lost: u32,
    /// Frames the server sent on a channel this side sends media on, read
    /// and dropped.
    pub frames_dropped_inbound: u64,
    /// Frames still queued, not yet handed to the connection, when the
    /// session ended or its TEARDOWN was sent: they were never written.
    pub frames_dropped_at_close: u64,
}

/// What a track carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackKind {
    Mp2t,
    H264,
    H265,
    Klv,
}

/// One announced track.
pub(crate) struct Track {
    kind: TrackKind,
    state: TrackState,
    /// Counts the receiver reports from the track's RTCP channel.
    rtcp: Arc<Mutex<RtcpStats>>,
    /// The interleaved channels of the track's RTP and RTCP: the pair its
    /// SETUP asks for until the reply names the pair to use.
    channels: (u8, u8),
    /// The NTP and RTP times of the last packet sent, which the track's
    /// last sender report repeats.
    last_sent: Option<(u64, u32)>,
    /// The track's SETUP was answered with a reply the client took.
    set_up: bool,
}

impl Track {
    fn new(index: u8, kind: TrackKind) -> Self {
        let payload_type = match kind {
            TrackKind::Mp2t => PT_MP2T,
            TrackKind::H264 | TrackKind::H265 => PT_VIDEO,
            TrackKind::Klv => PT_KLV,
        };
        Self {
            kind,
            state: TrackState::new(payload_type),
            rtcp: Arc::default(),
            channels: (2 * index, 2 * index + 1),
            last_sent: None,
            set_up: false,
        }
    }
}

/// The tracks of an accepted ANNOUNCE, in the order of its description.
pub(crate) struct Announced {
    tracks: Vec<Track>,
}

// By hand: a track's header state has no `Debug`.
impl std::fmt::Debug for Announced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Announced")
            .field("tracks", &self.tracks.len())
            .finish()
    }
}

impl PushDescription {
    /// The description as the SDP writer takes it, and its tracks in SDP
    /// order.
    fn tracks(&self) -> (SdpTracks<'_>, Vec<TrackKind>) {
        match self {
            Self::Mp2t => (SdpTracks::Mp2t, vec![TrackKind::Mp2t]),
            Self::Elementary { video, klv } => {
                let (video, kind) = match video {
                    VideoTrack::H264 { parameter_sets } => {
                        (SdpVideo::H264 { parameter_sets }, TrackKind::H264)
                    }
                    VideoTrack::H265 { vps, sps, pps } => {
                        (SdpVideo::H265 { vps, sps, pps }, TrackKind::H265)
                    }
                };
                let mut kinds = vec![kind];
                if *klv {
                    kinds.push(TrackKind::Klv);
                }
                (SdpTracks::Elementary { video, klv: *klv }, kinds)
            }
        }
    }
}

/// The address a description names as its origin: the control socket's
/// local one, or the unspecified address of the peer's family when the
/// socket did not tell.
fn origin_address(local: Option<IpAddr>, peer: IpAddr) -> IpAddr {
    local.unwrap_or(match peer {
        IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    })
}

const SETUP_BEFORE_ANNOUNCE: &str = "SETUP for recording before ANNOUNCE";
const SETUP_WITHOUT_SESSION: &str = "SETUP reply for recording names no session";
const RECORD_BEFORE_SETUP: &str = "RECORD before every track is set up";

impl RtspClient {
    /// Send ANNOUNCE with `desc` as an SDP body at the client's URL: the
    /// first step of publishing. [`Self::setup_record`] and
    /// [`Self::record`] follow; [`RtspPush::connect`] runs all three.
    ///
    /// A client that has announced publishes only: `describe`, `setup*`
    /// and `play` are refused on it, as is a second `announce`. A client
    /// made by [`Self::connect`] and its siblings sends no keepalive
    /// pings; one made by
    /// [`RtspClientBuilder::connect`](crate::RtspClientBuilder::connect)
    /// does.
    ///
    /// # Errors
    ///
    /// - [`RtspError::NotAllowed`], with nothing written: a description
    ///   whose video track lacks a parameter set, a client that has
    ///   announced or has a play session, or an RTSP 2.0 client (RTSP 2.0
    ///   has no ANNOUNCE).
    /// - [`RtspError::Protocol`] with the status when the server answers
    ///   anything but a `2xx`. A `3xx` is not followed.
    /// - [`RtspError::AuthFailed`], [`RtspError::AuthUnsupported`],
    ///   [`RtspError::Io`], [`RtspError::BadResponse`] and
    ///   [`RtspError::Timeout`] as for [`Self::options`].
    /// - [`RtspError::LocalCancel`] when the cancel handle fires before the
    ///   reply arrives, or is set when the call fails.
    pub fn announce(&mut self, desc: &PushDescription) -> Result<(), RtspError> {
        let (sdp_tracks, kinds) = desc.tracks();
        let origin = origin_address(self.local_ip, self.peer_ip);
        let sdp = write_announce(&sdp_tracks, origin, random_u32())
            .map_err(|detail| self.refused(RtspError::NotAllowed { detail }))?;
        let tracks: Vec<Track> = (0u8..)
            .zip(kinds)
            .map(|(index, kind)| Track::new(index, kind))
            .collect();
        let deadline = self.request_deadline();
        let resp = self.request(
            RequestSpec {
                method: RtspMethod::Announce,
                uri: self.url.render_no_credentials(),
                headers: Vec::new(),
                body: Some(RequestBody {
                    content_type: "application/sdp",
                    bytes: Bytes::from(sdp),
                }),
                announce_tracks: Some(tracks.len() as u8),
                setup: None,
            },
            deadline,
            None,
        )?;
        self.expect_2xx(&resp)?;
        self.push = Some(Announced { tracks });
        Ok(())
    }

    /// Send SETUP for every announced track, in the order of the
    /// description, each over the control connection (`RTP/AVP/TCP`,
    /// `mode=record`) and all on one session. Track `N`'s request URL is
    /// the client's URL followed by `/streamid=N`.
    ///
    /// A URL without `?transport=`, or with `?transport=tcp`, publishes
    /// over the control connection; publishing over UDP is not
    /// implemented.
    ///
    /// # Errors
    ///
    /// - [`RtspError::UnsupportedTransport`], with nothing written, for a
    ///   `?transport=udp` URL.
    /// - [`RtspError::NotAllowed`], with nothing written, before
    ///   [`Self::announce`], and on a second call once a track is set up
    ///   (a call that failed part-way is not resumed: use a fresh client).
    /// - [`RtspError::Protocol`] with the status when the server answers a
    ///   SETUP with anything but a `2xx`.
    /// - [`RtspError::BadResponse`] or [`RtspError::UnsupportedTransport`]
    ///   for a `2xx` reply the client cannot use: no session, a transport
    ///   that is not interleaved or does not parse, one channel for both
    ///   RTP and RTCP, a channel another track uses, or another session
    ///   than the first SETUP's. The server may have made a session for
    ///   that reply, so the client gives the session up before it returns
    ///   the error. If it knows the session's id it sends TEARDOWN (500 ms
    ///   at most); it knows the id only from a SETUP reply it accepted, so
    ///   not when the first track's reply is the one it refuses. In every
    ///   case it then ends its connection and waits up to 2 s for its
    ///   thread, so the server's session ends with the connection at the
    ///   latest. The client is finished after that: every later request
    ///   on it is [`RtspError::Io`].
    /// - The other errors of [`Self::announce`].
    pub fn setup_record(&mut self) -> Result<(), RtspError> {
        if self.url.transport_preference == RtspTransportPref::ForceUdp {
            return Err(self.refused(RtspError::UnsupportedTransport));
        }
        // Out of the client for the loop, which borrows the client for
        // each request and a track beside it.
        let Some(mut announced) = self.push.take() else {
            return Err(self.refused(RtspError::NotAllowed {
                detail: SETUP_BEFORE_ANNOUNCE,
            }));
        };
        let r = self.setup_record_tracks(&mut announced.tracks);
        self.push = Some(announced);
        r
    }

    fn setup_record_tracks(&mut self, tracks: &mut [Track]) -> Result<(), RtspError> {
        let base = self.url.render_no_credentials();
        // Some servers take a track's URL only in exactly this form.
        let base = base.strip_suffix('/').unwrap_or(&base);
        for (index, track) in (0u8..).zip(tracks) {
            let taken = self.setup_record_track(index, track, format!("{base}/streamid={index}"));
            if let Err(e @ (RtspError::BadResponse { .. } | RtspError::UnsupportedTransport)) =
                &taken
            {
                self.give_up_after(e);
            }
            // Media goes out on the pair the server named, which need not
            // be the pair asked for.
            if let Some(accepted) = taken? {
                track.channels = (accepted.rtp_channel, accepted.rtcp_channel);
            }
            track.set_up = true;
        }
        Ok(())
    }

    /// One track's SETUP: the route its reply installed.
    fn setup_record_track(
        &mut self,
        index: u8,
        track: &Track,
        uri: String,
    ) -> Result<Option<SetupAccepted>, RtspError> {
        let (rtp, rtcp) = track.channels;
        let id = RouteId(index);
        // The server's receiver reports for this track arrive on its RTCP
        // channel; no media does.
        let route = RouteSpec {
            id,
            inbound_media: None,
            inbound_rtcp: RtcpSink {
                stats: track.rtcp.clone(),
                ssrc: track.state.ssrc,
            },
        };
        let mut headers = vec![(
            "transport",
            format!("RTP/AVP/TCP;unicast;interleaved={rtp}-{rtcp};mode=record"),
        )];
        // The bare id: some servers refuse the reply's `;timeout=` suffix
        // sent back.
        if let Some(sid) = &self.session_id {
            headers.push(("session", sid.clone()));
        }
        let deadline = self.request_deadline();
        let outcome = self.request_routed(
            RequestSpec {
                method: RtspMethod::Setup,
                uri,
                headers,
                body: None,
                announce_tracks: None,
                setup: Some(SetupRoute {
                    id,
                    kind: RouteKind::Push,
                }),
            },
            Some(route),
            deadline,
            None,
        )?;
        self.expect_2xx(&outcome.response)?;
        // The session is the first SETUP reply's.
        if self.session_id.is_none() {
            let sid = outcome
                .response
                .session_id()
                .ok_or(RtspError::BadResponse {
                    detail: SETUP_WITHOUT_SESSION,
                })?;
            self.session_id = Some(sid.to_string());
        }
        Ok(outcome.setup)
    }

    /// The server answered a SETUP `2xx` with a reply this side cannot
    /// use, and may hold a session for it. With the session's id known it
    /// is told so; the id of a refused first reply is not known, so in
    /// every case the connection is ended, which ends the server's session
    /// with it.
    fn give_up_after(&mut self, e: &RtspError) {
        // First, so that neither the TEARDOWN's answer nor the stop below
        // names another reason.
        record_end(
            &self.end_reason,
            EndReason::ProtocolError {
                msg: format!("unusable SETUP reply: {e}"),
            },
        );
        let _ = self.teardown_within(CLOSE_TEARDOWN_BOUND);
        self.driver.stop();
        let _ = self.driver.join_within(CLOSE_JOIN_BOUND);
    }

    /// Send RECORD: the server starts taking media, and the client becomes
    /// the [`RtspPush`] that sends it. On an error the client is dropped,
    /// which sets its cancel and, with a session open, writes TEARDOWN.
    ///
    /// # Errors
    ///
    /// - [`RtspError::NotAllowed`], with nothing written, on a client
    ///   without a session or with an announced track that is not set up.
    /// - [`RtspError::Protocol`] with the status when the server answers
    ///   anything but a `2xx`.
    /// - The other errors of [`Self::announce`].
    pub fn record(mut self) -> Result<RtspPush, RtspError> {
        match (self.push.take(), self.session_id.clone()) {
            (Some(announced), Some(sid)) if announced.tracks.iter().all(|t| t.set_up) => {
                let uri = self.url.render_no_credentials();
                let headers = vec![("session", sid.clone())];
                let resp = self.exchange(RtspMethod::Record, uri, headers)?;
                self.expect_2xx(&resp)?;
                Ok(RtspPush::new(self, sid, announced.tracks))
            }
            _ => Err(self.refused(RtspError::NotAllowed {
                detail: RECORD_BEFORE_SETUP,
            })),
        }
    }

    /// What a request this side refuses reports: the cancel when it is
    /// set, as for a request that fails on the wire.
    fn refused(&self, e: RtspError) -> RtspError {
        if self.cancel.is_cancelled() {
            RtspError::LocalCancel
        } else {
            e
        }
    }
}
