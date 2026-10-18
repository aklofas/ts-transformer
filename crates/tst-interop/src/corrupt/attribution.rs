use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use super::coord::{Coord, Injection, LogHeader, default_approx_slack};
use super::tap::*;

/// How far ahead of an injection's own PCR base a later base may be and
/// still resolve it approximately — four 100 ms PCR intervals in 90 kHz
/// ticks (H.222.0 §2.4.2.2 caps the interval at 100 ms).
///
/// Without this bound a reconnect outage silently corrupts the verdict:
/// the sender keeps logging through it while the receiver sees nothing, so
/// the first PCR after the outage would resolve every injection logged
/// during it to that one receiver ordinal — and all but one would then be
/// reported `undetected` against a receiver that never had the chance to
/// see them. Beyond the bound an injection stays unresolved instead, which
/// means it is never judged.
pub const MAX_APPROX_TICKS: u64 = 4 * 9000;

/// How far AHEAD of the receiver's current PCR base a logged base may be
/// and still be "not yet arrived" rather than history: half of the 33-bit
/// counter's half-range (≈ 6.6 h). The log runs ahead of the wire by at
/// most seconds, so anything further is an injection from before this
/// receiver listened (a restarted leg re-reading the log) whose age wrapped
/// the counter — stranded, so the cursor moves on. Known limit: a source
/// clock that jumps FORWARD by more than this strands the few seconds of
/// injections logged before the receiver sees the new clock.
pub const MAX_FUTURE_TICKS: u64 = 1 << 31;

/// Largest step between two consecutive ACCEPTED PCR anchors, in 90 kHz
/// ticks (10 s). A real clock advances at most a few seconds between the
/// anchors a receiver drains; a "PCR" decoded from a mis-framed packet is a
/// uniform random 33-bit number. A sample further than this from the last
/// accepted anchor is held as a candidate and becomes an anchor only when
/// the NEXT sample agrees with it (a genuine discontinuity: the source
/// restarted) — one bogus decode never re-anchors the attribution.
pub const MAX_ANCHOR_JUMP_TICKS: u64 = 90_000 * 10;

/// How many of the most recent `(pcr_base, at)` pairs [`Attribution`]
/// keeps so an injection logged AFTER its anchor PCR already reached the
/// receiver can still resolve against it — see [`Attribution::append`].
/// The lag that needs covering is the stream distance from an anchor PCR
/// to the injected packet: the sender is paced in real time, so an
/// injection `since_pcr` packets after its PCR is logged that much later.
/// `since_pcr` has been measured at up to ~2.3 PCR intervals (see
/// [`Attribution::backward_reach`]); 64 PCRs is over 2.5 s even at the
/// 40 ms cadence, ample headroom at a constant cost.
///
/// If the ring were ever outrun — an injection's anchor base already
/// evicted when its line is appended — the replay would treat the anchor
/// like a destroyed one. An anchor up to [`MAX_APPROX_TICKS`] older than
/// the oldest retained base resolves approximately against that base: it
/// is judged, but at a position up to one PCR interval late, so it could
/// surface as a false unexplained/undetected pair. An anchor older than
/// that is stranded: counted `unresolved` and never judged. Neither case
/// is reachable at the measured lag of ~2.3 intervals against a 64-entry
/// ring.
const PCR_HISTORY: usize = 64;
/// Cap on each of [`AttributionReport`]'s human-readable sample lists
/// (`unexplained_events`, `undetected`, `unrecovered`). Every one of them
/// has an uncapped counter beside it, so a badly-broken run is still
/// counted in full — only the quoted examples are bounded, and a
/// multi-day run cannot turn a report into a gigabyte of strings.
///
/// **A cap on a list must never become a cap on a JUDGEMENT.** The
/// verdicts read the counters, never `len()`; see
/// [`AttributionReport::unexplained_total`].
pub const MAX_SAMPLES: usize = 64;

/// Injections retired below [`Attribution`]'s `lo` cursor before their
/// accounting is folded into the running counters and their per-injection
/// state dropped. Batched rather than one at a time so the retained
/// prefix is removed with one memmove per batch instead of per injection;
/// the batch size is the entire bound on how much of the log the engine
/// holds once a run is under way.
pub const PRUNE_BATCH: usize = 1024;

/// How many ALREADY-LOGGED injections a receiver keeps when it attaches to
/// a corruption log the sender has been writing for a while — a hold's
/// restarted leg. Older injections are ones this receiver never listened
/// for (unresolvable, and the oldest would cross the 33-bit PCR half-range
/// and block every later resolution); the newest 4096 span about two hours
/// at the hold's rate (5 per 10 000 packets at ≈ 1 100 packets/s) — more
/// than any transport buffer holds, so every injection still on the wire
/// is kept. A first receiver attaches to an empty log and skips nothing.
pub const REATTACH_HISTORY: usize = 4096;

// ============================================================
// Attribution
// ============================================================

/// Receiver-side error-event kinds the attribution engine understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// The raw reader lost and regained packet sync.
    Resync,
    ContinuityJump,
    OtherDiscontinuity,
    PsiChecksum,
    MalformedPes,
    /// `NonConformantIssue::PcrAnomaly` — a PCR that jumped against the
    /// previous one on its PID. Its own signal rather than
    /// [`Signal::OtherNonConformant`] because under lossy judgement it has
    /// one excusal the other non-conformances never get: a FORWARD PCR
    /// jump that coincides with a continuity gap on the same PID is that
    /// gap's timestamp signature, not corruption — see
    /// [`Attribution::on_signal`].
    ///
    /// `delta` is the demuxer's signed difference (27 MHz ticks, new minus
    /// previous). The sign is what the excusal turns on: packets that went
    /// missing took their share of the timeline with them, so the clock
    /// the receiver sees can only jump ahead. A clock that went BACK is
    /// never the signature of loss.
    PcrAnomaly {
        delta: i64,
    },
    OtherNonConformant,
}

/// What the receiver's evidence says about the injections.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AttributionReport {
    pub injected: u64,
    pub detectable: u64,
    pub resolved: u64,
    pub unresolved: u64,
    pub events: u64,
    pub attributed_events: u64,
    /// Attributed events whose signal was a non-conformance
    /// (`PsiChecksum` / `MalformedPes` / `OtherNonConformant`).
    pub attributed_nonconformant: u64,
    /// Attributed events whose signal was a discontinuity
    /// (`ContinuityJump` / `OtherDiscontinuity`).
    pub attributed_discontinuities: u64,
    /// [`attributed_events`](Self::attributed_events) split by the PID
    /// the signal surfaced on. A verifier widening a per-PID floor by
    /// what the capture explains needs the split, not the total: seven
    /// explained video events say nothing about a KLV PID (see
    /// `oracles::Explained`).
    ///
    /// `#[serde(default)]` so an archived pre-split report still parses.
    #[serde(default)]
    pub attributed_events_by_pid: BTreeMap<u16, u64>,
    /// Attributed events no PID owns — the `Resync` signals, which
    /// `on_signal` receives with `pid: None` because sync was lost for
    /// the whole multiplex rather than for one stream in it. This and the
    /// values of `attributed_events_by_pid` together sum to
    /// `attributed_events`.
    #[serde(default)]
    pub attributed_events_unpinned: u64,
    /// Sample of events no injection explains and no transport-loss
    /// excusal covers — capped at [`MAX_SAMPLES`]. The true count is
    /// [`unexplained_total`](Self::unexplained_total); never judge a run
    /// by this list's length.
    pub unexplained_events: Vec<String>,
    /// Uncapped count of unexplained `Resync` events. A resync is the one
    /// signal with no second detection path — it is not a `DemuxEvent`,
    /// and in resync mode it does not surface as `rawts_sync_loss` either
    /// — so it must never be lost behind a sample cap.
    #[serde(default)]
    pub unexplained_resyncs: u64,
    /// Uncapped count of unexplained discontinuity-family events that
    /// were NOT excused as transport loss (always the whole family under
    /// strict judgement; always zero under lossy, where the excusal takes
    /// them all — see [`unexplained_transport_loss`](Self::unexplained_transport_loss)).
    #[serde(default)]
    pub unexplained_discontinuities: u64,
    /// Uncapped count of unexplained non-conformance-family events
    /// (`PsiChecksum` / `MalformedPes` / `PcrAnomaly` /
    /// `OtherNonConformant`). Never excused in either tier — a lost packet
    /// does not forge a bad CRC — except the PCR anomalies counted in
    /// [`pcr_anomalies_excused`](Self::pcr_anomalies_excused), which never
    /// reach this count.
    #[serde(default)]
    pub unexplained_nonconformant: u64,
    /// First UNEXPLAINED event of each family, uncapped and
    /// first-writer-wins. A verifier's surviving
    /// `nonconformant_event`/`discontinuity_event` failures quote these:
    /// the first event of a class and the first UNEXPLAINED event of it
    /// are routinely different events, and naming the explained one would
    /// point a reader at evidence the report has already accounted for.
    pub first_unexplained_nonconformant: Option<String>,
    pub first_unexplained_discontinuity: Option<String>,
    /// Sample of injections a conformant receiver had to notice and did
    /// not — capped at [`MAX_SAMPLES`];
    /// [`undetected_count`](Self::undetected_count) is the true number.
    pub undetected: Vec<String>,
    /// Uncapped count of the same.
    #[serde(default)]
    pub undetected_count: u64,
    /// Sample of injections the stream never produced media after —
    /// capped at [`MAX_SAMPLES`];
    /// [`unrecovered_count`](Self::unrecovered_count) is the true number.
    pub unrecovered: Vec<String>,
    /// Uncapped count of the same.
    #[serde(default)]
    pub unrecovered_count: u64,
    /// Injections that would have landed in [`undetected`](Self::undetected)
    /// but were excused as lost in transit — a `ContinuityJump` inside
    /// their attribution window says the transport itself dropped packets
    /// there, and a corrupted packet the network never delivered cannot be
    /// noticed by its damage. Only ever nonzero for a report finished with
    /// transport-loss excusal on (see [`Attribution::lossy`]).
    #[serde(default)]
    pub undetected_lost: u64,
    /// Same excusal, for [`unrecovered`](Self::unrecovered): media that a
    /// dropped-packet gap swallowed is not the injection failing to
    /// recover.
    #[serde(default)]
    pub unrecovered_lost: u64,
    /// Detectable injections the receiver's OWN demuxer reported (a
    /// `Discontinuity` / `NonConformant` event inside the window).
    #[serde(default)]
    pub detected_by_demux: u64,
    /// Detectable injections noticed ONLY by the harness's raw-TS reader
    /// ([`Signal::Resync`], which is not a `DemuxEvent`): garbage runs and
    /// destroyed sync bytes that tst-core re-syncs past without an event,
    /// by design. Reported separately so the evidence page does not
    /// credit the receiver with the harness's own detection.
    #[serde(default)]
    pub detected_by_reader_only: u64,
    /// The same, per class name ([`Class::name`]).
    #[serde(default)]
    pub detected_by_reader_only_per_class: BTreeMap<String, u64>,
    /// Unexplained events kept out of
    /// [`unexplained_events`](Self::unexplained_events) as transport loss
    /// rather than corruption — discontinuity-family signals under
    /// transport-loss excusal. Recorded so `events - attributed_events`
    /// still reconciles with the list a reader is shown.
    #[serde(default)]
    pub unexplained_transport_loss: u64,
    /// Of [`unexplained_transport_loss`](Self::unexplained_transport_loss),
    /// the `PcrAnomaly` signals excused as the timestamp signature of a
    /// continuity gap on the same PID — a `ContinuityJump` within the
    /// attribution window before them, or on the same packet after them.
    /// Only a forward jump, only on a PID a PMT currently declares as
    /// `PCR_PID`, only against a gap seen since the last reconnect, and
    /// at most one per gap. Lossy only; strict charges every one.
    #[serde(default)]
    pub pcr_anomalies_excused: u64,
    /// `DemuxEvent::ReconnectDiscontinuity` markers the receiver fed.
    #[serde(default)]
    pub reconnects_seen: u64,
    /// Of [`undetected_lost`](Self::undetected_lost), the injections whose
    /// position resolved inside a reconnect gap — between the last media
    /// before a marker and the marker's own window, or the next marker if
    /// that comes first — that the demuxer
    /// was never in a position to notice: resolved approximately, or from
    /// an anchor the receiver saw before the marker, or placed before the
    /// reset demuxer had produced media again (on the injection's own PID
    /// for damage noticed as a continuity jump, on any PID otherwise).
    /// One resolved exactly from an anchor received after the marker, with
    /// that media before it, is never counted here. Lossy only.
    #[serde(default)]
    pub lost_in_reconnect_gap: u64,
    /// Injections already in the log at attach that the receiver skipped as
    /// history it never listened for ([`REATTACH_HISTORY`]); add to
    /// `injected` when comparing against the sender's total.
    #[serde(default)]
    pub history_skipped: u64,
    /// Injections already in the log at attach that this receiver could
    /// never have heard: stranded before its first anchor. Also counted
    /// in `unresolved`; coverage judges a restarted leg against
    /// `injected - history_unheard`.
    #[serde(default)]
    pub history_unheard: u64,
    pub resyncs: u64,
    pub injected_fraction: f64,
    pub attribution_window: u64,
    pub recovery_bound: u64,
    /// The tap spec the sender's log header declares, carried through so
    /// a report can be checked against what the run was CONFIGURED to
    /// inject rather than only against what it happens to contain — see
    /// `report soak`'s `corruption_declared_<leg>` verdict.
    /// `#[serde(default)]` so an archived pre-realism report still
    /// deserializes (as a zero rate, which that verdict reads as "the
    /// report predates the declaration").
    #[serde(default)]
    pub rate_per_10k: u32,
    #[serde(default)]
    pub min_gap: u64,
    #[serde(default)]
    pub classes: Vec<Class>,
    /// The header's `approx_slack`, so a report shows the whole geometry
    /// it was judged with (pre-scaling reports read as [`APPROX_SLACK`]).
    #[serde(default = "default_approx_slack")]
    pub approx_slack: u64,
    /// PCR samples held as implausible and never confirmed — decodes from
    /// mis-framed packets.
    #[serde(default)]
    pub anchors_rejected: u64,
}

