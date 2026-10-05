//! Stats accounting + nonconformant event queueing.
//!
//! Hosts 4 helper methods on `Demuxer`, all `pub(super)`:
//!
//! - `queue_nonconformant(stream, issue)` — pushes a `NonConformant`
//!   event onto the queue and captures the first strict-rejected issue
//!   per `feed` call (the `fatal` field on `Demuxer`).
//! - `stream_stats_entry(pid, stream_type, program_number)` — lazily
//!   creates a `StreamStats` entry for a PID; returns `&mut StreamStats`
//!   so the caller can increment `items`, `bytes`, or `discontinuities`.
//! - `record_discontinuity(stream, kind)` — bumps `discontinuities_count`,
//!   increments the per-PID `StreamStats::discontinuities` counter via
//!   `stream_stats_entry`, and pushes a `Discontinuity` event.
//! - `record_item(stream, program_number, bytes)` — the shared body of
//!   `pes_emit.rs`'s per-arm item accounting: bumps `items`, stamps
//!   `touch_last_seen`, adds `bytes`, and returns the entry for arms
//!   that do one thing more with it (subtitle label, KLV counter bump).
//!
//! The per-PID codec counter bumps (`bump_video_counters` /
//! `bump_klv_counters` / `bump_audio_counters`) are shared free functions
//! in `mpegts::stats` (also used by the muxer's stats accounting) — call
//! sites pass `&mut self.stream_codec_counters` directly.
//!
//! The 3 public stats accessors (`stats`, `reset_stats`,
//! `stream_codec_stats`) stay in `demuxer.rs` (the coordinator) since
//! they're part of the `Demuxer` public surface.
//!
//! No wrapper struct: the helpers write straight into the `Demuxer`'s own
//! stats, counters and event queue, so recording needs no extra borrow or
//! indirection.

use crate::mpegts::demux::event::{DemuxEvent, DiscontinuityKind, NonConformantIssue, StreamId};

impl super::demuxer::Demuxer {
    /// Lazily creates a `StreamStats` entry for `pid` and returns a mutable
    /// reference to it. The caller is responsible for incrementing `items`,
    /// `bytes`, `discontinuities`, or any other field after this call.
    pub(super) fn stream_stats_entry(
        &mut self,
        pid: u16,
        stream_type: u8,
        program_number: u16,
    ) -> &mut crate::mpegts::stats::StreamStats {
        use crate::mpegts::common::StreamTypeCode;
        self.stats_per_stream
            .entry(pid)
            .or_insert_with(|| crate::mpegts::stats::StreamStats {
                pid,
                stream_type: StreamTypeCode::from_byte(stream_type),
                program_number,
                ..Default::default()
            })
    }

    /// Bumps `discontinuities_count`, increments the per-PID
    /// `StreamStats::discontinuities` counter, and pushes a
    /// `DemuxEvent::Discontinuity` event for `stream`/`kind`.
    pub(super) fn record_discontinuity(&mut self, stream: StreamId, kind: DiscontinuityKind) {
        self.discontinuities_count += 1;
        self.stream_stats_entry(
            stream.pid,
            super::pmt_classify::stream_type_from_kind(&stream.kind),
            stream.program_number,
        )
        .discontinuities += 1;
        self.queue
            .push_back(DemuxEvent::Discontinuity { stream, kind });
    }

    pub(super) fn queue_nonconformant(&mut self, stream: StreamId, issue: NonConformantIssue) {
        // Capture the first strict-rejected issue per `feed` call. The
        // event itself is still queued so a caller draining events
        // before/after the `feed` error sees the narrative.
        if self.options.strict.rejects(&issue) && self.fatal.is_none() {
            self.fatal = Some(issue.clone());
        }
        self.nonconformant_count += 1;
        self.queue
            .push_back(DemuxEvent::NonConformant { stream, issue });
    }

    /// Lazily creates (or fetches) the stats entry for `stream`, bumps
    /// `items` by 1, stamps `touch_last_seen`, and adds `bytes`. Returns
    /// the entry so a couple of `pes_emit.rs` arms can do one thing more
    /// with it (subtitle label, KLV counter bump).
    pub(super) fn record_item(
        &mut self,
        stream: &StreamId,
        program_number: u16,
        bytes: usize,
    ) -> &mut crate::mpegts::stats::StreamStats {
        let entry = self.stream_stats_entry(
            stream.pid,
            super::pmt_classify::stream_type_from_kind(&stream.kind),
            program_number,
        );
        entry.items += 1;
        entry.touch_last_seen();
        entry.bytes += bytes as u64;
        entry
    }
}
