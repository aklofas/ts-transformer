//! Demuxer supporting types — stats, options, per-program tracker, and
//! builder. Co-resident with the `Demuxer` impl in `demuxer.rs`
//! before the Phase 5 split (audit theme I).

use crate::mpegts::demux::event::{StreamInfo, StreamKind};
use crate::mpegts::demux::strict::StrictMode;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use hashbrown::HashSet;

/// Stats snapshot for [`Demuxer`](crate::mpegts::demux::Demuxer). Used by
/// `tst_pipeline::DemuxReceiver` to compose its own `DemuxReceiverStats`;
/// also exposed publicly for callers using
/// [`Demuxer`](crate::mpegts::demux::Demuxer) directly.
///
/// Per-stream entries are created lazily as events are emitted — the
/// receiver discovers topology rather than configuring it up front. PSI
/// PIDs (PAT 0x0000, active PMT PID) get hardcoded labels "PAT" / "PMT".
#[must_use]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DemuxerStats {
    /// Number of `ProgramMap` events emitted (one per PMT version seen).
    pub program_maps_seen: u64,
    /// Number of distinct PMT version_number values seen, including the
    /// initial sighting. Resets to zero on `reset_stats`, so the next PMT
    /// always increments this counter.
    pub pmt_versions_seen: u64,
    /// Total discontinuity events emitted across all PIDs.
    pub discontinuities: u64,
    /// Total non-conformant events emitted across all PIDs.
    pub nonconformant: u64,
    /// Number of programs currently tracked (entries in the PAT that have
    /// been received). Reflects the live PAT — increases when a PAT version
    /// bump adds a program, decreases when one is removed.
    pub programs_seen: u32,
    /// Number of distinct subtitle PIDs the demuxer has seen at least one
    /// PES sample for. Increments on the first `SamplePayload::Subtitle`
    /// event per PID; resets to zero on `reset_stats`.
    pub subtitle_streams_seen: u32,
    /// Per-PID counters. Keys are PIDs. Entries are created on first event
    /// for a given PID; PSI PIDs (0x0000 for PAT, the PMT PID) are added
    /// with fixed "PAT"/"PMT" labels when a `ProgramMap` event fires.
    pub per_stream: BTreeMap<u16, crate::mpegts::stats::StreamStats>,
}

/// Default per-PID PES reassembly cap. 4 MiB accommodates 4K H.265 IDR
/// keyframes (typically 1–2 MB) with headroom, and matches the order of
/// magnitude of typical per-PID PES sizes in well-formed streams. Breach
/// surfaces as a `Discontinuity { kind: PesOversize }` event and the
/// partial PES on that PID is dropped.
pub(super) const DEFAULT_PES_CAP_PER_PID: usize = 4 * 1024 * 1024;
/// Default aggregate PES reassembly cap across all PIDs. 64 MiB defends
/// against a multi-PID flood scenario where each PID stays under its own
/// per-PID cap but the aggregate explodes. Breach surfaces as a
/// `Discontinuity { kind: PesTotalOversize }` event and all in-flight
/// partial PES on every PID are dropped.
pub(super) const DEFAULT_PES_CAP_TOTAL: usize = 64 * 1024 * 1024;

/// Default per-PID AU cell reassembly buffer cap (1 MiB). Comfortable
/// for any realistic MISB sync-metadata AU (ST 0903 VMTI with hundreds
/// of target packs is ~hundreds of KB at most); well below the
/// per-PES default. Configurable via
/// [`DemuxerConfigBuilder::au_cell_cap_per_pid`].
pub(super) const DEFAULT_AU_CELL_CAP_PER_PID: usize = 1024 * 1024;

/// Default aggregate AU cell reassembly cap across all PIDs (16 MiB).
/// Defends a multi-PID flood where each PID stays under its own per-PID
/// cap but the in-flight total explodes — mirrors `DEFAULT_PES_CAP_TOTAL`.
/// 16 PIDs at the per-PID default (1 MiB) fit; breach surfaces as
/// `NonConformantIssue::MultiCellAu { reason: OverflowTotal, .. }` and the
/// offending PID's partial buffer is dropped.
pub(super) const DEFAULT_AU_CELL_CAP_TOTAL: usize = 16 * 1024 * 1024;

