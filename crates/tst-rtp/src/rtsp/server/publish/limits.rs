//! The bounds of an RTSP server's publish path in one value, so the memory
//! a connected publisher can hold is computed in one place.
//! `docs/reference/resource-policy.md` derives the server's bound from
//! these numbers, and the unit tests pin the figures it quotes. There is no
//! setter yet; the constants the publish path uses are read from
//! [`PublishLimits::DEFAULT`].

use crate::rtsp::server::mount::RTP_PAYLOAD_SIZE;

/// Bounds of the publish path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PublishLimits {
    /// Largest access unit an elementary publisher's depacketizer accepts
    /// (the depacketizer's default `max_au_bytes`).
    pub(crate) max_au_bytes: usize,
    /// KLV units the alignment hold keeps while waiting for video.
    pub(crate) align_hold_units: usize,
    /// Bytes of KLV the alignment hold keeps.
    pub(crate) align_hold_bytes: usize,
    /// On-demand publish mounts created by ANNOUNCE and not yet taken by
    /// `RtspServer::next_publisher`.
    pub(crate) publish_queue: usize,
    /// On-demand publish mounts in the mount table.
    pub(crate) on_demand_mounts: usize,
}

/// TS packets in one RTP frame of a re-muxed publisher (1 316 B / 188 B).
const TS_PER_FRAME: usize = RTP_PAYLOAD_SIZE / 188;
/// Bytes of one queued RTP frame: 12-byte RTP header + payload.
#[cfg(test)]
const FRAME_BYTES: usize = 12 + RTP_PAYLOAD_SIZE;

impl PublishLimits {
    /// The values the server uses.
    pub(crate) const DEFAULT: Self = Self {
        max_au_bytes: 8 * 1024 * 1024,
        align_hold_units: 4096,
        align_hold_bytes: 4 * 1024 * 1024,
        publish_queue: 64,
        on_demand_mounts: 256,
    };

    /// Every bound is non-zero. Checked at compile time for the defaults
    /// (test builds), and by the unit tests.
    #[cfg(test)]
    pub(crate) const fn is_valid(&self) -> bool {
        self.max_au_bytes > 0
            && self.align_hold_units > 0
            && self.align_hold_bytes > 0
            && self.publish_queue > 0
            && self.on_demand_mounts > 0
    }

    /// Frames of a publish mount's application queue: one maximum access
    /// unit re-muxed into 7-packet frames, plus 64 frames of headroom for
    /// PSI, PCR and KLV.
    pub(crate) const fn app_queue_frames(&self) -> usize {
        self.max_au_bytes.div_ceil(184).div_ceil(TS_PER_FRAME) + 64
    }

    /// The re-muxer's packet-queue cap: room for the largest access unit,
    /// doubled for PSI and adaptation-field overhead, never below the
    /// muxer's own 10 000-packet default.
    pub(crate) const fn muxer_queue_packets(&self) -> usize {
        let needed = 2 * self.max_au_bytes.div_ceil(184);
        if needed > 10_000 { needed } else { 10_000 }
    }

    /// Worst-case bytes one connected elementary publisher holds: the
    /// largest access unit in the depacketizer, a full re-muxer queue, the
    /// PES scratch buffer, a full KLV hold and a full application queue.
    #[cfg(test)]
    pub(crate) const fn worst_case_publisher_bytes(&self) -> usize {
        self.max_au_bytes
            + self.muxer_queue_packets() * 188
            + self.max_au_bytes
            + self.align_hold_bytes
            + self.app_queue_frames() * FRAME_BYTES
    }
}

#[cfg(test)]
const _: () = assert!(PublishLimits::DEFAULT.is_valid());

#[cfg(test)]
mod tests {
    use super::PublishLimits;

    #[test]
    fn the_defaults_are_the_documented_bounds() {
        let l = PublishLimits::DEFAULT;
        assert_eq!(
            l.max_au_bytes,
            crate::depay::DepayConfig::default().max_au_bytes
        );
        assert_eq!(l.app_queue_frames(), 6_577);
        assert_eq!(l.muxer_queue_packets(), 91_182);
        // 8 MiB AU + 91 182 × 188 B muxer queue + 8 MiB PES scratch + 4 MiB
        // KLV hold + 6 577 × 1 328 B application queue.
        assert_eq!(l.worst_case_publisher_bytes(), 46_847_992);
        assert_eq!(64 * l.worst_case_publisher_bytes(), 2_998_271_488);
        assert!(l.is_valid());
    }

    #[test]
    fn a_zero_bound_is_invalid() {
        let mut l = PublishLimits::DEFAULT;
        l.align_hold_units = 0;
        assert!(!l.is_valid());
    }
}
