//! Elementary-shape publish adapter: one H.264 (RFC 6184) video track and
//! an optional KLV (RFC 6597) track, depacketized, aligned onto one PTS
//! line, and re-muxed into MPEG-TS for the mount's two sinks.
//!
//! The pipeline per packet:
//!
//! - Video: [`H264Depacketizer`] → access units. Every AU the
//!   depacketizer emits reports the depacketizer's PTS zero (its RTP
//!   timestamp minus its PTS) to the [`Aligner`] as the video origin, so
//!   both tracks share one PTS line. AUs before the first IDR that reaches the muxer are dropped:
//!   a reader cannot decode them.
//! - KLV: [`KlvDepacketizer`] → KLV units → [`Aligner`], which holds them
//!   until the two tracks' clocks are related (RTCP sender reports, or a
//!   two-second first-packet-coincidence fallback), places each on the
//!   video line, and returns it once a video AU at or past its PTS has
//!   been muxed (KLV muxed ahead of the video would drag the PCR ahead of
//!   the video frames still to come). A unit placed before the video
//!   origin (negative PTS) is dropped: a TS PTS cannot be negative.
//! - Both are pushed into one [`Muxer`] (program 1, PMT PID 0x1000,
//!   video PID 0x100, KLV PID 0x101 as `PrivateData` with a PTS). After
//!   each push the muxer is drained in bundles of at most
//!   [`RTP_PAYLOAD_SIZE`] bytes (7 TS packets), the same framing the MP2T
//!   adapter re-chunks into: PLAY readers get the bundle, the application
//!   transport gets it behind a synthesized 12-byte RTP header (PT 33,
//!   this adapter's own sequence counter, the publisher's video SSRC).
//!
//! The application side's RTP timestamp is the highest PTS pushed so far,
//! truncated to 32 bits: a monotonic 90 kHz value.
//! [`crate::transport::RtpRecvTransport`] does not read it; the TS PES
//! headers carry the real timing.
//!
//! B-frame publishers: RTP timestamps are presentation times sent in
//! decode order, so a B-frame arrives with a PTS below an AU already
//! muxed. Each such AU is counted in `aus_reordered` and still muxed, with
//! its PTS only: the muxer derives no DTS and paces its PCR from the PTS,
//! so the re-muxed TS is not conformant for B-frame streams and a
//! PCR-slaved player may show those frames late. A reorder window that
//! derives DTS is deferred.
//!
//! Nothing here fails outward. A packet that does not parse, carries the
//! wrong payload type, or names an unknown track counts as
//! `malformed_packets`; an AU or KLV unit the muxer refuses (for example
//! `BufferFull` or `InvalidNal`) counts as `aus_dropped` /
//! `klv_units_dropped`, and the adapter carries on.

use std::sync::Arc;
use std::time::Instant;

use tst_core::error::MuxError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

use crate::h264::{H264Au, H264Depacketizer, H264DepayConfig, ParameterSetInjection};
use crate::packet::{RTP_HEADER_LEN, RtpHeader};
use crate::rtcp::SenderReport;
use crate::rtsp::server::mount::RTP_PAYLOAD_SIZE;

use super::adapter::{PublishAdapter, synth_rtp_packet};
use super::align::Aligner;
use super::klv_depacketizer::{KlvDepacketizer, KlvUnit};
use super::mount::{ClockAlignment, PublishMountState};
use super::shape::AnnouncedTrack;

/// TS layout of the re-muxed program.
const PROGRAM_NUMBER: u16 = 1;
const PMT_PID: u16 = 0x1000;
const VIDEO_PID: u16 = 0x100;
const KLV_PID: u16 = 0x101;

/// Muxer queue cap, in TS packets, big enough for the largest AU the
/// H.264 depacketizer can emit (its default `max_au_bytes`, 8 MiB): the
/// muxer's own default (10 000 packets, about 1.84 MB) would refuse every
/// AU between the two caps with `BufferFull`. Sized like the stress
/// harness does, `max(10 000, (programs + 1) × ⌈largest AU / 184⌉)`, the
/// `+ 1` covering PSI and adaptation-field overhead. The queue grows on
/// demand and is drained after every push, so the cap costs no memory
/// until such an AU arrives. After one does, the muxer keeps the memory
/// for the publisher's life: neither its packet queue (about 17 MB at the
/// cap) nor its PES scratch buffer (about 8 MiB) shrinks once drained.
fn muxer_buffer_packets() -> usize {
    let largest_au = H264DepayConfig::default().max_au_bytes;
    10_000.max((1 + 1) * largest_au.div_ceil(184))
}

/// RTCP packet type of a sender report (RFC 3550 §6.4.1).
const RTCP_PT_SR: u8 = 200;

/// The KLV half of an elementary publisher.
struct KlvTrack {
    index: usize,
    payload_type: u8,
    /// The KLV track's current SSRC (see [`latch_ssrc`]).
    ssrc: Option<u32>,
    depay: KlvDepacketizer,
    /// `depay.stats().units_dropped` already folded into the mount stats.
    dropped_seen: u64,
}

/// Latch `ssrc` as a track's current source. Returns `true` when it
/// replaces a different one: a source restart. The first SSRC a track
/// sees, from an RTP packet or a sender report (ffmpeg sends its first
/// report before any media), latches without counting.
fn latch_ssrc(current: &mut Option<u32>, ssrc: u32) -> bool {
    let changed = current.is_some_and(|c| c != ssrc);
    *current = Some(ssrc);
    changed
}

/// Which announced track a packet's `track` index names.
enum Route {
    Video,
    Klv,
}

/// Re-muxes an elementary (H.264, optionally + KLV) publisher into TS.
/// See the [module docs](self).
pub(crate) struct EsAdapter {
    mount: Arc<PublishMountState>,
    video_index: usize,
    video_pt: u8,
    /// The video track's current SSRC (see [`latch_ssrc`]); unlike
    /// `ssrc`, it follows a source restart.
    video_ssrc: Option<u32>,
    h264: H264Depacketizer,
    /// `h264.stats().aus_dropped` already folded into the mount stats.
    h264_dropped_seen: u64,
    klv: Option<KlvTrack>,
    aligner: Aligner,
    /// `aligner.steps()` already folded into the mount stats.
    steps_seen: u64,
    /// `aligner.dropped()` already folded into the mount stats.
    aligner_dropped_seen: u64,
    muxer: Muxer,
    /// An IDR has reached the muxer; AUs before it are dropped.
    seen_keyframe: bool,
    /// Sequence number of the next synthesized application-side packet.
    seq: u16,
    /// SSRC of the synthesized packets: the publisher's video SSRC,
    /// latched from its first video packet and kept for the publisher's
    /// life so the application side sees one continuous source.
    ssrc: Option<u32>,
    /// Highest PTS pushed into the muxer — the synthesized RTP timestamp.
    max_pts: i64,
    /// Highest video PTS pushed into the muxer; an AU below it is a
    /// reordered (B-frame) AU.
    max_video_pts: Option<i64>,
    out: Box<[u8; RTP_PAYLOAD_SIZE]>,
    /// Time source for the aligner's fallback window: `Instant::now`
    /// outside tests.
    now: Box<dyn Fn() -> Instant + Send>,
}

