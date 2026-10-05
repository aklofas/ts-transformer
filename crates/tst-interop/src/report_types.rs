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
    /// confirmed empirically during a soak smoke test) rather than
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
    /// `NonConformant` demux events seen. Always counted, excused or not.
    /// Fatal to `pass` in both `VerifyMode::Strict` and
    /// `VerifyMode::Lossy` — unlike `discontinuities` — except for the
    /// events a corruption log accounts for: the ones attributed to an
    /// injection that can cause them, and, in `Lossy` only, a forward
    /// `PcrAnomaly` on a PMT-declared PCR PID beside a continuity gap on
    /// that PID, at most one per gap
    /// (`corrupt::AttributionReport::pcr_anomalies_excused`). Without a
    /// corruption log every one is fatal.
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
    /// See [`ManagedSendStats`]. `Some` only on the report of a
    /// `send --managed` run; every other producer of this struct (a plain
    /// `send`, `recv`, `verify`) has no managed send transport to ask, and
    /// a report archived before the field existed loads as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_send: Option<ManagedSendStats>,
}

/// What a `send --managed` run's `tst_pipeline::ManagedTransport` said
/// about its own reconnects and gap buffer, read once the push loop has
/// finished and the transport has been dropped — the final values of
/// `tst_pipeline::ManagedTransportStats`' counters.
///
/// The SEND side's own account, and the only one there is: a receiver
/// counts its own rebuilds ([`VerifyReport::reconnects`]) but cannot see
/// what the sender's gap buffer accepted or evicted during an outage.
/// That is exactly what tells the two reconnect modes apart after the
/// fact. `Blocking` parks the producer inside the one send that found
/// the transport dead, so the gap buffer never holds more than that
/// message and evicts nothing. `Background` keeps accepting sends while
/// the transport is down, so an outage longer than the buffer shows up
/// here as evictions — every one a message the producer was told `Ok`
/// for and the wire never carried.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ManagedSendStats {
    /// Every reconnect-factory call, successful or not.
    pub reconnect_attempts: u64,
    /// Factory calls that produced a transport that was installed.
    pub reconnect_successes: u64,
    /// Messages evicted from the gap buffer (oldest first) to make room
    /// for newer ones, plus any dropped as oversized after a reconnect.
    pub gap_messages_dropped: u64,
    /// Bytes lost to the same.
    pub gap_bytes_dropped: u64,
    /// The gap buffer's capacity in messages — the policy value the run
    /// was built with, recorded so a reader can bound what an outage
    /// could have replayed without knowing which revision wrote the file.
    pub gap_buffer_capacity: u64,
    /// Messages still in the gap buffer when the sender was dropped:
    /// accepted by `send_bytes` in Background mode, never delivered and
    /// never counted as dropped (a terminal cancel or the end-of-run Drop
    /// strands the backlog). `0` for Blocking mode and for archived
    /// reports written before this field existed. Recorded, not gated.
    #[serde(default)]
    pub gap_len_at_exit: u64,
    /// The `tst_pipeline::ReconnectMode` the transport was built with, as
    /// one of [`RECONNECT_MODES`]. The counters above cannot stand in for
    /// it: a run whose outages all fit the gap buffer evicts nothing in
    /// either mode. `report soak` checks this against the mode the run
    /// declared (`reconnect_mode_declared_<leg>`).
    ///
    /// `None` on a report archived before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect_mode: Option<String>,
    /// The gap buffer's `tst_pipeline::OverflowPolicy` — `"drop_oldest"`
    /// or `"reject"` — which decides whether a full buffer shows up as
    /// the evictions counted above or as a failed send. Recorded, not
    /// checked. `None` on a report archived before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow_policy: Option<String>,
}