impl AttributionReport {
    /// Unexplained events that are corruption evidence — every
    /// unattributed event except the ones transport-loss excusal took.
    ///
    /// **This, not `unexplained_events.len()`, is what a verdict must
    /// gate on.** The list is a capped sample: a run that produced
    /// thousands of unexplained events shows sixty-four of them, and a
    /// check reading the list's length would read a badly-broken run as
    /// a mildly-broken one.
    #[must_use]
    pub fn unexplained_total(&self) -> u64 {
        Self::judged(
            self.unexplained_resyncs
                + self.unexplained_discontinuities
                + self.unexplained_nonconformant,
            &self.unexplained_events,
        )
    }

    /// Injections a conformant receiver had to notice and did not,
    /// uncapped — the count a verdict gates on, not
    /// `undetected.len()`.
    #[must_use]
    pub fn undetected_total(&self) -> u64 {
        Self::judged(self.undetected_count, &self.undetected)
    }

    /// Injections the stream never produced media after, uncapped.
    #[must_use]
    pub fn unrecovered_total(&self) -> u64 {
        Self::judged(self.unrecovered_count, &self.unrecovered)
    }

    /// Reconcile an uncapped counter with the sample list beside it.
    ///
    /// The counters are `#[serde(default)]`, so a report written BEFORE
    /// they existed deserializes with every one of them zero while its
    /// sample list still holds real findings. Gating on the counter alone
    /// would then read such a report as clean and pass a run that failed
    /// — verified against a 1-hour smoke run's artifacts, where
    /// re-judging them under the new counters flipped both
    /// `corruption_detected_*` failures to passes while the verdict
    /// detail still quoted the finding.
    ///
    /// A sample list can never be LONGER than the true count (it is a
    /// prefix of it, capped at [`MAX_SAMPLES`]), so the larger of the two
    /// is the honest answer for a report of either vintage: the counter
    /// for a current one, the list length for an archived one.
    fn judged(count: u64, sample: &[String]) -> u64 {
        count.max(sample.len() as u64)
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct InjState {
    /// Receiver packet ordinal this injection landed at, once its PCR
    /// anchor has been seen.
    pub(super) resolved_at: Option<u64>,
    pub(super) detected: bool,
    pub(super) recovered: bool,
    /// Resolved against a LATER base than the one logged (the logged one
    /// never arrived), so the position is approximate.
    pub(super) approx: bool,
    /// Receiver ordinal of the PCR packet this injection resolved from.
    /// `None` for one that carries no anchor (logged before the stream's
    /// first PCR). Together with `approx` this is the arrival evidence a
    /// reconnect gap has to respect — see [`ReconnectGap::holds`].
    pub(super) anchor_at: Option<u64>,
    /// Its anchor base fell more than `MAX_APPROX_TICKS` behind the first
    /// base the receiver saw afterwards, so it can never be placed. Kept
    /// distinct from "not resolved yet" so the scan cursors can move past
    /// it instead of stalling on it forever.
    pub(super) stranded: bool,
    /// A `ContinuityJump` landed inside this injection's attribution
    /// window, on a PID this injection could itself have suffered a gap
    /// on ([`reaches`]). Evidence that packets went missing around here
    /// for a reason the corruption log cannot own: an impairment proxy's
    /// loss, an SRT/RIST buffer overrun, a reconnect after an outage. See
    /// [`Attribution::lossy`] for what it excuses.
    pub(super) cc_jump_in_window: bool,
    /// Set when a NON-[`Signal::Resync`] signal detected this injection,
    /// i.e. when the receiver's own demuxer reported it rather than only
    /// the harness's raw reader. See
    /// [`AttributionReport::detected_by_reader_only`].
    pub(super) detected_by_demux: bool,
}

/// Matches receiver events against a corruption log. Pure: no I/O, no
/// clock, and no knowledge of the receiver beyond the three `on_*` calls.
///
/// All three `on_*` methods expect monotonically non-decreasing `at`
/// values (receiver packet ordinals), which is how a receiver naturally
/// produces them. Internally that lets the engine keep a sliding cursor
/// over the injection list instead of rescanning it per event — a 72-hour
/// soak produces millions of events against a quarter-million injections,
/// and the quadratic form of that scan would not finish.
/// The half of a logged [`Injection`] the attribution engine reads.
///
/// Deliberately not the `Injection` itself. That carries `before`/`after`
/// byte images of every corrupted packet (up to ~500 bytes each), and a
/// 72-hour run logs a quarter of a million of them — state an engine that
/// only ever looks at the class, the PID and two flags has no business
/// holding onto. `Copy`, so retiring one costs nothing.
#[derive(Clone, Copy, Debug)]
struct Tracked {
    coord: Coord,
    class: Class,
    pid: u16,
    detectable: bool,
    psi: bool,
    /// Damage that breaks packet framing for the whole multiplex — a
    /// truncation, an inserted garbage run, or a header rewrite of the
    /// sync byte (offset 0). Such an injection can surface on ANY PID;
    /// every other class is confined to [`Tracked::pid`].
    framing_wide: bool,
    /// The PID a `Header` PID-rewrite (offsets `[1, 2]`) moved the packet
    /// to — the receiver reports whatever it makes of that packet on THIS
    /// PID, never on [`Tracked::pid`]. `None` for every other shape.
    moved_to: Option<u16>,
    /// Damage a receiver notices as a continuity jump on
    /// [`Tracked::pid`] and in no other way: a `Drop`, and the `Header`
    /// sub-kinds that rewrite the PID (offsets `[1, 2]`, the packet goes
    /// missing from its PID) or the counter itself (`[3]`). Such a jump
    /// needs a previous counter on that PID — see [`ReconnectGap::holds`].
    noticed_by_continuity: bool,
}

impl From<&Injection> for Tracked {
    fn from(i: &Injection) -> Tracked {
        Tracked {
            coord: i.coord,
            class: i.class,
            pid: i.pid,
            detectable: i.detectable,
            psi: i.psi,
            framing_wide: matches!(i.class, Class::Truncate | Class::Garbage)
                || (i.class == Class::Header && i.offsets.first() == Some(&0)),
            moved_to: (i.class == Class::Header && i.offsets.as_slice() == [1, 2])
                .then_some(REWRITTEN_PID),
            noticed_by_continuity: i.class == Class::Drop
                || (i.class == Class::Header && matches!(i.offsets.as_slice(), [1, 2] | [3])),
        }
    }
}

pub struct Attribution {
    /// Injections still in play, i.e. from `base` onwards. Everything
    /// before `base` has been judged into the counters below and dropped
    /// — see [`Attribution::prune`].
    inj: Vec<Tracked>,
    st: Vec<InjState>,
    /// Absolute index of `inj[0]`. Every cursor below is absolute, so
    /// they survive pruning unchanged.
    pub(super) base: usize,
    window: u64,
    recovery_bound: u64,
    /// The header's approximate-resolution slack (see [`APPROX_SLACK`]).
    approx_slack: u64,
    /// The sender's declared tap spec, carried from the log header into
    /// the report unchanged.
    rate_per_10k: u32,
    min_gap: u64,
    classes: Vec<Class>,
    /// First injection whose windows may still be open.
    lo: usize,
    /// One past the last injection resolved at or before the latest event.
    hi: usize,
    /// First injection not yet resolved; resolution runs front-to-back
    /// because logged coordinates are in stream order.
    next_unresolved: usize,
    /// Whether an unexplained discontinuity-family signal is charged to
    /// the corruption tap or written off as transport loss. Fixed at
    /// construction, not at `finish`: a multi-day run cannot keep every
    /// unexplained event around waiting to be classified, and the tier is
    /// a property of the capture (`VerifyMode`), known before the first
    /// byte arrives. See [`Attribution::lossy`].
    excuse_transport_loss: bool,
    events: u64,
    attributed_events: u64,
    attributed_nonconformant: u64,
    attributed_discontinuities: u64,
    /// `attributed_events` split by the PID the signal surfaced on, and
    /// the tail of them that names no PID (a resync) — see the two
    /// fields of the same names on [`AttributionReport`]. Under
    /// corruption the signal can surface on a PID the profile never
    /// muxed (a damaged PID field can name any 13-bit value), so the
    /// real bound is the PID space (8192), not the multiplex's own
    /// PID count — still hard-bounded and O(1), not by the run's
    /// length.
    attributed_events_by_pid: BTreeMap<u16, u64>,
    attributed_events_unpinned: u64,
    resyncs: u64,
    /// Injections ever added, across `new` and every `append` — `inj.len()`
    /// no longer answers this once pruning starts.
    logged: u64,
    // Per-injection verdicts, accumulated as injections retire (and, at
    // `finish`, over whatever is still retained).
    detectable: u64,
    resolved: u64,
    unresolved: u64,
    undetected_count: u64,
    unrecovered_count: u64,
    undetected_lost: u64,
    unrecovered_lost: u64,
    detected_by_demux: u64,
    detected_by_reader_only: u64,
    detected_by_reader_only_per_class: BTreeMap<String, u64>,
    undetected_samples: Vec<String>,
    unrecovered_samples: Vec<String>,
    // Unexplained-event verdicts. The counts are per family and uncapped;
    // only `unexplained_samples` is bounded.
    unexplained_resyncs: u64,
    unexplained_discontinuities: u64,
    unexplained_nonconformant: u64,
    unexplained_transport_loss: u64,
    /// The last `ContinuityJump` seen per PID, explained or not: the
    /// evidence that packets went missing on that PID around there, and
    /// whether a PCR anomaly has already claimed it. Bounded by the PID
    /// space.
    last_cc_jump_by_pid: BTreeMap<u16, CcGap>,
    /// `PcrAnomaly` signals waiting to learn whether a continuity jump on
    /// their PID lands on the SAME packet — `tst_core` queues a packet's
    /// `PcrAnomaly` before its `ContinuityJump` (`check_pcr` runs before
    /// `check_continuity`), so the gap evidence arrives one event late.
    /// Settled by the first event more than a window later, or at `finish`.
    pending_pcr: Vec<PendingPcr>,
    pcr_anomalies_excused: u64,
    /// Receiver ordinal of the last media event — the last packet known to
    /// have arrived before a reconnect gap opens.
    last_media_at: u64,
    /// Every reconnect gap seen. One entry per reconnect — bounded by the
    /// run's outage count, not its length.
    reconnect_gaps: Vec<ReconnectGap>,
    /// The `PCR_PID`s the receiver's demuxer CURRENTLY declares, as far as
    /// its events show. The demuxer keeps a PCR timeline only for these,
    /// so they are the only PIDs a `PcrAnomaly` can honestly surface on.
    declared: DeclaredPcrPids,
    reconnects_seen: u64,
    lost_in_reconnect_gap: u64,
    history_skipped: u64,
    /// How many injections `build` was handed: the batch already in the
    /// log at attach. Absolute indices below it are history.
    attach_len: usize,
    /// Of that batch, the ones that stranded — see
    /// [`AttributionReport::history_unheard`].
    history_unheard: u64,
    unexplained_samples: Vec<String>,
    first_unexplained_nc: Option<String>,
    first_unexplained_disc: Option<String>,
    /// Whether any PCR has been seen yet — decides whether an injection
    /// APPENDED mid-capture can still trust its ordinal-0 anchor. See
    /// [`Attribution::append`].
    seen_pcr: bool,
    /// The last [`PCR_HISTORY`] `(pcr_base, at)` pairs fed to
    /// [`Attribution::on_pcr`], oldest first — replayed by
    /// [`Attribution::append`].
    recent_pcrs: VecDeque<(u64, u64)>,
    /// The last `(pcr_base, at)` [`Attribution::on_pcr`] accepted.
    last_anchor: Option<(u64, u64)>,
    /// A sample more than [`MAX_ANCHOR_JUMP_TICKS`] from `last_anchor`,
    /// held until the next sample confirms or replaces it.
    candidate_anchor: Option<(u64, u64)>,
    anchors_rejected: u64,
}

/// Which PIDs the demuxer currently declares as a program's `PCR_PID`,
/// rebuilt from the only three things its events say about topology.
///
/// - A `ProgramMap` declares a program's `PCR_PID`. The demuxer emits one
///   for a program's first PMT and for every PMT version after it, so a
///   later map for the same program REPLACES the PID.
/// - The demuxer keeps one program per PMT PID. A map for another program
///   on a PMT PID already in the set therefore shows that the PAT gave
///   that PID away, and the program that held it is gone.
/// - A reconnect resets the demuxer ([`DeclaredPcrPids::clear`]): it
///   holds no program at all until it reads a PAT and a PMT again.
///
/// What the events do NOT say is that a PAT dropped a program and left its
/// PMT PID unused: the demuxer retires that program and its PCR history
/// without an event. Such a program stays in this set until a reconnect or
/// a map supersedes it, so a `PcrAnomaly` on its former `PCR_PID` would
/// still be treated as one on a declared PID. The set can only be too
/// large, never too small — every PID the demuxer does declare reached it
/// through a `ProgramMap` — so the error can excuse, and cannot charge.
#[derive(Clone, Debug, Default)]
pub struct DeclaredPcrPids {
    /// Program number to `(pmt_pid, pcr_pid)`. Bounded by the program
    /// count.
    by_program: BTreeMap<u16, (u16, u16)>,
}

impl DeclaredPcrPids {
    /// A `ProgramMap`: `program_number`, whose PMT rides `pmt_pid`,
    /// carries its time base on `pcr_pid`.
    pub fn declare(&mut self, program_number: u16, pmt_pid: u16, pcr_pid: u16) {
        self.by_program.retain(|_, &mut (pmt, _)| pmt != pmt_pid);
        self.by_program.insert(program_number, (pmt_pid, pcr_pid));
    }

    /// The demuxer was reset: nothing is declared.
    pub fn clear(&mut self) {
        self.by_program.clear();
    }

    /// Whether any program declares `pid` as its `PCR_PID`.
    #[must_use]
    pub fn declares(&self, pid: u16) -> bool {
        self.by_program.values().any(|&(_, pcr)| pcr == pid)
    }

    /// Every declaration, as `(program_number, pmt_pid, pcr_pid)`.
    pub fn iter(&self) -> impl Iterator<Item = (u16, u16, u16)> + '_ {
        self.by_program
            .iter()
            .map(|(&program, &(pmt, pcr))| (program, pmt, pcr))
    }
}

/// One continuity gap, as PCR-anomaly evidence.
#[derive(Clone, Copy, Debug)]
struct CcGap {
    at: u64,
    /// A PCR anomaly has been excused against this gap. One gap is one
    /// span of lost packets on the PID, and the first PCR to arrive after
    /// it carries the whole jump: the demuxer re-bases its timeline on
    /// that PCR, so the next one compares against a value from AFTER the
    /// loss and is in step again. A second anomaly needs a second loss,
    /// which leaves its own gap. One gap therefore accounts for at most
    /// one anomaly.
    claimed: bool,
}

/// One reconnect, as receiver ordinals.
#[derive(Clone, Debug)]
struct ReconnectGap {
    /// The last media before the marker.
    from: u64,
    /// The `ReconnectDiscontinuity` marker itself.
    marker: u64,
    /// One past the last ordinal the gap covers: the marker plus the
    /// widest window an approximately-resolved injection can be placed
    /// in, or the next reconnect's marker if that comes first — see
    /// [`Attribution::on_reconnect`].
    end: u64,
    /// The first media event on each PID after the marker, as long as it
    /// falls inside the gap: the evidence that the demuxer, which the
    /// reconnect reset, was parsing that PID again. Read by
    /// [`ReconnectGap::holds`]. Bounded by the multiplex's PID count.
    first_media: BTreeMap<u16, u64>,
}

impl ReconnectGap {
    /// Whether an injection placed at `r` inside this gap is HELD to its
    /// detection obligation rather than excused as lost.
    ///
    /// Two things have to be shown, because the excusal is that the
    /// demuxer was never in a position to notice the damage.
    ///
    /// That the injection ARRIVED: it resolved exactly, from an anchor the
    /// receiver saw after the marker. Its own PCR packet came through the
    /// rebuilt connection and it sits `since_pcr` packets behind that. An
    /// anchor that never arrived (`approx`), one from before the marker
    /// (the link died between it and the injection) and no anchor at all
    /// show nothing of the kind. `>=`, not `>`: a PCR is stamped with its
    /// packet's 0-based ordinal and the marker with the COUNT of packets
    /// received when it surfaced, so an anchor stamped equal to the
    /// marker is the first packet after it.
    ///
    /// That the demuxer COULD DETECT it. The reconnect reset the demuxer:
    /// no PAT, no PMT, no continuity counter on any PID. What it needs
    /// back depends on how the class is noticed.
    ///
    /// - A `Drop`, a PID rewrite and a continuity-counter rewrite are
    ///   noticed as a continuity jump on the injection's own PID, which
    ///   the demuxer reports only when it holds a previous counter for
    ///   that PID and the PMT has resolved the PID to a stream. A media
    ///   event on that PID shows both at once: a sample is only ever
    ///   emitted for a resolved stream, from packets whose counters were
    ///   recorded on the way. Media on another PID shows neither for this
    ///   one.
    /// - Everything else is noticed without per-PID history. A damaged
    ///   PAT fails its CRC on PID 0, which is always parsed; a damaged PMT
    ///   fails it once the PAT has named the PMT's PID; an overrun
    ///   adaptation-field length is rejected packet by packet; destroyed
    ///   framing is a resync. Media on ANY PID shows all of it: no sample
    ///   is emitted before the PAT and a PMT are read, and none without
    ///   packet sync.
    ///
    /// A media event is the evidence because events are all the engine
    /// sees of the demuxer. It is sufficient, and it arrives a little
    /// after the demuxer became able — a sample surfaces when its PES
    /// completes — so an injection in that last stretch is excused though
    /// it could have been noticed. The stretch is one access unit on the
    /// PID in question and cannot outlast the gap's own window.
    ///
    /// The media has to precede the injection (`<= r`: an event is
    /// stamped with a packet count, so the packets behind it have
    /// ordinals below the stamp).
    fn holds(&self, inj: &Tracked, st: &InjState, r: u64) -> bool {
        let arrived = !st.approx && st.anchor_at.is_some_and(|a| a >= self.marker);
        let first_media = if inj.noticed_by_continuity {
            self.first_media.get(&inj.pid).copied()
        } else {
            self.first_media.values().copied().min()
        };
        arrived && first_media.is_some_and(|m| m <= r)
    }
}

#[derive(Clone, Debug)]
struct PendingPcr {
    at: u64,
    pid: u16,
    /// The sample text it is charged under if no gap claims it.
    text: String,
}

/// How far `a` is ahead of `b` on the 33-bit PCR-base circle. A PCR base
/// wraps every ~26.5 hours, so plain subtraction would mis-order every
/// coordinate straddling a wrap; by the standard half-space convention a
/// result below 2^32 means "ahead", and anything larger means `a` is
/// really behind `b`.
fn ticks_ahead(a: u64, b: u64) -> u64 {
    a.wrapping_sub(b) & ((1 << 33) - 1)
}

/// Signals a conformant receiver must produce for a given injection.
/// `true` for classes whose observable effect is not pinned to one signal
/// (any event inside the window then counts as having noticed).
fn expects(inj: &Tracked, sig: Signal) -> bool {
    match inj.class {
        // All three destroy packet framing: the reader resyncs, or the
        // packet vanishes from its PID and the CC jumps. An
        // adaptation-field-length overrun (one of the `Header` sub-kinds)
        // is instead reported as a plain non-conformance, so that counts
        // too — and so does a malformed PES, because all three can leave
        // a decoder reading a PES header off bytes that are not one: a
        // truncation misaligns the byte stream until a parser re-locks
        // (see `truncation_explains`), a garbage run does the same, and
        // an overrun adaptation-field length moves the payload offset
        // inside an otherwise intact packet. Measured: the `truncate`
        // positive control in `tests/corruption.rs` produces exactly one
        // `MalformedPes` on the video PID at the re-lock.
        //
        // A misaligned or overrun packet can parse a bogus PCR just as it
        // parses a bogus PES header, so a `PcrAnomaly` counts here too —
        // of either sign, since a bogus value can land on either side of
        // the real clock — and ONLY here. `PsiFlip` and a PSI `BodyFlip` keep
        // `OtherNonConformant` without it: a damaged section is rejected
        // before it can move a program's clock, so those classes cannot
        // cause a PCR jump.
        Class::Header | Class::Truncate | Class::Garbage => {
            matches!(
                sig,
                Signal::Resync
                    | Signal::ContinuityJump
                    | Signal::MalformedPes
                    | Signal::PcrAnomaly { .. }
                    | Signal::OtherNonConformant
            )
            // A PAT/PMT cut or garbled inside its section reaches the
            // demuxer as a section whose CRC fails: that checksum event is
            // the receiver noticing this injection, not an unexplained one.
            || (inj.psi && sig == Signal::PsiChecksum)
        }
        Class::Drop => matches!(sig, Signal::ContinuityJump),
        // A broken section usually fails its CRC, but a flipped pointer
        // field or section-length can surface as a table-id / section
        // -length non-conformance before the CRC is ever reached.
        Class::PsiFlip => matches!(sig, Signal::PsiChecksum | Signal::OtherNonConformant),
        Class::BodyFlip if inj.psi => {
            matches!(sig, Signal::PsiChecksum | Signal::OtherNonConformant)
        }
        // Everything else — a non-PSI body flip, a duplicate — is never
        // `detectable`, so this arm only decides a flag nothing reads.
        _ => true,
    }
}

/// The PID half of causality: whether `inj` can reach a signal reported
/// on `pid`. A resync carries no PID (sync was lost for the whole
/// multiplex) and a PSI checksum failure is the PSI PID's own — but only
/// an injection that itself touched a PSI table (`inj.psi`) may claim
/// that escape, or an injection on an unrelated media PID could explain
/// away a PAT/PMT checksum failure it never came near; a framing-wide
/// injection reaches everything; anything else has to land on the PID
/// it damaged — a PID rewrite reaches the PID it moved the packet to as
/// well as the one it left.
fn reaches(inj: &Tracked, pid: Option<u16>, sig: Signal) -> bool {
    match pid {
        None => true,
        Some(p) => {
            inj.framing_wide
                || (inj.psi && sig == Signal::PsiChecksum)
                || inj.pid == p
                || inj.moved_to == Some(p)
        }
    }
}

/// Whether `inj` can be the CAUSE of `sig` on `pid`: the class rule
/// ([`expects`]) and the PID rule ([`reaches`]) together. Position in
/// the window is the caller's third condition.
fn can_explain(inj: &Tracked, pid: Option<u16>, sig: Signal) -> bool {
    expects(inj, sig) && reaches(inj, pid, sig)
}

fn describe(inj: &Tracked, at: u64) -> String {
    format!(
        "{} on pid 0x{:04x} at packet {at}",
        inj.class.name(),
        inj.pid
    )
}

impl Attribution {
    /// Build from a parsed log, judging every finding strictly. Use for
    /// an offline capture (`verify`, and `recv --strict` on a transparent
    /// cell): there is no transport between a file and its verifier, so
    /// nothing is written off as transport loss.
    ///
    /// Injections logged before the stream's first PCR carry no base and
    /// resolve immediately, against receiver ordinal 0.
    #[must_use]
    pub fn strict(injections: Vec<Injection>, header: &LogHeader) -> Self {
        Self::build(injections, header, false)
    }