/// Default ceiling on concurrently in-flight AU-cell reassembly PIDs (64).
/// Real STANAG 4609 streams carry a small number of metadata PIDs; 64 is
/// generous headroom. Bounds active-PID state against an adversary opening
/// a `First` for thousands of distinct PIDs without ever sending `Last`.
/// Breach surfaces as
/// `NonConformantIssue::MultiCellAu { reason: TooManyPids, .. }`.
pub(super) const DEFAULT_AU_CELL_MAX_IN_FLIGHT_PIDS: usize = 64;

/// Caller-supplied overrides for the demuxer.
///
/// `Default` is hand-written (not derived) so `cfi_tolerance` can default
/// to `true`. Every other field uses its own `Default` impl.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct DemuxerConfig {
    pub strict: StrictMode,
    /// Per-PID PES reassembly cap. `None` uses `DEFAULT_PES_CAP_PER_PID`
    /// (4 MiB). Tune up for ultra-high-bitrate streams whose IDR keyframes
    /// exceed the default; tune down for adversarial-input scenarios where
    /// faster failure (and a tighter memory bound) is preferable.
    pub pes_cap_per_pid: Option<usize>,
    /// Aggregate PES reassembly cap across all PIDs. `None` uses
    /// `DEFAULT_PES_CAP_TOTAL` (64 MiB). Tune up for streams with many
    /// concurrent high-bitrate PIDs; tune down to bound multi-PID flood
    /// memory growth in adversarial-input scenarios.
    pub pes_cap_total: Option<usize>,
    /// Ceiling on the demuxer's pre-sync ingress buffer, in bytes. This bounds
    /// how many bytes a single `feed` call (plus any unconsumed residue) may
    /// hold before sync-scan; a whole-file feed larger than the ceiling is
    /// rejected with [`crate::error::DemuxError::SyncBufExhausted`]. `None`
    /// uses the 4 MiB default. Distinct from `pes_cap_*`, which bound PES
    /// *reassembly*.
    pub sync_buf_cap: Option<usize>,
    pub klv_link_overrides: Vec<(u16, u16)>,
    pub stream_kind_overrides: BTreeMap<u16, StreamKind>,
    /// When `true`, PSI section reassembly accepts continuation packets
    /// across continuity-counter jumps (today's permissive behavior —
    /// section either passes by luck or fails CRC). Default `false` is
    /// strict-correctness: drop the partial section on jump and emit
    /// `NonConformantIssue::PsiCcDiscontinuity`. Matches ffmpeg
    /// `mpegts.c:3118-3142`.
    pub lenient_psi_reassembly: bool,
    /// AV1 PES carriage mode the demuxer expects. Default
    /// [`crate::mpegts::mux::Av1CarriageMode::Mpeg2TsBinding`]
    /// (spec-conformant per the AV1-in-MPEG-2-TS binding).
    ///
    /// In `Mpeg2TsBinding` mode the demuxer expects PES
    /// `stream_id=0xBD` and `ts_open_bitstream_unit()` framing on each
    /// OBU. Violations surface as
    /// [`crate::mpegts::demux::NonConformantIssue::Av1WrongStreamId`]
    /// and [`crate::mpegts::demux::NonConformantIssue::Av1MissingTsObuFraming`].
    /// The demuxer falls back to raw-OBU parsing in lenient mode so the
    /// sample still surfaces.
    ///
    /// In `InteropRawObu` mode the demuxer accepts raw OBUs without the
    /// `ts_open_bitstream_unit` framing (matches ffmpeg / libaom /
    /// hls.js / mediamtx today) and does not raise the binding issues.
    pub av1_carriage: crate::mpegts::mux::Av1CarriageMode,
    /// Per-PID cap on the in-flight AU cell reassembly buffer.
    /// `None` uses `DEFAULT_AU_CELL_CAP_PER_PID` (1 MiB). When the
    /// buffered inner-byte total would exceed this, the demuxer drops
    /// the buffer and emits
    /// [`crate::mpegts::demux::NonConformantIssue::MultiCellAu`] with
    /// `reason = MultiCellAuReason::Overflow`. Tune up for streams with
    /// unusually large sync-metadata AUs; tune down for adversarial-input
    /// scenarios where faster failure is preferable.
    pub au_cell_cap_per_pid: Option<usize>,
    /// Aggregate cap on in-flight AU cell reassembly bytes across all PIDs.
    /// `None` uses `DEFAULT_AU_CELL_CAP_TOTAL` (16 MiB). When buffering a
    /// cell would push the aggregate total over this, the demuxer drops
    /// the offending PID's buffer and emits
    /// [`crate::mpegts::demux::NonConformantIssue::MultiCellAu`] with
    /// `reason = MultiCellAuReason::OverflowTotal`. Defends a multi-PID
    /// flood where each PID stays under `au_cell_cap_per_pid` but the
    /// aggregate explodes.
    pub au_cell_cap_total: Option<usize>,
    /// Ceiling on the number of PIDs with a concurrently in-flight AU cell
    /// reassembly (an open `First` awaiting its `Last`). `None` uses
    /// `DEFAULT_AU_CELL_MAX_IN_FLIGHT_PIDS` (64). A `First` cell that would
    /// open reassembly on a PID beyond this is rejected with
    /// [`crate::mpegts::demux::NonConformantIssue::MultiCellAu`]
    /// `reason = MultiCellAuReason::TooManyPids`; existing reassemblies are
    /// left intact. Bounds active-PID state against an adversary opening a
    /// `First` for thousands of distinct PIDs without ever sending `Last`.
    pub au_cell_max_in_flight_pids: Option<usize>,
    /// Tolerate sync-metadata AU cells that arrive with
    /// `cell_fragment_indication` bits set to `0b00` (middle) or `0b01`
    /// (last) when there is no active reassembly buffer for the PID.
    ///
    /// **Default `true`** — pragmatic for real-world STANAG 4609 streams.
    /// Corpus-wide validation across 251 captures (37 GB, multiple
    /// platforms) found ~99% of `NonConformant` events in the field are
    /// `MalformedAuCellCfiTolerated`: industry encoders default-initialize
    /// the field to zero and ship CFI=`0b00` (Middle) on single-cell AUs
    /// that should be `0b11` (Complete). MISB ST 1402.2 Appendix B lists
    /// the four bit patterns without semantic explanation, and no other
    /// public reference decoder (FFmpeg's `mpegtsenc.c`, GStreamer's
    /// `tsdemux.c::parse_pes_metadata_frame`, TSDuck, paretech/klvdata,
    /// jimcavoy/klvp) enforces CFI — the spec-strict reading lost its
    /// last enforcement battery, and producers ship malformed CFI bits
    /// without anything downstream catching it.
    ///
    /// When tolerance is on, the demuxer additionally validates the
    /// orphan cell's inner payload as a single complete KLV unit
    /// (SMPTE 336M UL prefix `06 0e 2b 34` followed by a BER length
    /// that describes exactly the available payload). If it passes, the
    /// cell is emitted as a
    /// [`crate::mpegts::demux::MetadataKind::KlvSyncAuCell`] with
    /// `cell_fragment_indication = Complete` AND a
    /// [`crate::mpegts::demux::NonConformantIssue::CfiTolerated`]
    /// diagnostic so the malformation remains visible to callers (this
    /// is what makes the corpus-wide pattern observable in the first
    /// place — tolerance recovers data without hiding the bug).
    ///
    /// Set to `false` for spec-strict conformance testing per
    /// H.222.0 V9 §2.12.4.2 Table 2-157: orphan Middle/Last cells then
    /// produce
    /// [`crate::mpegts::demux::NonConformantIssue::MultiCellAu`] with
    /// `reason = MultiCellAuReason::Orphan` and no metadata event. Use
    /// when validating a producer against the wire spec rather than
    /// consuming real-world traffic. (Note: this is asymmetric with
    /// `lenient_psi_reassembly`, which still defaults `false`/strict.
    /// The asymmetry is calibrated to corpus evidence — we have
    /// empirical proof the CFI bug is dominant in real traffic; we do
    /// not have equivalent evidence for PSI reassembly violations.)
    pub cfi_tolerance: bool,
    /// Unwrap the demuxer's raw 33-bit 90 kHz PTS/DTS into a monotonic
    /// `i64` timeline, per PID.
    ///
    /// **Default `false`** — off. When `false`,
    /// [`DemuxEvent::Sample`](crate::mpegts::demux::DemuxEvent::Sample)
    /// and
    /// [`DemuxEvent::Metadata`](crate::mpegts::demux::DemuxEvent::Metadata)
    /// carry the raw wire PTS/DTS exactly as they do today: a per-PID
    /// value in `0..2^33` that wraps to 0 at the H.222.0 §2.4.3.6
    /// rollover boundary (~26.5 h at 90 kHz).
    ///
    /// When `true`, a per-PID accumulator carries each sample's signed
    /// wrap-aware delta from the previous raw value onto the previous
    /// unwrapped value. A forward wrap therefore advances the timeline
    /// by a full `1 << 33`, while a genuine backward step — an
    /// out-of-order arrival, including a pre-wrap PTS delivered *after*
    /// the wrap — steps back by its true distance instead of being
    /// mistaken for another wrap (the emitted value is then allowed to
    /// be non-monotonic, correctly reflecting the reorder). The first
    /// observed PTS on a PID anchors the timeline (emitted value == the
    /// raw value). The timeline is never rebased to zero, so a video PID
    /// and a KLV PID sharing one 33-bit wire clock stay directly
    /// comparable across the whole session — this is what lets a
    /// consumer pair KLV to video frames by PTS on a long-running stream
    /// that crosses the rollover.
    ///
    /// DTS unwraps against its own PES's PTS rather than the shared
    /// per-PID offset, so a DTS that straddles the wrap boundary (DTS
    /// still pre-wrap while its PES's PTS has already wrapped) lands in
    /// the correct epoch instead of over-shifting by a full `1 << 33`.
    /// A synthesized `pts = 0` (see
    /// [`NonConformantIssue::MissingRequiredPts`](crate::mpegts::demux::NonConformantIssue::MissingRequiredPts))
    /// is never fed to the accumulator.
    ///
    /// The accumulator resets alongside all other per-PID parse state on
    /// [`Demuxer::reset_sync`](crate::mpegts::demux::Demuxer::reset_sync)
    /// — a reconnect restarts the unwrap timeline.
    ///
    /// # C ABI
    ///
    /// `tst_demux_config_set_unwrap_timestamps` — see
    /// `bindings/c/include/tstrans.h`.
    pub unwrap_timestamps: bool,
}

