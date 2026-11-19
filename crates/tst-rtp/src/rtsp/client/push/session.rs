//! [`RtspPush`]: a recording session and its data calls.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Instant, SystemTime};

use bytes::Bytes;
use tst_core::codec::annexb::nals;
use tst_core::mpegts::common::Pts90khz;
use tst_core::transport::{
    BrokenCause, EndReason, SocketStats, Transport, TransportCancel, TransportError,
};

use super::{PUSH_MP2T_MAX_PAYLOAD, PushDescription, PushError, PushStats, Track, TrackKind};
use crate::clock::RtpClock;
use crate::error::RtspError;
use crate::packet::{Chunk, RTP_HEADER_LEN};
use crate::rtcp::sender::{
    PushTimeline, ntp_from_system_time, random_cname, sender_report_compound,
};
use crate::rtsp::client::builder::RtspClientBuilder;
use crate::rtsp::client::media_queue::MediaWaitEnd;
use crate::rtsp::client::{CLOSE_TEARDOWN_BOUND, RtspCancelHandle, RtspClient};
use crate::rtsp::message::RtspMethod;
use crate::transport::{random_u32, record_end};

/// The largest RTP payload of a video packet: with the headers below it,
/// it fits a 1500-byte path.
const VIDEO_MAX_PAYLOAD: usize = 1400;

/// The largest RTP payload one interleaved frame carries (its 16-bit
/// length less the RTP header). A KLV unit up to it travels as one packet:
/// some receivers take each packet of the KLV track as a whole unit.
const KLV_MAX_PAYLOAD: usize = 65_535 - RTP_HEADER_LEN;

const TS_PACKET_LEN: usize = 188;
const TS_SYNC_BYTE: u8 = 0x47;

/// A recording RTSP session: what [`RtspClient::record`] and
/// [`RtspPush::connect`] return. It owns the client, its control
/// connection and its thread, and sends media to the server interleaved
/// on that connection.
///
/// A push announced as [`PushDescription::Mp2t`] takes whole TS packets
/// through [`Self::send_ts`], and is a
/// [`Transport`], so a sender shell
/// can own it. One announced as [`PushDescription::Elementary`] takes
/// access units and KLV units through [`Self::send_video`] and
/// [`Self::send_klv`].
///
/// # Example
///
/// ```no_run
/// use tst_rtp::{PushDescription, RtspClientBuilder, RtspPush};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let ts_packets = [0x47u8; 188];
/// let builder = RtspClientBuilder::new("rtsp://server.example/live")?;
/// let mut push = RtspPush::connect(builder, &PushDescription::Mp2t)?;
/// push.send_ts(&ts_packets)?;
/// push.close()?;
/// # Ok(())
/// # }
/// ```
///
/// # Blocking
///
/// A data call returns once its packets are queued for the client's
/// thread, not once they are on the wire. The queue is bounded: against a
/// server that reads slowly a data call waits for room. The wait ends when
/// room frees, within about 100 ms of a cancel
/// ([`Self::cancel_handle`], from any thread), when the session ends, and
/// at the latest when the server has taken no byte for 5 s, which ends the
/// connection.
///
/// [`Self::close`] and [`Transport::close`] wait for the queued media to
/// be written before they send TEARDOWN, and that wait has the same three
/// ends. A sender shell over a push closes its transport when it is
/// finished and also when it is dropped unfinished, so both wait that way.
/// Dropping the push itself does not wait: it sets the cancel, discards
/// what is queued, writes TEARDOWN without waiting for its reply and
/// leaves the thread to end on its own.
///
/// # Keepalive
///
/// A server may end a session it reads nothing from for its own timeout
/// (10 s on some servers by default), whatever session timeout it
/// advertised. A push that can stay silent that long needs keepalive pings
/// more often than the timeout:
/// [`RtspClientBuilder::keepalive_interval`]. The pings come from the
/// builder: a client made by [`RtspClient::connect`] and its siblings
/// sends none.
///
/// # Pause
///
/// After [`Self::pause`] the data calls still queue and write their
/// packets; some servers end the session on a packet sent while it is
/// paused.
///
/// # How it ends
///
/// [`Self::end_reason`] names the end; the first reason recorded stays. A
/// data call after the end returns:
///
/// - [`TransportError::ExplicitClose`] once the cancel handle has fired,
///   whatever else has happened (the reason recorded is then `Cancelled`
///   unless the session had ended before). The session itself stays open
///   until the push is closed or dropped.
/// - [`TransportError::Closed`] after [`Transport::close`].
/// - [`TransportError::Broken`] with [`BrokenCause::CleanEof`] when the
///   server closed the connection (`CleanTeardown`).
/// - [`TransportError::Broken`] with [`BrokenCause::Unspecified`] when the
///   connection failed or took no byte for 5 s (`TransportFailed`), the
///   server answered a keepalive ping `454` (`SessionExpired`), or the
///   server broke the message framing (`ProtocolError`).
///
/// # Send
///
/// This type is `Send` and not `Sync`: one thread sends, and another ends
/// a waiting send through the cancel handle.
pub struct RtspPush {
    client: RtspClient,
    session_id: String,
    tracks: Vec<Track>,
    /// The media timeline the elementary tracks share, from the first
    /// data call on.
    timeline: Option<PushTimeline>,
    /// The RTP timestamps of an MPEG-TS track.
    clock: RtpClock,
    cname: String,
    packets_sent: u64,
    bytes_sent: u64,
    sender_reports_sent: u64,
    paused: bool,
    /// [`Transport::close`] has run.
    closed: bool,
    /// Added to the time the report schedule reads.
    #[cfg(test)]
    pub(super) clock_skew: std::time::Duration,
}