    /// Build from a parsed log, writing UNEXPLAINED DISCONTINUITIES off
    /// as transport loss rather than charging them to the corruption tap.
    ///
    /// Use for a live capture that crossed a real impaired link
    /// (`VerifyMode::Lossy`). That tier's whole contract is that packets
    /// go missing for reasons the sender never logged — an impairment
    /// proxy's loss, an SRT/RIST buffer overrun, the gap a reconnect
    /// leaves after an outage — and the engine has no way to tell such a
    /// gap from one the tap caused. With this:
    ///
    /// - Unexplained `ContinuityJump`/`OtherDiscontinuity` signals are
    ///   not corruption evidence at all. They are already counted in the
    ///   verifier's own `discontinuities`, which is exactly what the
    ///   lossy contract says to do with them, and they land in
    ///   [`AttributionReport::unexplained_transport_loss`] instead.
    ///   Unexplained `Resync`/`PsiChecksum`/`MalformedPes`/
    ///   `OtherNonConformant` still fail — with one exception: a forward
    ///   `PcrAnomaly` beside a `ContinuityJump` on its own PID is the
    ///   gap's timestamp signature and is excused, one per gap (counted
    ///   in [`AttributionReport::pcr_anomalies_excused`]). A backward one
    ///   is not: loss never turns a clock back. Otherwise a lost packet
    ///   does not forge a bad CRC or a malformed PES header.
    /// - An undetected or unrecovered injection with a FOREIGN
    ///   `ContinuityJump` in its window is excused into
    ///   [`AttributionReport::undetected_lost`] /
    ///   [`AttributionReport::unrecovered_lost`]: a corrupted packet the
    ///   network then threw away cannot be noticed by its damage, and
    ///   media the same gap swallowed is not the injection failing to
    ///   recover. Foreign is load-bearing — an injection's OWN jump never
    ///   excuses it (see [`Attribution::on_signal`]), or a `Drop`, whose
    ///   only observable is a continuity jump, would arrive pre-excused
    ///   and never have to recover at all.
    ///
    /// The excusal is applied as each event arrives, not at `finish`.
    /// Deciding late would mean holding every unexplained event for the
    /// length of the run, and a bounded hold would then let a run's
    /// excused events crowd out the one signal — an unexplained resync —
    /// that has no second detection path.
    #[must_use]
    pub fn lossy(injections: Vec<Injection>, header: &LogHeader) -> Self {
        Self::build(injections, header, true)
    }

