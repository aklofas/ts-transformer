//! Ingest path for incoming RTCP — RR → fraction-lost / jitter /
//! cumulative-lost ticks into `RtcpStats`; SR → NTP/RTP anchor stored
//! for future use.
//!
//! `rtt_us` is always reported as 0. The `compute_rtt_us` helper that
//! computes from RFC 3550 §6.4.1 is retained as public API but is not
//! called from `ingest_rr`: the anchor ingested via `ingest_sr` comes from
//! the PEER's SR (measuring the peer's clock domain), while the RTT formula
//! needs the timestamp of OUR SR that we sent to the peer — and the SR
//! reporter in `transport.rs` sends `ntp_timestamp: 0`. That mismatch makes
//! a value computed here meaningless, so it stays unwired until the SR
//! reporter carries a real NTP timestamp. The helper itself no longer
//! truncates: the µs narrowing saturates and anything above 60 s (a wrapped
//! subtraction on peer-controlled `last_sr` / `delay_since_last_sr`) is
//! `None`. Full RFC 3550 RTT is deferred; see
//! `docs/project/deferred-features.md` (RTCP statistics reporting).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::rtcp::stats::RtcpStats;
use crate::rtcp::{ReceiverReport, ReportBlock, SenderReport};

/// Snapshot of the last SR observed from peer, used for RTT calc when
/// peer's next RR arrives.
#[derive(Debug, Clone, Copy)]
pub struct SrAnchor {
    /// Middle 32 bits of NTP timestamp from the SR (RFC 3550 §6.4.1).
    pub last_sr_ntp_mid: u32,
    /// Local time when the SR was received.
    pub received_at: SystemTime,
}

/// Upper bound on a believable round-trip time: 60 s. Anything above it is
/// a wrapped subtraction (an LSR "from the future"), a stale anchor, or a
/// peer feeding garbage — reported as "no estimate", never as a number.
pub(crate) const MAX_RTT_US: u32 = 60_000_000;

/// Compute RTT in microseconds from a peer's RR + our stored SR anchor.
///
/// Returns `None` if no SR anchor is available, if the RR's `last_sr`
/// doesn't match our anchor (timing drift), or if the result is not a
/// believable RTT (above 60 s — a wrapped subtraction on peer-controlled
/// input; the value is never truncated into range).
pub fn compute_rtt_us(rb: &ReportBlock, anchor: Option<SrAnchor>) -> Option<u32> {
    let anchor = anchor?;
    if anchor.last_sr_ntp_mid != rb.last_sr {
        return None;
    }
    let now_ntp_mid = system_time_to_ntp_mid(SystemTime::now());
    rtt_us_from_ntp_mid(now_ntp_mid, rb.last_sr, rb.delay_since_last_sr)
}

/// RFC 3550 §6.4.1: `RTT = A − LSR − DLSR` in 16.16 s, converted to µs.
/// The u64→u32 narrowing saturates and the result is bounded by
/// [`MAX_RTT_US`]; both `last_sr` and `delay_since_last_sr` come from the
/// peer, so `diff` can be anything.
pub(crate) fn rtt_us_from_ntp_mid(
    now_ntp_mid: u32,
    last_sr: u32,
    delay_since_last_sr: u32,
) -> Option<u32> {
    let diff = now_ntp_mid
        .wrapping_sub(last_sr)
        .wrapping_sub(delay_since_last_sr);
    // 16.16 s → µs: (diff * 1_000_000) >> 16, at most ≈ 6.6e10 — narrow with
    // saturation, then apply the ceiling.
    let us = ((diff as u64) * 1_000_000) >> 16;
    let us = u32::try_from(us).unwrap_or(u32::MAX);
    (us <= MAX_RTT_US).then_some(us)
}

/// Convert `SystemTime` to NTP middle 32 bits (16.16 fixed-point
/// seconds from NTP epoch 1900-01-01).
///
/// NTP epoch is 2_208_988_800 seconds before Unix epoch (1970-01-01).
pub fn system_time_to_ntp_mid(t: SystemTime) -> u32 {
    let dur = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let secs = dur.as_secs() + 2_208_988_800;
    let nanos = dur.subsec_nanos();
    let frac = ((nanos as u64 * 0x1_0000_0000) / 1_000_000_000) as u32;
    // Middle 32 bits = low 16 bits of secs concatenated with high 16 bits of frac.
    ((secs as u32) << 16) | (frac >> 16)
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

/// Ingest one SR — stores an anchor on `stats` for future RTT calc and
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

    #[test]
    fn ntp_mid_conversion_smoke() {
        // 2026-01-01T00:00:00 UTC = 1_767_225_600 Unix seconds.
        // = 1_767_225_600 + 2_208_988_800 = 3_976_214_400 NTP seconds.
        // Lower 32 bits of secs: 3_976_214_400 & 0xFFFFFFFF
        let t = UNIX_EPOCH + Duration::from_secs(1_767_225_600);
        let mid = system_time_to_ntp_mid(t);
        assert_eq!(mid >> 16, (3_976_214_400u64 & 0xFFFF) as u32);
    }

    /// CORR-28: the 16.16 s → µs conversion must not wrap. An LSR "from the
    /// future" (anchor and `last_sr` half an NTP-mid epoch — 32 768 s —
    /// ahead of now) makes the wrapping subtraction come out ≈ 0x8000_0000;
    /// before the fix `((diff as u64) * 1_000_000) >> 16` (≈ 3.3e10) was cast
    /// `as u32` and reported as 2_703_228_928 µs (~45 min) — a
    /// plausible-looking lie. The value is deterministic regardless of the
    /// few ticks between the two `SystemTime::now()` calls.
    #[test]
    fn compute_rtt_us_rejects_a_wrapped_diff() {
        let ahead = system_time_to_ntp_mid(SystemTime::now()).wrapping_add(0x8000_0000);
        let anchor = SrAnchor {
            last_sr_ntp_mid: ahead,
            received_at: SystemTime::now(),
        };
        let rb = ReportBlock {
            ssrc: 1,
            fraction_lost: 0,
            cumulative_lost: 0,
            extended_highest_seq: 0,
            jitter: 0,
            last_sr: ahead,
            delay_since_last_sr: 0,
        };
        assert_eq!(compute_rtt_us(&rb, Some(anchor)), None);
    }

    #[test]
    fn rtt_us_from_ntp_mid_converts_and_bounds() {
        // 1.0 s in 16.16 → 1_000_000 µs.
        assert_eq!(rtt_us_from_ntp_mid(0x0001_0000, 0, 0), Some(1_000_000));
        // DLSR is subtracted: A − LSR − DLSR = 3 s − 1 s − 1 s.
        assert_eq!(
            rtt_us_from_ntp_mid(0x0003_0000, 0x0001_0000, 0x0001_0000),
            Some(1_000_000)
        );
        // Exactly the 60 s ceiling is still a value; one tick above is not.
        assert_eq!(rtt_us_from_ntp_mid(60 << 16, 0, 0), Some(MAX_RTT_US));
        assert_eq!(rtt_us_from_ntp_mid((60 << 16) + 1, 0, 0), None);
        // A wrapped subtraction (now < LSR): diff = u32::MAX → None, not a
        // truncated 1_111_490_544.
        assert_eq!(rtt_us_from_ntp_mid(0, 1, 0), None);
        assert_eq!(rtt_us_from_ntp_mid(0, 0, 1), None);
    }

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