impl std::fmt::Debug for RtspPush {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtspPush")
            .field("session_id", &self.session_id)
            .field("tracks", &self.tracks.len())
            .field("paused", &self.paused)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

/// A whole `$` frame on `channel`: `parts`, one after the other.
fn frame(channel: u8, parts: &[&[u8]]) -> Bytes {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    debug_assert!(len <= usize::from(u16::MAX), "a frame of {len} bytes");
    let mut out = Vec::with_capacity(4 + len);
    out.push(b'$');
    out.push(channel);
    out.extend_from_slice(&(len as u16).to_be_bytes());
    for part in parts {
        out.extend_from_slice(part);
    }
    Bytes::from(out)
}

fn broken(msg: impl Into<String>, cause: BrokenCause) -> TransportError {
    TransportError::Broken {
        msg: msg.into(),
        errno_code: None,
        cause,
    }
}

impl RtspPush {
    pub(super) fn new(client: RtspClient, session_id: String, tracks: Vec<Track>) -> Self {
        Self {
            client,
            session_id,
            tracks,
            timeline: None,
            clock: RtpClock::new(random_u32()),
            cname: random_cname(),
            packets_sent: 0,
            bytes_sent: 0,
            sender_reports_sent: 0,
            paused: false,
            closed: false,
            #[cfg(test)]
            clock_skew: std::time::Duration::ZERO,
        }
    }

    /// Connect, ANNOUNCE `desc`, SETUP every track and RECORD, in one
    /// call: [`RtspClientBuilder::connect`], then
    /// [`RtspClient::announce`], [`RtspClient::setup_record`] and
    /// [`RtspClient::record`].
    ///
    /// # Errors
    ///
    /// The first error of those four calls. The connection is closed on
    /// any of them; with a session open, TEARDOWN is written first.
    pub fn connect(builder: RtspClientBuilder, desc: &PushDescription) -> Result<Self, RtspError> {
        let mut client = builder.connect()?;
        client.announce(desc)?;
        client.setup_record()?;
        client.record()
    }

