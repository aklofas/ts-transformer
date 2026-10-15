//! RTP depacketization shared by the H.264 (RFC 6184) and H.265
//! (RFC 7798) depacketizers: the access unit they emit, their
//! configuration and their counters. The state machine is crate-private
//! (`core`); each codec contributes only its payload-header rules.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

pub(crate) mod core;

use tst_core::mpegts::common::Pts90khz;

/// A fully reassembled access unit (H.264 or H.265), ready for decoding or muxing.
///
/// The `annexb` buffer contains all NALUs concatenated in Annex B framing
/// (`[0,0,0,1]` start code before each NALU), in RTP packet order.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AccessUnit {
    /// Annex B–framed NALU bytes (one or more NALUs concatenated with
    /// `[0,0,0,1]` start codes).
    pub annexb: Vec<u8>,
    /// 90 kHz presentation timestamp, delivered in decode order, derived
    /// from the RTP timestamp.
    ///
    /// Zero-based at the first started AU (even one later dropped as
    /// poisoned, so the first emitted AU need not read 0); unwrapped across the 32-bit RTP
    /// timestamp rollover. Because RTP timestamps are presentation times sent in decode order, B-frame
    /// content produces non-monotonic PTS values — the depacketizer passes them
    /// through unaltered. DTS is not derivable from RTP; callers with B-frame
    /// sources must derive DTS themselves for `push_video_to_with_dts`.
    ///
    /// Values can be negative if a later AU's unwrapped timestamp falls below
    /// the first AU's anchor (e.g. after an SSRC reset with a lower timestamp
    /// origin). PTS **is** monotonic across SSRC resets: the re-anchor adds
    /// one nominal frame step (3003 ticks at 90 kHz ≈ 30 fps) to the last
    /// emitted value before anchoring the new stream.
    pub pts: Pts90khz,
    /// `true` if the access unit contains an IDR picture (H.264 NAL type 5;
    /// H.265 types 19 and 20).
    pub key_frame: bool,
    /// The RTP timestamp for this AU as carried in the RTP header.
    pub rtp_timestamp: u32,
}

/// Controls whether out-of-band parameter sets are injected before IDR frames.
///
/// See the parameter-set cache section of the
/// [`H264Depacketizer`](crate::h264::H264Depacketizer) docs for the full
/// cache-and-injection contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParameterSetInjection {
    /// No injection — pass NALUs through exactly as received.
    None,
    /// Inject cached parameter sets (H.264: SPS, PPS; H.265: VPS, SPS, PPS)
    /// before every IDR access unit that lacks them.
    BeforeIdr,
}

impl Default for ParameterSetInjection {
    fn default() -> Self {
        Self::BeforeIdr
    }
}

/// Configuration for a depacketizer ([`H264Depacketizer`](crate::h264::H264Depacketizer)).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DepayConfig {
    /// Expected RTP payload type (7 bits).
    ///
    /// This field is carried here so that SDP negotiation can hand the receiver
    /// a single config object. The depacketizer's `feed` itself does **not**
    /// read it — the depacketizer processes whatever packet it is given,
    /// regardless of PT. PT filtering is the I/O layer's responsibility: the
    /// receiver ([`H264Receiver`](crate::h264::H264Receiver)) compares `header.payload_type`
    /// against this value before calling `feed`. Callers that source packets
    /// from a foreign mux or socket must filter PT upstream before feeding.
    pub payload_type: u8,
    /// Whether to inject cached parameter sets before IDR frames.
    pub parameter_set_injection: ParameterSetInjection,
    /// Out-of-band parameter sets from SDP (`sprop-parameter-sets` for H.264;
    /// `sprop-vps`, `sprop-sps`, `sprop-pps` for H.265). Each element is one
    /// NAL unit with its header (H.264 SPS 7 / PPS 8; H.265 VPS 32 / SPS 33 /
    /// PPS 34). Seeded into the cache at construction and used by
    /// [`ParameterSetInjection::BeforeIdr`].
    pub initial_parameter_sets: Vec<Vec<u8>>,
    /// Maximum combined byte count for a single AU's accumulation buffers
    /// (`au_buf` + the open FU-A buffer). When this limit is exceeded the
    /// buffers are immediately cleared (memory released, not just flagged),
    /// the AU is poisoned, and [`DepayStats::aus_dropped_oversize`] is
    /// incremented. The drop is also counted in
    /// [`DepayStats::aus_dropped`] at the normal AU-boundary tick site.
    ///
    /// This bound closes a DoS vector: on an unconnected UDP socket (or a
    /// hostile interleaved RTSP server) an attacker can hold a constant RTP
    /// timestamp and send contiguous sequence numbers indefinitely, causing
    /// `au_buf` to grow without limit. The default (8 MiB) is generous for
    /// any real H.264 AU; lower the value in memory-constrained environments.
    ///
    /// A value of `0` is degenerate: every non-empty AU exceeds a zero cap and
    /// is dropped, so nothing is ever emitted. The Python and JVM config
    /// builders reject `0`; the raw Rust struct accepts it (documented here).
    pub max_au_bytes: usize,
}

impl Default for DepayConfig {
    fn default() -> Self {
        Self {
            payload_type: 96,
            parameter_set_injection: ParameterSetInjection::BeforeIdr,
            initial_parameter_sets: Vec::new(),
            max_au_bytes: 8 * 1024 * 1024, // 8 MiB
        }
    }
}

/// Counters for monitoring a depacketizer.
///
/// Returned by value from the depacketizer's `stats` (the struct is `Copy`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DepayStats {
    /// Number of complete, unpoisoned AUs emitted.
    pub aus_emitted: u64,
    /// Number of AUs discarded due to poisoning (seq gaps, F-bit, etc.).
    /// Includes AUs dropped for exceeding `max_au_bytes`
    /// (those are also counted in `aus_dropped_oversize`).
    pub aus_dropped: u64,
    /// Number of AUs dropped specifically because their accumulated buffer
    /// size exceeded [`DepayConfig::max_au_bytes`]. Every oversize drop
    /// also increments `aus_dropped` at the normal AU-boundary tick site.
    pub aus_dropped_oversize: u64,
    /// Number of RTP packets discarded (empty, reserved, interleaved types).
    pub packets_discarded: u64,
    /// Number of NALUs discarded (F-bit set, open FU at AU completion, etc.).
    pub nalus_discarded: u64,
    /// Number of sequence-number gaps detected.
    pub seq_gaps: u64,
    /// Number of duplicate sequence numbers detected.
    pub duplicate_packets: u64,
    /// Number of times cached parameter sets were updated (in-band
    /// parameter-set bytes differed from the cached value).
    pub parameter_set_updates: u64,
    /// Number of SSRC changes (source restarts) detected.
    pub ssrc_changes: u64,
}