impl EsAdapter {
    /// Build the adapter for `video` (and `klv`, when announced).
    ///
    /// # Errors
    /// The muxer configuration is fixed, so this only fails if the muxer
    /// rejects it — a programming error, answered with 500 by
    /// `handle_announce`.
    pub(crate) fn new(
        mount: Arc<PublishMountState>,
        video: &AnnouncedTrack,
        klv: Option<&AnnouncedTrack>,
    ) -> Result<Self, MuxError> {
        let mut prog = MuxerProgramConfigBuilder::new(PROGRAM_NUMBER, PMT_PID);
        prog.add_video(VIDEO_PID, VideoCodec::H264);
        if klv.is_some() {
            prog.add_klv(KLV_PID, KlvStreamType::PrivateData, true);
        }
        let mut cfg = MuxerConfig::builder();
        cfg.add_program(prog.build())
            .buffer_packets(muxer_buffer_packets());
        Ok(Self::with_muxer(
            mount,
            video,
            klv,
            Muxer::new(cfg.build()?)?,
        ))
    }

    /// Test-only: build with a caller-chosen muxer configuration (for
    /// example a small `buffer_packets`).
    #[cfg(test)]
    fn with_muxer_config(
        mount: Arc<PublishMountState>,
        video: &AnnouncedTrack,
        klv: Option<&AnnouncedTrack>,
        cfg: MuxerConfig,
    ) -> Self {
        Self::with_muxer(mount, video, klv, Muxer::new(cfg).unwrap())
    }

    fn with_muxer(
        mount: Arc<PublishMountState>,
        video: &AnnouncedTrack,
        klv: Option<&AnnouncedTrack>,
        muxer: Muxer,
    ) -> Self {
        let depay = H264DepayConfig {
            payload_type: video.payload_type,
            initial_parameter_sets: video
                .h264_fmtp
                .as_ref()
                .map(|f| f.sprop_parameter_sets.clone())
                .unwrap_or_default(),
            parameter_set_injection: ParameterSetInjection::BeforeIdr,
            ..Default::default()
        };
        // A new publisher starts unaligned (`Pending` with a KLV track,
        // `NotApplicable` without one); a previous publisher's mode must
        // not linger in the mount stats.
        let alignment = if klv.is_some() {
            ClockAlignment::Pending
        } else {
            ClockAlignment::NotApplicable
        };
        mount.tick(|s| s.alignment = alignment);
        Self {
            mount,
            video_index: video.index,
            video_pt: video.payload_type,
            video_ssrc: None,
            h264: H264Depacketizer::new(depay),
            h264_dropped_seen: 0,
            klv: klv.map(|t| KlvTrack {
                index: t.index,
                payload_type: t.payload_type,
                ssrc: None,
                depay: KlvDepacketizer::new(),
                dropped_seen: 0,
            }),
            aligner: Aligner::new(),
            steps_seen: 0,
            aligner_dropped_seen: 0,
            muxer,
            seen_keyframe: false,
            seq: 0,
            ssrc: None,
            max_pts: 0,
            max_video_pts: None,
            out: Box::new([0u8; RTP_PAYLOAD_SIZE]),
            now: Box::new(Instant::now),
        }
    }

    /// Test-only: replace the time source, so a test can advance a fake
    /// clock through the aligner's fallback window.
    #[cfg(test)]
    fn set_clock(&mut self, now: impl Fn() -> Instant + Send + 'static) {
        self.now = Box::new(now);
    }

    fn route(&self, track: usize) -> Option<Route> {
        if track == self.video_index {
            Some(Route::Video)
        } else if self.klv.as_ref().is_some_and(|k| k.index == track) {
            Some(Route::Klv)
        } else {
            None
        }
    }

    /// Push one AU from the depacketizer.
    fn push_au(&mut self, au: H264Au) {
        // The aligner's video origin must be the depacketizer's PTS zero:
        // the RTP timestamp of the first AU it STARTED, which may have
        // been dropped as poisoned (a stream joined mid-FU, a gap in the
        // first AU). The aligner derives it from this AU's timestamp and
        // PTS rather than assuming the first emitted AU sits at PTS 0.
        // Later AUs yield the same zero, unless the depacketizer
        // re-anchored on a new video SSRC.
        self.aligner
            .on_video_au(au.rtp_timestamp, au.pts.as_ticks());
        self.mux_au(au);
        // Held KLV may be due on elapsed time alone (the aligner's
        // fallback window) while only video arrives.
        let placed = self.aligner.poll((self.now)());
        self.place_klv(placed);
    }

    /// Mux one AU, or drop it when it precedes the first IDR or the muxer
    /// refuses it.
    fn mux_au(&mut self, au: H264Au) {
        if !au.key_frame && !self.seen_keyframe {
            self.mount.tick(|s| s.aus_dropped += 1);
            return;
        }
        let pts = au.pts.as_ticks();
        let reordered = self.max_video_pts.is_some_and(|m| pts < m);
        match self.muxer.push_video(&au.annexb, au.pts, au.key_frame) {
            Ok(()) => {
                self.seen_keyframe |= au.key_frame;
                self.aligner.on_video_muxed(pts);
                self.max_pts = self.max_pts.max(pts);
                self.max_video_pts = Some(self.max_video_pts.map_or(pts, |m| m.max(pts)));
                self.mount.tick(|s| {
                    s.aus_emitted += 1;
                    if reordered {
                        s.aus_reordered += 1;
                    }
                });
                self.drain_muxer();
            }
            Err(e) => {
                tracing::debug!(
                    target: "tst_rtp::server::publish",
                    error = ?e,
                    bytes = au.annexb.len(),
                    "muxer refused a published AU; dropped"
                );
                self.mount.tick(|s| s.aus_dropped += 1);
            }
        }
    }

    fn drain_video(&mut self) {
        while let Some(au) = self.h264.next_au() {
            self.push_au(au);
        }
    }

    /// Push one KLV unit the aligner placed at `pts`.
    fn push_klv(&mut self, unit: KlvUnit, pts: i64) {
        if pts < 0 {
            tracing::debug!(
                target: "tst_rtp::server::publish",
                pts = ?pts,
                "KLV unit placed before the video origin; dropped"
            );
            self.mount.tick(|s| s.klv_units_dropped += 1);
            return;
        }
        match self.muxer.push_klv(&unit.bytes, Pts90khz::new(pts), 0) {
            Ok(()) => {
                self.max_pts = self.max_pts.max(pts);
                self.mount.tick(|s| s.klv_units_emitted += 1);
                self.drain_muxer();
            }
            Err(e) => {
                tracing::debug!(
                    target: "tst_rtp::server::publish",
                    error = ?e,
                    bytes = unit.bytes.len(),
                    "muxer refused a published KLV unit; dropped"
                );
                self.mount.tick(|s| s.klv_units_dropped += 1);
            }
        }
    }

