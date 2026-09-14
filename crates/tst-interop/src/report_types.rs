//! Serde report types shared by the `verify`, `recv`, and `report`
//! subcommands. Kept in their own module (rather than folded into
//! `verify.rs`) because later tasks (`recv`/`report`) need to
//! (de)serialize these same shapes without depending on the verification
//! logic itself.

use serde::{Deserialize, Serialize};

/// Wire-format facts tallied from one demuxed MPEG-TS/KLV capture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellMetrics {
    pub video_aus: u64,
    pub keyframes: u64,
    pub klv_records: u64,
    /// Order-insensitive fingerprint of the KLV record set: sort the
    /// per-record sha256 hex digests, then sha256 the concatenation of
    /// the sorted digests. Two captures with the same KLV records in a
    /// different arrival order hash identically; a single differing
    /// record (or a differing count) changes the hash.
    ///
    /// `None` iff computed with `send`/`recv --no-klv-digest`: that flag
    /// skips accumulating a growing per-record digest list entirely
    /// (unbounded over a multi-day soak — ~4 MiB/h at 10 Hz KLV,
    /// confirmed empirically during Task 14's smoke run) rather than
    /// just omitting the hash after the fact. `verify` never sets it
    /// (offline-file checks aren't multi-day, so the memory concern
    /// doesn't apply) and always produces `Some`.
    pub klv_set_sha256: Option<String>,
    pub audio_frames: u64,
    pub programs_seen: u8,
    /// Rollover-aware: see `verify::pts_is_monotonic_step` for the
    /// exact 33-bit-wrap rule.
    pub pts_monotonic: bool,
    pub misp_sei_seen: bool,
    pub bytes: u64,
    /// Whole-capture sha256 — the byte-transparent tier (bit-for-bit
    /// identity), independent of and stricter than every other field here.
    pub stream_sha256: String,
    /// `Discontinuity` demux events seen. Always counted; only fatal to
    /// `pass` in `VerifyMode::Strict` (`VerifyMode::Lossy` counts it and
    /// moves on — see `verify::VerifyMode`'s own doc comment).
    #[serde(default)]
    pub discontinuities: u64,
    /// `NonConformant` demux events seen. Always counted, and always
    /// fatal to `pass` — unlike `discontinuities`, this fails the check
    /// in both `VerifyMode::Strict` and `VerifyMode::Lossy`.
    #[serde(default)]
    pub nonconformant: u64,
    /// Sender-side corruption tap counters (`send --corrupt`), `None`
    /// when the tap was off. The SENT side of the same run the receiver
    /// judges via `corruption_attribution` below: this says what was
    /// deliberately done to the stream, that says what the receiver made
    /// of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corruption: Option<crate::corrupt::CorruptionStats>,
    /// What the receiver's evidence said about a corruption log supplied
    /// with `recv --corruption-log` (or, offline,
    /// `verify::verify_bytes_with_corruption`), or
    /// `None` when the capture was judged without one (every interop
    /// matrix cell, and every soak cell until the corruption tap is
    /// switched on). See `crate::corrupt`'s module doc for how a
    /// sender-side injection is matched to a receiver-side event at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corruption_attribution: Option<crate::corrupt::AttributionReport>,
}

/// Outcome of checking one [`CellMetrics`] tally against a
/// [`crate::profiles::Profile`]'s invariants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifyReport {
    pub pass: bool,
    /// Empty iff `pass`. Each entry is a human-readable description of one
    /// violated invariant (e.g. names the observed vs. expected count).
    pub failures: Vec<String>,
    pub metrics: CellMetrics,
    /// Number of times the underlying transport was successfully
    /// rebuilt (`tst_pipeline::ManagedRecvTransport::reconnects_count`
    /// — for a listener-mode SRT URL, a re-bind + re-accept), or `None`
    /// for a plain (non-`recv --managed`) capture, which has no such
    /// counter at all.
    ///
    /// Counts RECEIVE-SIDE rebuilds only — a distinct event from
    /// however many times the (independently managed) SENDER
    /// reconnected, which this process can't observe directly. For
    /// `soak.sh`'s topology the two are expected to track closely (the
    /// receive-side accepted socket only dies because the sender's
    /// connection died first, and vice versa — both driven by the same
    /// underlying outage), but this field is NOT itself a measurement
    /// of "how many times the sender reconnected," only "how many times
    /// this recv rebuilt its own transport."
    pub reconnects: Option<u64>,
}