impl Default for DemuxerConfig {
    fn default() -> Self {
        Self {
            strict: StrictMode::default(),
            pes_cap_per_pid: None,
            pes_cap_total: None,
            sync_buf_cap: None,
            klv_link_overrides: Vec::new(),
            stream_kind_overrides: BTreeMap::new(),
            lenient_psi_reassembly: false,
            av1_carriage: crate::mpegts::mux::Av1CarriageMode::default(),
            au_cell_cap_per_pid: None,
            au_cell_cap_total: None,
            au_cell_max_in_flight_pids: None,
            // Tolerance-by-default — see field rustdoc above.
            cfi_tolerance: true,
            unwrap_timestamps: false,
        }
    }
}

/// Per-program demuxer state. Crate-private — accessed only by `Demuxer`
/// and its `pub(crate) fn programs_for_test()` accessor for in-crate tests.
#[derive(Debug)]
pub(crate) struct ProgramTracker {
    pub program_number: u16,
    pub pmt_version: Option<u8>,
    pub pcr_pid: Option<u16>,
    pub streams: Vec<StreamInfo>,
    /// PIDs that have already had a KLV stream-type-mismatch nonconformant
    /// emitted for the current PMT version. Cleared on PMT version bump.
    pub(crate) klv_mismatch_coalesce: HashSet<u16>,
}