/// The reconnect-mode names shared by `send --reconnect-mode`, a soak
/// config's declared `legs.<leg>.reconnect_mode` and
/// [`ManagedSendStats::reconnect_mode`].
pub const RECONNECT_MODES: [&str; 2] = ["blocking", "background"];

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
    /// `ReconnectDiscontinuity` markers the VERIFIER was fed, which can
    /// legitimately differ from [`VerifyReport::reconnects`] — that one is
    /// the managed transport's own rebuild counter, patched in by `recv`.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A send report exactly as written before `managed_send` existed.
    const ARCHIVED_SEND_REPORT: &str = r#"{
        "video_aus": 90, "keyframes": 3, "klv_records": 30,
        "klv_set_sha256": null, "audio_frames": 0, "programs_seen": 1,
        "pts_monotonic": true, "misp_sei_seen": false,
        "bytes": 25004, "stream_sha256": "00"
    }"#;

    /// Archived soak evidence is read back by `report soak`, and a new
    /// optional field must not invalidate it. Pins `managed_send`'s
    /// `#[serde(default)]`.
    #[test]
    fn cell_metrics_without_managed_send_still_deserialize() {
        let parsed: CellMetrics =
            serde_json::from_str(ARCHIVED_SEND_REPORT).expect("archived report must parse");
        assert_eq!(parsed.video_aus, 90);
        assert!(parsed.managed_send.is_none());
    }

    /// A managed send report exactly as written before the mode and the
    /// overflow policy were recorded beside the counters.
    const ARCHIVED_MANAGED_SEND_REPORT: &str = r#"{
        "video_aus": 90, "keyframes": 3, "klv_records": 30,
        "klv_set_sha256": null, "audio_frames": 0, "programs_seen": 1,
        "pts_monotonic": true, "misp_sei_seen": false,
        "bytes": 25004, "stream_sha256": "00",
        "managed_send": {
            "reconnect_attempts": 4, "reconnect_successes": 2,
            "gap_messages_dropped": 7, "gap_bytes_dropped": 9212,
            "gap_buffer_capacity": 256
        }
    }"#;

    /// Pins the `#[serde(default)]` on both new fields: the counters of an
    /// archived report still load, with no mode and no policy to report.
    #[test]
    fn managed_send_without_a_reconnect_mode_still_deserializes() {
        let parsed: CellMetrics =
            serde_json::from_str(ARCHIVED_MANAGED_SEND_REPORT).expect("archived report must parse");
        let managed = parsed.managed_send.expect("the counters were recorded");
        assert_eq!(managed.gap_messages_dropped, 7);
        assert_eq!(managed.gap_buffer_capacity, 256);
        assert!(managed.reconnect_mode.is_none());
        assert!(managed.overflow_policy.is_none());
        // Written before the stranded backlog was recorded: reads as none.
        assert_eq!(managed.gap_len_at_exit, 0);
    }

    /// The backlog stranded in the gap buffer at exit is read back as
    /// written.
    #[test]
    fn managed_send_records_the_gap_len_at_exit() {
        let json = ARCHIVED_MANAGED_SEND_REPORT.replace(
            r#""gap_buffer_capacity": 256"#,
            r#""gap_buffer_capacity": 256, "gap_len_at_exit": 4"#,
        );
        assert!(json.contains("gap_len_at_exit"), "the replace must apply");
        let parsed: CellMetrics = serde_json::from_str(&json).expect("report must parse");
        let managed = parsed
            .managed_send
            .as_ref()
            .expect("the counters were recorded");
        assert_eq!(managed.gap_len_at_exit, 4);
        assert_eq!(managed.gap_messages_dropped, 7);
        let back: CellMetrics =
            serde_json::from_str(&serde_json::to_string(&parsed).expect("serialize"))
                .expect("round trip");
        assert_eq!(back.managed_send.expect("kept").gap_len_at_exit, 4);
    }

    /// The written shape: both names are lowercase strings, and they
    /// survive a round trip.
    #[test]
    fn managed_send_records_the_mode_and_policy_as_lowercase_strings() {
        let mut metrics: CellMetrics =
            serde_json::from_str(ARCHIVED_SEND_REPORT).expect("archived report must parse");
        metrics.managed_send = Some(ManagedSendStats {
            reconnect_mode: Some("background".to_string()),
            overflow_policy: Some("drop_oldest".to_string()),
            ..ManagedSendStats::default()
        });
        let json = serde_json::to_string(&metrics).expect("serialize");
        assert!(json.contains(r#""reconnect_mode":"background""#), "{json}");
        assert!(
            json.contains(r#""overflow_policy":"drop_oldest""#),
            "{json}"
        );
        let back: CellMetrics = serde_json::from_str(&json).expect("round trip");
        assert_eq!(back, metrics);
    }

    /// And the other direction: a report with nothing to say about a
    /// managed send (every `recv`, every `verify`, every plain `send`)
    /// serializes without the key, so those files are byte-for-byte what
    /// they were. Pins the `skip_serializing_if`.
    #[test]
    fn cell_metrics_without_managed_send_omit_the_key() {
        let mut metrics: CellMetrics =
            serde_json::from_str(ARCHIVED_SEND_REPORT).expect("archived report must parse");
        let json = serde_json::to_string(&metrics).expect("serialize");
        assert!(!json.contains("managed_send"), "got: {json}");

        metrics.managed_send = Some(ManagedSendStats {
            gap_messages_dropped: 3,
            ..ManagedSendStats::default()
        });
        let json = serde_json::to_string(&metrics).expect("serialize");
        let back: CellMetrics = serde_json::from_str(&json).expect("round trip");
        assert_eq!(back, metrics);
    }
}
