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
    /// with `recv --corruption-log`, or `None` when the capture was
    /// judged without one (every interop matrix cell, and every soak cell
    /// until the corruption tap is switched on).
    ///
    /// `recv` is the only subcommand that READS a corruption log —
    /// `send --corruption-log` is the other half of the pair and WRITES
    /// one, and the `verify` subcommand has no such flag at all.
    /// Offline, the same judgement is reachable from Rust through
    /// `verify::verify_bytes_with_corruption`, which is how this crate's
    /// own round-trip and mutation tests exercise it.
    ///
    /// See `crate::corrupt`'s module doc for how a sender-side injection
    /// is matched to a receiver-side event at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corruption_attribution: Option<crate::corrupt::AttributionReport>,
    /// What the rich-KLV decode oracles made of this capture's ST 0601
    /// records, or `None` when the capture was judged in the default
    /// `--klv-set compact` mode (every interop matrix cell), where there
    /// is no seeded presence schedule to check a record against. See
    /// [`KlvRichMetrics`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub klv_rich: Option<KlvRichMetrics>,
    /// See [`SinceReconnect`]. `None` until the first reconnect marker,
    /// so an offline `verify` report serializes exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_reconnect: Option<SinceReconnect>,
}

/// Per-record findings of the three rich-KLV oracles (spec §5.5), filled
/// in only for a capture judged with `--klv-set rich`.
///
/// The counters are cumulative over the capture; `first_problem`
/// describes the FIRST record that tripped any of the three, so a report
/// carries one concrete example alongside the totals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KlvRichMetrics {
    /// ST 0601 records offered to the oracles — every `Metadata` event
    /// the capture produced while in rich mode.
    pub records: u64,
    /// Records `klv::st0601::decode` rejected outright.
    pub decode_errors: u64,
    /// Records that decoded but carried at least one `field_errors`
    /// entry (a tag whose bytes the decoder could not make sense of).
    pub field_error_records: u64,
    /// Records whose observed tag set differed from
    /// `fixtures::rich_presence(seed, seq)` — including records whose
    /// `timestamp_us` is missing or off the rich cadence grid, since
    /// without it there is no `seq` to check the presence schedule at.
    pub census_mismatches: u64,
    /// Records the presence schedule said must carry ST 0601 Tag 48.
    pub security_expected: u64,
    /// Of those, how many carried a nested ST 0102 set that decoded with
    /// no field errors and a security classification.
    pub security_ok: u64,
    /// Records the sender's corruption tap plausibly damaged — an
    /// injection's attribution window covered them — which the three
    /// oracles above therefore SKIPPED. They still count in
    /// [`records`](Self::records): the record was delivered and demuxed,
    /// it simply cannot be held to a content contract about bytes the
    /// sender deliberately rewrote. Always 0 without a corruption log.
    ///
    /// All three oracles skip together, not just the decode one: damage
    /// that leaves a record decodable can still cost it a tag, which would
    /// otherwise surface as a census or security failure instead.
    #[serde(default)]
    pub damaged_by_injection: u64,
    /// The first record to trip any of the three oracles, described.
    pub first_problem: Option<String>,
}

/// Where a leg's error events sit relative to its reconnects — the
/// question the 2026-09-17 72-h soak could not answer offline (its srt
/// leg excused ~208 continuity gaps per outage window against a per-window
/// allowance of 2, with nothing recording WHEN in the window they fell).
///
/// Buckets are PACKETS since the last `ReconnectDiscontinuity` — a count,
/// not a clock, so an offline re-judge reproduces it — with edges
/// `bucket_edges_packets` (`< 1 000` ≈ 1 s at the soak's ~1 100 pkt/s,
/// `< 10 000`, `< 100 000`, `< 1 000 000`), then `>= 1 000 000`, then
/// "before any reconnect". Present only when a reconnect was seen.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SinceReconnect {
    pub reconnects: u64,
    pub bucket_edges_packets: [u64; 4],
    pub discontinuities: [u64; 6],
    pub nonconformant: [u64; 6],
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
    /// The [`crate::profiles::Profile`] this capture was judged against,
    /// set by `recv` from its own `--expect`. `None` for an offline
    /// `verify` (whose caller already knows which profile it asked for,
    /// and whose per-cell JSON the interop matrix reads through
    /// `report merge`'s own declared inventory) and for an archived
    /// report written before this field existed — hence
    /// `#[serde(default)]`.
    ///
    /// Exists so `report soak` can check a leg's recv report against the
    /// profile `soak-config.json` DECLARED for that leg: without it, a
    /// harness bug that sent one profile's traffic while the config
    /// claimed another would produce a fully-passing run whose published
    /// evidence names the wrong wire shape.
    #[serde(default)]
    pub profile: Option<String>,
}
