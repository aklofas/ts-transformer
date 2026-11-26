//! Opt-in KLV ↔ video pairer.
//!
//! Stateful transducer that ingests `DemuxEvent`s and emits typed
//! `PairerOutput`s. Two strategies:
//!
//! * [`Pairer::with_config`] — match each video AU against the KLV
//!   with nearest PTS, within a configured tolerance. Two modes:
//!   [`PairerMode::Realtime`] (zero buffer, eager emission) and
//!   [`PairerMode::Buffered`] (bounded arrival skew, bidirectional
//!   matching).
//! * [`Pairer::last_before_pts`] — sample-and-hold: each video AU pairs
//!   with the most recent KLV where `klv.pts <= video.pts`, optionally
//!   bounded by a freshness ceiling. The last eight KLV samples are kept,
//!   so a reordered frame still finds the sample before it.
//!
//! For byte-feeding callers (language bindings, offline ingest) that
//! have TS bytes rather than [`DemuxEvent`]s, see [`PairingDemuxer`] —
//! it owns a `Demuxer` + `Pairer` and feeds bytes straight to
//! `PairerOutput`s.
//!
//! The pairer is **opt-in** — callers construct it explicitly;
//! [`crate::DemuxReceiver`] does not reach for it by default, preserving
//! the demux module's decoupled-pairing posture.
//!
//! # PTS handling
//!
//! All time values are in 90 kHz ticks per ISO/IEC 13818-1. The demuxer
//! emits per-event PTS values bounded `0..(2^33 − 1)` per H.222.0 V9
//! §2.4.3.7 — the field's 33-bit unsigned range. Two consequences for
//! the pairer:
//!
//! - **PTS is not monotone in decode order.** Streams with B-frames
//!   deliver video AUs whose PTS falls below a frame already seen, and
//!   below KLV that already arrived. The nearest pairer matches against
//!   its KLV history; in [`PairerMode::Realtime`] a video that arrives
//!   before its KLV is emitted as `UnpairedVideo` at once. The
//!   last-before pairer keeps the last eight KLV samples and pairs a
//!   frame with the one carrying the largest PTS at or before its own,
//!   so a reordered frame whose KLV is still in that history pairs with
//!   the sample that preceded it rather than going unpaired.
//! - **Across the 33-bit rollover boundary** (≈ 26.5 h at 90 kHz), the
//!   demuxer's emitted PTS wraps back to 0 (it does not accumulate
//!   stream-monotonic deltas). The nearest pairer in
//!   [`PairerMode::Buffered`] treats a PTS more than `2^32` ticks below
//!   the newest one it has seen as a wrap: it classifies every buffered
//!   video against the pre-wrap history (paired if in tolerance, else
//!   `UnpairedVideo`), emits never-used history as `UnpairedKlv`, and
//!   starts afresh, so frames buffered before the wrap are released
//!   rather than held. A reordered straddle (post-wrap frames, then a
//!   pre-wrap B-frame, then post-wrap frames again) raises the newest PTS
//!   back above the wrap and triggers a second reset: every output is
//!   still accounted for, but post-wrap frames buffered at that moment
//!   may be released unpaired. No pairing is made across the wrap itself;
//!   long-session callers who need continuity across it should use a
//!   monotone PTS source. This holds only when
//!   `DemuxerConfig::unwrap_timestamps` is `false` (the default); with
//!   it enabled — reachable here through `PairingDemuxerConfig.demuxer`
//!   — the emitted timeline is unwrapped, never jumps back by `2^32`,
//!   and the pairer's saturating i64 arithmetic handles the extended
//!   range.
//! - **A reconnect or a signalled discontinuity starts a new time base.**
//!   PTS values on either side of it are unrelated, so both strategies
//!   drop their history there and pair nothing across it; see the
//!   "Epochs" section of [`Pairer`].
//!
//! Internal arithmetic uses `saturating_add` / `saturating_sub` against
//! `i64::MAX` so non-conformant sources feeding raw 33-bit PTS values
//! (or values past the spec range) cannot trigger overflow.
//! `tst_core::mpegts::common::pts_diff_33bit` is available as a
//! defensive primitive if downstream pairer extensions need wrap-aware
//! delta math.
//!
//! Use [`tst_core::mpegts::demux::pts_to_duration`] for diagnostic
//! conversion to `Duration`.
//!
//! # Cross-language wrappers
//!
//! Shipped in Python (`tstrans.pipeline`) and JVM (`org.tstrans.pipeline.Pairer`).
//! C ABI / UniFFI exposure is not shipped yet. The Rust types in this
//! module are designed to translate cleanly to it; [`PairingDemuxer`] is the byte-feeding
//! surface those bindings consume (no `DemuxEvent` crosses the FFI
//! boundary).
//!
//! # Cookbook
//!
//! See `docs/cookbook/pairing/pairer-realtime.md`,
//! `docs/cookbook/pairing/pairer-batch.md`,
//! `docs/cookbook/pairing/pairer-last-before-pts.md`, and
//! `docs/cookbook/pairing/eo-ir-shared-klv-pairer.md` for canonical
//! realtime, batch-ingest, async sample-and-hold, and EO+IR composition
//! patterns.

mod demuxer;
mod last_before;
mod nearest;
#[cfg(test)]
mod test_support;
mod types;

pub use demuxer::{PairingDemuxer, PairingDemuxerConfig};
pub use types::{KlvSample, PairerConfig, PairerMode, PairerOutput, PairerStats, VideoSample};

use std::time::Duration;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, DiscontinuityKind, SamplePayload};

/// Convert a `Duration` to MPEG-TS 90 kHz PTS ticks. Saturating —
/// inputs larger than `i64::MAX / 90_000` seconds clamp to `i64::MAX`.
fn duration_to_pts_ticks(d: Duration) -> i64 {
    // 90 kHz ticks: ticks = secs * 90_000. Use as_nanos to preserve
    // sub-second precision down to ~11 µs (one tick).
    let nanos = d.as_nanos();
    // 1 tick = 1/90_000 s = 11_111.111... ns. ticks = nanos * 90_000 / 1e9
    //                                               = nanos * 9 / 100_000.
    let ticks_u128 = nanos.saturating_mul(9) / 100_000;
    if ticks_u128 > i64::MAX as u128 {
        i64::MAX
    } else {
        ticks_u128 as i64
    }
}

/// Categorized form of a `DemuxEvent` after PID-based routing. Both pairing
/// state machines share this to avoid duplicating the match arms in their
/// respective `feed()` methods.
pub(super) enum EventClass {
    Video(VideoSample),
    Klv(KlvSample),
    Other(DemuxEvent),
}