    /// Send whole TS packets as one RTP packet on an MPEG-TS push: `ts` is
    /// a multiple of 188 bytes, at most [`PUSH_MP2T_MAX_PAYLOAD`], and
    /// each packet starts with the sync byte `0x47`.
    ///
    /// # Errors
    ///
    /// Once the push is closed, cancelled or has ended: as the type's
    /// "How it ends" lists. On an open push, with nothing sent:
    ///
    /// - [`TransportError::TooLarge`] for more than
    ///   [`PUSH_MP2T_MAX_PAYLOAD`] bytes.
    /// - [`TransportError::Broken`] for a payload that is empty, not whole
    ///   TS packets, or holds a packet without the sync byte, and for any
    ///   payload on a push that announced elementary streams. These leave
    ///   the session open.
    pub fn send_ts(&mut self, ts: &[u8]) -> Result<(), TransportError> {
        self.check_open()?;
        let Some(at) = self.track(|kind| kind == TrackKind::Mp2t) else {
            return Err(broken(
                "this push announced elementary streams and takes no MPEG-TS",
                BrokenCause::Unspecified,
            ));
        };
        if ts.len() > PUSH_MP2T_MAX_PAYLOAD {
            return Err(TransportError::TooLarge {
                len: ts.len(),
                max: PUSH_MP2T_MAX_PAYLOAD,
            });
        }
        let whole_packets = !ts.is_empty()
            && ts
                .chunks(TS_PACKET_LEN)
                .all(|p| p.len() == TS_PACKET_LEN && p[0] == TS_SYNC_BYTE);
        if !whole_packets {
            return Err(broken(
                "an MPEG-TS payload is whole 188-byte packets, each starting 0x47",
                BrokenCause::Unspecified,
            ));
        }
        // A wall-clock pair: the payload's own clock is the PCR inside it.
        let ntp = ntp_from_system_time(SystemTime::now());
        let rtp = self.clock.now_ticks();
        self.send_packets(at, ntp, rtp, [(ts, false)])
    }

    /// Send one video access unit on an elementary push. `annexb` is the
    /// access unit in Annex B form (NAL units behind start codes) and
    /// `pts` its presentation time; successive calls are in decoding
    /// order. A NAL unit over 1400 bytes is fragmented. The parameter sets
    /// travel only where the access unit carries them.
    ///
    /// # Errors
    ///
    /// - [`PushError::Transport`] once the push is closed, cancelled or
    ///   has ended: as the type's "How it ends" lists. The cancel or the
    ///   session's end during the call can leave the access unit sent in
    ///   part.
    /// - [`PushError::NoSuchTrack`] on an MPEG-TS push, with nothing sent.
    /// - [`PushError::Empty`] for an access unit with no NAL unit, with
    ///   nothing sent.
    pub fn send_video(&mut self, annexb: &[u8], pts: Pts90khz) -> Result<(), PushError> {
        self.check_open()?;
        let at = self
            .track(|kind| matches!(kind, TrackKind::H264 | TrackKind::H265))
            .ok_or(PushError::NoSuchTrack("video"))?;
        let chunks = match self.tracks[at].kind {
            TrackKind::H265 => crate::h265::packetizer::packetize(nals(annexb), VIDEO_MAX_PAYLOAD),
            _ => crate::h264::packetizer::packetize(nals(annexb), VIDEO_MAX_PAYLOAD),
        };
        if chunks.is_empty() {
            return Err(PushError::Empty("access unit"));
        }
        Ok(self.send_unit(at, pts, &chunks)?)
    }

    /// Send one KLV unit on an elementary push announced with KLV. `klv`
    /// is the whole unit (a local or universal set, without a metadata
    /// access unit header) and `pts` its presentation time, on the
    /// timeline of the video's. A unit up to 65 523 bytes is one RTP
    /// packet; a larger one is split.
    ///
    /// # Errors
    ///
    /// - [`PushError::Transport`] once the push is closed, cancelled or
    ///   has ended: as the type's "How it ends" lists.
    /// - [`PushError::NoSuchTrack`] on a push without a KLV track, with
    ///   nothing sent.
    /// - [`PushError::Empty`] for an empty unit, with nothing sent.
    pub fn send_klv(&mut self, klv: &[u8], pts: Pts90khz) -> Result<(), PushError> {
        self.check_open()?;
        let at = self
            .track(|kind| kind == TrackKind::Klv)
            .ok_or(PushError::NoSuchTrack("KLV"))?;
        if klv.is_empty() {
            return Err(PushError::Empty("KLV unit"));
        }
        let chunks = crate::klv::packetize(klv, KLV_MAX_PAYLOAD);
        Ok(self.send_unit(at, pts, &chunks)?)
    }