    fn build(injections: Vec<Injection>, header: &LogHeader, excuse_transport_loss: bool) -> Self {
        let st = injections
            .iter()
            .map(|i| InjState {
                resolved_at: match i.coord.pcr_base {
                    None => Some(i.coord.since_pcr),
                    Some(_) => None,
                },
                ..InjState::default()
            })
            .collect();
        Attribution {
            logged: injections.len() as u64,
            inj: injections.iter().map(Tracked::from).collect(),
            st,
            base: 0,
            window: header.attribution_window,
            recovery_bound: header.recovery_bound,
            approx_slack: header.approx_slack,
            rate_per_10k: header.rate_per_10k,
            min_gap: header.min_gap,
            classes: header.classes.clone(),
            lo: 0,
            hi: 0,
            next_unresolved: 0,
            excuse_transport_loss,
            events: 0,
            attributed_events: 0,
            attributed_nonconformant: 0,
            attributed_discontinuities: 0,
            attributed_events_by_pid: BTreeMap::new(),
            attributed_events_unpinned: 0,
            resyncs: 0,
            detectable: 0,
            resolved: 0,
            unresolved: 0,
            undetected_count: 0,
            unrecovered_count: 0,
            undetected_lost: 0,
            unrecovered_lost: 0,
            detected_by_demux: 0,
            detected_by_reader_only: 0,
            detected_by_reader_only_per_class: BTreeMap::new(),
            undetected_samples: Vec::new(),
            unrecovered_samples: Vec::new(),
            unexplained_resyncs: 0,
            unexplained_discontinuities: 0,
            unexplained_nonconformant: 0,
            unexplained_transport_loss: 0,
            last_cc_jump_by_pid: BTreeMap::new(),
            pending_pcr: Vec::new(),
            pcr_anomalies_excused: 0,
            last_media_at: 0,
            reconnect_gaps: Vec::new(),
            declared: DeclaredPcrPids::default(),
            reconnects_seen: 0,
            lost_in_reconnect_gap: 0,
            history_skipped: 0,
            attach_len: injections.len(),
            history_unheard: 0,
            unexplained_samples: Vec::new(),
            first_unexplained_nc: None,
            first_unexplained_disc: None,
            seen_pcr: false,
            recent_pcrs: VecDeque::with_capacity(PCR_HISTORY),
            last_anchor: None,
            candidate_anchor: None,
            anchors_rejected: 0,
        }
    }

