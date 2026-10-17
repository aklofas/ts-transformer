//! Muxer per-stream counter accounting + public stats accessor surface.
//!
//! Owns `MuxerStats` (the public snapshot struct) and the three public
//! accessor methods (`stats`, `reset_stats`, `stream_codec_stats`). The
//! per-PID counter-bump helpers (`bump_video_counters` /
//! `bump_klv_counters` / `bump_audio_counters`) are shared free functions
//! in `mpegts::stats` (also used by the demuxer's stats accounting).
//!
//! The `Muxer` struct's `ts_packets_emitted` / `ts_bytes_emitted` /
//! `per_stream` / `stream_codec_counters` fields back this module's
//! operations; they stay in the struct definition (`mod.rs`).

use alloc::collections::BTreeMap;

use super::Muxer;

crate::binding_record! {
    /// Frozen snapshot of the muxer's cumulative counters.
    ///
    /// All counters are cumulative since construction (or the last stats
    /// reset). The per-stream push counters are not surfaced to the hosts.
    ///
    /// <!-- rust-only -->
    /// Returned by [`Muxer::stats`]. All counters are cumulative since
    /// construction (or the last [`Muxer::reset_stats`] call).
    ///
    /// `per_stream` is keyed by PID. Entries are created eagerly at
    /// [`Muxer::new`] for every configured video and KLV stream so callers
    /// can always index by a known PID without first checking for key
    /// presence.
    #[must_use]
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct MuxerStats {
        /// Total 188-byte TS packets drained via [`Muxer::pull`].
        pub ts_packets_emitted: u64,
        /// Total bytes drained via [`Muxer::pull`] (`ts_packets_emitted * 188`).
        pub ts_bytes_emitted: u64,
        /// Number of programs (PAT entries) in this muxer's configuration.
        pub programs_configured: u32,
        /// Number of subtitle streams configured across all programs in this
        /// muxer. Counts the `StreamSpec::Subtitle` entries from
        /// `MuxerConfig::programs`.
        pub subtitle_streams_configured: u32,
        /// Per-stream counters, keyed by PID. One entry per configured
        /// video or KLV stream. `StreamStats::items` = push_video_to /
        /// push_klv_to call count; `StreamStats::bytes` = raw ES bytes pushed
        /// (before PES/TS framing overhead).
        @[skip]
        pub per_stream: BTreeMap<u16, crate::mpegts::stats::StreamStats>,
    }
}

impl Muxer {
    /// Return a snapshot of the current stats counters.
    ///
    /// All per-stream entries are present regardless of whether any data has
    /// been pushed to that stream yet.
    pub fn stats(&self) -> MuxerStats {
        MuxerStats {
            ts_packets_emitted: self.ts_packets_emitted,
            ts_bytes_emitted: self.ts_bytes_emitted,
            programs_configured: self.config.programs.len() as u32,
            subtitle_streams_configured: self.subtitle_streams.iter().map(|s| s.len() as u32).sum(),
            per_stream: self.per_stream.clone(),
        }
    }

    /// Per-PID codec-specific counters. See
    /// [`crate::mpegts::stats::StreamCodecStats`] for the semantics of
    /// the return value (`None` vs `Some(Unknown)` vs typed variant).
    ///
    /// **Muxer-specific note:** the Muxer pre-populates `per_stream` for
    /// every PID listed in `MuxerConfig` at construction time, so a
    /// configured-but-never-pushed PID returns `Some(Unknown)`, NOT
    /// `None`. (Contrast with the Demuxer, where `Some(Unknown)`
    /// requires an event to have been emitted on that PID.) `None` is
    /// only returned for PIDs the Muxer was not configured with.
    ///
    /// # C ABI
    ///
    /// `tst_muxer_get_stream_codec_stats` — see
    /// `bindings/c/include/tstrans.h`.
    pub fn stream_codec_stats(&self, pid: u16) -> Option<crate::mpegts::stats::StreamCodecStats> {
        if let Some(c) = self.stream_codec_counters.get(&pid) {
            return Some(c.to_public());
        }
        if self.per_stream.contains_key(&pid) {
            return Some(crate::mpegts::stats::StreamCodecStats::Unknown);
        }
        None
    }

    /// Zero all flow counters.
    ///
    /// Per-stream entries are preserved (their `pid` and `stream_type`
    /// identity fields remain set); only the flow counters (`items`,
    /// `bytes`, `discontinuities`) are zeroed. Codec-counter entries
    /// are cleared on reset, so previously-pushed PIDs revert to
    /// `Some(Unknown)` (or `None` for never-configured PIDs) until the
    /// next push re-materializes the typed variant.
    pub fn reset_stats(&mut self) {
        self.ts_packets_emitted = 0;
        self.ts_bytes_emitted = 0;
        for s in self.per_stream.values_mut() {
            s.items = 0;
            s.bytes = 0;
            s.discontinuities = 0;
        }
        self.stream_codec_counters.clear();
    }
}