/// Builder for [`DemuxerConfig`].
///
/// Construct via [`DemuxerConfig::builder`], [`DemuxerConfigBuilder::new`], or
/// [`DemuxerConfigBuilder::default`], chain option methods, and call
/// [`DemuxerConfigBuilder::build`] to produce a [`DemuxerConfig`].
/// Pass the config to [`Demuxer::with_config`](crate::mpegts::demux::Demuxer::with_config)
/// to create the demuxer.
#[must_use]
#[derive(Debug, Default)]
pub struct DemuxerConfigBuilder {
    options: DemuxerConfig,
}

impl DemuxerConfigBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn strict(mut self, mode: StrictMode) -> Self {
        self.options.strict = mode;
        self
    }

    pub fn pes_cap_per_pid(mut self, bytes: usize) -> Self {
        self.options.pes_cap_per_pid = Some(bytes);
        self
    }

    pub fn pes_cap_total(mut self, bytes: usize) -> Self {
        self.options.pes_cap_total = Some(bytes);
        self
    }

    /// Set the pre-sync ingress buffer ceiling. See
    /// [`DemuxerConfig::sync_buf_cap`].
    pub fn sync_buf_cap(mut self, bytes: usize) -> Self {
        self.options.sync_buf_cap = Some(bytes);
        self
    }

    pub fn link_klv(mut self, klv_pid: u16, video_pid: u16) -> Self {
        self.options.klv_link_overrides.push((klv_pid, video_pid));
        self
    }

    pub fn treat_as(mut self, pid: u16, kind: StreamKind) -> Self {
        self.options.stream_kind_overrides.insert(pid, kind);
        self
    }

    /// Set the expected AV1 PES carriage mode. Default
    /// [`crate::mpegts::mux::Av1CarriageMode::Mpeg2TsBinding`] (spec-conformant).
    /// Set to `InteropRawObu` to match ffmpeg/libaom/hls.js senders.
    pub fn av1_carriage(mut self, mode: crate::mpegts::mux::Av1CarriageMode) -> Self {
        self.options.av1_carriage = mode;
        self
    }

    /// Set the per-PID AU cell reassembly buffer cap. See
    /// [`DemuxerConfig::au_cell_cap_per_pid`].
    pub fn au_cell_cap_per_pid(mut self, bytes: usize) -> Self {
        self.options.au_cell_cap_per_pid = Some(bytes);
        self
    }

    /// Set the aggregate AU cell reassembly cap across all PIDs. See
    /// [`DemuxerConfig::au_cell_cap_total`].
    pub fn au_cell_cap_total(mut self, bytes: usize) -> Self {
        self.options.au_cell_cap_total = Some(bytes);
        self
    }

    /// Set the ceiling on concurrently in-flight AU cell reassembly PIDs.
    /// See [`DemuxerConfig::au_cell_max_in_flight_pids`].
    pub fn au_cell_max_in_flight_pids(mut self, pids: usize) -> Self {
        self.options.au_cell_max_in_flight_pids = Some(pids);
        self
    }

    /// Enable opt-in tolerance for sync-metadata AU cells whose
    /// `cell_fragment_indication` bits are set to `0b00` (middle) or
    /// `0b01` (last) without a prior First cell. See
    /// [`DemuxerConfig::cfi_tolerance`].
    pub fn cfi_tolerance(mut self, enable: bool) -> Self {
        self.options.cfi_tolerance = enable;
        self
    }

    /// Enable the opt-in monotonic PTS/DTS unwrap. See
    /// [`DemuxerConfig::unwrap_timestamps`].
    pub fn unwrap_timestamps(mut self, enable: bool) -> Self {
        self.options.unwrap_timestamps = enable;
        self
    }

    pub fn build(self) -> DemuxerConfig {
        self.options
    }
}