    /// Injections the log held before this receiver attached and which it
    /// deliberately did not ingest (see [`REATTACH_HISTORY`]); reported so
    /// coverage can be judged against what the sender logged in total.
    pub fn note_history_skipped(&mut self, n: u64) {
        self.history_skipped = self.history_skipped.saturating_add(n);
    }

    /// Whether this attribution writes unexplained discontinuities off as
    /// transport loss — i.e. whether it was built by
    /// [`Attribution::lossy`]. Read by `verify::Tally::finish` to check
    /// that the tier the capture is being judged in is the tier the
    /// attribution was built for.
    #[must_use]
    pub fn excuses_transport_loss(&self) -> bool {
        self.excuse_transport_loss
    }

    /// Injections whose per-injection state is still held. Bounded by
    /// [`PRUNE_BATCH`] plus whatever the sender has logged ahead of the
    /// receiver's current position; exposed so a test can assert the
    /// bound rather than infer it.
    #[must_use]
    pub fn retained(&self) -> usize {
        self.inj.len()
    }

    /// One past the last absolute injection index.
    fn end(&self) -> usize {
        self.base + self.inj.len()
    }

    fn tracked(&self, i: usize) -> &Tracked {
        &self.inj[i - self.base]
    }

    pub(super) fn state(&self, i: usize) -> &InjState {
        &self.st[i - self.base]
    }

    fn state_mut(&mut self, i: usize) -> &mut InjState {
        &mut self.st[i - self.base]
    }

    /// Add injections the sender logged AFTER this attribution was built —
    /// what a live receiver's [`LogTail`](super::LogTail) hands back as the run proceeds.
    ///
    /// They arrive in log order, which is stream order, so they extend the
    /// list past every cursor and the forward scan stays valid.
    ///
    /// One case cannot be carried over honestly. An injection whose
    /// coordinate has no PCR anchor (`pcr_base: None`) means "before the
    /// stream's first PCR", and [`Attribution::strict`] and [`Attribution::lossy`] resolve it against
    /// receiver ordinal 0 — sound only for a receiver that was listening
    /// from the stream's first packet. Once this attribution has seen a
    /// PCR, the receiver is demonstrably past that point, so an anchorless
    /// injection appended now belongs to a part of the stream it can no
    /// longer place: it is stranded instead, counted `unresolved` and
    /// never judged. Absent evidence is not evidence of a failure.
    ///
    /// An appended injection's anchor PCR may ALREADY have been fed to
    /// [`Attribution::on_pcr`]. The tap logs once per `send_bytes`
    /// message, so an injection in the message after the one carrying its
    /// PCR is written only after that PCR left the sender — and over a
    /// zero-latency link (UDP or TCP on loopback) the receiver can have
    /// read it already. Resolving such an injection against the NEXT base
    /// instead would misplace it by a whole PCR interval, past the event
    /// it caused, which then reads as unexplained and the injection as
    /// undetected. So the recent PCRs are replayed, oldest first, over the
    /// newly appended injections; a link with real latency never needs
    /// them, because its log runs ahead of the receiver and every new
    /// anchor is still in the future.
    pub fn append(&mut self, injections: Vec<Injection>) {
        if injections.is_empty() {
            return;
        }
        for i in injections {
            let anchorless = i.coord.pcr_base.is_none();
            let st = InjState {
                resolved_at: match (anchorless, self.seen_pcr) {
                    (true, false) => Some(i.coord.since_pcr),
                    _ => None,
                },
                stranded: anchorless && self.seen_pcr,
                ..InjState::default()
            };
            self.inj.push(Tracked::from(&i));
            self.st.push(st);
            self.logged += 1;
        }
        for k in 0..self.recent_pcrs.len() {
            let (base, at) = self.recent_pcrs[k];
            self.resolve_against(base, at);
        }
    }

    /// The receiver saw `pcr_base` on the packet at receiver ordinal `at`.
    ///
    /// Resolves every injection anchored at that base, plus any anchored
    /// at a base that never arrived — the corruption may have destroyed
    /// the packet carrying it — for which this is the first base at or
    /// after the anchor, and which is within [`MAX_APPROX_TICKS`] of it.
    /// Those resolve approximately and are given one PCR interval of extra
    /// attribution window. An anchor further behind than that is stranded:
    /// the receiver was not listening (a reconnect outage), so the
    /// injection stays unresolved and is never judged. A sample more than
    /// [`MAX_ANCHOR_JUMP_TICKS`] from the last accepted anchor is held as a
    /// candidate rather than accepted outright — see [`MAX_ANCHOR_JUMP_TICKS`].
    pub fn on_pcr(&mut self, pcr_base: u64, at: u64) {
        let near = |x: u64, y: u64| {
            ticks_ahead(x, y) <= MAX_ANCHOR_JUMP_TICKS || ticks_ahead(y, x) <= MAX_ANCHOR_JUMP_TICKS
        };
        if let Some((prev, _)) = self.last_anchor {
            if !near(pcr_base, prev) {
                match self.candidate_anchor.take() {
                    Some((cand_base, cand_at)) if near(pcr_base, cand_base) => {
                        // Two consecutive samples agree on a clock far from
                        // the last anchor: the clock really moved.
                        self.accept_anchor(cand_base, cand_at);
                        self.accept_anchor(pcr_base, at);
                    }
                    Some(_) => {
                        self.anchors_rejected += 1;
                        self.candidate_anchor = Some((pcr_base, at));
                    }
                    None => {
                        self.candidate_anchor = Some((pcr_base, at));
                    }
                }
                return;
            }
        }
        if self.candidate_anchor.take().is_some() {
            // The sample after a wild one is back on the old clock: the
            // wild one was a bogus decode.
            self.anchors_rejected += 1;
        }
        self.accept_anchor(pcr_base, at);
    }

    fn accept_anchor(&mut self, pcr_base: u64, at: u64) {
        self.seen_pcr = true;
        self.last_anchor = Some((pcr_base, at));
        if self.recent_pcrs.len() == PCR_HISTORY {
            self.recent_pcrs.pop_front();
        }
        self.recent_pcrs.push_back((pcr_base, at));
        self.resolve_against(pcr_base, at);
    }

    /// Resolve every not-yet-resolved injection that the base `pcr_base`,
    /// seen at receiver ordinal `at`, places — the body of
    /// [`Attribution::on_pcr`], also replayed by [`Attribution::append`].
    fn resolve_against(&mut self, pcr_base: u64, at: u64) {
        while self.next_unresolved < self.end() {
            let i = self.next_unresolved;
            let Some(b) = self.tracked(i).coord.pcr_base else {
                // Resolved at construction.
                self.next_unresolved += 1;
                continue;
            };
            let ahead = ticks_ahead(pcr_base, b);
            if b != pcr_base {
                if ahead >= 1 << 32 {
                    // The counter says the injection's base is ahead of us.
                    // Genuinely future (the log runs a little ahead of the
                    // wire) only if it is ahead by a plausible amount; a
                    // base "ahead" by more than MAX_FUTURE_TICKS is history
                    // whose age wrapped the 33-bit counter.
                    if ticks_ahead(b, pcr_base) <= MAX_FUTURE_TICKS {
                        // Still in the future; so is every later injection,
                        // because logged coordinates are ordered.
                        break;
                    }
                    self.state_mut(i).stranded = true;
                    self.next_unresolved += 1;
                    continue;
                }
                if ahead > MAX_APPROX_TICKS {
                    // Permanently unresolvable — every later base is
                    // further still — so advance past it rather than
                    // stalling the cursor on it.
                    self.state_mut(i).stranded = true;
                    self.next_unresolved += 1;
                    continue;
                }
            }
            // Saturating: a corrupt log must not panic the verifier.
            let since = self.tracked(i).coord.since_pcr;
            let st = self.state_mut(i);
            st.resolved_at = Some(at.saturating_add(since));
            st.approx = b != pcr_base;
            st.anchor_at = Some(at);
            self.next_unresolved += 1;
        }
    }

    /// How far BEFORE its resolved position an injection's own evidence
    /// may surface.
    ///
    /// `resolved_at` is `ord(anchor PCR) + since_pcr`: a receiver ordinal
    /// plus a SENDER-side packet count. That sum is only exact if every
    /// packet between the anchor and the injection also reached the
    /// receiver. Under transport loss it over-shoots by however many of
    /// them went missing, so the injected packet's true receiver ordinal
    /// lies somewhere in `[r - since_pcr, r]` — the lower end being the
    /// degenerate case where the whole span was lost. A forward-only
    /// window (`r <= at`) therefore rejects an injection's OWN event
    /// whenever the backward displacement exceeds the demuxer's forward
    /// lateness, and the injection is reported undetected for damage the
    /// receiver noticed perfectly well.
    ///
    /// Measured on a 1-hour smoke run: 3 of 1738 detectable
    /// injections across two legs. Rare because the displacement usually
    /// loses to the lateness `ATTRIBUTION_WINDOW` was sized for, but the
    /// rate scales with run length and loss — order 100-200 over 72
    /// hours.
    ///
    /// CLAMPED, because windows must stay disjoint: two injections are
    /// `min_gap` apart, so extending one backwards by more than
    /// `min_gap - (window + APPROX_SLACK) - 1` would let it overlap its
    /// predecessor's, and `hit` would hand an event to the newer of the
    /// two — exactly the ambiguity [`CorruptConfig::validate`]'s `min_gap`
    /// floor exists to prevent. The clamp keeps the invariant true for
    /// ANY valid config rather than resting on a bound `since_pcr`
    /// happens to obey.
    ///
    /// The `- 1` is load-bearing. [`Attribution::window_contains`] is
    /// INCLUSIVE at both ends, so without it a predecessor's forward edge
    /// `rA + window + APPROX_SLACK` and a successor's lower edge
    /// `rB - reach` land on the same packet, and that one packet sits in
    /// both windows. `APPROX_SLACK` is in the budget unconditionally
    /// because the predecessor may have resolved approximately, which is
    /// what widens its forward edge.
    ///
    /// The bound is not "one PCR interval": `since_pcr` counts packets
    /// since the last packet that CARRIED a PCR, and the muxer's PCR
    /// cadence is not perfectly regular (`pcr_only_due` catch-up packets,
    /// and the PCR PID's own push rate). Measured on the same smoke, it
    /// reached 302 packets on a 40 ms-PCR profile and 393 on a 100 ms
    /// one — roughly 2.3 nominal intervals — against the ~130 a
    /// one-interval model predicts. At the default `min_gap` of 1000 the
    /// clamp is 371, which covers every injection in that run bar 4 of
    /// 3026.
    pub(super) fn backward_reach(&self, i: usize) -> u64 {
        let budget = self
            .min_gap
            .saturating_sub(self.window.saturating_add(self.approx_slack))
            .saturating_sub(1);
        self.tracked(i).coord.since_pcr.min(budget)
    }