    fn place_klv(&mut self, placed: Vec<(KlvUnit, i64)>) {
        for (unit, pts) in placed {
            self.push_klv(unit, pts);
        }
    }

    /// Hand every completed KLV unit to the aligner and push what it
    /// places.
    fn drain_klv(&mut self, now: Instant) {
        while let Some(unit) = self.klv.as_mut().and_then(|k| k.depay.next_unit()) {
            let placed = self.aligner.on_klv_unit(unit, now);
            self.place_klv(placed);
        }
    }

    /// Emit every TS bundle the muxer holds to the mount's sinks.
    fn drain_muxer(&mut self) {
        loop {
            let n = self.muxer.pull(&mut self.out[..]);
            if n == 0 {
                break;
            }
            let app = synth_rtp_packet(
                self.seq,
                self.max_pts as u32,
                self.ssrc.unwrap_or(0),
                &self.out[..n],
            );
            self.seq = self.seq.wrapping_add(1);
            // Readers get a zero-copy slice of the application packet.
            self.mount.emit(app.slice(RTP_HEADER_LEN..), app);
        }
    }

    /// Fold the depacketizers' own drop counters and the aligner's state
    /// into the mount stats.
    fn sync_stats(&mut self) {
        let h264_dropped = self.h264.stats().aus_dropped;
        let dh = h264_dropped - self.h264_dropped_seen;
        self.h264_dropped_seen = h264_dropped;
        let dk = match self.klv.as_mut() {
            Some(k) => {
                let dropped = k.depay.stats().units_dropped;
                let d = dropped - k.dropped_seen;
                k.dropped_seen = dropped;
                d
            }
            None => 0,
        };
        let steps = self.aligner.steps();
        let ds = steps - self.steps_seen;
        self.steps_seen = steps;
        let aligner_dropped = self.aligner.dropped();
        let dk = dk + (aligner_dropped - self.aligner_dropped_seen);
        self.aligner_dropped_seen = aligner_dropped;
        let alignment = if self.klv.is_some() {
            self.aligner.mode()
        } else {
            ClockAlignment::NotApplicable
        };
        self.mount.tick(|s| {
            s.aus_dropped += dh;
            s.klv_units_dropped += dk;
            s.alignment_steps += ds;
            s.alignment = alignment;
        });
    }
}

impl PublishAdapter for EsAdapter {
    fn on_rtp(&mut self, track: usize, packet: &[u8]) {
        self.mount.tick(|s| {
            s.rtp_packets_received += 1;
            s.bytes_received += packet.len() as u64;
        });
        let Some(route) = self.route(track) else {
            tracing::debug!(
                target: "tst_rtp::server::publish",
                track = ?track,
                "RTP for an unannounced track; dropped"
            );
            self.mount.tick(|s| s.malformed_packets += 1);
            return;
        };
        let expected_pt = match route {
            Route::Video => self.video_pt,
            Route::Klv => self.klv.as_ref().map_or(0, |k| k.payload_type),
        };
        let parsed = match RtpHeader::decode(packet) {
            Ok(p) if p.header.payload_type == expected_pt => p,
            Ok(p) => {
                tracing::debug!(
                    target: "tst_rtp::server::publish",
                    pt = ?p.header.payload_type,
                    expected = expected_pt,
                    "elementary publisher packet with unexpected PT; dropped"
                );
                self.mount.tick(|s| s.malformed_packets += 1);
                return;
            }
            Err(e) => {
                tracing::debug!(
                    target: "tst_rtp::server::publish",
                    error = ?e,
                    "unparseable RTP from publisher; dropped"
                );
                self.mount.tick(|s| s.malformed_packets += 1);
                return;
            }
        };
        let payload = &packet[parsed.payload_offset..parsed.payload_end];
        let ssrc = parsed.header.ssrc;
        match route {
            Route::Video => {
                self.ssrc.get_or_insert(ssrc);
                // A source restart: the depacketizer re-anchors on its own
                // (keeping PTS monotonic); the aligner must forget the old
                // source's clock before any of the new one's reports.
                if latch_ssrc(&mut self.video_ssrc, ssrc) {
                    self.aligner.on_video_source_change();
                    self.mount.tick(|s| s.ssrc_changes += 1);
                }
                self.h264.feed(&parsed.header, payload);
                self.drain_video();
            }
            Route::Klv => {
                if let Some(k) = self.klv.as_mut() {
                    if latch_ssrc(&mut k.ssrc, ssrc) {
                        self.aligner.on_klv_source_change();
                        self.mount.tick(|s| s.ssrc_changes += 1);
                    }
                    k.depay.feed(&parsed.header, payload);
                }
                let now = (self.now)();
                self.drain_klv(now);
            }
        }
        self.sync_stats();
    }

    fn on_rtcp(&mut self, track: usize, packet: &[u8]) {
        // Only a sender report steers anything; a compound packet starts
        // with one (RFC 3550 §6.1), and other types are ignored.
        if packet.len() < 2 || packet[1] != RTCP_PT_SR {
            return;
        }
        let sr = match SenderReport::decode(packet) {
            Ok((sr, _)) => sr,
            Err(e) => {
                tracing::debug!(
                    target: "tst_rtp::server::publish",
                    error = ?e,
                    "undecodable RTCP sender report from publisher; ignored"
                );
                return;
            }
        };
        // A report describes its sender's clock: only the track's current
        // source may steer it. A report from any other SSRC (a restarted
        // source whose first RTP packet has not arrived yet) is ignored.
        let current = match self.route(track) {
            Some(Route::Video) => &mut self.video_ssrc,
            Some(Route::Klv) => match self.klv.as_mut() {
                Some(k) => &mut k.ssrc,
                None => return,
            },
            None => return,
        };
        if current.is_some_and(|c| c != sr.ssrc) {
            tracing::debug!(
                target: "tst_rtp::server::publish",
                ssrc = sr.ssrc,
                current = ?*current,
                "RTCP sender report from a foreign SSRC; ignored"
            );
            return;
        }
        current.get_or_insert(sr.ssrc);
        match self.route(track) {
            Some(Route::Video) => self.aligner.on_video_sr(&sr),
            Some(Route::Klv) => self.aligner.on_klv_sr(&sr),
            None => return,
        }
        self.sync_stats();
    }

    fn flush(&mut self) {
        self.drain_video();
        if let Some(au) = self.h264.flush() {
            self.push_au(au);
        }
        self.drain_video();
        let now = (self.now)();
        self.drain_klv(now);
        if let Some(unit) = self.klv.as_mut().and_then(|k| k.depay.flush()) {
            let placed = self.aligner.on_klv_unit(unit, now);
            self.place_klv(placed);
        }
        self.drain_klv(now);
        let placed = self.aligner.drain(now);
        self.place_klv(placed);
        self.drain_muxer();
        self.sync_stats();
    }
}