/// Route a `DemuxEvent` by PID into one of three categories.
///
/// Video events on `video_pid` with a `SamplePayload::Video` become
/// `EventClass::Video`; `Metadata` events on `klv_pid` become
/// `EventClass::Klv`; everything else (audio on video_pid, video on klv_pid,
/// PMT, Discontinuity, etc.) becomes `EventClass::Other`.
pub(super) fn classify(event: DemuxEvent, video_pid: u16, klv_pid: u16) -> EventClass {
    match event {
        DemuxEvent::Sample {
            stream,
            pts,
            dts,
            payload:
                SamplePayload::Video {
                    codec,
                    raw,
                    random_access_indicator,
                    av1_carriage,
                },
        } if stream.pid == video_pid => EventClass::Video(VideoSample {
            stream,
            pts,
            dts,
            codec,
            raw,
            random_access_indicator,
            av1_carriage,
        }),
        DemuxEvent::Metadata {
            stream,
            pts,
            kind,
            payload,
        } if stream.pid == klv_pid => EventClass::Klv(KlvSample {
            stream,
            pts,
            kind,
            payload,
        }),
        other => EventClass::Other(other),
    }
}

/// Saturating absolute PTS distance in 90 kHz ticks.
///
/// Returns `|a − b|` with saturation: if the subtraction overflows (possible
/// when a non-conformant source emits PTS values near the `i64` extremes),
/// the result clamps to `i64::MAX` rather than panicking or wrapping.
pub(super) fn pts_distance(a: Pts90khz, b: Pts90khz) -> i64 {
    a.as_ticks().saturating_sub(b.as_ticks()).saturating_abs()
}

/// How near the newest pre-flag PTS the first KLV or video PES after a
/// signalled discontinuity must be to count as the old-clock PES in flight: two
/// seconds, past the sample interval of a 1 Hz KLV feed plus its jitter.
const OLD_CLOCK_WINDOW: i64 = 2 * 90_000;

/// Whether the next KLV sample may be the old-clock PES in flight.
#[derive(Clone, Copy)]
enum InFlight {
    /// Nothing old can be in flight.
    No,
    /// A discontinuity was signalled; holds the newest KLV and video PTS
    /// before it (at least one is `Some`).
    Suspect {
        klv: Option<Pts90khz>,
        video: Option<Pts90khz>,
    },
    /// The old-clock PES was found; its further cells share this PTS.
    Old(Pts90khz),
}

/// The epoch state both pairing strategies keep: which off-route events
/// start a new time base, and which samples after one still carry the old.
///
/// An epoch boundary is `ReconnectDiscontinuity`, or
/// `Discontinuity { kind: AdaptationFieldFlag }` on the video PID, the KLV PID
/// or the PCR PID of the program carrying the video (learned from its
/// `ProgramMap`). The strategy empties its history there, ahead of the
/// passed-through event. A flag on any other PID, and a silent restart, are
/// invisible: the caller flushes. The PCR PID counts whether or not it also
/// carries an elementary stream: the demuxer reports a flag on a PID that
/// carries only PCR too, so a restart signalled there starts an epoch like
/// any other.
///
/// A signalled discontinuity does not end the PES in flight on the KLV or
/// video PID: it completes after the flag, on the old clock its header was
/// coded on (H.222.0 §2.4.3.5), whichever PID carried the flag. Only one PES
/// is in flight per PID, so only the first KLV sample and the first video
/// sample completing after the flag can be old. A KLV sample is taken as old
/// when its PTS lies within [`OLD_CLOCK_WINDOW`] of the newest KLV PTS or the
/// newest video PTS before the flag (the video reference covers KLV sent
/// rarely or on change, and a flag before any KLV); a video sample, within
/// [`OLD_CLOCK_WINDOW`] of the newest video PTS. The strategy lets each leave
/// as `UnpairedKlv` or `UnpairedVideo`, and a KLV one does not enter the
/// history. A new clock that lands within the window of the old one loses its
/// first KLV sample and its first video frame the same way.
pub(super) struct EpochTracker {
    video_pid: u16,
    klv_pid: u16,
    /// PCR PID of the program carrying `video_pid`, once its program map has
    /// passed. Kept when a later map drops the video PID. A flag there resets
    /// the time base both paired PIDs run on.
    pcr_pid: Option<u16>,
    /// PTS of the newest KLV sample since the last reconnect.
    last_klv_pts: Option<Pts90khz>,
    /// PTS of the newest video sample since the last reconnect.
    last_video_pts: Option<Pts90khz>,
    in_flight: InFlight,
    /// The newest video PTS before a signalled discontinuity, until the first
    /// video after it is judged against it.
    video_in_flight: Option<Pts90khz>,
}

impl EpochTracker {
    pub(super) fn new(video_pid: u16, klv_pid: u16) -> Self {
        Self {
            video_pid,
            klv_pid,
            pcr_pid: None,
            last_klv_pts: None,
            last_video_pts: None,
            in_flight: InFlight::No,
            video_in_flight: None,
        }
    }

    /// Takes note of an off-route event. True when it starts a new epoch: the
    /// caller empties its history before passing the event through.
    pub(super) fn starts_epoch(&mut self, event: &DemuxEvent) -> bool {
        if let DemuxEvent::ProgramMap(pmt) = event
            && pmt.streams.iter().any(|s| s.pid == self.video_pid)
        {
            self.pcr_pid = Some(pmt.pcr_pid);
        }
        if !self.is_epoch_boundary(event) {
            return false;
        }
        // A reconnect resets the demuxer, so no PES survives it.
        if matches!(event, DemuxEvent::ReconnectDiscontinuity) {
            self.last_klv_pts = None;
            self.last_video_pts = None;
            self.in_flight = InFlight::No;
            self.video_in_flight = None;
            // The new connection may lay its programs out differently; the
            // PCR PID is relearned from its video program map.
            self.pcr_pid = None;
        } else if self.last_klv_pts.is_some() || self.last_video_pts.is_some() {
            self.in_flight = InFlight::Suspect {
                klv: self.last_klv_pts,
                video: self.last_video_pts,
            };
            self.video_in_flight = self.last_video_pts;
        }
        true
    }

    /// True for the events that start a new epoch: a transport reconnect, or
    /// a signalled discontinuity on the video PID, the KLV PID or the PCR PID
    /// of their program. A `ContinuityJump` (packet loss) and any event on
    /// another PID are not epochs.
    fn is_epoch_boundary(&self, event: &DemuxEvent) -> bool {
        match event {
            DemuxEvent::ReconnectDiscontinuity => true,
            DemuxEvent::Discontinuity {
                stream,
                kind: DiscontinuityKind::AdaptationFieldFlag,
            } => {
                stream.pid == self.video_pid
                    || stream.pid == self.klv_pid
                    || self.pcr_pid == Some(stream.pid)
            }
            _ => false,
        }
    }

    /// Records a video sample's PTS. True when it is the old-clock PES in
    /// flight at a signalled discontinuity: it leaves as `UnpairedVideo`.
    pub(super) fn video_is_old(&mut self, pts: Pts90khz) -> bool {
        self.last_video_pts = Some(pts);
        matches!(
            self.video_in_flight.take(),
            Some(before) if pts_distance(pts, before) <= OLD_CLOCK_WINDOW
        )
    }