    /// First receiver ordinal injection `i`'s window covers, or `None`
    /// when it has no resolved position at all.
    pub(super) fn window_lo(&self, i: usize) -> Option<u64> {
        let r = self.state(i).resolved_at?;
        Some(r.saturating_sub(self.backward_reach(i)))
    }

    /// Move the scan cursors up to event ordinal `at`, then retire what
    /// they have left behind.
    fn advance(&mut self, at: u64) {
        // A stranded injection has no position at all, so it must not park
        // the cursor: injections logged AFTER an outage resolve normally
        // and still need to be reachable.
        //
        // The test is the window's LOWER edge, not `resolved_at`: an
        // injection resolved past `at` can still cover `at` through
        // `backward_reach`, and a cursor keyed on `resolved_at <= at`
        // would leave it outside the scan range entirely.
        while self.hi < self.end()
            && (self.state(self.hi).stranded || self.window_lo(self.hi).is_some_and(|lo| lo <= at))
        {
            self.hi += 1;
        }
        let span = self.window.max(self.recovery_bound) + self.approx_slack;
        while self.lo < self.hi
            && (self.state(self.lo).stranded
                || self
                    .state(self.lo)
                    .resolved_at
                    .is_some_and(|r| r.saturating_add(span) < at))
        {
            self.lo += 1;
        }
        self.prune();
    }

    /// Fold injections both cursors have passed into the running
    /// counters and drop their per-injection state.
    ///
    /// Safe to do at all because the two cursors only ever move forward
    /// and nothing below them is ever read again: `lo` advances only past
    /// an injection that is STRANDED (a terminal state — it can never be
    /// resolved, so it can never be judged) or RESOLVED with every window
    /// closed, and `next_unresolved` is the only other index into the
    /// list. Retiring at the minimum of the two keeps both valid.
    ///
    /// Without this the engine holds every injection of the run: a
    /// 72-hour soak logs a quarter of a million of them, and even reduced
    /// to [`Tracked`] that is state growing with the capture, on the same
    /// process the run's own `rss_slope_*_recv` verdict gates at 200
    /// KiB/h.
    ///
    /// `next_unresolved` only moves when a PCR is fed — by
    /// [`Attribution::on_pcr`] directly, or by [`Attribution::append`]
    /// replaying the ones already seen — so a receiver that stopped seeing PCRs entirely would stop
    /// retiring. That is not a memory hazard worth guarding: a capture
    /// with no PCR resolves no coordinate at all, and its whole
    /// corruption verdict is already `unresolved`.
    fn prune(&mut self) {
        let keep_from = self.lo.min(self.next_unresolved);
        let n = keep_from - self.base;
        if n < PRUNE_BATCH {
            return;
        }
        for k in 0..n {
            let (inj, st) = (self.inj[k], self.st[k]);
            self.note_if_unheard(self.base + k, &st);
            // A retired injection's recovery window is necessarily closed:
            // `lo` only passed it once `resolved_at + max(window,
            // recovery_bound) + APPROX_SLACK < at`, and `at` is a receiver
            // ordinal, so it is at most the capture's final packet count.
            self.judge(&inj, &st, true);
        }
        self.inj.drain(..n);
        self.st.drain(..n);
        self.base = keep_from;
    }

    /// Count an injection from the attach batch (absolute index `i`) that
    /// stranded: history the receiver was never listening for.
    fn note_if_unheard(&mut self, i: usize, st: &InjState) {
        if i < self.attach_len && st.stranded && st.resolved_at.is_none() {
            self.history_unheard += 1;
        }
    }

    /// Fold one injection's verdict into the running counters.
    /// `recovery_window_closed` says whether the capture ran far enough
    /// past it to hold it to a recovery obligation at all.
    fn judge(&mut self, inj: &Tracked, st: &InjState, recovery_window_closed: bool) {
        if inj.detectable {
            self.detectable += 1;
        }
        let Some(r) = st.resolved_at else {
            self.unresolved += 1;
            return;
        };
        self.resolved += 1;
        // Position inside a reconnect gap is not enough: an injection
        // that arrived at a demuxer able to notice it owes the detection
        // wherever it sits — see `ReconnectGap::holds`. No gap reaches
        // past the marker of the reconnect after it, so from a marker
        // onwards the only gap with a say is that marker's own.
        let in_gap = self
            .reconnect_gaps
            .iter()
            .any(|g| g.from <= r && r < g.end && !g.holds(inj, st, r));
        let lost = self.excuse_transport_loss && (st.cc_jump_in_window || in_gap);
        if inj.detectable && !st.detected {
            if lost {
                self.undetected_lost += 1;
                if in_gap {
                    self.lost_in_reconnect_gap += 1;
                }
            } else {
                self.undetected_count += 1;
                if self.undetected_samples.len() < MAX_SAMPLES {
                    self.undetected_samples.push(describe(inj, r));
                }
            }
        }
        // Split the detection credit. A `Resync` is the
        // harness's raw reader noticing; everything else is the
        // receiver's own demuxer.
        if inj.detectable && st.detected {
            if st.detected_by_demux {
                self.detected_by_demux += 1;
            } else {
                self.detected_by_reader_only += 1;
                *self
                    .detected_by_reader_only_per_class
                    .entry(inj.class.name().to_string())
                    .or_insert(0) += 1;
            }
        }
        if !st.recovered && recovery_window_closed {
            if lost {
                self.unrecovered_lost += 1;
            } else {
                self.unrecovered_count += 1;
                if self.unrecovered_samples.len() < MAX_SAMPLES {
                    self.unrecovered_samples.push(describe(inj, r));
                }
            }
        }
    }

    /// Index of the resolved injection whose attribution window contains
    /// `at`, newest first — the closest preceding injection is the one
    /// that explains something happening at `at`, if any does.
    fn hit(&mut self, at: u64) -> Option<usize> {
        self.advance(at);
        (self.lo..self.hi)
            .rev()
            .find(|&i| self.window_contains(i, at))
    }

    /// Index of the newest resolved injection whose window contains
    /// `at` AND that can cause `sig` on `pid` — see [`can_explain`].
    /// [`Attribution::hit`] keeps the position-only answer for
    /// [`Attribution::truncation_explains`].
    fn explainer(&mut self, at: u64, pid: Option<u16>, sig: Signal) -> Option<usize> {
        self.advance(at);
        (self.lo..self.hi)
            .rev()
            .find(|&i| self.window_contains(i, at) && can_explain(self.tracked(i), pid, sig))
    }

    /// Whether injection `i` is resolved and its attribution window
    /// contains `at`. Takes `&self` — unlike [`Attribution::hit`] it never
    /// advances the cursors, so it is usable from read-only queries that
    /// run after some `on_*` call has already positioned them.
    pub(super) fn window_contains(&self, i: usize, at: u64) -> bool {
        let Some(r) = self.state(i).resolved_at else {
            return false;
        };
        let w = if self.state(i).approx {
            self.window + self.approx_slack
        } else {
            self.window
        };
        // `[r - backward_reach, r + w]`. The backward half absorbs the
        // transport loss `resolved_at` cannot see (see `backward_reach`);
        // the forward half is the demuxer lateness the window was always
        // sized for.
        let lo = r.saturating_sub(self.backward_reach(i));
        lo <= at && at <= r.saturating_add(w)
    }

    /// Whether an injection plausibly DAMAGED the bytes a receiver-side
    /// decoder is about to reject at receiver ordinal `at` on `pid`.
    /// Records nothing: it is a question a content oracle asks before
    /// judging a record, not a receiver event.
    ///
    /// True when `at` falls in the window of a resolved injection that
    /// either targeted `pid` (any class — a body flip, a header rewrite or
    /// a dropped packet all leave the record on that PID malformed or
    /// incomplete) or was a [`Class::Truncate`]/[`Class::Garbage`] on any
    /// PID (both destroy packet framing for the whole multiplex, so the
    /// damage is not confined to the PID they hit).
    ///
    /// The point is not to excuse a decoder bug. A record the tap never
    /// touched still has to decode, and a record inside a window still has
    /// to be DELIVERED — this only stops a content oracle from reporting
    /// deliberate damage as a producer defect, which would make every run
    /// with the tap enabled fail for doing its job.
    ///
    /// Takes `&mut self` for the same reason
    /// [`Attribution::truncation_explains`] does: it advances the sliding
    /// cursors to `at` itself rather than trusting that some other `on_*`
    /// call already did. Depending on the caller's ordering would make the
    /// answer silently wrong (an unpositioned cursor range is empty, so
    /// every query would answer "no") the moment anyone reordered the
    /// receiver's event handling. Like the `on_*` methods it expects
    /// non-decreasing `at`.
    #[must_use]
    pub fn explains_damage(&mut self, at: u64, pid: u16) -> bool {
        self.advance(at);
        (self.lo..self.hi).rev().any(|i| {
            self.window_contains(i, at)
                && (self.tracked(i).pid == pid
                    || matches!(self.tracked(i).class, Class::Truncate | Class::Garbage))
        })
    }

