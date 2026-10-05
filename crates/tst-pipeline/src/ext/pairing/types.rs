//! Public types for the `pairing` module.
//!
//! These are the data shapes the caller sees on the `feed`/`flush`
//! boundary. All are flat structs/enums; no methods. Designed to
//! translate cleanly to future C ABI / JNI / UniFFI surfaces.

use std::time::Duration;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    DemuxEvent, MetadataKind, NonConformantIssue, StreamId, VideoCodec, VideoPayload,
};
use tst_core::mpegts::mux::Av1CarriageMode;
use tst_core::shared::SharedBytes;

/// Pairer matching mode for [`Pairer::with_config`](super::Pairer::with_config).
/// `last_before_pts` is past-only by definition and ignores this knob.
///
/// Field-style `Buffered { max_lag }` is unit-explicit (`Duration`
/// instead of bare ticks) and FFI-friendly.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairerMode {
    /// Pair on each `feed()` call; emit immediately. KLV that arrives
    /// after its matching video frame is dropped from pairing
    /// (surfaces as `UnpairedKlv` on eviction).
    Realtime,
    /// Buffer up to `max_lag` of arrival skew before forced emit.
    /// Higher latency, more complete pairing.
    Buffered {
        /// Maximum arrival-skew window to buffer before forced emit.
        max_lag: Duration,
    },
}

/// Options for [`Pairer::with_config`](super::Pairer::with_config).
///
/// Field-style construction is unit-explicit (`Duration` instead of bare
/// ticks) and FFI-friendly.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct PairerConfig {
    /// Matching mode: realtime (eager) or buffered (lookahead).
    pub mode: PairerMode,
    /// Maximum |video_pts - klv_pts| considered a match.
    /// `Duration::ZERO` means exact-PTS.
    pub tolerance: Duration,
    /// Cap on KLV records buffered awaiting a video match.
    /// Must be greater than zero; `Pairer::with_config` panics otherwise
    /// (`Pairer::new` uses the nonzero default and cannot hit the panic).
    pub max_buffered_klv: u64,
    /// Cap on video AUs buffered awaiting a KLV match.
    /// Must be greater than zero when using [`PairerMode::Buffered`].
    /// Ignored in [`PairerMode::Realtime`] — video is never buffered
    /// in that mode.
    pub max_buffered_video: u64,
}

impl Default for PairerConfig {
    fn default() -> Self {
        Self {
            mode: PairerMode::Realtime,
            tolerance: Duration::from_millis(300),
            max_buffered_klv: 32,
            max_buffered_video: 32,
        }
    }
}

/// One emission from [`Pairer::feed`](super::Pairer::feed) or
/// [`Pairer::flush`](super::Pairer::flush).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairerOutput {
    /// Video paired with KLV per the configured strategy.
    Paired { video: VideoSample, klv: KlvSample },
    /// Video for which no within-window KLV match could be found.
    UnpairedVideo(VideoSample),
    /// KLV that the pairer ingested but never used in a `Paired` output.
    /// Useful for telemetry — quantifies "metadata received but not
    /// consumed."
    UnpairedKlv(KlvSample),
    /// Any `DemuxEvent` not on the configured `video_pid` or `klv_pid`,
    /// or a Sample/Metadata event on a configured PID whose payload
    /// shape didn't match (e.g., audio Sample on `video_pid`). Surfaces
    /// `ProgramMap`, `NonConformant`, `Discontinuity`, samples on other
    /// PIDs, etc. Caller still has full visibility for topology
    /// discovery and diagnostics.
    PassThrough(DemuxEvent),
}

/// Projection of a `DemuxEvent::Sample { payload: SamplePayload::Video, .. }`
/// event for downstream consumption.
///
/// Raw-first: the exact encoded access unit (`raw`) and the random-access
/// indicator (`random_access_indicator`) are carried verbatim from the demuxer.
/// Parsing into typed NAL/OBU units is **opt-in** via [`VideoSample::split_units`];
/// this avoids an eager alloc per sample and preserves all bytes for lossless
/// remux after pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoSample {
    pub stream: StreamId,
    pub pts: Pts90khz,
    pub dts: Option<Pts90khz>,
    pub codec: VideoCodec,
    /// The exact encoded access unit (Annex-B for H.26x; on-wire PES payload
    /// for AV1). Parse into typed units with [`split_units`](Self::split_units).
    pub raw: SharedBytes,
    /// Whether this access unit is a random-access point (IDR for H.264/H.265,
    /// key frame for AV1). Derived from the PES header's `random_access_indicator`
    /// flag as set by the upstream encoder.
    pub random_access_indicator: bool,
    /// AV1 carriage provenance from the demuxer. `Some(mode)` for AV1 samples;
    /// `None` for H.264/H.265/H.266. Pass this to [`split_units`](Self::split_units)
    /// and to `push_video_wire_to` for a faithful round-trip.
    pub av1_carriage: Option<Av1CarriageMode>,
}

impl VideoSample {
    /// Opt-in: split `raw` into typed NAL/OBU units.
    ///
    /// Returns `(payload, issues)`. For AV1, `av1_carriage` is forwarded
    /// automatically so the framing expectation matches the on-wire bytes.
    /// Use the issue list to surface parse errors that the pairer itself
    /// does not surface — it carries all bytes intact.
    pub fn split_units(&self) -> (VideoPayload, Vec<NonConformantIssue>) {
        tst_core::mpegts::demux::split_video(
            &self.raw,
            self.codec,
            self.av1_carriage.unwrap_or_default(),
        )
    }
}

/// Projection of a `DemuxEvent::Metadata` event for downstream
/// consumption. `payload` is the raw KLV LS bytes — the demuxer has
/// already peeled the 5-byte H.222.0 §2.12.4.2 AU cell header for
/// `KlvSyncAuCell`, so the bytes feed directly to
/// `tst_core::klv::st0601::decode`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KlvSample {
    pub stream: StreamId,
    pub pts: Pts90khz,
    pub kind: MetadataKind,
    pub payload: Vec<u8>,
}

/// Counter snapshot for telemetry. Symmetric with the rest of
/// `tst-pipeline`.
#[must_use]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PairerStats {
    pub paired: u64,
    pub unpaired_video: u64,
    pub unpaired_klv: u64,
    pub pass_through: u64,
}