impl DemuxerConfig {
    /// Create a [`DemuxerConfigBuilder`] with default options.
    ///
    /// Mirrors [`MuxerConfig::builder`](crate::mpegts::mux::MuxerConfig::builder):
    /// the builder produces the config, then pass it to
    /// [`Demuxer::with_config`](crate::mpegts::demux::Demuxer::with_config).
    pub fn builder() -> DemuxerConfigBuilder {
        DemuxerConfigBuilder::default()
    }
}

#[cfg(test)]
pub(crate) const fn default_pes_cap_per_pid() -> usize {
    DEFAULT_PES_CAP_PER_PID
}

#[cfg(test)]
pub(crate) const fn default_pes_cap_total() -> usize {
    DEFAULT_PES_CAP_TOTAL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cfi_tolerance_is_true() {
        // Tolerance-by-default posture: corpus-wide validation showed
        // the producer-side CFI=00-on-single-cell-AU bug is dominant in
        // real STANAG 4609 traffic (~99% of NonConformant events). The
        // CfiTolerated diagnostic still fires so the malformation stays
        // visible to validators. Receivers can set `cfi_tolerance: false`
        // explicitly for spec-strict conformance testing.
        assert!(DemuxerConfig::default().cfi_tolerance);
    }

    #[test]
    fn builder_sets_cfi_tolerance() {
        let builder = DemuxerConfig::builder().cfi_tolerance(true);
        let config = builder.options;
        assert!(config.cfi_tolerance);
    }

    #[test]
    fn builder_can_toggle_cfi_tolerance_off() {
        let builder = DemuxerConfig::builder()
            .cfi_tolerance(true)
            .cfi_tolerance(false);
        assert!(!builder.options.cfi_tolerance);
    }

    #[test]
    fn default_unwrap_timestamps_is_false() {
        assert!(!DemuxerConfig::default().unwrap_timestamps);
    }

    #[test]
    fn builder_sets_unwrap_timestamps() {
        let builder = DemuxerConfig::builder().unwrap_timestamps(true);
        assert!(builder.options.unwrap_timestamps);
    }
}