    /// Whether a TRUNCATION explains an anomaly observed at receiver
    /// ordinal `at`, WITHOUT recording it as an event.
    ///
    /// One class, for one measured reason. [`Class::Truncate`] drops a
    /// non-multiple of 188 bytes, so from that point the byte stream is
    /// MISALIGNED and stays misaligned until a parser hunts a new
    /// 188-stride lock. During that hunt it can lock onto a 0x47 that is
    /// live media payload and read the bytes after it as a PES header —
    /// yielding a "PTS" that was never a timestamp. That is the
    /// truncation doing exactly what it is for, not a timing defect, and
    /// a receiver that re-locks and carries on is behaving correctly.
    ///
    /// No other class can do it. [`Class::Garbage`] inserts bytes but
    /// never emits 0x47, precisely so it cannot fake a packet start;
    /// every other class preserves packet length and therefore alignment.
    /// Measured over 6 profiles × 7 classes × 40 seeds: 42 PTS
    /// monotonicity breaks, all 42 from `truncate`, none from anything
    /// else. So the exception stays pinned to the one class rather than
    /// excusing derived anomalies in general — a PTS defect inside a body
    /// flip's, drop's, dup's, header's or psi_flip's window still fails
    /// the run, and after the `BodyFlip` draw range was moved past the PES
    /// header no flip can manufacture one anyway.
    ///
    /// Deliberately NOT routed through [`Attribution::on_signal`]: that
    /// would inflate `attributed_events` with something the receiver never
    /// reported, and a verifier subtracting those counts from its own
    /// event tallies would then excuse one real event per anomaly.
    ///
    /// Like the `on_*` methods this expects non-decreasing `at`.
    pub fn truncation_explains(&mut self, at: u64) -> bool {
        self.hit(at)
            .is_some_and(|i| self.tracked(i).class == Class::Truncate)
    }

