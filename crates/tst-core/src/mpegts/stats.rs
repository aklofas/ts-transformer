//! Shared per-stream stats shape used by `mpegts::mux::MuxerStats` and
//! `mpegts::demux::DemuxerStats`. Identity is the PID; kind/codec lives
//! in `stream_type`. Same shape across sender and receiver sites so the
//! `tst-c` ABI is one struct + one fixed-size array.

use crate::mpegts::common::StreamTypeCode;
use alloc::string::String;

/// Per-stream counters. Used at every site that emits or receives TS
/// elementary streams. PID is identity; `stream_type` is the PMT byte
/// (or `0x00` for a PID no PMT declares as a stream); `label` names a
/// subtitle stream's codec and is `None` on every other PID.
///
/// `items`, `bytes` and `discontinuities` are counters: they only grow
/// until `reset_stats`. The muxer's `reset_stats` zeroes them and keeps
/// every entry (and its `last_seen`); the demuxer's drops every entry, and
/// a PID's entry is created again by its next unit or discontinuity. A PID
/// that leaves the demuxer's topology keeps its entry until then, and
/// `Demuxer::reset_sync` keeps the entries too.
#[must_use]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct StreamStats {
    /// The PID: the entry's identity, not a counter.
    pub pid: u16,
    /// The PMT `stream_type` byte that declares the PID (on the demuxer,
    /// the latest PMT's), or `0x00` for a PID no PMT declares as a stream:
    /// a PSI PID, a PID that carries only a program's PCR. Not a counter.
    pub stream_type: StreamTypeCode,
    /// The program that owns the PID at this site: on the muxer the
    /// configured program; on the demuxer the program whose PMT declares (or
    /// last declared) the stream, or, for a PID that carries only a PCR (a
    /// PMT PID included), the lowest-numbered program that declares it as
    /// its `PCR_PID`; `0` when no program owns it. Not a counter.
    pub program_number: u16,
    /// The codec's name on a subtitle PID (`"DVB-Subtitling"`,
    /// `"DVB-Teletext"`, `"CEA-708-Standalone"`, `"WebVTT-in-TS"`): the
    /// muxer sets it when it is built, the demuxer at the PID's first
    /// subtitle sample. `None` on every other PID.
    pub label: Option<String>,
    /// Units through this site, a counter. Muxer: one per push on the PID.
    /// Demuxer: one per `Sample` or `Metadata` event the PID delivers, and
    /// one for a subtitle or audio PES whose sample a strict mode drops (a
    /// subtitle data-field or alignment issue, a missing AC-3 syncword, an
    /// AAC-LATM framing issue).
    pub items: u64,
    /// Bytes of those units, a counter, without PES or TS framing: the
    /// elementary-stream bytes pushed (muxer); the PES payload (demuxer; for
    /// a DVB subtitle the whole PES_data_field with its envelope, though the
    /// `Sample` carries only the segments; for a synchronous metadata access
    /// unit, its payload without the cell headers).
    pub bytes: u64,
    /// `DemuxEvent::Discontinuity` events reported under this PID, a
    /// counter. The muxer reports none and reads `0`.
    pub discontinuities: u64,
    /// Wall-clock time this stream last carried an item through this
    /// component (mux: last push; demux: last emitted item). `None` = never.
    /// A timestamp, not a counter: the muxer's `reset_stats` keeps it.
    ///
    /// `std`-only: no wall clock exists under `no_std` (the field is absent
    /// entirely on that build).
    ///
    /// # C ABI
    ///
    /// `tst_demux_receiver_get_stream_last_seen_micros` /
    /// `tst_managed_demux_receiver_get_stream_last_seen_micros` /
    /// `tst_rist_demux_receiver_get_stream_last_seen_micros` /
    /// `tst_rtp_demux_receiver_get_stream_last_seen_micros` /
    /// `tst_tcp_demux_receiver_get_stream_last_seen_micros` /
    /// `tst_udp_demux_receiver_get_stream_last_seen_micros` — see
    /// `bindings/c/include/tstrans.h`. Exposed as a Unix epoch microsecond
    /// `uint64_t` getter, not a struct-copied field (`tst_stream_stats_t`
    /// stays byte-size-asserted, plain-integer counters only).
    #[cfg(feature = "std")]
    pub last_seen: Option<std::time::SystemTime>,
}