#[cfg(test)]
mod payload {
    //! Test-only RFC 6184 / RFC 6597 payloader: single-NALU packets,
    //! FU-A fragments, and KLV units.

    use crate::packet::{RTP_HEADER_LEN, RtpHeader};

    pub(super) const SSRC: u32 = 0x1234_5678;

    /// One RTP packet: V=2, no padding/extension/CSRC.
    pub(super) fn rtp(seq: u16, ts: u32, pt: u8, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut h = RtpHeader::new(seq, ts, SSRC);
        h.payload_type = pt;
        h.marker = marker;
        let mut v = vec![0u8; RTP_HEADER_LEN];
        h.encode_into(&mut v);
        v.extend_from_slice(payload);
        v
    }

    /// A synthetic NALU: `header` byte then `len - 1` non-zero body bytes
    /// (no zero byte, so no start-code emulation).
    pub(super) fn nalu(header: u8, len: usize) -> Vec<u8> {
        let mut n = vec![header];
        n.extend((1..len).map(|i| (i % 251) as u8 | 1));
        n
    }

    /// A whole AU as one single-NALU packet (RFC 6184 §5.6), marker set.
    pub(super) fn single(seq: u16, ts: u32, header: u8, len: usize, pt: u8) -> Vec<u8> {
        rtp(seq, ts, pt, true, &nalu(header, len))
    }