    /// The retained injection closest to `at`, for an unexplained sample's
    /// text: "what was nearby" is the first question a reader asks of an
    /// event nothing explains. Scans only what is retained (bounded by
    /// `PRUNE_BATCH` plus the sender's lookahead), and only when a sample
    /// is actually CHARGED — never for an excused event (a lossy
    /// discontinuity, a PCR anomaly a gap explains), which is not a
    /// finding and whose text nobody reads for context.
    fn nearest_context(&self, at: u64) -> String {
        let mut best: Option<(u64, usize)> = None;
        for i in self.base..self.base + self.inj.len() {
            if let Some(r) = self.state(i).resolved_at {
                let d = r.abs_diff(at);
                if best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, i));
                }
            }
        }
        match best {
            Some((d, i)) => {
                let t = self.tracked(i);
                let r = self.state(i).resolved_at.unwrap_or(0);
                format!(
                    " (nearest injection: {} on pid 0x{:04x} at packet {r}, {d} packets {})",
                    t.class.name(),
                    t.pid,
                    if r <= at { "before" } else { "after" }
                )
            }
            None => String::new(),
        }
    }

    fn excuse_pcr_anomaly(&mut self) {
        self.unexplained_transport_loss += 1;
        self.pcr_anomalies_excused += 1;
    }

    fn charge_pcr_anomaly(&mut self, q: PendingPcr) {
        self.first_unexplained_nc
            .get_or_insert_with(|| q.text.clone());
        self.unexplained_nonconformant += 1;
        if self.unexplained_samples.len() < MAX_SAMPLES {
            self.unexplained_samples.push(q.text);
        }
    }

    /// Charge every pending PCR anomaly whose window `at` has passed.
    fn settle_pending_pcr(&mut self, at: u64) {
        let window = self.window;
        let mut keep = Vec::with_capacity(self.pending_pcr.len());
        for q in std::mem::take(&mut self.pending_pcr) {
            if at > q.at.saturating_add(window) {
                self.charge_pcr_anomaly(q);
            } else {
                keep.push(q);
            }
        }
        self.pending_pcr = keep;
    }

    /// An error-class event surfaced at receiver ordinal `at` (`pid` is
    /// `None` for a resync, which is not attributable to a PID).
    ///
    /// The event is EXPLAINED by the newest injection whose window
    /// contains `at` and that can actually cause `sig` on `pid` — the
    /// class rule (`expects`) and the PID rule (`reaches`) together,
    /// see `can_explain`. An event of a class an injection cannot
    /// cause, or on a PID it never touched, is therefore UNEXPLAINED
    /// even when it lands squarely inside that injection's window:
    /// window position is proximity, not causation.
    pub fn on_signal(&mut self, at: u64, pid: Option<u16>, sig: Signal) {
        self.settle_pending_pcr(at);
        if sig == Signal::ContinuityJump {
            if let Some(p) = pid {
                // A pending PCR jump on this PID at this very packet: the
                // gap it was waiting for. It takes the gap with it — one
                // gap, one anomaly (see `CcGap::claimed`) — so a second
                // anomaly pending on the same packet stays pending and is
                // charged.
                let claimed = match self
                    .pending_pcr
                    .iter()
                    .position(|q| q.pid == p && q.at == at)
                {
                    Some(k) => {
                        self.pending_pcr.swap_remove(k);
                        self.excuse_pcr_anomaly();
                        true
                    }
                    None => false,
                };
                self.last_cc_jump_by_pid.insert(p, CcGap { at, claimed });
            }
        }
        self.events += 1;
        if sig == Signal::Resync {
            self.resyncs += 1;
        }
        // The demuxer keeps a PCR timeline only for a PID some PMT
        // declares as `PCR_PID`, so a `PcrAnomaly` anywhere else is not
        // something an injection or a lost packet can have caused: it
        // means a timeline was kept for a PID that has none. A PID rewrite
        // explains a packet APPEARING on the PID it was moved to, not
        // that. Never attributed and never excused, in either tier.
        let undeclared_pcr = matches!(sig, Signal::PcrAnomaly { .. })
            && !pid.is_some_and(|p| self.declared.declares(p));
        let attributed_to = self.explainer(at, pid, sig).filter(|_| !undeclared_pcr);
        // A continuity jump is the one signal that also means "packets
        // went missing here", so it is recorded against every injection
        // whose window contains it — EXCEPT the injection it is attributed
        // to.
        //
        // That exception is the whole point. An injection's own jump is
        // its own damage surfacing; it cannot also be evidence that the
        // network swallowed that same injection. Without the exception a
        // `Drop` would excuse itself: its only observable IS a continuity
        // jump, so every drop would arrive pre-excused and its recovery
        // obligation would evaporate. A jump from an OUTAGE or from
        // another injection still excuses, which is what this flag is for.
        //
        // `hit` picks the newest covering injection, so the loop can still
        // mark an older one whose window also covers `at`. Under a valid
        // config windows cannot overlap (`min_gap >= 2 *
        // ATTRIBUTION_WINDOW`) and only an `approx` resolution's widened
        // window reaches this at all, but marking solely the newest would
        // silently under-record when they do overlap.
        if sig == Signal::ContinuityJump {
            for i in self.lo..self.hi {
                // Same causal restriction as the explanation above: a
                // gap on a PID this injection never touched is not
                // evidence its own packet went missing.
                if !self.window_contains(i, at) || !reaches(self.tracked(i), pid, sig) {
                    continue;
                }
                // An injection's own jump is its own damage surfacing —
                // but ONLY for a class whose expected signals include a
                // continuity jump. There the jump IS the detection, and
                // letting it also excuse would pre-excuse every `Drop`,
                // whose only observable is exactly that jump, so no drop
                // would ever have to show the stream recovering.
                //
                // For a class whose expected signals EXCLUDE it — a
                // `PsiFlip`, or a `BodyFlip` on a PSI packet, both of
                // which have to answer with a bad CRC or a
                // non-conformance — an attributed continuity jump means
                // something else entirely: the packet carrying the
                // injection went missing in transit, so the damage never
                // reached the receiver to be noticed. That is the
                // transport-loss evidence `undetected_lost` exists for.
                //
                // Measured on a 1-hour smoke run: a PSI body flip on
                // the srt leg whose corrupted PMT packet the proxy
                // dropped. The receiver emitted a `ContinuityJump` on
                // PID 0x1000, `on_signal` attributed it to that very
                // injection, `expects` rejected it, and the old blanket
                // exclusion then kept it out of `undetected_lost` too —
                // so the run failed for damage nobody could have seen.
                //
                // `explainer` only ever hands back an injection whose
                // expected signals include this one, so being the
                // explainer IS the class check.
                if Some(i) == attributed_to {
                    continue;
                }
                self.state_mut(i).cc_jump_in_window = true;
            }
        }
        match attributed_to {
            Some(i) => {
                self.attributed_events += 1;
                // …and the same event again, keyed to the PID it
                // surfaced on, so a verifier can widen THAT PID's floor
                // and no other. A resync names no PID (sync was lost for
                // the whole multiplex) and lands in the unpinned tail.
                match pid {
                    Some(p) => *self.attributed_events_by_pid.entry(p).or_insert(0) += 1,
                    None => self.attributed_events_unpinned += 1,
                }
                match sig {
                    Signal::PsiChecksum
                    | Signal::MalformedPes
                    | Signal::PcrAnomaly { .. }
                    | Signal::OtherNonConformant => {
                        self.attributed_nonconformant += 1;
                    }
                    Signal::ContinuityJump | Signal::OtherDiscontinuity => {
                        self.attributed_discontinuities += 1;
                    }
                    Signal::Resync => {}
                }
                self.state_mut(i).detected = true;
                // A resync is the harness's own raw reader
                // losing and regaining packet sync, not a `DemuxEvent`.
                // Only the other signals are the receiver reporting.
                if sig != Signal::Resync {
                    self.state_mut(i).detected_by_demux = true;
                }
            }
            // Unexplained. Which family it belongs to — and therefore
            // whether transport-loss excusal takes it — is decided HERE,
            // not at `finish`.
            //
            // Deciding late was a hole with teeth. The sample list is
            // capped, and a lossy multi-day run produces excused
            // continuity jumps by the thousand: they would fill the cap
            // within the first minutes, after which a genuinely
            // unexplained RESYNC — the one signal with no second
            // detection path, since it is not a `DemuxEvent` and resync
            // mode suppresses `rawts_sync_loss` — would be dropped on the
            // floor and the run would pass. Excused events now consume no
            // cap budget, and each family is counted without a cap.
            None => {
                // The plain description first. The nearest-injection
                // annotation is appended only where a sample is actually
                // CHARGED — an excused event is not a finding, so nobody
                // reads its text looking for what was nearby, and
                // `nearest_context` scans the retained injection list.
                let text = format!("{sig:?} on pid {pid:?} at packet {at}");
                // The two excused shapes return here, before the scan.
                match sig {
                    // Forward only. Lost packets take their share of the
                    // timeline with them, so the PCR after a gap is AHEAD
                    // of the one before it by about the lost span. No
                    // amount of loss produces a PCR behind its
                    // predecessor, so a backward jump beside a gap is
                    // still a finding. The size of a forward jump is not
                    // bounded here: a continuity counter says packets
                    // went missing, not how many.
                    Signal::PcrAnomaly { delta }
                        if self.excuse_transport_loss && !undeclared_pcr && delta > 0 =>
                    {
                        if let Some(p) = pid {
                            let window = self.window;
                            if let Some(gap) = self
                                .last_cc_jump_by_pid
                                .get_mut(&p)
                                .filter(|g| !g.claimed && at.saturating_sub(g.at) <= window)
                            {
                                gap.claimed = true;
                                self.excuse_pcr_anomaly();
                                return;
                            }
                            // Still undecided: charged by `settle_pending_pcr`
                            // or `finish` unless a gap claims it first, so it
                            // carries the annotation it would be charged under.
                            let text = text + &self.nearest_context(at);
                            self.pending_pcr.push(PendingPcr { at, pid: p, text });
                            return;
                        }
                    }
                    Signal::ContinuityJump | Signal::OtherDiscontinuity
                        if self.excuse_transport_loss =>
                    {
                        self.first_unexplained_disc.get_or_insert(text);
                        self.unexplained_transport_loss += 1;
                        return;
                    }
                    _ => {}
                }
                // Everything from here is charged as a finding.
                let text = text + &self.nearest_context(at);
                match sig {
                    Signal::PcrAnomaly { .. }
                    | Signal::PsiChecksum
                    | Signal::MalformedPes
                    | Signal::OtherNonConformant => {
                        self.first_unexplained_nc
                            .get_or_insert_with(|| text.clone());
                        self.unexplained_nonconformant += 1;
                    }
                    Signal::ContinuityJump | Signal::OtherDiscontinuity => {
                        self.first_unexplained_disc
                            .get_or_insert_with(|| text.clone());
                        self.unexplained_discontinuities += 1;
                    }
                    Signal::Resync => self.unexplained_resyncs += 1,
                }
                if self.unexplained_samples.len() < MAX_SAMPLES {
                    self.unexplained_samples.push(text);
                }
            }
        }
    }

    /// A Sample/Metadata event on `pid` surfaced at receiver ordinal `at` —
    /// evidence the stream recovered from whatever preceded it.
    pub fn on_media(&mut self, at: u64, pid: u16) {
        self.settle_pending_pcr(at);
        self.last_media_at = at;
        // Only the latest gap: media after an older marker says nothing
        // once a later reconnect has reset the demuxer again.
        if let Some(g) = self.reconnect_gaps.last_mut().filter(|g| at < g.end) {
            g.first_media.entry(pid).or_insert(at);
        }
        self.advance(at);
        for i in self.lo..self.hi {
            if self.state(i).recovered {
                continue;
            }
            let Some(r) = self.state(i).resolved_at else {
                continue;
            };
            if at < r || at > r.saturating_add(self.recovery_bound) {
                continue;
            }
            // Truncation and inserted garbage break packet sync for the
            // whole multiplex, so media on ANY PID proves recovery; the
            // other classes damage one PID and must be answered on it.
            //
            // A PAT/PMT PID is the exception to "answered on it": it
            // carries no media at all. `Sample`/`Metadata` events only
            // ever name an elementary stream, so an injection on a PSI PID
            // would wait forever for media on PID 0 and be reported
            // unrecovered no matter how healthy the stream was. What
            // recovery means there is that the multiplex kept delivering —
            // which is exactly media on any PID.
            let any_pid = self.tracked(i).psi
                || matches!(self.tracked(i).class, Class::Garbage | Class::Truncate);
            if any_pid || self.tracked(i).pid == pid {
                self.state_mut(i).recovered = true;
            }
        }
    }

    /// The receiver's managed transport rebuilt its connection and reset
    /// the demuxer (`DemuxEvent::ReconnectDiscontinuity`) at receiver
    /// ordinal `at`. Everything the sender logged between the last media
    /// the receiver saw and this marker never arrived — including the PCR
    /// anchors those injections were logged against, so they resolve
    /// approximately against the first base AFTER the marker, just past
    /// `at`. Under lossy judgement an injection resolved inside
    /// `[last_media_at, at + window + APPROX_SLACK]` is judged lost in
    /// transit rather than undetected — unless it resolved exactly from
    /// an anchor received after the marker, which shows it arrived, and
    /// media after the marker shows the demuxer had re-acquired what it
    /// needs to notice it: media on the injection's own PID for damage
    /// noticed as a continuity jump, on any PID for the rest. Strict
    /// never excuses.
    ///
    /// The marker also ends everything that described the connection
    /// before it: the previous reconnect's gap, the declared `PCR_PID`s
    /// and the continuity gaps seen so far.
    pub fn on_reconnect(&mut self, at: u64) {
        self.settle_pending_pcr(at);
        self.reconnects_seen += 1;
        // A gap belongs to one reconnect epoch, and this marker ends the
        // previous one, so the previous gap stops HERE rather than at its
        // own window. From `at` on, a packet that arrived came through
        // the connection this marker opened, and what the demuxer could
        // notice is what it re-acquired after THIS reset. The older gap
        // knows neither — `on_media` records only into the latest — so
        // left to reach past `at` it would never hold anything there and
        // would excuse what the newest gap holds to detection.
        //
        // Nothing legitimately lost is given up. Below `at` the older gap
        // keeps its whole reach. From `at` on the new gap covers at least
        // as far as the old one did (its window opens later), and it
        // excuses by the same rule: resolved approximately, anchored
        // before this marker, never anchored, or placed before the
        // demuxer's first media after it.
        //
        // `end` is exclusive because a marker is a packet COUNT: the
        // packet with ordinal `at` is the first of the new epoch.
        if let Some(g) = self.reconnect_gaps.last_mut() {
            g.end = g.end.min(at);
        }
        self.reconnect_gaps.push(ReconnectGap {
            from: self.last_media_at,
            marker: at,
            end: at
                .saturating_add(self.window)
                .saturating_add(self.approx_slack)
                .saturating_add(1),
            first_media: BTreeMap::new(),
        });
        // The reset demuxer holds no program until it reads a PAT and a
        // PMT again, and says so with a `ProgramMap` before it can report
        // a `PcrAnomaly`: it keeps a PCR baseline only for a PID a program
        // declares, and adopts a declaration in the same step that queues
        // the map.
        self.declared.clear();
        // It holds no PCR baseline either, so an anomaly from here on is
        // a jump between two PCRs of the NEW connection. A continuity gap
        // seen on the old one is not its cause.
        self.last_cc_jump_by_pid.clear();
    }

    /// The receiver's demuxer reported a PMT: `program_number`, whose PMT
    /// rides `pmt_pid`, carries its time base on `pcr_pid`. It replaces
    /// what it supersedes — see [`DeclaredPcrPids`]. Until one is fed no
    /// PID is declared, at the start of the capture and again after every
    /// reconnect, and every `PcrAnomaly` is a finding.
    pub fn on_program_map(&mut self, program_number: u16, pmt_pid: u16, pcr_pid: u16) {
        self.declared.declare(program_number, pmt_pid, pcr_pid);
    }

    /// Final verdict over a capture of `packets_total` receiver packets.
    ///
    /// An injection whose recovery window runs past the end of the capture
    /// is not judged for recovery, and an injection that never resolved is
    /// not judged at all — absent evidence is not evidence of a failure.
    ///
    /// Whether findings are judged strictly or with transport-loss
    /// excusal was fixed when this attribution was built
    /// ([`Attribution::strict`] / [`Attribution::lossy`]).
    #[must_use]
    pub fn finish(mut self, packets_total: u64) -> AttributionReport {
        // Everything already retired has been judged (`Attribution::judge`
        // via `prune`); this folds in whatever is still retained.
        for k in 0..self.inj.len() {
            let (inj, st) = (self.inj[k], self.st[k]);
            self.note_if_unheard(self.base + k, &st);
            let closed = st
                .resolved_at
                .is_some_and(|r| r.saturating_add(self.recovery_bound) <= packets_total);
            self.judge(&inj, &st, closed);
        }
        // A PCR anomaly still waiting for a gap when the capture ends is
        // charged, never dropped: the evidence that would excuse it can
        // no longer arrive.
        for q in std::mem::take(&mut self.pending_pcr) {
            self.charge_pcr_anomaly(q);
        }
        AttributionReport {
            injected: self.logged,
            detectable: self.detectable,
            resolved: self.resolved,
            unresolved: self.unresolved,
            events: self.events,
            attributed_events: self.attributed_events,
            attributed_nonconformant: self.attributed_nonconformant,
            attributed_discontinuities: self.attributed_discontinuities,
            attributed_events_by_pid: self.attributed_events_by_pid,
            attributed_events_unpinned: self.attributed_events_unpinned,
            unexplained_events: self.unexplained_samples,
            unexplained_resyncs: self.unexplained_resyncs,
            unexplained_discontinuities: self.unexplained_discontinuities,
            unexplained_nonconformant: self.unexplained_nonconformant,
            first_unexplained_nonconformant: self.first_unexplained_nc,
            first_unexplained_discontinuity: self.first_unexplained_disc,
            undetected: self.undetected_samples,
            undetected_count: self.undetected_count,
            unrecovered: self.unrecovered_samples,
            unrecovered_count: self.unrecovered_count,
            undetected_lost: self.undetected_lost,
            unrecovered_lost: self.unrecovered_lost,
            detected_by_demux: self.detected_by_demux,
            detected_by_reader_only: self.detected_by_reader_only,
            detected_by_reader_only_per_class: self.detected_by_reader_only_per_class,
            unexplained_transport_loss: self.unexplained_transport_loss,
            pcr_anomalies_excused: self.pcr_anomalies_excused,
            reconnects_seen: self.reconnects_seen,
            lost_in_reconnect_gap: self.lost_in_reconnect_gap,
            history_skipped: self.history_skipped,
            history_unheard: self.history_unheard,
            resyncs: self.resyncs,
            injected_fraction: self.logged as f64 / packets_total.max(1) as f64,
            attribution_window: self.window,
            recovery_bound: self.recovery_bound,
            rate_per_10k: self.rate_per_10k,
            min_gap: self.min_gap,
            classes: self.classes,
            approx_slack: self.approx_slack,
            anchors_rejected: self.anchors_rejected,
        }
    }
}