impl StreamStats {
    /// Stamp [`StreamStats::last_seen`] to the current wall-clock time.
    /// Called at every site that increments `items`.
    ///
    /// No-op under `no_std` (the field doesn't exist there — no wall
    /// clock). Kept as a same-shaped method on both configs, mirroring
    /// `tst_pipeline::mutex::ShellMutex`'s std/no_std split, so call sites
    /// compile unchanged either way.
    #[cfg(feature = "std")]
    pub(crate) fn touch_last_seen(&mut self) {
        self.last_seen = Some(std::time::SystemTime::now());
    }

    #[cfg(not(feature = "std"))]
    pub(crate) fn touch_last_seen(&mut self) {}
}

/// Mux-side codec label. Used to populate [`StreamStats::label`] for
/// subtitle PIDs at `Muxer::new` time. Static labels cover the four
/// supported subtitle codecs:
/// * `DvbSubtitling` → `"DVB-Subtitling"`
/// * `DvbTeletext`   → `"DVB-Teletext"`
/// * `Cea708Standalone` → `"CEA-708-Standalone"`
/// * `WebVttInTs`    → `"WebVTT-in-TS"`
pub fn subtitle_codec_label(codec: &crate::mpegts::mux::SubtitleCodec) -> &'static str {
    match codec {
        crate::mpegts::mux::SubtitleCodec::DvbSubtitling { .. } => "DVB-Subtitling",
        crate::mpegts::mux::SubtitleCodec::DvbTeletext { .. } => "DVB-Teletext",
        crate::mpegts::mux::SubtitleCodec::Cea708Standalone => "CEA-708-Standalone",
        crate::mpegts::mux::SubtitleCodec::WebVttInTs => "WebVTT-in-TS",
    }
}

/// Demux-side codec label. Same labels as [`subtitle_codec_label`] but
/// over the param-less demux-side enum `mpegts::demux::event::SubtitleCodec`.
pub fn demux_subtitle_codec_label(
    codec: crate::mpegts::demux::event::SubtitleCodec,
) -> &'static str {
    use crate::mpegts::demux::event::SubtitleCodec;
    match codec {
        SubtitleCodec::DvbSubtitling => "DVB-Subtitling",
        SubtitleCodec::DvbTeletext => "DVB-Teletext",
        SubtitleCodec::Cea708Standalone => "CEA-708-Standalone",
        SubtitleCodec::WebVttInTs => "WebVTT-in-TS",
    }
}

