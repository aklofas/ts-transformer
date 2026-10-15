//! Ingest path for incoming RTCP on the interleaved RTSP client path —
//! RR → fraction-lost / jitter / cumulative-lost into `RtcpStats`; SR →
//! counters plus the NTP anchor stored in `RtcpStats::last_sr_anchor`.
//!
//! `rtt_us` is always reported as 0: a round-trip time needs the peer to
//! echo one of our own sender reports (LSR/DLSR, RFC 3550 §6.4.1), and no
//! transport sends RTCP. See `docs/project/deferred-features.md` (RTCP
//! statistics reporting).

use std::time::SystemTime;

use crate::rtcp::stats::RtcpStats;
use crate::rtcp::{ReceiverReport, SenderReport};

/// Snapshot of the last SR observed from peer, used for RTT calc when
/// peer's next RR arrives.
#[derive(Debug, Clone, Copy)]
pub struct SrAnchor {
    /// Middle 32 bits of NTP timestamp from the SR (RFC 3550 §6.4.1).
    pub last_sr_ntp_mid: u32,
    /// Local time when the SR was received.
    pub received_at: SystemTime,
}

/// Ingest one RR — updates `RtcpStats` fraction_lost + jitter +
/// cumulative_lost_send from the first report block referencing our SSRC
/// (if matched). `rtt_us` is never updated by this function; it stays at
/// its prior value (0 by default). Returns true if the RR was applicable.
pub fn ingest_rr(stats: &mut RtcpStats, our_ssrc: u32, rr: &ReceiverReport) -> bool {
    stats.rr_packets_received += 1;
    stats.last_rr_ts = Some(SystemTime::now());
    let mut matched = false;
    for rb in &rr.report_blocks {
        if rb.ssrc == our_ssrc {
            stats.fraction_lost_q8 = rb.fraction_lost;
            // Convert jitter from 1/90000 sec units (RTP timestamp clock)
            // to microseconds: (jitter * 1_000_000) / 90_000.
            stats.interarrival_jitter_us = ((rb.jitter as u64) * 1_000_000 / 90_000) as u32;
            // RFC 3550 §6.4.1: cumulative_lost is a 24-bit signed field.
            // Clamp negatives to 0 — peer reporting a negative cumulative
            // loss (duplicates exceeded losses) projects to "0 lost" on
            // SocketStats.packets_lost_send.
            stats.cumulative_lost_send = rb.cumulative_lost.max(0) as u32;
            // rtt_us: not computed here — see module doc for the reason.
            matched = true;
        }
    }
    matched
}