    /// Records a KLV sample's PTS. True when it is (a cell of) the old-clock
    /// PES in flight at a signalled discontinuity: it leaves as `UnpairedKlv`
    /// and stays out of the history.
    pub(super) fn klv_is_old(&mut self, pts: Pts90khz) -> bool {
        self.last_klv_pts = Some(pts);
        let old = match self.in_flight {
            InFlight::No => false,
            InFlight::Suspect { klv, video } => [klv, video]
                .into_iter()
                .flatten()
                .any(|before| pts_distance(pts, before) <= OLD_CLOCK_WINDOW),
            InFlight::Old(old_pts) => pts == old_pts,
        };
        self.in_flight = if old {
            InFlight::Old(pts)
        } else {
            InFlight::No
        };
        old
    }

    /// Forgets the clock and whatever was in flight, so the next sample starts
    /// clean. The program's PCR PID is kept.
    pub(super) fn forget_clock(&mut self) {
        self.in_flight = InFlight::No;
        self.last_klv_pts = None;
        self.last_video_pts = None;
        self.video_in_flight = None;
    }
}

/// Stateful KLV ↔ video pairer. Construct with one of the strategy
/// constructors; feed `DemuxEvent`s; collect `PairerOutput`s.
///
/// The pairer holds bounded internal state per its strategy. It is
/// video-driven: each `Sample::Video` event on the configured
/// `video_pid` produces exactly one `Paired` or `UnpairedVideo` output.
/// A `Metadata` event on the configured `klv_pid` stays in the KLV
/// history after it pairs, so one KLV may appear in several `Paired`
/// outputs; only a KLV that leaves the history without ever pairing is
/// emitted as `UnpairedKlv`. Off-route events surface as `PassThrough`.
///
/// # Epochs
///
/// Every strategy drops its history when the demuxer signals a new time
/// base: at `DemuxEvent::ReconnectDiscontinuity` (emitted by
/// `ManagedDemuxReceiver` after a reconnect) and at a
/// `Discontinuity { kind: AdaptationFieldFlag }` on the video PID, the KLV
/// PID or the PCR PID of the program carrying the video (a set
/// `discontinuity_indicator`, H.222.0 §2.4.3.5; the PCR PID is learned from
/// that program's `ProgramMap`). In `PairerMode::Buffered` the buffered
/// video AUs are first classified against the history held so far (`Paired`
/// if in tolerance, else `UnpairedVideo`); then the KLV entries that never
/// paired are emitted as `UnpairedKlv`; then the event is passed through.
/// The history the pairer holds never pairs across the boundary.
///
/// Samples of the old clock that complete after a flag are not marked,
/// except the first of each kind (next paragraph). With KLV muxed more than
/// one sample behind its video and a new clock within the pairing tolerance
/// of the old one, a second old-clock KLV sample can enter the new history
/// and pair with new-clock frames.
///
/// The PES in flight at a flag completes after it on the old clock. The
/// first KLV sample and the first video sample completing after a flag
/// leave unpaired (`UnpairedKlv`, `UnpairedVideo`) when their PTS lies
/// within two seconds of the newest PTS before the flag (for KLV, of the
/// newest KLV or video PTS). A new clock that lands within two seconds of
/// the old one loses its first KLV sample and its first video frame the
/// same way. A reconnect resets the demuxer, so nothing is in flight after
/// one.
///
/// A `ContinuityJump` (packet loss), a flag on any other PID, and an encoder
/// restart that signals nothing (a silent clock jump) are not epochs; the
/// last two are invisible to the pairer: call [`Self::flush`] at that
/// boundary yourself. The video program's PCR PID counts whether or not it
/// also carries an elementary stream: a restart signalled on a PID that
/// carries only PCR reaches the pairer as a demuxer event and starts an
/// epoch like any other flag.
///
/// # Closing
///
/// `Pairer` is a passive aggregator — it owns no transport and no OS
/// handles. Drop is the only shutdown and is trivially synchronous.
/// Call [`Self::flush`] before drop at end-of-stream to drain any
/// remaining state: unused KLV history is emitted as trailing
/// `UnpairedKlv` in **both** modes, and in `PairerMode::Buffered` the
/// buffered video AUs are additionally force-drained. Most load-bearing
/// in `Buffered`, but skipping it in `Realtime` can still drop trailing
/// metadata.
///
/// ## Per-language idiom
///
/// | Language | Idiom |
/// |----------|-------|
/// | Rust | `let _ = pairer.flush(); drop(pairer);` (or just let it fall out of scope) |
/// | Java | Drain via `flush()`, then `close()` (try-with-resources) — the JVM `Pairer` is `AutoCloseable` and holds a native handle with no GC fallback |
/// | Kotlin | Drain via `flush()`, then `close()` (`use { }`) — same native-handle lifecycle as Java |
/// | Swift | `deinit` calls drop; explicit `flush()` before exit to drain trailing outputs |
/// | Python | `pairer.flush()` at end-of-stream; let GC reclaim |
/// | C | (pairer C ABI not shipped yet) |
pub struct Pairer {
    state: PairerState,
    stats: PairerStats,
}

enum PairerState {
    Nearest(nearest::NearestState),
    LastBefore(last_before::LastBeforeState),
}

impl Pairer {
    /// Construct a [`Pairer`] for the given video + KLV PIDs using
    /// the default [`PairerConfig`].
    ///
    /// Equivalent to:
    /// ```ignore
    /// Pairer::with_config(video_pid, klv_pid, PairerConfig::default())
    /// ```
    ///
    /// The default config uses `PairerMode::Realtime`, 300 ms tolerance,
    /// and 32-entry video / 32-entry KLV history caps. Use
    /// [`Self::with_config`] to override any of these.
    ///
    /// # Epochs
    ///
    /// At a reconnect and at a signalled discontinuity the pairer emits the
    /// KLV history that never paired as `UnpairedKlv`, ahead of the
    /// passed-through event, and pairs nothing across it. An encoder restart
    /// that signals nothing is invisible to it: call [`Self::flush`] there.
    /// The full rule is in the [type's "Epochs" section](Pairer#epochs).
    ///
    /// # Example
    ///
    /// ```
    /// use tst_pipeline::ext::pairing::Pairer;
    ///
    /// let mut pairer = Pairer::new(0x0100, 0x0102);
    /// // ... feed DemuxEvents into pairer.feed(...) ...
    /// let _stats = pairer.stats();
    /// ```
    pub fn new(video_pid: u16, klv_pid: u16) -> Self {
        Self::with_config(video_pid, klv_pid, PairerConfig::default())
    }