/// Per-stream codec-specific counters keyed by PID.
///
/// Returned by [`Muxer::stream_codec_stats`](crate::mpegts::mux::Muxer::stream_codec_stats)
/// and [`Demuxer::stream_codec_stats`](crate::mpegts::demux::Demuxer::stream_codec_stats).
///
/// The outer `Option` distinguishes three states:
/// * `None` — PID has never been observed at this site.
/// * `Some(StreamCodecStats::Unknown)` — PID is known to the site
///   but no counter family applies in v1 (PSI PIDs, subtitle PIDs,
///   audio PIDs whose codec lacks a frame iterator: LATM, AC-3).
/// * `Some(Video / Klv / Audio)` — PID is known and counters apply.
///
/// The demuxer creates a PID's entry at its first unit or discontinuity and
/// its counters at the first unit it counts: a video PID reads `None` until
/// its first unit or discontinuity, then `Some(Unknown)` until its first
/// random-access unit, and an audio PID `Some(Unknown)` until its first
/// counted frame. Every field is a counter that only grows. `reset_stats`
/// drops them: the muxer then answers `Some(Unknown)` for the PID and the
/// demuxer `None`, until the PID counts again.
///
/// `#[non_exhaustive]` on the enum + every variant so additive counters
/// land without a major bump.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StreamCodecStats {
    /// PID is known to the site but no counter family applies in v1.
    #[default]
    Unknown,
    /// H.264 / H.265 / H.266 (NALs) or AV1 (OBUs).
    #[non_exhaustive]
    Video {
        /// NAL units (H.264/H.265/H.266) or OBUs (AV1), a counter.
        ///
        /// Sender side: counted in each pushed access unit; the caller
        /// derives NALs/AU as `nals_or_obus / items` where `items` is read
        /// from the unified [`StreamStats`].
        ///
        /// Receiver side: always 0. The demuxer hands out whole access
        /// units and does not split them; count them with
        /// [`count_nal_units`](crate::codec::util::count_nal_units) on the
        /// `Sample` payloads.
        nals_or_obus: u64,
        /// Access units that are random-access points, a counter.
        ///
        /// Sender side: counts pushes where `key_frame == true`
        /// (caller-supplied flag on `Muxer::push_video` /
        /// `Muxer::push_video_to`).
        ///
        /// Receiver side: counts `Sample` events whose
        /// `random_access_indicator` (sourced from the TS adaptation
        /// field) is `true`.
        random_access_aus: u64,
    },
    /// KLV metadata stream (`stream_type` 0x15 synchronous, or 0x06+KLVA
    /// registration-descriptor private data).
    #[non_exhaustive]
    Klv {
        /// KLV records on this PID, a counter.
        ///
        /// Sender side: increments per `Muxer::push_klv` call (the
        /// muxer's contract is one record per call; the variant is
        /// surfaced for receiver-side symmetry and to give a stable
        /// counter surface if the muxer ever grows a batch API).
        ///
        /// Receiver side: increments once per `DemuxEvent::Metadata` event
        /// on this PID: one per completed access unit for a PES carried in
        /// metadata AU cells (an access unit may span PES packets, and one
        /// PES packet may complete several), one for a PES that starts with
        /// a bare KLV key instead (one event with the whole payload),
        /// whichever KLV stream type the PMT declares.
        records: u64,
    },
    /// Audio stream — only populated for MP2 + AAC-ADTS codecs in v1.
    ///
    /// AAC-LATM and AC-3 audio PIDs return [`StreamCodecStats::Unknown`]
    /// because no frame iterator exists in `codec::*` for those codecs
    /// today (tracked in `deferred-features.md`).
    #[non_exhaustive]
    Audio {
        /// Audio frames on this PID, a counter, iterated via
        /// [`crate::codec::aac::frames`] (for AAC-ADTS) or
        /// [`crate::codec::mpegaudio::frames`] (for MP2). Truncated /
        /// bad-sync tail frames are skipped (counter only counts the
        /// `Ok` results of the iterator).
        frames: u64,
    },
}

/// Internal storage. Not part of the public surface — the public
/// [`StreamCodecStats`] is built from this on accessor call.
///
/// Choosing flat-struct-plus-discriminator over a nested Rust enum keeps
/// the counter-bump call sites branch-free at the field-write line; the
/// kind discriminator is set once at first-touch and never re-checked
/// on the hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamCodecCounters {
    pub kind: CodecKind,
    /// Only meaningful when `kind == Video`. Zero otherwise.
    pub nals_or_obus: u64,
    /// Only meaningful when `kind == Video`. Zero otherwise.
    pub random_access_aus: u64,
    /// Only meaningful when `kind == Klv`. Zero otherwise.
    pub records: u64,
    /// Only meaningful when `kind == Audio`. Zero otherwise.
    pub frames: u64,
}

/// Discriminator for [`StreamCodecCounters`]. Materialized at first
/// push (Muxer) or first event (Demuxer) for a PID whose `stream_type`
/// falls into a counted family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodecKind {
    Video,
    Klv,
    Audio,
}

impl StreamCodecCounters {
    /// Accumulate video deltas (saturating).
    pub(crate) fn add_video(&mut self, nals_or_obus_delta: u64, ra_delta: u64) {
        self.nals_or_obus = self.nals_or_obus.saturating_add(nals_or_obus_delta);
        self.random_access_aus = self.random_access_aus.saturating_add(ra_delta);
    }

    /// Accumulate KLV records (saturating).
    pub(crate) fn add_klv(&mut self, records_delta: u64) {
        self.records = self.records.saturating_add(records_delta);
    }

    /// Accumulate audio frames (saturating).
    pub(crate) fn add_audio(&mut self, frames_delta: u64) {
        self.frames = self.frames.saturating_add(frames_delta);
    }

    pub(crate) fn new(kind: CodecKind) -> Self {
        Self {
            kind,
            nals_or_obus: 0,
            random_access_aus: 0,
            records: 0,
            frames: 0,
        }
    }

    pub(crate) fn to_public(self) -> StreamCodecStats {
        match self.kind {
            CodecKind::Video => StreamCodecStats::Video {
                nals_or_obus: self.nals_or_obus,
                random_access_aus: self.random_access_aus,
            },
            CodecKind::Klv => StreamCodecStats::Klv {
                records: self.records,
            },
            CodecKind::Audio => StreamCodecStats::Audio {
                frames: self.frames,
            },
        }
    }
}