    /// Pause the recording: a last sender report per track is queued, the
    /// call waits until the queued media is written (see the type's
    /// "Blocking"), and then PAUSE is sent. [`Self::resume`] records
    /// again.
    ///
    /// # Errors
    ///
    /// - [`RtspError::LocalCancel`] when the cancel handle is set or fires
    ///   during the wait or before the reply; PAUSE is not sent once the
    ///   wait has seen it.
    /// - [`RtspError::NotAllowed`], with nothing written, on a push that
    ///   is already paused.
    /// - [`RtspError::Protocol`] with the status when the server answers
    ///   anything but a `2xx`; the session is left as it is.
    /// - [`RtspError::Io`], [`RtspError::Timeout`] and the other errors of
    ///   a request.
    pub fn pause(&mut self) -> Result<(), RtspError> {
        // Before the request: some servers close the connection on a frame
        // that follows a PAUSE.
        if self.flush() == Err(MediaWaitEnd::Cancelled) {
            return Err(RtspError::LocalCancel);
        }
        self.request(RtspMethod::Pause)?;
        self.paused = true;
        Ok(())
    }

    /// Send RECORD again on a paused push.
    ///
    /// # Errors
    ///
    /// - [`RtspError::NotAllowed`], with nothing written, on a push that
    ///   is not paused.
    /// - [`RtspError::Protocol`] with the status when the server answers
    ///   anything but a `2xx`; the session is left as it is.
    /// - [`RtspError::LocalCancel`], [`RtspError::Io`],
    ///   [`RtspError::Timeout`] and the other errors of a request.
    pub fn resume(&mut self) -> Result<(), RtspError> {
        self.request(RtspMethod::Record)?;
        self.paused = false;
        Ok(())
    }

    /// End the session and the client: queue a last sender report per
    /// track, wait for the queued media to be written (see the type's
    /// "Blocking"), then [`RtspClient::close`]: TEARDOWN with one 500 ms
    /// bound over writing it and its reply, then up to 2 s for the
    /// client's thread to end.
    ///
    /// A wait ended by the cancel or by the session's end goes straight on
    /// to the TEARDOWN, and what was still queued is dropped.
    ///
    /// The end reason it leaves, unless one was recorded before, is
    /// `CleanTeardown` when the server accepts the TEARDOWN (`2xx` or
    /// `454`) on a push that was not cancelled, and `Cancelled` otherwise.
    ///
    /// # Errors
    ///
    /// The TEARDOWN's result, as [`RtspClient::close`] reports it: a
    /// refusal is [`RtspError::Protocol`] with its status, a cancelled
    /// push [`RtspError::LocalCancel`] (the TEARDOWN is still written).
    /// The client's thread is stopped on every outcome.
    pub fn close(mut self) -> Result<(), RtspError> {
        if !self.closed {
            let _ = self.flush();
        }
        self.client.close()
    }

    /// A handle that ends a waiting data call, `pause` or `close` from
    /// any thread. A cancelled push sends nothing more; closing or
    /// dropping it still writes TEARDOWN.
    pub fn cancel_handle(&self) -> RtspCancelHandle {
        self.client.cancel_handle()
    }

    /// False once the session is known dead: the control connection
    /// ended, or a keepalive ping was answered `454`.
    pub fn is_session_alive(&self) -> bool {
        self.client.is_session_alive()
    }

    /// Why the session ended, once it has; the first reason recorded
    /// stays. See the type's "How it ends".
    pub fn end_reason(&self) -> Option<EndReason> {
        self.client.end_reason.get()
    }

    /// The push's counters now.
    pub fn stats(&self) -> PushStats {
        let mut stats = PushStats {
            packets_sent: self.packets_sent,
            bytes_sent: self.bytes_sent,
            sender_reports_sent: self.sender_reports_sent,
            frames_dropped_inbound: self.client.frames_dropped_inbound.load(Ordering::Relaxed),
            frames_dropped_at_close: self.client.driver.frames_dropped_at_close(),
            ..PushStats::default()
        };
        let mut latest = None;
        for track in &self.tracks {
            let Ok(rtcp) = track.rtcp.lock() else {
                continue;
            };
            stats.receiver_reports += rtcp.rr_packets_received;
            if rtcp.last_rr_ts.is_some() && rtcp.last_rr_ts >= latest {
                latest = rtcp.last_rr_ts;
                stats.fraction_lost_q8 = rtcp.fraction_lost_q8;
                stats.cumulative_lost = rtcp.cumulative_lost_send;
            }
        }
        stats
    }