    /// Construct a nearest-PTS pairer with the given options.
    ///
    /// Field-style construction is unit-explicit (`Duration` instead of
    /// bare ticks) and FFI-friendly.
    ///
    /// # Behavior
    ///
    /// Each video AU pairs against the KLV with nearest PTS, within
    /// `opts.tolerance`. The KLV history holds up to
    /// `opts.max_buffered_klv` entries (FIFO eviction on overflow).
    /// In [`PairerMode::Buffered`], up to `opts.max_buffered_video`
    /// video AUs are held while looking ahead for a within-tolerance
    /// match.
    ///
    /// # Epochs
    ///
    /// A reconnect (`DemuxEvent::ReconnectDiscontinuity`) and a signalled
    /// discontinuity on the video PID, the KLV PID or their program's PCR
    /// PID start a new time base. Ahead of the passed-through event the
    /// pairer classifies the video AUs it holds (`PairerMode::Buffered`)
    /// against the history held so far and emits the KLV that never paired
    /// as `UnpairedKlv`; the history held never pairs across the boundary. After a
    /// signalled discontinuity the first KLV sample and the first video AU
    /// leave unpaired when their PTS lies within two seconds of the newest
    /// PTS before it (they may be the old clock's last). An encoder restart
    /// that signals nothing is invisible to the pairer: call [`Self::flush`]
    /// at that boundary yourself. The full rule is in the
    /// [type's "Epochs" section](Pairer#epochs).
    ///
    /// # Panics
    ///
    /// Panics if `opts.max_buffered_klv == 0` or, in
    /// `PairerMode::Buffered`, `opts.max_buffered_video == 0`. A cap of
    /// zero entries is useless; the constructor refuses rather than
    /// emit `UnpairedVideo` for every input silently.
    ///
    /// # Example — realtime nearest-PTS pairer with a 300 ms tolerance
    ///
    /// `PairerConfig` is `#[non_exhaustive]`; construct via
    /// [`Default::default()`] and assign overrides.
    ///
    /// ```
    /// use std::time::Duration;
    /// use tst_pipeline::ext::pairing::{Pairer, PairerMode, PairerConfig};
    ///
    /// let mut opts = PairerConfig::default();
    /// opts.mode = PairerMode::Realtime;
    /// opts.tolerance = Duration::from_millis(300);
    /// opts.max_buffered_klv = 32;
    /// opts.max_buffered_video = 32;
    ///
    /// let mut pairer = Pairer::with_config(
    ///     0x0100, // video PID
    ///     0x0102, // KLV PID
    ///     opts,
    /// );
    ///
    /// // Feed each `DemuxEvent` from a `DemuxReceiver` into `pairer.feed(...)`
    /// // and consume the resulting `PairerOutput`s. Call `pairer.flush()`
    /// // at end-of-stream: it emits never-paired KLV as `UnpairedKlv` in
    /// // both modes and also drains buffered video AUs in `Buffered`.
    /// let _stats = pairer.stats();
    /// ```
    pub fn with_config(video_pid: u16, klv_pid: u16, config: PairerConfig) -> Self {
        assert!(
            config.max_buffered_klv > 0,
            "PairerConfig::max_buffered_klv must be > 0"
        );
        let tolerance_ticks = duration_to_pts_ticks(config.tolerance);
        let internal_mode = match config.mode {
            PairerMode::Realtime => nearest::InternalMode::Realtime,
            PairerMode::Buffered { max_lag } => {
                assert!(
                    config.max_buffered_video > 0,
                    "PairerConfig::max_buffered_video must be > 0 for Buffered mode"
                );
                // `max_lag` is the PTS-skew "wait window": how long a
                // buffered video can sit in the buffer (measured against
                // the newest observed KLV PTS) before forced emit. It is
                // independent of `tolerance` (the match window) but must
                // be at least as large — a max_lag smaller than tolerance
                // is nonsensical (you'd give up before tolerance is even
                // tested). Clamp up to preserve a sane minimum.
                let raw_max_lag_ticks = duration_to_pts_ticks(max_lag);
                let max_lag_ticks = raw_max_lag_ticks.max(tolerance_ticks);
                nearest::InternalMode::Buffered {
                    max_video_buffer: config.max_buffered_video as usize,
                    max_lag_ticks,
                }
            }
        };
        let max_klv_history = config.max_buffered_klv as usize;
        Self {
            state: PairerState::Nearest(nearest::NearestState::new(
                video_pid,
                klv_pid,
                tolerance_ticks,
                max_klv_history,
                internal_mode,
            )),
            stats: PairerStats::default(),
        }
    }

    /// Sample-and-hold: each video AU pairs with the most recent KLV
    /// where `klv.pts <= video.pts`. If `freshness` is `Some(d)`, emit
    /// `UnpairedVideo` when the held KLV is older than `d` behind the
    /// video; if `None`, attach regardless of staleness. Past-only by
    /// definition; no [`PairerMode`] knob applies.
    ///
    /// # Epochs
    ///
    /// The KLV history is cleared at a reconnect and at a signalled
    /// discontinuity on the video PID, the KLV PID or their program's PCR PID;
    /// entries that never paired are emitted as `UnpairedKlv` ahead of the
    /// passed-through event. After a signalled discontinuity the first KLV
    /// sample and the first video sample leave unpaired when their PTS lies
    /// within two seconds of the newest PTS before it (they may be the old
    /// clock's last); a new clock that loses its first KLV sample this way has
    /// every frame until the next sample unpaired. An encoder restart that
    /// signals nothing is invisible to the pairer: call [`Self::flush`] at that
    /// boundary yourself. The full rule is in the
    /// [type's "Epochs" section](Pairer#epochs).
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use tst_pipeline::ext::pairing::Pairer;
    ///
    /// // 2 s freshness ceiling: drop pairing if held KLV is staler.
    /// let pairer = Pairer::last_before_pts(
    ///     0x0100,
    ///     0x0102,
    ///     Some(Duration::from_secs(2)),
    /// );
    /// let _ = pairer;
    /// ```
    pub fn last_before_pts(video_pid: u16, klv_pid: u16, freshness: Option<Duration>) -> Self {
        let freshness_ticks = freshness.map(duration_to_pts_ticks);
        Self {
            state: PairerState::LastBefore(last_before::LastBeforeState::new(
                video_pid,
                klv_pid,
                freshness_ticks,
            )),
            stats: PairerStats::default(),
        }
    }

    /// Feed one demux event. Returns 0+ outputs in feed-time order.
    pub fn feed(&mut self, event: DemuxEvent) -> Vec<PairerOutput> {
        let outputs = match &mut self.state {
            PairerState::Nearest(s) => s.feed(event),
            PairerState::LastBefore(s) => s.feed(event),
        };
        self.tally(&outputs);
        outputs
    }

    /// Drain remaining state at end-of-stream. Idempotent; it drops the KLV
    /// history and every epoch marker, so the next `feed` starts fresh; the
    /// program's PCR PID is kept.
    pub fn flush(&mut self) -> Vec<PairerOutput> {
        let outputs = match &mut self.state {
            PairerState::Nearest(s) => s.flush(),
            PairerState::LastBefore(s) => s.flush(),
        };
        self.tally(&outputs);
        outputs
    }

    /// Increment the per-variant counter for each output.
    fn tally(&mut self, outputs: &[PairerOutput]) {
        for o in outputs {
            match o {
                PairerOutput::Paired { .. } => self.stats.paired += 1,
                PairerOutput::UnpairedVideo(_) => self.stats.unpaired_video += 1,
                PairerOutput::UnpairedKlv(_) => self.stats.unpaired_klv += 1,
                PairerOutput::PassThrough(_) => self.stats.pass_through += 1,
            }
        }
    }

