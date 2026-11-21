//! Byte-feeding composite of a [`Demuxer`] + a [`Pairer`].
//!
//! `PairingDemuxer` is to [`Pairer`](super::Pairer) what
//! [`crate::DemuxReceiver`] is to [`Demuxer`]: it owns the demuxer so
//! callers feed raw TS bytes and receive [`PairerOutput`]s directly,
//! never threading `DemuxEvent`s across an API boundary. This is the
//! shape language bindings (Python / JVM / UniFFI) consume, since the
//! event projection across FFI is one-way.
//!
//! The decoupled event-feeding [`Pairer`](super::Pairer) is unchanged
//! and remains the right tool for Rust callers who already hold
//! `DemuxEvent`s.

use std::time::Duration;
use tst_core::DemuxError;
use tst_core::mpegts::demux::{Demuxer, DemuxerConfig, DemuxerStats};

use super::{Pairer, PairerConfig, PairerOutput, PairerStats};

/// Options for [`PairingDemuxer::with_config`].
///
/// Bundles the two halves' configs so the constructor stays
/// FFI-friendly (one config argument, not a positional explosion).
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct PairingDemuxerConfig {
    /// Pairing strategy + tolerances (nearest-PTS). See [`PairerConfig`].
    pub pairer: PairerConfig,
    /// Demuxer parsing config. See [`DemuxerConfig`].
    pub demuxer: DemuxerConfig,
}

/// Byte-feeding KLV↔video pairer: owns a [`Demuxer`] + a [`Pairer`].
///
/// Feed TS bytes; collect [`PairerOutput`]s. Call [`Self::flush`] at
/// end-of-stream to drain any remaining state: unused KLV history is
/// emitted as trailing `UnpairedKlv` in **both** modes (e.g. metadata that
/// arrived after the last video AU), and in
/// [`PairerMode::Buffered`](super::PairerMode::Buffered) the buffered video
/// AUs are additionally force-drained (best-effort matched).
///
/// A signalled discontinuity in the bytes starts a new epoch in the pairer,
/// as the ["Epochs" section of `Pairer`](super::Pairer#epochs) describes. The
/// owned demuxer emits no `ReconnectDiscontinuity`, so a reconnect of the
/// byte source is invisible here, like an encoder restart that signals
/// nothing.
pub struct PairingDemuxer {
    demuxer: Demuxer,
    pairer: Pairer,
}

impl PairingDemuxer {
    /// Construct for the given video + KLV PIDs using default configs
    /// (nearest-PTS, 300 ms tolerance — see [`PairerConfig`]).
    pub fn new(video_pid: u16, klv_pid: u16) -> Self {
        Self::with_config(video_pid, klv_pid, PairingDemuxerConfig::default())
    }

    /// Construct a nearest-PTS pairing demuxer with explicit configs.
    ///
    /// # Panics
    ///
    /// Panics under the same conditions as
    /// [`Pairer::with_config`](super::Pairer::with_config)
    /// (`config.pairer.max_buffered_klv == 0`, or `max_buffered_video
    /// == 0` in `Buffered` mode).
    pub fn with_config(video_pid: u16, klv_pid: u16, config: PairingDemuxerConfig) -> Self {
        Self {
            demuxer: Demuxer::with_config(config.demuxer),
            pairer: Pairer::with_config(video_pid, klv_pid, config.pairer),
        }
    }

    /// Construct a sample-and-hold pairing demuxer (each video AU pairs
    /// with the most recent KLV where `klv.pts <= video.pts`). See
    /// [`Pairer::last_before_pts`](super::Pairer::last_before_pts).
    pub fn last_before_pts(
        video_pid: u16,
        klv_pid: u16,
        freshness: Option<Duration>,
        demuxer: DemuxerConfig,
    ) -> Self {
        Self {
            demuxer: Demuxer::with_config(demuxer),
            pairer: Pairer::last_before_pts(video_pid, klv_pid, freshness),
        }
    }

    /// Feed a buffer of TS bytes. Demuxes internally and returns the
    /// pairing outputs produced, in feed-time order. Propagates
    /// [`DemuxError`] exactly as [`Demuxer::feed`].
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<PairerOutput>, DemuxError> {
        self.demuxer.feed(bytes)?;
        let mut out = Vec::new();
        while let Some(ev) = self.demuxer.next_event() {
            out.extend(self.pairer.feed(ev));
        }
        Ok(out)
    }

    /// Drain remaining state at end-of-stream: flush the demuxer, pair
    /// any trailing events, then flush the pairer. Idempotent.
    pub fn flush(&mut self) -> Vec<PairerOutput> {
        self.demuxer.flush();
        let mut out = Vec::new();
        while let Some(ev) = self.demuxer.next_event() {
            out.extend(self.pairer.feed(ev));
        }
        out.extend(self.pairer.flush());
        out
    }

    /// Snapshot the pairing counters.
    pub fn stats(&self) -> PairerStats {
        self.pairer.stats()
    }

    /// Snapshot the underlying demuxer counters (drops, non-conformant,
    /// etc.) for diagnostics.
    pub fn demuxer_stats(&self) -> DemuxerStats {
        self.demuxer.stats()
    }

    /// Reset the pairing counters to zero. Does not touch demuxer stats.
    pub fn reset_stats(&mut self) {
        self.pairer.reset_stats();
    }
}