/// Lazily creates a video [`StreamCodecCounters`] entry for `pid` and
/// accumulates the given deltas. The muxer's: the demuxer keeps its
/// counters in its per-PID state and goes through
/// [`StreamCodecCounters::add_video`] directly.
pub(crate) fn bump_video_counters(
    counters: &mut alloc::collections::BTreeMap<u16, StreamCodecCounters>,
    pid: u16,
    nals_or_obus_delta: u64,
    ra_delta: u64,
) {
    counters
        .entry(pid)
        .or_insert_with(|| StreamCodecCounters::new(CodecKind::Video))
        .add_video(nals_or_obus_delta, ra_delta);
}

/// Lazily creates a KLV [`StreamCodecCounters`] entry for `pid` and
/// accumulates `records_delta`. The muxer's, as
/// [`bump_video_counters`].
pub(crate) fn bump_klv_counters(
    counters: &mut alloc::collections::BTreeMap<u16, StreamCodecCounters>,
    pid: u16,
    records_delta: u64,
) {
    counters
        .entry(pid)
        .or_insert_with(|| StreamCodecCounters::new(CodecKind::Klv))
        .add_klv(records_delta);
}

/// Lazily creates an audio [`StreamCodecCounters`] entry for `pid` and
/// accumulates `frames_delta`. The muxer's, as
/// [`bump_video_counters`].
pub(crate) fn bump_audio_counters(
    counters: &mut alloc::collections::BTreeMap<u16, StreamCodecCounters>,
    pid: u16,
    frames_delta: u64,
) {
    counters
        .entry(pid)
        .or_insert_with(|| StreamCodecCounters::new(CodecKind::Audio))
        .add_audio(frames_delta);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_all_zero() {
        let s = StreamStats::default();
        assert_eq!(s.pid, 0);
        assert_eq!(s.stream_type, StreamTypeCode::default());
        assert_eq!(s.label, None);
        assert_eq!(s.items, 0);
        assert_eq!(s.bytes, 0);
        assert_eq!(s.discontinuities, 0);
    }

    #[test]
    fn stream_codec_stats_default_is_unknown() {
        let s = StreamCodecStats::default();
        assert_eq!(s, StreamCodecStats::Unknown);
    }

    #[test]
    fn counters_video_to_public_roundtrip() {
        let mut c = StreamCodecCounters::new(CodecKind::Video);
        c.nals_or_obus = 7;
        c.random_access_aus = 1;
        assert_eq!(
            c.to_public(),
            StreamCodecStats::Video {
                nals_or_obus: 7,
                random_access_aus: 1,
            },
        );
    }

    #[test]
    fn counters_klv_to_public_roundtrip() {
        let mut c = StreamCodecCounters::new(CodecKind::Klv);
        c.records = 42;
        assert_eq!(c.to_public(), StreamCodecStats::Klv { records: 42 });
    }

    #[test]
    fn counters_audio_to_public_roundtrip() {
        let mut c = StreamCodecCounters::new(CodecKind::Audio);
        c.frames = 100;
        assert_eq!(c.to_public(), StreamCodecStats::Audio { frames: 100 });
    }

    #[test]
    fn bump_video_counters_accumulates_and_lazily_creates() {
        let mut counters = alloc::collections::BTreeMap::new();
        bump_video_counters(&mut counters, 0x100, 2, 1);
        bump_video_counters(&mut counters, 0x100, 3, 0);
        let c = counters[&0x100];
        assert_eq!(c.kind, CodecKind::Video);
        assert_eq!(c.nals_or_obus, 5);
        assert_eq!(c.random_access_aus, 1);
    }

    #[test]
    fn bump_klv_counters_accumulates_and_lazily_creates() {
        let mut counters = alloc::collections::BTreeMap::new();
        bump_klv_counters(&mut counters, 0x200, 1);
        bump_klv_counters(&mut counters, 0x200, 1);
        assert_eq!(counters[&0x200].records, 2);
    }

    #[test]
    fn bump_audio_counters_accumulates_and_lazily_creates() {
        let mut counters = alloc::collections::BTreeMap::new();
        bump_audio_counters(&mut counters, 0x300, 4);
        bump_audio_counters(&mut counters, 0x300, 6);
        assert_eq!(counters[&0x300].frames, 10);
    }
}
