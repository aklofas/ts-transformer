//! Sender-side RTCP and RTP header state for a track the client sends.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::packet::RtpHeader;
use crate::rtcp::{SdesPacket, SenderReport};
use crate::transport::random_u32;

/// How often a track sends a sender report.
pub(crate) const SR_INTERVAL: Duration = Duration::from_secs(5);

/// Seconds from 1900-01-01 to 1970-01-01.
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
const CLOCK_RATE: i128 = 90_000;

/// RFC 3550 §4: 32.32 fixed point seconds since 1900. A time before 1970
/// reads as the Unix epoch.
pub(crate) fn ntp_from_system_time(t: SystemTime) -> u64 {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs().wrapping_add(NTP_UNIX_OFFSET) & 0xFFFF_FFFF;
    let frac = (u64::from(d.subsec_nanos()) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

/// An SR followed by an SDES CNAME, as one compound packet. The SR comes
/// first: a receiver reads the first packet of a compound as the report.
///
/// # Panics
///
/// If `cname` is longer than 255 bytes.
pub(crate) fn sender_report_compound(
    ssrc: u32,
    ntp: u64,
    rtp: u32,
    packets: u32,
    octets: u32,
    cname: &str,
) -> Vec<u8> {
    let sr = SenderReport {
        ssrc,
        ntp_timestamp: ntp,
        rtp_timestamp: rtp,
        sender_packet_count: packets,
        sender_octet_count: octets,
        report_blocks: Vec::new(),
    };
    let sdes = SdesPacket {
        ssrc,
        cname: cname.to_owned(),
    };
    // No report blocks and a CNAME of 16 characters or a caller's short
    // string: neither length field can overflow.
    let mut out = sr.encode().expect("an SR without report blocks encodes");
    out.extend(sdes.encode().expect("the CNAME is at most 255 bytes"));
    out
}

/// 16 lower-case hex digits.
pub(crate) fn random_cname() -> String {
    format!("{:08x}{:08x}", random_u32(), random_u32())
}

/// The media timeline of one push: both elementary tracks share it, so
/// equal presentation times carry equal NTP times in their reports.
pub(crate) struct PushTimeline {
    ntp_base: SystemTime,
    pts_anchor: Option<i64>,
}

impl PushTimeline {
    pub(crate) fn new(ntp_base: SystemTime) -> Self {
        Self {
            ntp_base,
            pts_anchor: None,
        }
    }

    /// The NTP time of `pts` (90 kHz ticks). The first call fixes the
    /// anchor: its `pts` is `ntp_base`; an earlier `pts` maps before it.
    pub(crate) fn ntp_at(&mut self, pts: i64) -> u64 {
        let anchor = *self.pts_anchor.get_or_insert(pts);
        let delta = ((i128::from(pts) - i128::from(anchor)) << 32) / CLOCK_RATE;
        (i128::from(ntp_from_system_time(self.ntp_base)) + delta) as u64
    }
}

/// One track's RTP header state and report schedule.
pub(crate) struct TrackState {
    pub ssrc: u32,
    pub payload_type: u8,
    seq: u16,
    rtp_base: u32,
    packets: u32,
    octets: u32,
    last_sr: Option<Instant>,
}

impl TrackState {
    pub(crate) fn new(payload_type: u8) -> Self {
        Self {
            ssrc: random_u32(),
            payload_type,
            seq: random_u32() as u16,
            rtp_base: random_u32(),
            packets: 0,
            octets: 0,
            last_sr: None,
        }
    }

    /// The low 32 bits of `pts` on top of the random base.
    pub(crate) fn rtp_timestamp(&self, pts: i64) -> u32 {
        self.rtp_base.wrapping_add(pts as u32)
    }

    /// True before the first report and `SR_INTERVAL` after the last.
    pub(crate) fn report_due(&self, now: Instant) -> bool {
        self.last_sr
            .is_none_or(|t| now.saturating_duration_since(t) >= SR_INTERVAL)
    }

    pub(crate) fn mark_reported(&mut self, now: Instant) {
        self.last_sr = Some(now);
    }

    /// The 12-byte header for the next packet; advances the sequence
    /// number and the packet and octet counts.
    pub(crate) fn next_header(
        &mut self,
        rtp_timestamp: u32,
        marker: bool,
        payload_len: usize,
    ) -> [u8; 12] {
        let mut h = RtpHeader::new(self.seq, rtp_timestamp, self.ssrc);
        h.marker = marker;
        h.payload_type = self.payload_type;
        let mut buf = [0u8; 12];
        h.encode_into(&mut buf);
        self.seq = self.seq.wrapping_add(1);
        self.packets = self.packets.wrapping_add(1);
        self.octets = self.octets.wrapping_add(payload_len as u32);
        buf
    }

    /// Packets and payload octets sent so far.
    pub(crate) fn counts(&self) -> (u32, u32) {
        (self.packets, self.octets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64, nanos: u32) -> SystemTime {
        UNIX_EPOCH + Duration::new(secs, nanos)
    }

    #[test]
    fn ntp_of_the_unix_epoch_is_2208988800_seconds() {
        assert_eq!(ntp_from_system_time(UNIX_EPOCH), 2_208_988_800u64 << 32);
    }

    #[test]
    fn ntp_fraction_of_half_a_second_is_0x80000000() {
        let ntp = ntp_from_system_time(at(0, 500_000_000));
        assert_eq!(ntp & 0xFFFF_FFFF, 0x8000_0000);
    }

    #[test]
    fn the_compound_is_an_sr_followed_by_an_sdes_cname() {
        let c = sender_report_compound(0xAABB_CCDD, 77, 1234, 5, 600, "abc");
        let (sr, used) = SenderReport::decode(&c).unwrap();
        assert_eq!(
            (sr.ssrc, sr.ntp_timestamp, sr.rtp_timestamp),
            (0xAABB_CCDD, 77, 1234)
        );
        assert_eq!((sr.sender_packet_count, sr.sender_octet_count), (5, 600));
        let (sdes, rest) = SdesPacket::decode(&c[used..]).unwrap();
        assert_eq!((sdes.ssrc, sdes.cname.as_str()), (0xAABB_CCDD, "abc"));
        assert_eq!(used + rest, c.len());
    }

    #[test]
    fn a_random_cname_is_16_lower_case_hex_digits() {
        let n = random_cname();
        assert_eq!(n.len(), 16);
        assert!(
            n.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }

    #[test]
    fn a_track_reports_before_its_first_packet_and_then_every_five_seconds() {
        let mut t = TrackState::new(96);
        let now = Instant::now();
        assert!(t.report_due(now));
        t.mark_reported(now);
        assert!(!t.report_due(now));
        assert!(!t.report_due(now + Duration::from_millis(4_999)));
        assert!(t.report_due(now + SR_INTERVAL));
    }

    #[test]
    fn the_header_carries_seq_timestamp_ssrc_pt_and_marker() {
        let mut t = TrackState::new(97);
        let seq = t.seq;
        let h = t.next_header(0x0102_0304, true, 10);
        assert_eq!(h[0], 0x80);
        assert_eq!(h[1], 0x80 | 97);
        assert_eq!(u16::from_be_bytes([h[2], h[3]]), seq);
        assert_eq!(&h[4..8], &0x0102_0304u32.to_be_bytes());
        assert_eq!(&h[8..12], &t.ssrc.to_be_bytes());
        let h2 = t.next_header(1, false, 0);
        assert_eq!(h2[1], 97);
        assert_eq!(u16::from_be_bytes([h2[2], h2[3]]), seq.wrapping_add(1));
    }

    #[test]
    fn seq_wraps() {
        let mut t = TrackState::new(96);
        t.seq = u16::MAX;
        t.next_header(0, false, 0);
        let h = t.next_header(0, false, 0);
        assert_eq!(u16::from_be_bytes([h[2], h[3]]), 0);
    }

    #[test]
    fn counts_add_packets_and_payload_octets() {
        let mut t = TrackState::new(96);
        t.next_header(0, false, 1000);
        t.next_header(0, true, 234);
        assert_eq!(t.counts(), (2, 1234));
    }

    #[test]
    fn two_tracks_on_one_timeline_map_equal_pts_to_equal_ntp() {
        let mut line = PushTimeline::new(at(1_000, 0));
        let video = line.ntp_at(90_000);
        let klv = line.ntp_at(90_000);
        assert_eq!(video, klv);
        assert_eq!(video, ntp_from_system_time(at(1_000, 0)));
        assert_eq!(line.ntp_at(180_000), video + (1u64 << 32));
    }

    #[test]
    fn a_pts_before_the_anchor_maps_before_the_base() {
        let mut line = PushTimeline::new(at(1_000, 0));
        let base = line.ntp_at(90_000);
        assert_eq!(line.ntp_at(0), base - (1u64 << 32));
    }

    #[test]
    fn a_negative_pts_gives_a_wrapping_rtp_timestamp() {
        let t = TrackState::new(96);
        assert_eq!(t.rtp_timestamp(-1), t.rtp_base.wrapping_sub(1));
    }

    #[test]
    fn a_pts_that_wraps_33_bits_keeps_rtp_timestamps_continuous_mod_2_32() {
        let t = TrackState::new(96);
        assert_eq!(
            t.rtp_timestamp(600)
                .wrapping_sub(t.rtp_timestamp((1 << 33) - 3000)),
            3600
        );
    }

    #[test]
    fn a_time_before_1970_reads_as_the_epoch() {
        assert_eq!(
            ntp_from_system_time(UNIX_EPOCH - Duration::from_secs(5)),
            ntp_from_system_time(UNIX_EPOCH)
        );
    }

    #[test]
    fn ntp_seconds_wrap_after_2036_02_07() {
        // 2^32 - 2208988800 = 2085978496 is the unix second the field wraps at.
        assert_eq!(ntp_from_system_time(at(2_085_978_496 + 10, 0)), 10u64 << 32);
    }

    #[test]
    fn the_first_klv_ntp_is_offset_from_video_by_the_pts_difference() {
        let mut line = PushTimeline::new(at(1_000, 0));
        let video = line.ntp_at(900_000);
        let klv = line.ntp_at(900_000 + 45_000);
        assert_eq!(klv - video, 1u64 << 31);
    }
}
