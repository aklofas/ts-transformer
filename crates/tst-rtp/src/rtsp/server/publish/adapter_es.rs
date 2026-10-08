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
//!   two-second first-packet-coincidence fallback) and then returns each
//!   with a PTS on the video line. A unit placed before the video origin
//!   (negative PTS) is dropped: a TS PTS cannot be negative.
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
/// until such an AU arrives.
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
    depay: KlvDepacketizer,
    /// `depay.stats().units_dropped` already folded into the mount stats.
    dropped_seen: u64,
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
    h264: H264Depacketizer,
    /// `h264.stats().aus_dropped` already folded into the mount stats.
    h264_dropped_seen: u64,
    klv: Option<KlvTrack>,
    aligner: Aligner,
    /// `aligner.steps()` already folded into the mount stats.
    steps_seen: u64,
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
        // A new publisher starts unaligned; a previous publisher's mode
        // must not linger in the mount stats.
        mount.tick(|s| s.alignment = ClockAlignment::NotApplicable);
        Self {
            mount,
            video_index: video.index,
            video_pt: video.payload_type,
            h264: H264Depacketizer::new(depay),
            h264_dropped_seen: 0,
            klv: klv.map(|t| KlvTrack {
                index: t.index,
                payload_type: t.payload_type,
                depay: KlvDepacketizer::new(),
                dropped_seen: 0,
            }),
            aligner: Aligner::new(),
            steps_seen: 0,
            muxer,
            seen_keyframe: false,
            seq: 0,
            ssrc: None,
            max_pts: 0,
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
        // first AU). Derive it from this AU's timestamp and PTS rather
        // than assuming the first emitted AU sits at PTS 0. Only the first
        // call takes effect; later ones yield the same value.
        self.aligner
            .on_video_au(au.rtp_timestamp.wrapping_sub(au.pts.as_ticks() as u32));
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
        match self.muxer.push_video(&au.annexb, au.pts, au.key_frame) {
            Ok(()) => {
                self.seen_keyframe |= au.key_frame;
                self.max_pts = self.max_pts.max(au.pts.as_ticks());
                self.mount.tick(|s| s.aus_emitted += 1);
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
        match route {
            Route::Video => {
                self.ssrc.get_or_insert(parsed.header.ssrc);
                self.h264.feed(&parsed.header, payload);
                self.drain_video();
            }
            Route::Klv => {
                if let Some(k) = self.klv.as_mut() {
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
        let placed = self.aligner.drain();
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
        SenderReport {
            ssrc: payload::SSRC,
            ntp_timestamp: (ntp_secs << 32) | ntp_frac as u64,
            rtp_timestamp: rtp,
            sender_packet_count: 0,
            sender_octet_count: 0,
            report_blocks: vec![],
        }
        .encode()
        .unwrap()
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
        assert_eq!(s.malformed_packets, 0);
        assert_eq!(s.rtp_packets_received, 5);
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
    fn held_klv_is_placed_and_emitted_on_flush() {
        // No sender reports and the publisher ends inside the fallback
        // window: flush forces first-packet coincidence.
        let mount = PublishMountState::new("/p", 8);
        let mut t = app_transport(&mount);
        let mut a = EsAdapter::new(mount.clone(), &video_track(), Some(&klv_track())).unwrap();
        a.on_rtp(0, &payload::single(1, 1_000, 0x65, 400, VIDEO_PT));
        a.on_rtp(1, &klv_packet(1, 77_000, &klv_set(1)));
        a.on_rtp(1, &klv_packet(2, 77_900, &klv_set(2)));
        assert_eq!(mount.stats_snapshot().klv_units_emitted, 0, "held");
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