    /// The session id the server named at SETUP.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The client under the push, for a test that watches its queue or
    /// keeps its end reason past `close`.
    #[cfg(test)]
    pub(super) fn client(&self) -> &RtspClient {
        &self.client
    }

    /// The first track of a kind `wanted` accepts.
    fn track(&self, wanted: impl Fn(TrackKind) -> bool) -> Option<usize> {
        self.tracks.iter().position(|t| wanted(t.kind))
    }

    /// The time the report schedule reads.
    #[cfg(not(test))]
    fn now(&self) -> Instant {
        Instant::now()
    }

    #[cfg(test)]
    fn now(&self) -> Instant {
        Instant::now() + self.clock_skew
    }

    /// Refuse a data call on a push the caller has closed or cancelled.
    fn check_open(&self) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        if self.client.cancel.is_cancelled() {
            return Err(self.cancelled());
        }
        Ok(())
    }

    /// The caller's cancel ended a data call. The caller's thread records
    /// it: the client's thread does not act on the cancel.
    fn cancelled(&self) -> TransportError {
        record_end(&self.client.end_reason, EndReason::Cancelled);
        TransportError::ExplicitClose
    }

    /// What a data call reports when the media queue did not take its
    /// frame.
    fn refusal(&self, end: MediaWaitEnd) -> TransportError {
        if end == MediaWaitEnd::Cancelled || self.client.cancel.is_cancelled() {
            return self.cancelled();
        }
        match self.client.end_reason.get() {
            // The queue closed with nothing recorded: the client's thread
            // is gone.
            None => TransportError::Closed,
            Some(EndReason::Cancelled) => TransportError::ExplicitClose,
            Some(EndReason::CleanTeardown) => {
                broken("the server closed the connection", BrokenCause::CleanEof)
            }
            Some(EndReason::SessionExpired) => broken(
                "the server no longer knows the session",
                BrokenCause::Unspecified,
            ),
            Some(
                EndReason::TransportFailed { msg }
                | EndReason::KeepaliveFailed { msg }
                | EndReason::ProtocolError { msg },
            ) => broken(msg, BrokenCause::Unspecified),
            Some(other) => broken(
                format!("the session ended: {other:?}"),
                BrokenCause::Unspecified,
            ),
        }
    }

    /// Hand one frame to the client's thread, waiting while its queue is
    /// full.
    fn queue(&self, frame: Bytes) -> Result<(), TransportError> {
        self.client
            .driver
            .send_media(frame, &self.client.cancel)
            .map_err(|end| self.refusal(end))
    }

    /// A sender report of track `at` that pairs `ntp` with `rtp`.
    fn report(&self, at: usize, ntp: u64, rtp: u32) -> Bytes {
        let track = &self.tracks[at];
        let (packets, octets) = track.state.counts();
        let report =
            sender_report_compound(track.state.ssrc, ntp, rtp, packets, octets, &self.cname);
        frame(track.channels.1, &[report.as_slice()])
    }

    /// One unit of an elementary track, stamped with `pts` on the push's
    /// timeline.
    fn send_unit(
        &mut self,
        at: usize,
        pts: Pts90khz,
        chunks: &[Chunk],
    ) -> Result<(), TransportError> {
        let pts = pts.as_ticks();
        // Both tracks' reports pair their RTP times with one clock, so the
        // server can place a KLV unit against the video.
        let ntp = self
            .timeline
            .get_or_insert_with(|| PushTimeline::new(SystemTime::now()))
            .ntp_at(pts);
        let rtp = self.tracks[at].state.rtp_timestamp(pts);
        self.send_packets(
            at,
            ntp,
            rtp,
            chunks.iter().map(|c| (c.payload.as_slice(), c.marker)),
        )
    }