    /// Snapshot the current counters.
    pub fn stats(&self) -> PairerStats {
        self.stats.clone()
    }

    /// Reset all counters to zero.
    pub fn reset_stats(&mut self) {
        self.stats = PairerStats::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tst_core::mpegts::common::Pts90khz;
    use tst_core::mpegts::demux::{
        AudioCodec, DiscontinuityKind, MetadataKind, NonConformantIssue, ProgramMap, SamplePayload,
        StreamId, StreamKind, VideoCodec,
    };
    use tst_core::shared::SharedBytes;

    const VIDEO_PID: u16 = 0x100;
    const KLV_PID: u16 = 0x102;
    const OTHER_VIDEO_PID: u16 = 0x101;
    const AUDIO_PID: u16 = 0x110;

    fn make_pairer() -> Pairer {
        // 1 ms tolerance maps to 90 ticks @ 90 kHz. Tests below use deltas
        // of 0 (paired) and 10_000 ticks (unpaired) — both well-bracketed
        // by this tolerance.
        Pairer::with_config(
            VIDEO_PID,
            KLV_PID,
            PairerConfig {
                mode: PairerMode::Realtime,
                tolerance: Duration::from_millis(1),
                max_buffered_klv: 4,
                max_buffered_video: 4,
            },
        )
    }

    #[test]
    fn program_map_passes_through() {
        let mut p = make_pairer();
        let pmt = DemuxEvent::ProgramMap(ProgramMap {
            program_number: 1,
            pcr_pid: VIDEO_PID,
            pmt_pid: 0x20,
            streams: Vec::new(),
            klv_links: Vec::new(),
        });
        let out = p.feed(pmt.clone());
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], PairerOutput::PassThrough(e) if e == &pmt));
    }

    #[test]
    fn nonconformant_passes_through() {
        let mut p = make_pairer();
        let nc = DemuxEvent::NonConformant {
            stream: StreamId {
                pid: VIDEO_PID,
                kind: StreamKind::Video(VideoCodec::H264),
                program_number: 1,
            },
            issue: NonConformantIssue::PusiMidPes,
        };
        let out = p.feed(nc.clone());
        assert!(matches!(&out[0], PairerOutput::PassThrough(_)));
    }

    #[test]
    fn discontinuity_passes_through() {
        let mut p = make_pairer();
        let d = DemuxEvent::Discontinuity {
            stream: StreamId {
                pid: VIDEO_PID,
                kind: StreamKind::Video(VideoCodec::H264),
                program_number: 1,
            },
            kind: DiscontinuityKind::ContinuityJump {
                expected: 0x5,
                observed: 0x9,
            },
        };
        let out = p.feed(d);
        assert!(matches!(&out[0], PairerOutput::PassThrough(_)));
    }

    #[test]
    fn audio_sample_on_video_pid_passes_through() {
        let mut p = make_pairer();
        // Caller misconfigured: video_pid actually carries audio. Don't
        // fabricate a VideoSample; pass through.
        let audio = DemuxEvent::Sample {
            stream: StreamId {
                pid: VIDEO_PID,
                kind: StreamKind::Audio(AudioCodec::Aac),
                program_number: 1,
            },
            pts: Pts90khz::new(0),
            dts: None,
            payload: SamplePayload::Audio {
                codec: AudioCodec::Aac,
                frames: SharedBytes::from_vec(vec![0xFF, 0xF1]),
            },
        };
        let out = p.feed(audio);
        assert!(matches!(&out[0], PairerOutput::PassThrough(_)));
    }

    #[test]
    fn sample_on_klv_pid_passes_through() {
        let mut p = make_pairer();
        let v = DemuxEvent::Sample {
            stream: StreamId {
                pid: KLV_PID,
                kind: StreamKind::Video(VideoCodec::H264),
                program_number: 1,
            },
            pts: Pts90khz::new(0),
            dts: None,
            payload: SamplePayload::Video {
                codec: VideoCodec::H264,
                raw: SharedBytes::from_vec(Vec::new()),
                random_access_indicator: false,
                av1_carriage: None,
            },
        };
        let out = p.feed(v);
        assert!(matches!(&out[0], PairerOutput::PassThrough(_)));
    }

    #[test]
    fn metadata_on_video_pid_passes_through() {
        let mut p = make_pairer();
        let m = DemuxEvent::Metadata {
            stream: StreamId {
                pid: VIDEO_PID,
                kind: StreamKind::KlvAsync,
                program_number: 1,
            },
            pts: Pts90khz::new(0),
            kind: MetadataKind::KlvAsync,
            payload: Vec::new(),
        };
        let out = p.feed(m);
        assert!(matches!(&out[0], PairerOutput::PassThrough(_)));
    }

    #[test]
    fn video_on_other_pid_passes_through() {
        let mut p = make_pairer();
        let v = DemuxEvent::Sample {
            stream: StreamId {
                pid: OTHER_VIDEO_PID,
                kind: StreamKind::Video(VideoCodec::H265),
                program_number: 1,
            },
            pts: Pts90khz::new(0),
            dts: None,
            payload: SamplePayload::Video {
                codec: VideoCodec::H265,
                raw: SharedBytes::from_vec(Vec::new()),
                random_access_indicator: false,
                av1_carriage: None,
            },
        };
        let out = p.feed(v);
        assert!(matches!(&out[0], PairerOutput::PassThrough(_)));
    }

    #[test]
    fn metadata_kind_klv_sync_au_cell_pairs_same_as_klv_async() {
        let mut p = make_pairer();
        // Spec §3.6 / cookbook recipe 12: nearest-PTS pairing treats
        // both KlvSyncAuCell and KlvAsync as KLV candidates.
        let sync_klv = DemuxEvent::Metadata {
            stream: StreamId {
                pid: KLV_PID,
                kind: StreamKind::KlvSync {
                    declared_link: None,
                },
                program_number: 1,
            },
            pts: Pts90khz::new(50),
            kind: MetadataKind::KlvSyncAuCell {
                metadata_service_id: 0,
                sequence_number: 0,
                cell_fragment_indication:
                    tst_core::mpegts::au_cell::CellFragmentIndication::Complete,
                decoder_config_flag: false,
                random_access_indicator: true,
                was_reassembled: false,
                cell_count: 1,
            },
            payload: vec![0xAA],
        };
        let video = DemuxEvent::Sample {
            stream: StreamId {
                pid: VIDEO_PID,
                kind: StreamKind::Video(VideoCodec::H264),
                program_number: 1,
            },
            pts: Pts90khz::new(50),
            dts: None,
            payload: SamplePayload::Video {
                codec: VideoCodec::H264,
                raw: SharedBytes::from_vec(Vec::new()),
                random_access_indicator: false,
                av1_carriage: None,
            },
        };
        let _ = p.feed(sync_klv);
        let out = p.feed(video);
        assert!(matches!(&out[0], PairerOutput::Paired { .. }));
    }

    #[test]
    fn audio_pid_event_passes_through() {
        let mut p = make_pairer();
        let a = DemuxEvent::Sample {
            stream: StreamId {
                pid: AUDIO_PID,
                kind: StreamKind::Audio(AudioCodec::Aac),
                program_number: 1,
            },
            pts: Pts90khz::new(0),
            dts: None,
            payload: SamplePayload::Audio {
                codec: AudioCodec::Aac,
                frames: SharedBytes::from_vec(Vec::new()),
            },
        };
        let out = p.feed(a);
        assert!(matches!(&out[0], PairerOutput::PassThrough(_)));
    }

    fn make_pairer_stats() -> Pairer {
        Pairer::with_config(
            VIDEO_PID,
            KLV_PID,
            PairerConfig {
                mode: PairerMode::Realtime,
                tolerance: Duration::from_millis(1),
                max_buffered_klv: 4,
                max_buffered_video: 4,
            },
        )
    }

    fn klv_async_event(pid: u16, pts: i64) -> DemuxEvent {
        DemuxEvent::Metadata {
            stream: StreamId {
                pid,
                kind: StreamKind::KlvAsync,
                program_number: 1,
            },
            pts: Pts90khz::new(pts),
            kind: MetadataKind::KlvAsync,
            payload: vec![0xAA],
        }
    }

    fn video_event_for_stats(pts: i64) -> DemuxEvent {
        DemuxEvent::Sample {
            stream: StreamId {
                pid: VIDEO_PID,
                kind: StreamKind::Video(VideoCodec::H264),
                program_number: 1,
            },
            pts: Pts90khz::new(pts),
            dts: None,
            payload: SamplePayload::Video {
                codec: VideoCodec::H264,
                raw: SharedBytes::from_vec(Vec::new()),
                random_access_indicator: false,
                av1_carriage: None,
            },
        }
    }

    #[test]
    fn stats_starts_zero() {
        let p = make_pairer_stats();
        let s = p.stats();
        assert_eq!(s, PairerStats::default());
    }

    #[test]
    fn stats_increments_paired_and_unpaired_video() {
        let mut p = make_pairer_stats();
        let _ = p.feed(klv_async_event(KLV_PID, 0));
        let _ = p.feed(video_event_for_stats(0)); // Paired
        let _ = p.feed(video_event_for_stats(10_000)); // UnpairedVideo (no KLV in window)
        let s = p.stats();
        assert_eq!(s.paired, 1);
        assert_eq!(s.unpaired_video, 1);
    }

    #[test]
    fn stats_increments_unpaired_klv_on_eviction_and_flush() {
        let mut p = make_pairer_stats();
        // Fill history with 5 unused KLVs; max=4 so 1 evicts.
        for pts in [0, 10, 20, 30, 40] {
            let _ = p.feed(klv_async_event(KLV_PID, pts));
        }
        let s_after_evict = p.stats();
        assert_eq!(s_after_evict.unpaired_klv, 1);
        // Flush emits the remaining 4 unused.
        let _ = p.flush();
        let s_after_flush = p.stats();
        assert_eq!(s_after_flush.unpaired_klv, 5);
    }

    #[test]
    fn epoch_clear_is_tallied() {
        let mut p = Pairer::last_before_pts(VIDEO_PID, KLV_PID, None);
        for pts in [0, 10, 20] {
            let _ = p.feed(klv_async_event(KLV_PID, pts));
        }
        let out = p.feed(DemuxEvent::ReconnectDiscontinuity);
        assert_eq!(
            out.len(),
            4,
            "three unpaired KLV and the pass-through: {out:?}"
        );
        let s = p.stats();
        assert_eq!(s.unpaired_klv, 3);
        assert_eq!(s.pass_through, 1);
    }

    #[test]
    fn nearest_epoch_clear_is_tallied() {
        let mut p = make_pairer_stats();
        for pts in [0, 10, 20] {
            let _ = p.feed(klv_async_event(KLV_PID, pts));
        }
        let out = p.feed(DemuxEvent::ReconnectDiscontinuity);
        assert_eq!(
            out.len(),
            4,
            "three unpaired KLV and the pass-through: {out:?}"
        );
        let s = p.stats();
        assert_eq!(s.unpaired_klv, 3);
        assert_eq!(s.pass_through, 1);
    }

    #[test]
    fn stats_increments_pass_through() {
        let mut p = make_pairer_stats();
        let pmt = DemuxEvent::ProgramMap(ProgramMap {
            program_number: 1,
            pcr_pid: VIDEO_PID,
            pmt_pid: 0x20,
            streams: Vec::new(),
            klv_links: Vec::new(),
        });
        let _ = p.feed(pmt);
        let s = p.stats();
        assert_eq!(s.pass_through, 1);
    }

    #[test]
    fn reset_stats_zeros_all_counters() {
        let mut p = make_pairer_stats();
        let _ = p.feed(klv_async_event(KLV_PID, 0));
        let _ = p.feed(video_event_for_stats(0));
        p.reset_stats();
        assert_eq!(p.stats(), PairerStats::default());
    }

    /// The PID that carries only the PCR in the transport streams below.
    const PCR_ONLY_PID: u16 = 0x1FFE;

    /// One TS packet (H.222.0 §2.4.3.2) whose adaptation field (§2.4.3.4)
    /// fills what `payload` leaves, with the discontinuity flag and a PCR in
    /// 27 MHz units when given.
    fn ts_packet(
        pid: u16,
        cc: u8,
        pusi: bool,
        flag: bool,
        pcr: Option<u64>,
        payload: &[u8],
    ) -> [u8; 188] {
        assert!(payload.len() <= 182, "leave room for the flags byte");
        let mut p = [0xFFu8; 188];
        p[0] = 0x47;
        p[1] = (u8::from(pusi) << 6) | (pid >> 8) as u8;
        p[2] = pid as u8;
        p[3] = if payload.is_empty() { 0x20 } else { 0x30 } | (cc & 0x0F);
        let af_len = 183 - payload.len();
        p[4] = af_len as u8;
        p[5] = if flag { 0x80 } else { 0 } | if pcr.is_some() { 0x10 } else { 0 };
        if let Some(pcr) = pcr {
            let (base, ext) = (pcr / 300, pcr % 300);
            p[6..10].copy_from_slice(&((base >> 1) as u32).to_be_bytes());
            p[10] = (((base & 1) as u8) << 7) | 0x7E | ((ext >> 8) as u8 & 1);
            p[11] = ext as u8;
        }
        p[5 + af_len..].copy_from_slice(payload);
        p
    }

    /// A PSI section (H.222.0 §2.4.4) with its CRC_32, alone in one packet.
    fn section_packet(pid: u16, mut section: Vec<u8>) -> [u8; 188] {
        let crc = tst_core::mpegts::common::crc32::crc32_mpeg2(&section);
        section.extend_from_slice(&crc.to_be_bytes());
        let mut pointer_and_section = vec![0];
        pointer_and_section.extend_from_slice(&section);
        ts_packet(pid, 0, true, false, None, &pointer_and_section)
    }

    /// PAT (program 1 on PMT PID 0x1000) and PMT: H.264 on `VIDEO_PID`, KLV
    /// (stream type 0x06, registration `KLVA`) on `KLV_PID`, and
    /// `PCR_ONLY_PID` as the `PCR_PID`, a PID that carries no stream.
    fn pcr_only_psi() -> [[u8; 188]; 2] {
        let pat = vec![
            0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00,
        ];
        let mut pmt = vec![0x02, 0xB0, 0x00, 0x00, 0x01, 0xC1, 0x00, 0x00];
        pmt.extend_from_slice(&(0xE000 | PCR_ONLY_PID).to_be_bytes());
        pmt.extend_from_slice(&[0xF0, 0x00]);
        pmt.extend_from_slice(&[
            0x1B,
            0xE0 | (VIDEO_PID >> 8) as u8,
            VIDEO_PID as u8,
            0xF0,
            0x00,
        ]);
        pmt.extend_from_slice(&[0x06, 0xE0 | (KLV_PID >> 8) as u8, KLV_PID as u8, 0xF0, 0x06]);
        pmt.extend_from_slice(&[0x05, 0x04, b'K', b'L', b'V', b'A']);
        pmt[2] = (pmt.len() - 3 + 4) as u8;
        [section_packet(0, pat), section_packet(0x1000, pmt)]
    }

    /// A bounded PES (H.222.0 §2.4.3.6) with a PTS, alone in one packet.
    fn pes_packet(pid: u16, cc: u8, stream_id: u8, pts: u64, es: &[u8]) -> [u8; 188] {
        let mut pes = vec![0x00, 0x00, 0x01, stream_id];
        pes.extend_from_slice(&((8 + es.len()) as u16).to_be_bytes());
        pes.extend_from_slice(&[0x84, 0x80, 0x05]);
        pes.extend_from_slice(&[
            0x21 | (((pts >> 30) as u8 & 0x07) << 1),
            (pts >> 22) as u8,
            0x01 | (((pts >> 15) as u8 & 0x7F) << 1),
            (pts >> 7) as u8,
            0x01 | ((pts as u8 & 0x7F) << 1),
        ]);
        pes.extend_from_slice(es);
        ts_packet(pid, cc, true, false, None, &pes)
    }

    /// An H.264 access unit whose last byte is `tag`.
    fn video_packet(cc: u8, pts: u64, tag: u8) -> [u8; 188] {
        pes_packet(VIDEO_PID, cc, 0xE0, pts, &[0, 0, 0, 1, 0x65, 0x88, tag])
    }

    /// One KLV record (a 16-byte universal key, a one-byte length) whose
    /// one-byte value is `tag`.
    fn klv_packet(cc: u8, pts: u64, tag: u8) -> [u8; 188] {
        let mut record = vec![
            0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00,
            0x00, 0x00, 0x01,
        ];
        record.push(tag);
        pes_packet(KLV_PID, cc, 0xBD, pts, &record)
    }

    /// H.222.0 §2.4.3.5: a restart signalled on a PID that carries only the
    /// PCR reaches the pairer through a real demuxer and starts an epoch, so
    /// no new-clock frame pairs with old-clock KLV, although the new clock
    /// lands within the tolerance of the old one.
    #[test]
    fn a_reset_signalled_on_a_pcr_only_pid_starts_an_epoch() {
        const OLD: u8 = 0xA0;
        const NEW: u8 = 0xB0;
        let mut pkts = pcr_only_psi().to_vec();
        pkts.push(ts_packet(
            PCR_ONLY_PID,
            0,
            false,
            false,
            Some(100 * 27_000_000),
            &[],
        ));
        pkts.push(klv_packet(0, 9_000_000, OLD));
        pkts.push(video_packet(0, 9_000_000, OLD));
        pkts.push(klv_packet(1, 9_003_000, OLD));
        pkts.push(ts_packet(
            PCR_ONLY_PID,
            0,
            false,
            true,
            Some(27_000_000),
            &[],
        ));
        pkts.push(video_packet(1, 9_006_000, NEW));
        pkts.push(klv_packet(2, 9_009_000, NEW));
        pkts.push(klv_packet(3, 9_012_000, NEW));
        pkts.push(video_packet(2, 9_012_000, NEW));
        let mut d = PairingDemuxer::new(VIDEO_PID, KLV_PID);
        let mut out = d.feed(pkts.as_flattened()).unwrap();
        out.extend(d.flush());
        let pairs: Vec<(u8, u8)> = out
            .iter()
            .filter_map(|o| match o {
                PairerOutput::Paired { video, klv } => {
                    Some((*video.raw.last().unwrap(), *klv.payload.last().unwrap()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(pairs, [(OLD, OLD), (NEW, NEW)], "{out:?}");
    }

    /// The abuse case of the flag above: one on every packet of the PCR
    /// PID. Each starts an epoch, and the pairer holds no more after 10 000
    /// of them than after 10.
    #[test]
    fn ten_thousand_flags_on_a_pcr_only_pid_leave_the_pairer_bounded() {
        let held_after = |flags: u64| {
            let mut d = PairingDemuxer::new(VIDEO_PID, KLV_PID);
            d.feed(pcr_only_psi().as_flattened()).unwrap();
            let mut klv_out = 0;
            for i in 0..flags {
                let pkts = [
                    klv_packet(i as u8, 90_000 + 3_000 * i, 0),
                    ts_packet(PCR_ONLY_PID, 0, false, true, None, &[]),
                ];
                klv_out += d
                    .feed(pkts.as_flattened())
                    .unwrap()
                    .iter()
                    .filter(|o| matches!(o, PairerOutput::UnpairedKlv(_)))
                    .count();
            }
            let held = d.flush().len();
            assert_eq!(
                klv_out + held,
                flags as usize,
                "every KLV sample leaves once"
            );
            assert_eq!(
                d.demuxer_stats().per_stream[&PCR_ONLY_PID].discontinuities,
                flags
            );
            held
        };
        assert_eq!(held_after(10_000), held_after(10));
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use tst_core::mpegts::common::Pts90khz;
    use tst_core::mpegts::demux::{MetadataKind, SamplePayload, StreamId, StreamKind, VideoCodec};
    use tst_core::shared::SharedBytes;

    const VIDEO_PID: u16 = 0x100;
    const KLV_PID: u16 = 0x102;

    #[derive(Debug, Clone)]
    enum SyntheticEvent {
        Video(i64),
        Klv(i64),
    }

    fn to_demux_event(e: &SyntheticEvent) -> DemuxEvent {
        match e {
            SyntheticEvent::Video(pts) => DemuxEvent::Sample {
                stream: StreamId {
                    pid: VIDEO_PID,
                    kind: StreamKind::Video(VideoCodec::H264),
                    program_number: 1,
                },
                pts: Pts90khz::new(*pts),
                dts: None,
                payload: SamplePayload::Video {
                    codec: VideoCodec::H264,
                    raw: SharedBytes::from_vec(Vec::new()),
                    random_access_indicator: false,
                    av1_carriage: None,
                },
            },
            SyntheticEvent::Klv(pts) => DemuxEvent::Metadata {
                stream: StreamId {
                    pid: KLV_PID,
                    kind: StreamKind::KlvAsync,
                    program_number: 1,
                },
                pts: Pts90khz::new(*pts),
                kind: MetadataKind::KlvAsync,
                payload: vec![0xAA],
            },
        }
    }

    fn arb_event() -> impl Strategy<Value = SyntheticEvent> {
        prop_oneof![
            (-1_000_000i64..1_000_000).prop_map(SyntheticEvent::Video),
            (-1_000_000i64..1_000_000).prop_map(SyntheticEvent::Klv),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn nearest_realtime_conservation(events in proptest::collection::vec(arb_event(), 0..200)) {
            // 1000 ticks @ 90 kHz ≈ 11.1 ms. Use a Duration that comfortably
            // brackets that; tolerance values aren't load-bearing for the
            // conservation-property test (any positive value works).
            let mut p = Pairer::with_config(
                VIDEO_PID,
                KLV_PID,
                PairerConfig {
                    mode: PairerMode::Realtime,
                    tolerance: Duration::from_millis(12),
                    max_buffered_klv: 16,
                    max_buffered_video: 16,
                },
            );
            let mut video_count = 0u64;
            let mut klv_count = 0u64;
            let mut paired = 0u64;
            let mut unpaired_video = 0u64;
            let mut unpaired_klv = 0u64;
            for e in &events {
                match e {
                    SyntheticEvent::Video(_) => video_count += 1,
                    SyntheticEvent::Klv(_) => klv_count += 1,
                }
                for o in p.feed(to_demux_event(e)) {
                    match o {
                        PairerOutput::Paired { .. } => paired += 1,
                        PairerOutput::UnpairedVideo(_) => unpaired_video += 1,
                        PairerOutput::UnpairedKlv(_) => unpaired_klv += 1,
                        PairerOutput::PassThrough(_) => prop_assert!(false, "no pass-through expected"),
                    }
                }
            }
            for o in p.flush() {
                match o {
                    PairerOutput::Paired { .. } => paired += 1,
                    PairerOutput::UnpairedVideo(_) => unpaired_video += 1,
                    PairerOutput::UnpairedKlv(_) => unpaired_klv += 1,
                    PairerOutput::PassThrough(_) => prop_assert!(false, "no pass-through expected"),
                }
            }
            // Every video is accounted for: either paired or unpaired.
            prop_assert_eq!(paired + unpaired_video, video_count);
            // A single KLV entry can pair with multiple videos (marked
            // used=true but kept in history). So paired > klv_count is
            // possible; the tighter bound is unpaired_klv <= klv_count.
            prop_assert!(unpaired_klv <= klv_count);
        }

        #[test]
        fn nearest_buffered_conservation(events in proptest::collection::vec(arb_event(), 0..200)) {
            // Pre-Phase-3 the second knob was `max_video_buffer: 8`. Under
            // PairerConfig, the same effect comes from setting
            // `max_buffered_video = 8`. The new `max_lag: Duration` knob
            // is a separate constraint; pick something comfortably large
            // so the count cap is the binding limit (matching the
            // pre-Phase-3 behavior under test).
            let mut p = Pairer::with_config(
                VIDEO_PID,
                KLV_PID,
                PairerConfig {
                    mode: PairerMode::Buffered { max_lag: Duration::from_secs(1) },
                    tolerance: Duration::from_millis(12),
                    max_buffered_klv: 16,
                    max_buffered_video: 8,
                },
            );
            let mut video_count = 0u64;
            let mut klv_count = 0u64;
            let mut paired = 0u64;
            let mut unpaired_video = 0u64;
            let mut unpaired_klv = 0u64;
            for e in &events {
                match e {
                    SyntheticEvent::Video(_) => video_count += 1,
                    SyntheticEvent::Klv(_) => klv_count += 1,
                }
                for o in p.feed(to_demux_event(e)) {
                    match o {
                        PairerOutput::Paired { .. } => paired += 1,
                        PairerOutput::UnpairedVideo(_) => unpaired_video += 1,
                        PairerOutput::UnpairedKlv(_) => unpaired_klv += 1,
                        PairerOutput::PassThrough(_) => prop_assert!(false, "no pass-through expected"),
                    }
                }
            }
            for o in p.flush() {
                match o {
                    PairerOutput::Paired { .. } => paired += 1,
                    PairerOutput::UnpairedVideo(_) => unpaired_video += 1,
                    PairerOutput::UnpairedKlv(_) => unpaired_klv += 1,
                    PairerOutput::PassThrough(_) => prop_assert!(false, "no pass-through expected"),
                }
            }
            // Every video is accounted for: either paired or unpaired.
            prop_assert_eq!(paired + unpaired_video, video_count);
            // A single KLV entry can pair with multiple videos (marked
            // used=true but kept in history). So paired > klv_count is
            // possible; the tighter bound is unpaired_klv <= klv_count.
            prop_assert!(unpaired_klv <= klv_count);
        }

        #[test]
        fn last_before_conservation(events in proptest::collection::vec(arb_event(), 0..200)) {
            let mut p = Pairer::last_before_pts(VIDEO_PID, KLV_PID, None);
            let mut video_count = 0u64;
            let mut klv_count = 0u64;
            let mut paired = 0u64;
            let mut unpaired_video = 0u64;
            let mut unpaired_klv = 0u64;
            for e in &events {
                match e {
                    SyntheticEvent::Video(_) => video_count += 1,
                    SyntheticEvent::Klv(_) => klv_count += 1,
                }
                for o in p.feed(to_demux_event(e)) {
                    match o {
                        PairerOutput::Paired { .. } => paired += 1,
                        PairerOutput::UnpairedVideo(_) => unpaired_video += 1,
                        PairerOutput::UnpairedKlv(_) => unpaired_klv += 1,
                        PairerOutput::PassThrough(_) => prop_assert!(false, "no pass-through expected"),
                    }
                }
            }
            for o in p.flush() {
                match o {
                    PairerOutput::Paired { .. } => paired += 1,
                    PairerOutput::UnpairedVideo(_) => unpaired_video += 1,
                    PairerOutput::UnpairedKlv(_) => unpaired_klv += 1,
                    PairerOutput::PassThrough(_) => prop_assert!(false, "no pass-through expected"),
                }
            }
            // last_before paired_count == video_count requires every
            // video to find a usable slot — which depends on KLV
            // ordering. The conservation invariant still holds:
            prop_assert_eq!(paired + unpaired_video, video_count);
            // Note: a single KLV can be marked used by many video
            // pairings before being displaced, so paired + unpaired_klv
            // != klv_count in general (paired counts video events, not
            // KLV events). The KLV-side invariant is tighter:
            prop_assert!(unpaired_klv <= klv_count);
        }
    }
}