/// Ingest one SR — stores the peer's NTP anchor on `stats` and
/// updates `sr_packets_received` + `last_sr_ts`. The returned anchor is
/// a snapshot of what was written into `stats.last_sr_anchor`; callers
/// that want the value without re-reading the struct can use the return.
pub fn ingest_sr(stats: &mut RtcpStats, sr: &SenderReport) -> SrAnchor {
    stats.sr_packets_received += 1;
    let received_at = SystemTime::now();
    stats.last_sr_ts = Some(received_at);
    let ntp_mid = ((sr.ntp_timestamp >> 16) & 0xFFFFFFFF) as u32;
    let anchor = SrAnchor {
        last_sr_ntp_mid: ntp_mid,
        received_at,
    };
    stats.last_sr_anchor = Some(anchor);
    anchor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtcp::ReportBlock;

    #[test]
    fn ingest_rr_updates_fraction_lost() {
        let mut stats = RtcpStats::default();
        let rb = ReportBlock {
            ssrc: 0xCAFEBABE,
            fraction_lost: 42,
            cumulative_lost: 1000,
            extended_highest_seq: 5000,
            jitter: 9000, // 100 ms at 90 kHz
            last_sr: 0,
            delay_since_last_sr: 0,
        };
        let rr = ReceiverReport {
            ssrc: 0x11223344,
            report_blocks: vec![rb],
        };
        assert!(ingest_rr(&mut stats, 0xCAFEBABE, &rr));
        assert_eq!(stats.fraction_lost_q8, 42);
        assert_eq!(stats.interarrival_jitter_us, 100_000); // 100 ms in us
        assert_eq!(stats.rr_packets_received, 1);
        assert_eq!(stats.cumulative_lost_send, 1000);
        // No SR anchor stored → rtt_us stays at default 0.
        assert_eq!(stats.rtt_us, 0);
    }

    #[test]
    fn ingest_rr_after_sr_leaves_rtt_us_zero() {
        let mut stats = RtcpStats::default();
        // Stage an SR anchor by ingesting an SR first.
        let sr = SenderReport {
            ssrc: 0xCAFEBABE,
            ntp_timestamp: 0x83AA7E80_DEADBEEFu64,
            rtp_timestamp: 0,
            sender_packet_count: 0,
            sender_octet_count: 0,
            report_blocks: vec![],
        };
        ingest_sr(&mut stats, &sr);
        // Now ingest an RR whose RB's last_sr matches the SR's NTP mid.
        let rb = ReportBlock {
            ssrc: 0xCAFEBABE,
            fraction_lost: 0,
            cumulative_lost: 0,
            extended_highest_seq: 0,
            jitter: 0,
            last_sr: 0x7E80_DEAD, // matches SrAnchor.last_sr_ntp_mid above
            delay_since_last_sr: 0,
        };
        let rr = ReceiverReport {
            ssrc: 0,
            report_blocks: vec![rb],
        };
        assert!(ingest_rr(&mut stats, 0xCAFEBABE, &rr));
        // RTT is not computed by ingest_rr (see module doc); stays 0.
        assert_eq!(stats.rtt_us, 0, "rtt_us must stay 0: got {}", stats.rtt_us);
    }

    #[test]
    fn ingest_rr_clamps_negative_cumulative_lost() {
        let mut stats = RtcpStats::default();
        let rb = ReportBlock {
            ssrc: 0xCAFEBABE,
            fraction_lost: 0,
            cumulative_lost: -5, // negative = receiver saw duplicates
            extended_highest_seq: 0,
            jitter: 0,
            last_sr: 0,
            delay_since_last_sr: 0,
        };
        let rr = ReceiverReport {
            ssrc: 0,
            report_blocks: vec![rb],
        };
        assert!(ingest_rr(&mut stats, 0xCAFEBABE, &rr));
        assert_eq!(stats.cumulative_lost_send, 0);
    }

    #[test]
    fn ingest_rr_skips_unmatched_ssrc() {
        let mut stats = RtcpStats::default();
        let rb = ReportBlock {
            ssrc: 0xDEADBEEF, // not our ssrc
            fraction_lost: 100,
            cumulative_lost: 0,
            extended_highest_seq: 0,
            jitter: 0,
            last_sr: 0,
            delay_since_last_sr: 0,
        };
        let rr = ReceiverReport {
            ssrc: 0,
            report_blocks: vec![rb],
        };
        assert!(!ingest_rr(&mut stats, 0xCAFEBABE, &rr));
        assert_eq!(stats.fraction_lost_q8, 0);
    }

    #[test]
    fn ingest_sr_stores_anchor() {
        let mut stats = RtcpStats::default();
        let sr = SenderReport {
            ssrc: 0xCAFEBABE,
            ntp_timestamp: 0x83AA7E80_DEADBEEFu64,
            rtp_timestamp: 0,
            sender_packet_count: 0,
            sender_octet_count: 0,
            report_blocks: vec![],
        };
        let anchor = ingest_sr(&mut stats, &sr);
        // Middle 32 bits of 0x83AA7E80_DEADBEEF = 0x7E80_DEAD
        assert_eq!(anchor.last_sr_ntp_mid, 0x7E80_DEAD);
        assert_eq!(stats.sr_packets_received, 1);
        // Anchor is also persisted on stats so a later RR ingest can compute RTT.
        assert!(stats.last_sr_anchor.is_some());
        assert_eq!(stats.last_sr_anchor.unwrap().last_sr_ntp_mid, 0x7E80_DEAD);
    }
}