    /// A whole AU as FU-A fragments (RFC 6184 §5.8) of at most `frag`
    /// NALU-body bytes each, marker on the last.
    pub(super) fn fragmented(
        seq0: u16,
        ts: u32,
        header: u8,
        len: usize,
        pt: u8,
        frag: usize,
    ) -> Vec<Vec<u8>> {
        let n = nalu(header, len);
        let chunks: Vec<&[u8]> = n[1..].chunks(frag).collect();
        let last = chunks.len() - 1;
        chunks
            .iter()
            .enumerate()
            .map(|(i, body)| {
                let fu_ind = (header & 0x60) | 28;
                let fu_hdr = (u8::from(i == 0) << 7) | (u8::from(i == last) << 6) | (header & 0x1F);
                let mut p = vec![fu_ind, fu_hdr];
                p.extend_from_slice(body);
                rtp(seq0.wrapping_add(i as u16), ts, pt, i == last, &p)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tst_core::mpegts::common::Pts90khz;
    use tst_core::mpegts::demux::{DemuxEvent, Demuxer};
    use tst_core::mpegts::mux::{
        KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
    };
    use tst_core::transport::{RecvTransport, TransportError};

    use super::payload;
    use super::*;
    use crate::rtcp::SenderReport;
    use crate::rtsp::server::publish::adapter::PublishAdapter;
    use crate::rtsp::server::publish::mount::{
        ClockAlignment, PublishMountHandle, PublishMountState,
    };
    use crate::rtsp::server::publish::shape::{AnnouncedTrack, TrackKind};

    const VIDEO_PT: u8 = 96;
    const KLV_PT: u8 = 97;

    fn video_track() -> AnnouncedTrack {
        AnnouncedTrack {
            index: 0,
            control: Some("streamid=0".into()),
            payload_type: VIDEO_PT,
            kind: TrackKind::H264,
            h264_fmtp: None,
        }
    }
    fn klv_track() -> AnnouncedTrack {
        AnnouncedTrack {
            index: 1,
            control: Some("streamid=1".into()),
            payload_type: KLV_PT,
            kind: TrackKind::Klv,
            h264_fmtp: None,
        }
    }
    fn app_transport(m: &Arc<PublishMountState>) -> crate::transport::RtpRecvTransport {
        let mut t = PublishMountHandle { state: m.clone() }
            .into_recv_transport()
            .unwrap();
        t.set_recv_timeout(Some(Duration::from_millis(100)));
        t
    }
    /// A minimal well-formed KLV set: the ST 0601 16-byte UL, a BER short
    /// length, and a 3-byte value ending in `tag` (so units are told apart).
    fn klv_set(tag: u8) -> Vec<u8> {
        let mut v = vec![
            0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00,
            0x00, 0x00,
        ];
        v.extend_from_slice(&[0x03, 0x41, 0x01, tag]);
        v
    }
    /// A KLV unit as one RFC 6597 packet with the marker set.
    fn klv_packet(seq: u16, ts: u32, body: &[u8]) -> Vec<u8> {
        payload::rtp(seq, ts, KLV_PT, true, body)
    }
    fn sr(ntp_secs: u64, ntp_frac: u32, rtp: u32) -> Vec<u8> {
        sr_from(payload::SSRC, ntp_secs, ntp_frac, rtp)
    }
    fn sr_from(ssrc: u32, ntp_secs: u64, ntp_frac: u32, rtp: u32) -> Vec<u8> {
        SenderReport {
            ssrc,
            ntp_timestamp: (ntp_secs << 32) | ntp_frac as u64,
            rtp_timestamp: rtp,
            sender_packet_count: 0,
            sender_octet_count: 0,
            report_blocks: vec![],
        }
        .encode()
        .unwrap()
    }

    /// `pkt` re-stamped with `ssrc` (a restarted source).
    fn with_ssrc(mut pkt: Vec<u8>, ssrc: u32) -> Vec<u8> {
        pkt[8..12].copy_from_slice(&ssrc.to_be_bytes());
        pkt
    }

    /// What the application side demuxed: video sample PTSs (pid 0x100)
    /// and KLV `(pts, payload)` pairs (pid 0x101).
    #[derive(Default)]
    struct Demuxed {
        video: Vec<Pts90khz>,
        klv: Vec<(Pts90khz, Vec<u8>)>,
    }

    /// Read the app transport until it idles, demuxing everything.
    fn demux_app(t: &mut crate::transport::RtpRecvTransport) -> Demuxed {
        let mut dmx = Demuxer::new();
        let mut buf = vec![0u8; 4096];
        loop {
            match t.recv_bytes(&mut buf) {
                Ok(n) => dmx.feed(&buf[..n]).unwrap(),
                Err(TransportError::Backpressure { .. }) => break,
                Err(e) => panic!("app transport: {e:?}"),
            }
        }
        dmx.flush();
        let mut out = Demuxed::default();
        while let Some(ev) = dmx.next_event() {
            match ev {
                DemuxEvent::Sample { stream, pts, .. } if stream.pid == 0x100 => {
                    out.video.push(pts)
                }
                DemuxEvent::Metadata {
                    stream,
                    pts,
                    payload,
                    ..
                } if stream.pid == 0x101 => out.klv.push((pts, payload)),
                _ => {}
            }
        }
        out
    }

    #[test]
    fn h264_only_publisher_is_remuxed_and_starts_on_the_first_idr() {
        let mount = PublishMountState::new("/p", 8);
        let mut t = app_transport(&mount);
        let mut a = EsAdapter::new(mount.clone(), &video_track(), None).unwrap();
        // One P-frame before the IDR (must be dropped), then IDR + 3 P at
        // 3003-tick steps.
        a.on_rtp(0, &payload::single(1, 0, 0x41, 300, VIDEO_PT));
        a.on_rtp(0, &payload::single(2, 3003, 0x65, 400, VIDEO_PT));
        for i in 0..3u16 {
            a.on_rtp(
                0,
                &payload::single(3 + i, 3003 * (2 + i as u32), 0x41, 300, VIDEO_PT),
            );
        }
        a.flush();
        let d = demux_app(&mut t);
        assert_eq!(
            d.video.len(),
            4,
            "pre-IDR P-frame dropped, IDR + 3 P emitted"
        );
        let s = mount.stats_snapshot();
        assert_eq!(s.aus_dropped, 1);
        assert_eq!(s.aus_emitted, 4);
        assert_eq!(s.aus_reordered, 0);
        assert_eq!(s.malformed_packets, 0);
        assert_eq!(s.rtp_packets_received, 5);
    }

    #[test]
    fn a_klv_publisher_starts_pending_and_a_video_only_one_not_applicable() {
        let mount = PublishMountState::new("/p", 8);
        let _a = EsAdapter::new(mount.clone(), &video_track(), Some(&klv_track())).unwrap();
        assert_eq!(mount.stats_snapshot().alignment, ClockAlignment::Pending);
        // The next publisher on the same mount announces video only.
        let mut a = EsAdapter::new(mount.clone(), &video_track(), None).unwrap();
        assert_eq!(
            mount.stats_snapshot().alignment,
            ClockAlignment::NotApplicable
        );
        a.on_rtp(0, &payload::single(1, 0, 0x65, 400, VIDEO_PT));
        assert_eq!(
            mount.stats_snapshot().alignment,
            ClockAlignment::NotApplicable
        );
    }

    #[test]
    fn app_side_packets_are_pt33_with_the_adapters_own_contiguous_sequence() {
        let mount = PublishMountState::new("/p", 8);
        let app_rx = mount.take_app_rx().unwrap();
        let mut a = EsAdapter::new(mount.clone(), &video_track(), None).unwrap();
        for p in payload::fragmented(10, 0, 0x65, 5000, VIDEO_PT, 1000) {
            a.on_rtp(0, &p);
        }
        a.flush();
        let app: Vec<bytes::Bytes> = app_rx.try_iter().collect();
        assert!(app.len() > 1);
        for (i, pkt) in app.iter().enumerate() {
            let p = crate::packet::RtpHeader::decode(pkt).unwrap();
            assert_eq!(p.header.payload_type, crate::packet::RTP_PT_MP2T);
            assert_eq!(p.header.seq, i as u16);
            assert_eq!(p.header.ssrc, payload::SSRC);
            let ts = &pkt[p.payload_offset..p.payload_end];
            assert!(!ts.is_empty() && ts.len() % 188 == 0 && ts.len() <= 1316);
            assert!(crate::transport::is_valid_mp2t_payload(ts));
        }
        assert_eq!(mount.stats_snapshot().frames_emitted, app.len() as u64);
    }

    #[test]
    fn klv_units_land_at_video_relative_pts() {
        let mount = PublishMountState::new("/p", 8);
        let mut t = app_transport(&mount);
        let mut a = EsAdapter::new(mount.clone(), &video_track(), Some(&klv_track())).unwrap();
        // video SR: ntp 100.0 s ↔ rtp 180 000 ; klv SR: ntp 100.5 s ↔ rtp 500 000.
        a.on_rtcp(0, &sr(100, 0, 180_000));
        a.on_rtcp(1, &sr(100, 1 << 31, 500_000));
        // IDR at rtp 90 000 = the video origin (PTS 0 on the video line).
        a.on_rtp(0, &payload::single(1, 90_000, 0x65, 400, VIDEO_PT));
        // A KLV unit at the KLV SR instant (ntp 100.5) = video rtp
        // 180 000 + 45 000 = 225 000 → 135 000 ticks after the origin.
        let body = klv_set(7);
        a.on_rtp(1, &klv_packet(1, 500_000, &body));
        a.flush();
        let d = demux_app(&mut t);
        assert_eq!(d.video.len(), 1);
        assert_eq!(d.klv.len(), 1);
        assert_eq!(d.klv[0].1, body);
        let rel = d.klv[0].0.as_ticks() - d.video[0].as_ticks();
        assert!(
            (rel - 135_000).abs() <= 1,
            "klv pts relative to video = {rel}"
        );
        let s = mount.stats_snapshot();
        assert_eq!((s.klv_units_emitted, s.klv_units_dropped), (1, 0));
        assert_eq!(s.alignment, ClockAlignment::SenderReport);
        assert_eq!(s.alignment_steps, 0);
    }

    #[test]
    fn klv_aligns_to_the_depacketizers_zero_when_the_first_au_is_poisoned() {
        let mount = PublishMountState::new("/p", 8);
        let mut t = app_transport(&mount);
        let mut a = EsAdapter::new(mount.clone(), &video_track(), Some(&klv_track())).unwrap();
        a.on_rtcp(0, &sr(100, 0, 180_000));
        a.on_rtcp(1, &sr(100, 1 << 31, 500_000));
        // Joined mid-FU: the first packet is an IDR's tail fragment (no S
        // bit, E bit + marker) at rtp 90 000. The depacketizer starts an AU
        // there — its PTS zero — and then drops it as poisoned.
        let zero_rtp: i64 = 90_000;
        a.on_rtp(
            0,
            &payload::rtp(1, 90_000, VIDEO_PT, true, &[0x7C, 0x45, 0xAA]),
        );
        // A clean IDR one frame later is the first AU emitted.
        let idr_rtp: i64 = 93_003;
        a.on_rtp(0, &payload::single(2, 93_003, 0x65, 400, VIDEO_PT));
        let body = klv_set(9);
        a.on_rtp(1, &klv_packet(1, 500_000, &body));
        a.flush();
        // Spec: a KLV unit's PTS = (video rtp at the unit's NTP instant) −
        // (rtp of the video PTS zero); the video AU's PTS = its rtp − the
        // same zero. The unit sits at the KLV SR instant, ntp 100.5 s,
        // where the video clock reads 180 000 + 0.5 s × 90 000.
        let klv_video_rtp: i64 = 180_000 + 45_000;
        let expected_rel = (klv_video_rtp - zero_rtp) - (idr_rtp - zero_rtp);
        let d = demux_app(&mut t);
        assert_eq!(d.video.len(), 1);
        assert_eq!(d.klv.len(), 1);
        assert_eq!(d.klv[0].1, body);
        assert_eq!(d.klv[0].0.as_ticks() - d.video[0].as_ticks(), expected_rel);
        let s = mount.stats_snapshot();
        assert_eq!(
            (s.aus_emitted, s.aus_dropped),
            (1, 1),
            "poisoned first AU counted"
        );
    }

    #[test]
    fn default_muxer_takes_an_au_above_its_stock_buffer() {
        // 3 MB > the muxer's stock 10 000-packet (~1.84 MB) queue, < the
        // depacketizer's 8 MiB AU cap.
        let mount = PublishMountState::new("/p", 8);
        let mut a = EsAdapter::new(mount.clone(), &video_track(), None).unwrap();
        for p in payload::fragmented(1, 0, 0x65, 3_000_000, VIDEO_PT, 60_000) {
            a.on_rtp(0, &p);
        }
        a.flush();
        let s = mount.stats_snapshot();
        assert_eq!((s.aus_emitted, s.aus_dropped), (1, 0));
    }

    #[test]
    fn a_3mb_idr_reaches_a_draining_application_whole() {
        // The muxer emits a whole AU as ~2 300 bundles in one burst; the
        // application drains concurrently through a DemuxReceiver, as a
        // real application thread would. Nothing may be dropped.
        use tst_pipeline::{DemuxReceiver, ShellErrorKind};
        const IDR_LEN: usize = 3_000_000;
        let mount = PublishMountState::new("/p", 8);
        let mut t = app_transport(&mount);
        t.set_recv_timeout(Some(Duration::from_secs(5)));
        let reader = std::thread::spawn(move || {
            let mut rx = DemuxReceiver::new(t);
            let mut video: Vec<Vec<u8>> = Vec::new();
            while video.len() < 2 {
                match rx.recv_event() {
                    Ok(Some(DemuxEvent::Sample {
                        stream,
                        payload: tst_core::mpegts::demux::SamplePayload::Video { raw, .. },
                        ..
                    })) if stream.pid == 0x100 => video.push(raw.to_vec()),
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(e) if e.kind == ShellErrorKind::Backpressure => break,
                    Err(e) => panic!("app demux: {e:?}"),
                }
            }
            video
        });
        let mut a = EsAdapter::new(mount.clone(), &video_track(), None).unwrap();
        let idr = payload::fragmented(1, 0, 0x65, IDR_LEN, VIDEO_PT, 1400);
        let next_seq = 1 + idr.len() as u16;
        for p in &idr {
            a.on_rtp(0, p);
        }
        // A P slice completes the IDR's PES for the demuxer, and one more
        // completes the P slice's.
        a.on_rtp(0, &payload::single(next_seq, 3003, 0x41, 300, VIDEO_PT));
        a.on_rtp(0, &payload::single(next_seq + 1, 6006, 0x41, 300, VIDEO_PT));
        let video = reader.join().unwrap();
        let s = mount.stats_snapshot();
        assert_eq!(s.frames_dropped_app, 0, "frames dropped mid-IDR");
        assert_eq!(s.aus_emitted, 3);
        assert!(!video.is_empty(), "no video sample reached the application");
        let nalu = payload::nalu(0x65, IDR_LEN);
        assert!(
            video[0].ends_with(&nalu) && video[0].len() <= IDR_LEN + 64,
            "the IDR sample is {} bytes, not the whole {IDR_LEN}-byte IDR",
            video[0].len()
        );
    }

    #[test]
    fn held_klv_is_placed_and_emitted_on_flush() {
        // No sender reports and the publisher ends inside the fallback
        // window: flush forces first-packet coincidence.
        let mount = PublishMountState::new("/p", 8);
        let mut t = app_transport(&mount);
        let mut a = EsAdapter::new(mount.clone(), &video_track(), Some(&klv_track())).unwrap();
        a.on_rtp(0, &payload::single(1, 1_000, 0x65, 400, VIDEO_PT));
        a.on_rtp(1, &klv_packet(1, 77_000, &klv_set(1)));
        a.on_rtp(1, &klv_packet(2, 77_900, &klv_set(2)));
        let s = mount.stats_snapshot();
        assert_eq!(s.klv_units_emitted, 0, "held");
        assert_eq!(s.alignment, ClockAlignment::Pending);
        a.flush();
        let d = demux_app(&mut t);
        let rel: Vec<i64> = d
            .klv
            .iter()
            .map(|(p, _)| p.as_ticks() - d.video[0].as_ticks())
            .collect();
        assert_eq!(rel, [0, 900]);
        let s = mount.stats_snapshot();
        assert_eq!(s.klv_units_emitted, 2);
        assert_eq!(s.alignment, ClockAlignment::Provisional);
    }

    #[test]
    fn held_klv_is_emitted_once_the_fallback_expires_while_only_video_flows() {
        use std::sync::Mutex;
        // One KLV unit, no sender reports, then video only: the fallback
        // must fire on elapsed time, not wait for another KLV unit.
        let mount = PublishMountState::new("/p", 8);
        let mut t = app_transport(&mount);
        let mut a = EsAdapter::new(mount.clone(), &video_track(), Some(&klv_track())).unwrap();
        let t0 = Instant::now();
        let clock = Arc::new(Mutex::new(t0));
        let c = clock.clone();
        a.set_clock(move || *c.lock().unwrap());
        a.on_rtp(0, &payload::single(1, 1_000, 0x65, 400, VIDEO_PT));
        a.on_rtp(1, &klv_packet(1, 77_000, &klv_set(1)));
        assert_eq!(mount.stats_snapshot().klv_units_emitted, 0, "held");
        // 30 video AUs at 100 ms of simulated time each: 3 s > the window.
        for i in 1..=30u16 {
            *clock.lock().unwrap() = t0 + Duration::from_millis(100 * u64::from(i));
            a.on_rtp(
                0,
                &payload::single(1 + i, 1_000 + 9_000 * u32::from(i), 0x41, 300, VIDEO_PT),
            );
        }
        let s = mount.stats_snapshot();
        assert_eq!(s.klv_units_emitted, 1, "released without any further KLV");
        assert_eq!(s.alignment, ClockAlignment::Provisional);
        let d = demux_app(&mut t);
        assert_eq!(d.klv.len(), 1);
        assert_eq!(d.klv[0].1, klv_set(1));
        assert_eq!(d.klv[0].0.as_ticks() - d.video[0].as_ticks(), 0);
    }

    #[test]
    fn klv_placed_before_the_video_origin_is_dropped_not_muxed() {
        let mount = PublishMountState::new("/p", 8);
        let mut t = app_transport(&mount);
        let mut a = EsAdapter::new(mount.clone(), &video_track(), Some(&klv_track())).unwrap();
        // Both SRs at ntp 100.0: video rtp 90 000 ↔ klv rtp 100 000.
        a.on_rtcp(0, &sr(100, 0, 90_000));
        a.on_rtcp(1, &sr(100, 0, 100_000));
        a.on_rtp(0, &payload::single(1, 90_000, 0x65, 400, VIDEO_PT));
        a.on_rtp(1, &klv_packet(1, 91_000, &klv_set(1))); // 9 000 ticks before the origin
        a.on_rtp(1, &klv_packet(2, 109_000, &klv_set(2))); // 9 000 ticks after
        a.flush();
        let d = demux_app(&mut t);
        assert_eq!(d.klv.len(), 1);
        assert_eq!(d.klv[0].1, klv_set(2));
        assert_eq!(d.klv[0].0.as_ticks() - d.video[0].as_ticks(), 9_000);
        let s = mount.stats_snapshot();
        assert_eq!((s.klv_units_emitted, s.klv_units_dropped), (1, 1));
    }

    #[test]
    fn klv_held_without_video_is_dropped_and_counted_at_flush() {
        // H.264 + KLV announced, but only KLV ever arrives: there is no
        // video line, so flush abandons the held units — and counts them.
        let mount = PublishMountState::new("/p", 8);
        let mut a = EsAdapter::new(mount.clone(), &video_track(), Some(&klv_track())).unwrap();
        a.on_rtp(1, &klv_packet(1, 77_000, &klv_set(1)));
        a.on_rtp(1, &klv_packet(2, 77_900, &klv_set(2)));
        a.on_rtp(1, &klv_packet(3, 78_800, &klv_set(3)));
        assert_eq!(mount.stats_snapshot().klv_units_dropped, 0, "held");
        a.flush();
        let s = mount.stats_snapshot();
        assert_eq!((s.klv_units_emitted, s.klv_units_dropped), (0, 3));
    }

    #[test]
    fn a_klv_ssrc_change_drops_the_held_units_and_restarts_alignment() {
        let mount = PublishMountState::new("/p", 8);
        let mut t = app_transport(&mount);
        let mut a = EsAdapter::new(mount.clone(), &video_track(), Some(&klv_track())).unwrap();
        a.on_rtp(0, &payload::single(1, 1_000, 0x65, 400, VIDEO_PT));
        a.on_rtp(0, &payload::single(2, 4_003, 0x41, 300, VIDEO_PT));
        a.on_rtp(1, &klv_packet(1, 77_000, &klv_set(1)));
        a.on_rtp(1, &klv_packet(2, 77_900, &klv_set(2)));
        assert_eq!(mount.stats_snapshot().klv_units_dropped, 0, "held");
        // The KLV payloader restarts: new SSRC, new random origin.
        a.on_rtp(
            1,
            &with_ssrc(klv_packet(500, 3_000_000_000, &klv_set(3)), 0xBEEF),
        );
        a.on_rtp(
            1,
            &with_ssrc(klv_packet(501, 3_000_000_900, &klv_set(4)), 0xBEEF),
        );
        let s = mount.stats_snapshot();
        assert_eq!(s.ssrc_changes, 1, "one change, however many packets follow");
        assert_eq!(s.klv_units_dropped, 2, "the old source's held units");
        assert_eq!(s.alignment, ClockAlignment::Pending);
        // The fallback places the new source from its own first unit, at
        // the video line's position at the restart (the P-frame, PTS
        // 3 003), not at the line's start.
        a.flush();
        let d = demux_app(&mut t);
        let rel: Vec<i64> = d
            .klv
            .iter()
            .map(|(p, _)| p.as_ticks() - d.video[0].as_ticks())
            .collect();
        assert_eq!(rel, [3_003, 3_903]);
        assert_eq!(mount.stats_snapshot().klv_units_emitted, 2);
    }

    #[test]
    fn a_video_ssrc_change_is_counted_once() {
        let mount = PublishMountState::new("/p", 8);
        let mut a = EsAdapter::new(mount.clone(), &video_track(), None).unwrap();
        a.on_rtp(0, &payload::single(1, 0, 0x65, 400, VIDEO_PT));
        a.on_rtp(0, &payload::single(2, 3_003, 0x41, 300, VIDEO_PT));
        for i in 0..3u16 {
            let p = payload::single(
                100 + i,
                9_000_000 + 3_003 * u32::from(i),
                0x41,
                300,
                VIDEO_PT,
            );
            a.on_rtp(0, &with_ssrc(p, 0xBEEF));
        }
        assert_eq!(mount.stats_snapshot().ssrc_changes, 1);
    }

    #[test]
    fn a_sender_report_from_a_foreign_ssrc_is_ignored() {
        // A new video source's report arriving before its first packet
        // must not feed the current source's clock.
        let mount = PublishMountState::new("/p", 8);
        let mut t = app_transport(&mount);
        let mut a = EsAdapter::new(mount.clone(), &video_track(), Some(&klv_track())).unwrap();
        a.on_rtcp(0, &sr(100, 0, 180_000));
        a.on_rtcp(1, &sr(100, 1 << 31, 500_000));
        a.on_rtp(0, &payload::single(1, 90_000, 0x65, 400, VIDEO_PT));
        a.on_rtp(1, &klv_packet(1, 500_000, &klv_set(1))); // PTS 135 000
        // ntp 101.0 ↔ a new source's rtp 7 000 000: ignored.
        a.on_rtcp(0, &sr_from(0xBEEF, 101, 0, 7_000_000));
        // KLV 509 000 is 100 ms after the unit above: PTS 144 000.
        a.on_rtp(1, &klv_packet(2, 509_000, &klv_set(2)));
        a.flush();
        let d = demux_app(&mut t);
        let rel: Vec<i64> = d
            .klv
            .iter()
            .map(|(p, _)| p.as_ticks() - d.video[0].as_ticks())
            .collect();
        assert_eq!(rel, [135_000, 144_000], "both on the original mapping");
        let s = mount.stats_snapshot();
        assert_eq!(s.alignment, ClockAlignment::SenderReport);
        assert_eq!(s.alignment_steps, 0);
        assert_eq!(s.ssrc_changes, 0);
    }

    /// `(pid, pcr, pts)` for every TS packet in `frames` (application-side
    /// RTP packets, in emission order): the adaptation field's PCR base and
    /// a PES header's PTS, both in 90 kHz ticks, where present. Read from
    /// the bytes by hand: the demuxer reports neither PCR values nor where
    /// in the stream a PES started.
    fn ts_timing(frames: &[bytes::Bytes]) -> Vec<(u16, Option<i64>, Option<i64>)> {
        let mut out = Vec::new();
        for f in frames {
            for p in f[RTP_HEADER_LEN..].chunks(188) {
                let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
                let control = (p[3] >> 4) & 0x3;
                let mut at = 4;
                let mut pcr = None;
                if control & 0x2 != 0 {
                    let len = usize::from(p[4]);
                    if len >= 7 && p[5] & 0x10 != 0 {
                        let b: Vec<i64> = p[6..11].iter().map(|&x| i64::from(x)).collect();
                        pcr = Some(
                            (b[0] << 25) | (b[1] << 17) | (b[2] << 9) | (b[3] << 1) | (b[4] >> 7),
                        );
                    }
                    at = 5 + len;
                }
                let mut pts = None;
                let pusi = p[1] & 0x40 != 0;
                if control & 0x1 != 0
                    && pusi
                    && at + 14 <= p.len()
                    && p[at..at + 3] == [0, 0, 1]
                    && p[at + 7] & 0x80 != 0
                {
                    let b: Vec<i64> = p[at + 9..at + 14].iter().map(|&x| i64::from(x)).collect();
                    pts = Some(
                        (((b[0] >> 1) & 7) << 30)
                            | (b[1] << 22)
                            | ((b[2] >> 1) << 15)
                            | (b[3] << 7)
                            | (b[4] >> 1),
                    );
                }
                out.push((pid, pcr, pts));
            }
        }
        out
    }

    #[test]
    fn klv_stamped_ahead_of_the_video_waits_and_never_drags_the_pcr_ahead() {
        let mount = PublishMountState::new("/p", 8);
        let app_rx = mount.take_app_rx().unwrap();
        let mut a = EsAdapter::new(mount.clone(), &video_track(), Some(&klv_track())).unwrap();
        // One instant on both clocks: KLV RTP == video RTP.
        a.on_rtcp(0, &sr(100, 0, 0));
        a.on_rtcp(1, &sr(100, 0, 0));
        // 60 AUs, 3 003 ticks apart; with every third AU a KLV unit stamped
        // 500 ms (45 000 ticks) ahead of it, the way a capture-stamped KLV
        // source leads the encoder's output.
        let mut klv_sent = 0u16;
        for i in 0..60u32 {
            let header = if i == 0 { 0x65 } else { 0x41 };
            a.on_rtp(
                0,
                &payload::single(1 + i as u16, 3_003 * i, header, 300, VIDEO_PT),
            );
            if i % 3 == 0 {
                klv_sent += 1;
                a.on_rtp(
                    1,
                    &klv_packet(klv_sent, 3_003 * i + 45_000, &klv_set(i as u8)),
                );
            }
        }
        let frames: Vec<bytes::Bytes> = app_rx.try_iter().collect();
        let mut max_video: Option<i64> = None;
        let mut klv_seen = 0;
        for (pid, pcr, pts) in ts_timing(&frames) {
            if let (VIDEO_PID, Some(p)) = (pid, pts) {
                max_video = Some(max_video.map_or(p, |m| m.max(p)));
            }
            if let (KLV_PID, Some(p)) = (pid, pts) {
                klv_seen += 1;
                assert!(
                    max_video.is_some_and(|v| p <= v),
                    "KLV PES at PTS {p} ahead of the video muxed so far ({max_video:?})"
                );
            }
            if let Some(pcr) = pcr {
                let v = max_video.expect("a PCR before any video");
                assert!(
                    pcr <= v + 3_600,
                    "PCR {pcr} more than one 40 ms PCR interval past the video ({v})"
                );
            }
        }
        // A unit leaves once an AU at or past its PTS is muxed. The last AU
        // is at 177 177, which covers units with 3 003·i + 45 000 ≤ 177 177:
        // i = 0, 3, …, 42, fifteen of the twenty.
        assert_eq!(klv_seen, 15);
        // The other five leave at the end of the stream.
        a.flush();
        let s = mount.stats_snapshot();
        assert_eq!(
            (s.klv_units_emitted, s.klv_units_dropped),
            (u64::from(klv_sent), 0)
        );
    }

    #[test]
    fn max_klv_unit_is_the_muxers_klv_ceiling() {
        // The depacketizer's cap must be exactly what this adapter's muxer
        // takes: a unit at the cap muxes, one byte more is KlvTooLarge.
        let mount = PublishMountState::new("/p", 8);
        let mut a = EsAdapter::new(mount, &video_track(), Some(&klv_track())).unwrap();
        let max = crate::rtsp::server::publish::klv_depacketizer::MAX_KLV_UNIT_BYTES;
        a.muxer
            .push_klv(&vec![0u8; max], Pts90khz::new(0), 0)
            .expect("a unit at the cap muxes");
        assert!(matches!(
            a.muxer.push_klv(&vec![0u8; max + 1], Pts90khz::new(0), 0),
            Err(MuxError::KlvTooLarge { .. })
        ));
    }

    #[test]
    fn b_frames_are_counted_as_reordered_and_still_muxed() {
        // Decode order I P B B P B B (presentation 0 3 1 2 6 4 5, in 3003
        // ticks): every B arrives below a PTS already muxed.
        let mount = PublishMountState::new("/p", 8);
        let mut t = app_transport(&mount);
        let mut a = EsAdapter::new(mount.clone(), &video_track(), None).unwrap();
        let order = [0u32, 3, 1, 2, 6, 4, 5];
        for (i, &p) in order.iter().enumerate() {
            let header = if i == 0 { 0x65 } else { 0x41 };
            a.on_rtp(
                0,
                &payload::single(1 + i as u16, 9_000 + 3003 * p, header, 300, VIDEO_PT),
            );
        }
        a.flush();
        let s = mount.stats_snapshot();
        assert_eq!(s.aus_emitted, 7);
        assert_eq!(s.aus_reordered, 4, "the four B-frames");
        assert_eq!(demux_app(&mut t).video.len(), 7);
    }

    #[test]
    fn muxer_buffer_full_drops_the_au_and_continues() {
        let mount = PublishMountState::new("/p", 8);
        let mut t = app_transport(&mount);
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build()).buffer_packets(16);
        let cfg = b.build().unwrap();
        let mut a = EsAdapter::with_muxer_config(mount.clone(), &video_track(), None, cfg);
        // A 20 KB IDR needs > 100 TS packets: BufferFull, dropped.
        let big = payload::fragmented(1, 0, 0x65, 20_000, VIDEO_PT, 1200);
        for p in &big {
            a.on_rtp(0, p);
        }
        let s = mount.stats_snapshot();
        assert_eq!((s.aus_emitted, s.aus_dropped), (0, 1));
        // A 300-byte IDR still goes through: the adapter is alive.
        // Next sequence number, so the depacketizer sees no gap.
        let seq = 1 + big.len() as u16;
        a.on_rtp(0, &payload::single(seq, 3003, 0x65, 300, VIDEO_PT));
        a.flush();
        let s = mount.stats_snapshot();
        assert_eq!((s.aus_emitted, s.aus_dropped), (1, 1));
        assert_eq!(demux_app(&mut t).video.len(), 1);
    }

    #[test]
    fn foreign_pt_and_unknown_tracks_are_malformed_and_rtcp_other_than_sr_is_ignored() {
        let mount = PublishMountState::new("/p", 8);
        let mut a = EsAdapter::new(mount.clone(), &video_track(), Some(&klv_track())).unwrap();
        a.on_rtp(0, &payload::single(1, 0, 0x65, 50, KLV_PT)); // video track, KLV PT
        a.on_rtp(1, &klv_packet(1, 0, &[1])[..3]); // truncated header
        a.on_rtp(5, &payload::single(1, 0, 0x65, 50, VIDEO_PT)); // no such track
        a.on_rtcp(0, &[0x80, 201, 0, 1, 0, 0, 0, 1]); // RR: ignored
        a.on_rtcp(5, &sr(1, 0, 0)); // no such track: ignored
        let s = mount.stats_snapshot();
        assert_eq!(s.malformed_packets, 3);
        assert_eq!((s.aus_emitted, s.aus_dropped), (0, 0));
    }
}