    /// Queue `packets` (payload, marker) of track `at`, all at RTP time
    /// `rtp`, behind a sender report when the track's is due.
    fn send_packets<'a>(
        &mut self,
        at: usize,
        ntp: u64,
        rtp: u32,
        packets: impl IntoIterator<Item = (&'a [u8], bool)>,
    ) -> Result<(), TransportError> {
        // The first report goes ahead of the first packet: servers time a
        // track's first unit from it, and some drop every packet before it.
        let now = self.now();
        if self.tracks[at].state.report_due(now) {
            self.queue(self.report(at, ntp, rtp))?;
            self.sender_reports_sent += 1;
            self.tracks[at].state.mark_reported(now);
        }
        for (payload, marker) in packets {
            let track = &mut self.tracks[at];
            let header = track.state.next_header(rtp, marker, payload.len());
            track.last_sent = Some((ntp, rtp));
            let channel = track.channels.0;
            self.queue(frame(channel, &[header.as_slice(), payload]))?;
            self.packets_sent += 1;
            self.bytes_sent += (RTP_HEADER_LEN + payload.len()) as u64;
        }
        Ok(())
    }

    /// Queue a last sender report for every track that has sent, then
    /// wait until the queue and the connection hold no unsent media. A
    /// paused push sent its reports when it paused.
    fn flush(&mut self) -> Result<(), MediaWaitEnd> {
        if !self.paused {
            for at in 0..self.tracks.len() {
                let Some((ntp, rtp)) = self.tracks[at].last_sent else {
                    continue;
                };
                self.client
                    .driver
                    .send_media(self.report(at, ntp, rtp), &self.client.cancel)?;
                self.sender_reports_sent += 1;
            }
        }
        self.client.driver.drain_media(&self.client.cancel)
    }

    /// `method` with the session's id; any `2xx` is success.
    fn request(&mut self, method: RtspMethod) -> Result<(), RtspError> {
        let uri = self.client.url.render_no_credentials();
        let headers = vec![("session", self.session_id.clone())];
        let resp = self.client.exchange(method, uri, headers)?;
        self.client.expect_2xx(&resp)
    }
}

/// A push announced as [`PushDescription::Mp2t`] under a sender shell.
/// `send_bytes` is [`RtspPush::send_ts`] and `max_payload` is
/// [`PUSH_MP2T_MAX_PAYLOAD`]. A push that announced elementary streams
/// refuses every `send_bytes` with [`TransportError::Broken`].
///
/// `socket_stats` reports `bytes_sent` and `packets_sent` as
/// [`PushStats`] does, and `packets_lost_send` from
/// [`PushStats::cumulative_lost`]; every other field is 0.
impl Transport for RtspPush {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        self.send_ts(msg)
    }

    fn max_payload(&self) -> usize {
        PUSH_MP2T_MAX_PAYLOAD
    }

    /// False after `close`, after a cancel, and once the session is known
    /// dead.
    fn is_alive(&self) -> bool {
        !self.closed && !self.client.cancel.is_cancelled() && self.client.is_session_alive()
    }

    /// End the session: queue a last sender report per track, wait for the
    /// queued media to be written (see the type's "Blocking"), send
    /// TEARDOWN with one 500 ms bound over writing it and its reply, then
    /// tell the client's thread to stop without waiting for it. Later
    /// data calls return [`TransportError::Closed`]. A second call does
    /// nothing.
    fn close(&mut self) {
        if self.closed {
            return;
        }
        let _ = self.flush();
        let _ = self.client.teardown_within(CLOSE_TEARDOWN_BOUND);
        // A TEARDOWN the server accepted has recorded the clean end; any
        // other outcome is the caller ending the session.
        record_end(&self.client.end_reason, EndReason::Cancelled);
        self.client.driver.stop();
        self.closed = true;
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        Some(Arc::new(self.client.cancel.clone()))
    }

    fn socket_stats(&self) -> Option<SocketStats> {
        if self.closed {
            return None;
        }
        let stats = self.stats();
        // `SocketStats` is `#[non_exhaustive]` in another crate: it can
        // only be built from its default.
        let mut s = SocketStats::default();
        s.bytes_sent = stats.bytes_sent;
        s.packets_sent = stats.packets_sent;
        s.packets_lost_send = u64::from(stats.cumulative_lost);
        Some(s)
    }
}
