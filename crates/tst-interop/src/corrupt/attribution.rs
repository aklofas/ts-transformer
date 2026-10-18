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
struct InjState {
    /// Receiver packet ordinal this injection landed at, once its PCR
    /// anchor has been seen.
    resolved_at: Option<u64>,
    detected: bool,
    recovered: bool,
    /// Resolved against a LATER base than the one logged (the logged one
    /// never arrived), so the position is approximate.
    approx: bool,
    /// Receiver ordinal of the PCR packet this injection resolved from.
    /// `None` for one that carries no anchor (logged before the stream's
    /// first PCR). Together with `approx` this is the arrival evidence a
    /// reconnect gap has to respect — see [`ReconnectGap::holds`].
    anchor_at: Option<u64>,
    /// Its anchor base fell more than `MAX_APPROX_TICKS` behind the first
    /// base the receiver saw afterwards, so it can never be placed. Kept
    /// distinct from "not resolved yet" so the scan cursors can move past
    /// it instead of stalling on it forever.
    stranded: bool,
    /// A `ContinuityJump` landed inside this injection's attribution
    /// window, on a PID this injection could itself have suffered a gap
    /// on ([`reaches`]). Evidence that packets went missing around here
    /// for a reason the corruption log cannot own: an impairment proxy's
    /// loss, an SRT/RIST buffer overrun, a reconnect after an outage. See
    /// [`Attribution::lossy`] for what it excuses.
    cc_jump_in_window: bool,
    /// Set when a NON-[`Signal::Resync`] signal detected this injection,
    /// i.e. when the receiver's own demuxer reported it rather than only
    /// the harness's raw reader. See
    /// [`AttributionReport::detected_by_reader_only`].
    detected_by_demux: bool,
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
    base: usize,
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
pub(super) fn ticks_ahead(a: u64, b: u64) -> u64 {
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

    fn state(&self, i: usize) -> &InjState {
        &self.st[i - self.base]
    }

    /// How the engine placed injection `i`: `(resolved_at, approx, stranded)`.
    /// Test-only window onto the coordinate rules for the property tests
    /// in `coord.rs`.
    #[cfg(test)]
    pub(super) fn placement(&self, i: usize) -> (Option<u64>, bool, bool) {
        let st = self.state(i);
        (st.resolved_at, st.approx, st.stranded)
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
    fn backward_reach(&self, i: usize) -> u64 {
        let budget = self
            .min_gap
            .saturating_sub(self.window.saturating_add(self.approx_slack))
            .saturating_sub(1);
        self.tracked(i).coord.since_pcr.min(budget)
    }

    /// First receiver ordinal injection `i`'s window covers, or `None`
    /// when it has no resolved position at all.
    fn window_lo(&self, i: usize) -> Option<u64> {
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
    fn window_contains(&self, i: usize, at: u64) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corrupt::*;

    /// A PAT/PMT PID carries no media, so "media on the injected PID"
    /// can never arrive there — `Sample`/`Metadata` events only ever name
    /// an elementary stream. Recovery from a PSI injection is therefore
    /// proved by media on ANY PID, or every psi_flip in a soak run would
    /// be reported unrecovered against a perfectly healthy stream.
    #[test]
    fn a_psi_injection_recovers_on_media_from_any_pid() {
        let mut psi_inj = inj(1000, 0, Class::PsiFlip, 0, true);
        psi_inj.psi = true;
        let mut a = Attribution::strict(vec![psi_inj], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5010, Some(0), Signal::PsiChecksum);
        a.on_media(5020, 0x1011); // an elementary stream, not pid 0
        let r = a.finish(10_000);
        assert!(r.unrecovered.is_empty(), "{:?}", r.unrecovered);

        // A MEDIA-PID injection still has to be answered on its own PID:
        // the relaxation is scoped to PSI, not a blanket "any event
        // anywhere counts".
        let mut a = Attribution::strict(vec![inj(1000, 0, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5010, Some(0x1011), Signal::ContinuityJump);
        a.on_media(5020, 0x1100);
        let r = a.finish(10_000);
        assert_eq!(r.unrecovered.len(), 1, "media on a different PID");
    }

    /// `Class::Header`'s PID-rewrite sub-kind moves the packet to
    /// `REWRITTEN_PID`, so whatever the receiver reports about it, it
    /// reports THERE. Run 3 (2026-09-17): ~50 events on 0x1FFE per leg
    /// that nothing could explain because `reaches` only knew the
    /// original PID.
    #[test]
    fn a_pid_rewrite_explains_a_signal_on_the_pid_it_moved_the_packet_to() {
        let mut moved = inj(1000, 10, Class::Header, 0x1011, true);
        moved.offsets = vec![1, 2];
        let mut a = Attribution::strict(vec![moved], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5015, Some(REWRITTEN_PID), Signal::OtherNonConformant);
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 1, "{r:?}");
        assert!(
            r.unexplained_events.is_empty(),
            "{:?}",
            r.unexplained_events
        );

        // Every other header shape stays confined to its own PID: a CC
        // rewrite ([3]) and — the PSI shape, where the tap only ever
        // destroys the sync byte ([0]) — must not reach 0x1FFE.
        for (offsets, psi) in [(vec![3usize], false), (vec![0usize], true)] {
            let mut other = inj(1000, 10, Class::Header, if psi { 0 } else { 0x1011 }, true);
            other.offsets = offsets.clone();
            other.psi = psi;
            let mut a = Attribution::strict(vec![other], &hdr());
            a.on_pcr(1000, 5000);
            a.on_signal(5015, Some(REWRITTEN_PID), Signal::OtherNonConformant);
            let r = a.finish(10_000);
            // [0] is framing-wide and reaches everything by design; [3] is not.
            let want_unexplained = u64::from(offsets != vec![0]);
            assert_eq!(
                r.unexplained_nonconformant, want_unexplained,
                "{offsets:?}: {r:?}"
            );
        }
    }

    /// A derived anomaly (a PTS that stepped backwards) is asked about,
    /// not fed in — and only a TRUNCATION answers for it. Asking must not
    /// move any of the counters an event would.
    #[test]
    fn only_a_truncation_explains_a_derived_anomaly() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Truncate, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000); // resolves to receiver ordinal 5010
        // The window opens at `r - since_pcr` (see `backward_reach`), not
        // at `r`: with `since_pcr = 10` the injected packet's true
        // receiver ordinal is somewhere in 5000..=5010, depending on how
        // much of that span the transport lost.
        assert!(!a.truncation_explains(4999), "before the window opens");
        assert!(a.truncation_explains(5000), "earliest the injection can be");
        assert!(a.truncation_explains(5010), "at the injection");
        assert!(
            a.truncation_explains(5010 + ATTRIBUTION_WINDOW),
            "last packet in the window"
        );
        assert!(
            !a.truncation_explains(5011 + ATTRIBUTION_WINDOW),
            "past the window"
        );
        let r = a.finish(10_000);
        assert_eq!(
            (r.events, r.attributed_events, r.attributed_discontinuities),
            (0, 0, 0),
            "asking is not an event"
        );

        // Every other class leaves the anomaly unexplained: truncation is
        // the only one that misaligns the byte stream, so it is the only
        // one whose window may contain a PTS that was never a timestamp.
        for class in Class::ALL.iter().filter(|&&c| c != Class::Truncate) {
            let mut a = Attribution::strict(vec![inj(1000, 10, *class, 0x1011, true)], &hdr());
            a.on_pcr(1000, 5000);
            assert!(
                !a.truncation_explains(5010),
                "{class:?} must not excuse a derived anomaly"
            );
        }
    }

    /// The skipped count rides in the report so a coverage check can add it
    /// back to what the receiver ingested.
    #[test]
    fn history_skipped_is_reported() {
        let mut a = Attribution::lossy(vec![inj(1_000, 2, Class::Drop, 0x1011, true)], &hdr());
        a.note_history_skipped(904);
        a.on_pcr(1_000, 10);
        a.on_signal(13, Some(0x1011), Signal::ContinuityJump);
        a.on_media(20, 0x1011);
        let r = a.finish(2_000);
        assert_eq!(r.history_skipped, 904);
        assert_eq!(r.injected, 1);
    }

    /// A restarted receiver keeps the newest history in the log at attach,
    /// but its first anchor is far past every one of them: they strand.
    /// They stay `unresolved`, and are also named as history it could
    /// never have heard, so coverage can judge only what it could.
    #[test]
    fn attach_history_stranded_before_the_first_anchor_is_unheard() {
        let live = 1_000 + 10 * MAX_APPROX_TICKS;
        let mut a = Attribution::lossy(
            vec![
                inj(1_000, 2, Class::Drop, 0x1011, true),
                inj(1_000, 40, Class::Drop, 0x1011, true),
                inj(1_000, 80, Class::Drop, 0x1011, true),
                inj(live, 2, Class::Drop, 0x1011, true),
            ],
            &hdr(),
        );
        a.on_pcr(live, 10);
        a.on_signal(13, Some(0x1011), Signal::ContinuityJump);
        a.on_media(20, 0x1011);
        let r = a.finish(2_000);
        assert_eq!(r.history_unheard, 3, "{r:?}");
        assert_eq!(r.resolved, 1);
        assert_eq!(r.unresolved, 3);
        assert_eq!(r.injected, 4);
    }

    /// Only the batch present at attach is history. An injection appended
    /// later that strands is a live one the receiver failed to place.
    #[test]
    fn appended_injections_that_strand_are_not_history() {
        let mut a = Attribution::lossy(vec![inj(400_000, 2, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(400_000, 10);
        a.on_signal(13, Some(0x1011), Signal::ContinuityJump);
        a.on_media(20, 0x1011);
        let mut anchorless = inj(0, 0, Class::Drop, 0x1011, true);
        anchorless.coord.pcr_base = None;
        a.append(vec![
            inj(
                400_000 - 10 * MAX_APPROX_TICKS,
                2,
                Class::Drop,
                0x1011,
                true,
            ),
            anchorless,
        ]);
        let r = a.finish(2_000);
        assert_eq!(r.unresolved, 2, "{r:?}");
        assert_eq!(r.history_unheard, 0, "{r:?}");
    }

    /// A full restart history retires through `prune` long before
    /// `finish`; the count must survive that.
    #[test]
    fn unheard_history_is_counted_when_it_is_pruned() {
        let live = 1_000 + 10 * MAX_APPROX_TICKS;
        let n = PRUNE_BATCH + 5;
        let mut batch: Vec<Injection> = (0..n)
            .map(|k| inj(1_000, k as u64, Class::Drop, 0x1011, true))
            .collect();
        batch.push(inj(live, 2, Class::Drop, 0x1011, true));
        let mut a = Attribution::lossy(batch, &hdr());
        a.on_pcr(live, 10);
        a.on_signal(13, Some(0x1011), Signal::ContinuityJump);
        a.on_media(20, 0x1011);
        assert!(a.base >= PRUNE_BATCH, "the history was not pruned");
        let r = a.finish(2_000);
        assert_eq!(r.history_unheard, n as u64, "{r:?}");
        assert_eq!(r.unresolved, n as u64);
        assert_eq!(r.resolved, 1);
    }

    /// An injection appended mid-capture is judged like any other once a
    /// later PCR places it — but one whose coordinate has NO PCR anchor
    /// arrives too late to be placed at all, and must be left unresolved
    /// rather than blamed on a receiver that never saw that part of the
    /// stream.
    #[test]
    fn appended_injections_resolve_forward_and_anchorless_ones_strand() {
        let hdr = LogHeader {
            tap_version: TAP_VERSION,
            seed: 1,
            rate_per_10k: 5,
            min_gap: 1000,
            classes: Class::ALL.to_vec(),
            attribution_window: ATTRIBUTION_WINDOW,
            recovery_bound: RECOVERY_BOUND,
            approx_slack: APPROX_SLACK,
        };
        let mk = |pcr_base: Option<u64>, since: u64| Injection {
            ordinal: 0,
            coord: Coord {
                pcr_base,
                since_pcr: since,
            },
            class: Class::Drop,
            pid: 0x1011,
            offsets: vec![],
            before: vec![],
            after: vec![],
            detectable: true,
            psi: false,
            pes_start: false,
        };

        let mut a = Attribution::strict(vec![mk(Some(100), 2)], &hdr);
        a.on_pcr(100, 10);
        a.on_signal(13, Some(0x1011), Signal::ContinuityJump);
        a.on_media(20, 0x1011);

        // Appended after the capture is under way: anchored at a base
        // still to come, and at none at all.
        a.append(vec![mk(Some(200), 3), mk(None, 0)]);
        a.on_pcr(200, 500);
        a.on_signal(504, Some(0x1011), Signal::ContinuityJump);
        a.on_media(510, 0x1011);

        let rep = a.finish(2000);
        assert_eq!(rep.injected, 3);
        assert_eq!(rep.resolved, 2, "both anchored injections placed");
        assert_eq!(rep.unresolved, 1, "the anchorless late arrival is not");
        assert_eq!(rep.attributed_events, 2);
        assert!(rep.undetected.is_empty(), "{rep:?}");
        assert!(
            rep.unexplained_events.is_empty(),
            "{:?}",
            rep.unexplained_events
        );
    }

    /// A zero-latency transport (UDP/TCP straight over loopback) hands the
    /// receiver a PCR packet before the sender has even logged the
    /// injection anchored at it: the tap logs per `send_bytes` message,
    /// so an injection in the message AFTER the one carrying its PCR is
    /// written only once that PCR is already on the receiver's side. The
    /// late line must still resolve against the base it names, not
    /// against whichever base the receiver sees next — that is a full PCR
    /// interval downstream, past the event the injection caused.
    #[test]
    fn an_injection_logged_after_its_anchor_pcr_arrived_still_resolves_exactly() {
        let mut a = Attribution::strict(Vec::new(), &hdr());
        a.on_pcr(100, 10);
        a.append(vec![inj(100, 2, Class::Drop, 0x1011, true)]);
        a.on_signal(13, Some(0x1011), Signal::ContinuityJump);
        a.on_media(20, 0x1011);
        // The next base, one 40 ms PCR interval and 80 packets later.
        a.on_pcr(100 + 3600, 90);
        let rep = a.finish(2000);
        assert_eq!(rep.resolved, 1);
        assert_eq!(rep.attributed_events, 1, "{rep:?}");
        assert!(rep.undetected.is_empty(), "{rep:?}");
        assert!(rep.unexplained_events.is_empty(), "{rep:?}");
    }

    /// The order in which the log tail and the wire deliver the same
    /// evidence must not change the verdict: an injection read before its
    /// anchor PCR and one read just after it are the same injection.
    #[test]
    fn appending_before_or_after_the_anchor_pcr_gives_the_same_report() {
        let i1 = || inj(100, 2, Class::Drop, 0x1011, true);
        let i2 = || inj(3700, 3, Class::Header, 0x1011, true);
        let run = |log_first: bool| {
            let mut a = Attribution::strict(Vec::new(), &hdr());
            for (i, base, at) in [(i1(), 100, 10), (i2(), 3700, 1500)] {
                if log_first {
                    a.append(vec![i]);
                    a.on_pcr(base, at);
                } else {
                    a.on_pcr(base, at);
                    a.append(vec![i]);
                }
                a.on_signal(at + 3, Some(0x1011), Signal::ContinuityJump);
                a.on_media(at + 10, 0x1011);
            }
            a.on_pcr(7300, 3000);
            a.finish(4000)
        };
        let (log_first, pcr_first) = (run(true), run(false));
        assert_eq!(log_first.attributed_events, 2, "{log_first:?}");
        assert!(log_first.undetected.is_empty(), "{log_first:?}");
        assert_eq!(log_first, pcr_first);
    }

    /// Before the first PCR there is nothing to strand against, so an
    /// anchorless injection appended then still resolves at ordinal 0 —
    /// the same treatment the constructors give it.
    #[test]
    fn an_anchorless_injection_appended_before_any_pcr_still_resolves() {
        let hdr = LogHeader {
            tap_version: TAP_VERSION,
            seed: 1,
            rate_per_10k: 5,
            min_gap: 1000,
            classes: Class::ALL.to_vec(),
            attribution_window: ATTRIBUTION_WINDOW,
            recovery_bound: RECOVERY_BOUND,
            approx_slack: APPROX_SLACK,
        };
        let mut a = Attribution::strict(Vec::new(), &hdr);
        a.append(vec![Injection {
            ordinal: 0,
            coord: Coord {
                pcr_base: None,
                since_pcr: 4,
            },
            class: Class::Drop,
            pid: 0x1011,
            offsets: vec![],
            before: vec![],
            after: vec![],
            detectable: true,
            psi: false,
            pes_start: false,
        }]);
        a.on_signal(6, Some(0x1011), Signal::ContinuityJump);
        a.on_media(10, 0x1011);
        let rep = a.finish(2000);
        assert_eq!(rep.resolved, 1);
        assert_eq!(rep.unresolved, 0);
        assert_eq!(rep.attributed_events, 1);
        assert!(rep.undetected.is_empty(), "{rep:?}");
    }

    // ---- Attribution ----

    fn hdr() -> LogHeader {
        LogHeader {
            tap_version: 1,
            seed: 1,
            rate_per_10k: 5,
            min_gap: 1000,
            classes: Class::ALL.to_vec(),
            attribution_window: ATTRIBUTION_WINDOW,
            recovery_bound: RECOVERY_BOUND,
            approx_slack: APPROX_SLACK,
        }
    }
    fn inj(coord_pcr: u64, since: u64, class: Class, pid: u16, detectable: bool) -> Injection {
        Injection {
            ordinal: 0,
            coord: Coord {
                pcr_base: Some(coord_pcr),
                since_pcr: since,
            },
            class,
            pid,
            offsets: vec![],
            before: vec![],
            after: vec![],
            detectable,
            psi: false,
            pes_start: false,
        }
    }

    /// Ruling A. Under transport loss an injection's own evidence can
    /// surface BEFORE `resolved_at`, because that ordinal is a receiver
    /// count plus a sender-side offset and the packets between the two
    /// are exactly what the network may have eaten. A forward-only window
    /// rejects it and reports the injection undetected for damage the
    /// receiver noticed perfectly well.
    ///
    /// Measured live: 2 of 429 detectable `garbage`/`header` injections
    /// on a 1-hour smoke run's rist leg, whose 100 ms PCR cadence gives the
    /// widest `since_pcr` spans.
    #[test]
    fn an_injections_own_event_may_surface_before_its_resolved_position() {
        // since_pcr 40: 25 of those 40 packets were lost in transit, so
        // the injected packet actually arrived at receiver ordinal 4975
        // while its coordinate resolves to 5000.
        let mut a = Attribution::strict(vec![inj(1000, 40, Class::Garbage, 0x1011, true)], &hdr());
        a.on_pcr(1000, 4960);
        a.on_signal(4975, None, Signal::Resync);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);

        assert_eq!(r.attributed_events, 1, "{r:?}");
        assert_eq!(r.undetected_count, 0, "{:?}", r.undetected);
        assert_eq!(r.unexplained_total(), 0, "{:?}", r.unexplained_events);
    }

    /// The backward half is bounded by the injection's OWN `since_pcr`,
    /// not by the whole window: an event further back than the anchor
    /// span can reach is still unattributable.
    #[test]
    fn the_backward_reach_stops_at_the_anchor_span() {
        let mut a = Attribution::strict(vec![inj(1000, 40, Class::Garbage, 0x1011, true)], &hdr());
        a.on_pcr(1000, 4960); // resolves to 5000, window opens at 4960
        a.on_signal(4959, None, Signal::Resync);
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 0, "{r:?}");
        assert_eq!(r.unexplained_resyncs, 1, "{r:?}");
    }

    /// The clamp that keeps windows disjoint. Two injections one
    /// `min_gap` apart must not both cover one event, or `hit` would hand
    /// it to the newer and rob the older of its detection — the exact
    /// ambiguity `CorruptConfig::validate`'s `min_gap` floor exists to
    /// prevent.
    ///
    /// `since_pcr` really does reach this far: measured at 302 packets on
    /// a 40 ms-PCR profile and 393 on a 100 ms one over a 1-hour smoke
    /// run, against the ~130 a one-PCR-interval model predicts. So the
    /// bound has to come from `min_gap`, not from a cadence assumption.
    #[test]
    fn a_wide_anchor_span_cannot_reach_into_its_predecessors_window() {
        let h = hdr();
        let budget = h.min_gap - (h.attribution_window + APPROX_SLACK) - 1;

        // The worst case for disjointness, built deliberately: the
        // PREDECESSOR resolves APPROXIMATELY, which widens its forward
        // edge by APPROX_SLACK, and the SUCCESSOR carries a `since_pcr`
        // wider than the budget, which is what the clamp has to cut back.
        // Anchoring the first at a base the receiver never saw (1000)
        // and delivering a LATER one (1090) is what makes it approx.
        let wide = budget + 200;
        let mut a = Attribution::strict(
            vec![
                inj(1000, 10, Class::Header, 0x1011, true),
                inj(2000, wide, Class::Header, 0x1011, true),
            ],
            &h,
        );
        // The predecessor's own base (1000) never arrives; the next one
        // does, so it resolves approximately to 5010 with a forward edge
        // widened by APPROX_SLACK.
        a.on_pcr(1090, 5000);
        // The successor resolves exactly, and — as two real consecutive
        // injections always are — exactly `min_gap` further down the
        // stream: 5010 + 1000 = 6010.
        a.on_pcr(2000, 6010 - wide);
        assert!(
            a.state(0).approx,
            "the predecessor must resolve approximately"
        );
        assert!(!a.state(1).approx, "and the successor exactly");
        assert_eq!(a.state(0).resolved_at, Some(5010));
        assert_eq!(a.state(1).resolved_at, Some(6010));

        // The clamp bit at all: the declared span really is wider than
        // the reach the config can fund.
        let reach = a.backward_reach(1);
        assert!(reach < wide, "the wide span must be clamped, got {reach}");

        // Both edges of `window_contains` are inclusive, so the budget
        // has to leave one packet between a widened forward edge and the
        // next window's lower edge. Sweep every ordinal either window can
        // touch and require that none is covered twice.
        let (lo0, hi1) = (
            a.window_lo(0).unwrap(),
            a.state(1).resolved_at.unwrap() + h.attribution_window,
        );
        for at in lo0..=hi1 {
            let covered = (0..2).filter(|&i| a.window_contains(i, at)).count();
            assert!(covered <= 1, "packet {at} covered by {covered} windows");
        }
        // …and the sweep is not vacuous: the two windows really do come
        // within a packet or two of each other.
        let gap = a.window_lo(1).unwrap()
            - (a.state(0).resolved_at.unwrap() + h.attribution_window + APPROX_SLACK);
        assert!(
            gap <= 2,
            "windows must be adjacent for this to test anything, gap {gap}"
        );
    }

    /// The same disjointness at AU scale 4, where every number in the
    /// header is 4× — including the slack an approximate resolution
    /// widens by. An engine that kept the scale-1 slack would fund a
    /// backward reach 384 packets too generous and let a successor's
    /// window overlap its predecessor's widened edge.
    #[test]
    fn backward_reach_is_clamped_with_the_headers_scaled_slack() {
        let mut h = hdr();
        h.min_gap = 4000;
        h.attribution_window = 2000;
        h.recovery_bound = 2400;
        h.approx_slack = 512;
        let budget = h.min_gap - (h.attribution_window + h.approx_slack) - 1;
        let wide = budget + 200;
        let mut a = Attribution::strict(
            vec![
                inj(1000, 10, Class::Header, 0x1011, true),
                inj(2000, wide, Class::Header, 0x1011, true),
            ],
            &h,
        );
        a.on_pcr(1090, 5000);
        a.on_pcr(2000, 5010 + h.min_gap - wide);
        assert!(a.state(0).approx);
        assert_eq!(a.state(1).resolved_at, Some(5010 + h.min_gap));
        assert_eq!(
            a.backward_reach(1),
            budget,
            "the clamp must use the header's slack, not the scale-1 constant"
        );
        let (lo0, hi1) = (
            a.window_lo(0).unwrap(),
            a.state(1).resolved_at.unwrap() + h.attribution_window,
        );
        for at in lo0..=hi1 {
            let covered = (0..2).filter(|&i| a.window_contains(i, at)).count();
            assert!(covered <= 1, "packet {at} covered by {covered} windows");
        }
        let gap = a.window_lo(1).unwrap()
            - (a.state(0).resolved_at.unwrap() + h.attribution_window + h.approx_slack);
        assert!(gap <= 2, "windows must be adjacent, gap {gap}");
    }

    /// Ruling B. A continuity jump inside an injection's window, on the
    /// PID it damaged, excuses that injection when its class does NOT
    /// expect a jump: there the jump means the datagram carrying the
    /// injection was lost in transit, so the damage never reached the
    /// receiver to be noticed.
    ///
    /// The excusal survives; the ATTRIBUTION does not. A signal
    /// of a class the injection cannot cause is no longer explained by it
    /// (`can_explain`), so the jump is unexplained — transport loss under
    /// lossy judgement, a discontinuity finding under strict — while
    /// still marking the injection's packet as plausibly missing.
    #[test]
    fn a_cc_jump_a_class_does_not_expect_stays_unexplained_but_still_excuses() {
        let build = |lossy: bool| {
            let log = vec![inj(1000, 10, Class::PsiFlip, 0, true)];
            let mut a = if lossy {
                Attribution::lossy(log, &hdr())
            } else {
                Attribution::strict(log, &hdr())
            };
            a.on_pcr(1000, 5000);
            // Its own jump, inside its own window, attributed to it — and
            // NOT a signal `expects` accepts for a psi_flip.
            a.on_signal(5100, Some(0), Signal::ContinuityJump);
            a.on_media(5200, 0x1011);
            a.finish(10_000)
        };

        let lossy = build(true);
        assert_eq!(
            lossy.attributed_events, 0,
            "class-incompatible: not explained"
        );
        assert_eq!(lossy.unexplained_transport_loss, 1, "{lossy:?}");
        assert_eq!(lossy.undetected_lost, 1, "{lossy:?}");
        assert_eq!(lossy.undetected_count, 0, "{:?}", lossy.undetected);

        // Strict has no transport to blame, so the finding stands.
        let strict = build(false);
        assert_eq!(strict.undetected_count, 1, "{strict:?}");
        assert_eq!(strict.undetected_lost, 0, "{strict:?}");
        assert_eq!(strict.unexplained_discontinuities, 1, "{strict:?}");
    }

    /// The other half of Ruling B, and the reason the blanket exclusion
    /// existed: for a class whose expected signals INCLUDE a continuity
    /// jump, its own jump is the detection and must never also excuse it.
    #[test]
    fn an_attributed_cc_jump_still_detects_a_class_that_expects_one() {
        for class in [Class::Drop, Class::Header, Class::Truncate, Class::Garbage] {
            let mut a = Attribution::lossy(vec![inj(1000, 10, class, 0x1011, true)], &hdr());
            a.on_pcr(1000, 5000);
            a.on_signal(5100, Some(0x1011), Signal::ContinuityJump);
            a.on_media(5200, 0x1011);
            let r = a.finish(10_000);
            assert_eq!(r.undetected_count, 0, "{class:?}: {r:?}");
            assert_eq!(
                r.undetected_lost, 0,
                "{class:?}: excused, not detected: {r:?}"
            );
        }
    }

    /// C2, the regression this exists for: on a lossy multi-day run the
    /// excused continuity jumps must not be able to crowd an unexplained
    /// RESYNC out of the evidence.
    ///
    /// A resync is the one signal with no second detection path — it is
    /// not a `DemuxEvent`, and resync mode is exactly what suppresses the
    /// `rawts_sync_loss` failure that would otherwise catch it — so if
    /// the sample cap swallows it, `corruption_attributed` passes a run
    /// whose receiver lost packet sync for reasons nothing explains.
    #[test]
    fn excused_jumps_never_crowd_out_an_unexplained_resync() {
        let mut a = Attribution::lossy(vec![inj(1000, 10, Class::Header, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        // Comfortably more excusable jumps than the sample cap, none of
        // them inside any injection's window.
        for k in 0..(MAX_SAMPLES as u64 * 4) {
            a.on_signal(20_000 + k, Some(0x1011), Signal::ContinuityJump);
        }
        a.on_signal(90_000, None, Signal::Resync);
        let r = a.finish(100_000);

        assert_eq!(r.unexplained_transport_loss, MAX_SAMPLES as u64 * 4);
        assert_eq!(r.unexplained_resyncs, 1);
        assert_eq!(
            r.unexplained_total(),
            1,
            "the resync is the only corruption evidence here: {r:?}"
        );
        assert!(
            r.unexplained_events.iter().any(|e| e.contains("Resync")),
            "and it must be quotable in the failure: {:?}",
            r.unexplained_events
        );
    }

    /// A continuity jump on a PID an injection never touched is not that
    /// injection's evidence. Today `hit` places by
    /// window position alone, so the foreign jump is "attributed" and
    /// the `Drop` counts as detected.
    #[test]
    fn attribution_rejects_unrelated_pid() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0x1031), Signal::ContinuityJump);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 0, "{r:?}");
        assert_eq!(r.undetected_count, 1, "{r:?}");
        assert_eq!(r.unexplained_discontinuities, 1, "{r:?}");
    }

    /// PR review finding: `reaches` treated `Signal::PsiChecksum` as
    /// PID-unconstrained for ANY injection (its own doc claimed "a PSI
    /// checksum failure is the PSI PID's own", but the code never
    /// checked that `inj` was itself PSI-related), so a same-window
    /// `Drop` on an unrelated media PID could "explain" a PAT/PMT
    /// checksum failure it never came near — `Class::Drop`'s class check
    /// is a vacuous `_ => true` for `PsiChecksum`, so only the PID rule
    /// can be doing the rejecting here.
    #[test]
    fn attribution_rejects_a_psi_checksum_signal_from_a_non_psi_injection() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0), Signal::PsiChecksum);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 0, "{r:?}");
        assert_eq!(r.undetected_count, 1, "{r:?}");
        assert_eq!(r.unexplained_nonconformant, 1, "{r:?}");

        // Positive control: an injection that DID touch a PSI table still
        // gets the "PSI checksum failure is the PSI PID's own" escape.
        let mut psi_inj = inj(1000, 10, Class::PsiFlip, 0, true);
        psi_inj.psi = true;
        let mut a = Attribution::strict(vec![psi_inj], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0), Signal::PsiChecksum);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 1, "{r:?}");
        assert_eq!(r.undetected_count, 0, "{r:?}");
    }

    /// A PMT truncated inside its section reaches the demuxer as a section
    /// whose CRC fails: `PsiChecksum` is the receiver noticing, and it must
    /// count for a framing-class injection whose own PID is a PSI PID (six
    /// RC-soak RIST events were "unexplained" for exactly this).
    #[test]
    fn a_framing_injection_on_a_psi_pid_is_noticed_by_a_psi_checksum() {
        let mut pmt_cut = inj(1_000, 2, Class::Truncate, 0x1000, true);
        pmt_cut.psi = true;
        let mut a = Attribution::lossy(vec![pmt_cut], &hdr());
        a.on_pcr(1_000, 10);
        a.on_signal(16, Some(0x1000), Signal::PsiChecksum);
        a.on_media(30, 0x1011);
        let r = a.finish(2_000);
        assert_eq!(r.attributed_events, 1);
        assert_eq!(r.unexplained_total(), 0);
        assert_eq!(r.undetected_total(), 0);

        // The same signal on a NON-PSI framing injection is still not its
        // expected effect (nothing in a media packet has a CRC to fail).
        let mut a = Attribution::lossy(vec![inj(1_000, 2, Class::Truncate, 0x1011, true)], &hdr());
        a.on_pcr(1_000, 10);
        a.on_signal(16, Some(0x1011), Signal::PsiChecksum);
        a.on_media(30, 0x1011);
        let r = a.finish(2_000);
        assert_eq!(r.attributed_events, 0);
    }

    /// Same PID, wrong class: a `Drop` can only surface as a continuity
    /// jump, so a malformed-PES report on its own PID inside its window
    /// is somebody else's problem, not its detection.
    #[test]
    fn attribution_rejects_a_class_incompatible_signal_on_the_same_pid() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0x1011), Signal::MalformedPes);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 0, "{r:?}");
        assert_eq!(r.undetected_count, 1, "{r:?}");
        assert_eq!(r.unexplained_nonconformant, 1, "{r:?}");
    }

    /// The loss-excusal follows the same rule: a jump on a foreign PID
    /// says nothing about whether THIS injection's packet was lost, so
    /// under lossy judgement it neither detects nor excuses a PSI flip
    /// (whose packet would have to be missing from PID 0 to be excused).
    #[test]
    fn loss_excusal_needs_a_jump_on_the_injections_own_pid() {
        let mut a = Attribution::lossy(vec![inj(1000, 10, Class::PsiFlip, 0, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0x1031), Signal::ContinuityJump);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            r.undetected_lost, 0,
            "a foreign jump must not excuse: {r:?}"
        );
        assert_eq!(r.undetected_count, 1, "{r:?}");
        assert_eq!(r.unexplained_transport_loss, 1, "{r:?}");

        // Positive control: the jump on the PSI PID itself does excuse.
        let mut a = Attribution::lossy(vec![inj(1000, 10, Class::PsiFlip, 0, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0), Signal::ContinuityJump);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(r.undetected_lost, 1, "{r:?}");
        assert_eq!(r.undetected_count, 0, "{r:?}");
    }

    /// Framing-wide classes reach every PID: a truncation, a garbage
    /// run or a destroyed sync byte breaks the multiplex, so a jump on
    /// any PID inside the window is theirs. A `Header` that rewrote the
    /// CC (offset 3) is confined to its own PID.
    #[test]
    fn framing_wide_injections_explain_signals_on_any_pid() {
        for class in [Class::Truncate, Class::Garbage] {
            let mut a = Attribution::strict(vec![inj(1000, 10, class, 0x1011, true)], &hdr());
            a.on_pcr(1000, 5000);
            a.on_signal(5020, Some(0x1031), Signal::ContinuityJump);
            a.on_media(5100, 0x1031);
            let r = a.finish(10_000);
            assert_eq!(r.attributed_events, 1, "{class:?}: {r:?}");
            assert_eq!(r.undetected_count, 0, "{class:?}: {r:?}");
        }
        let sync_byte = |offsets: Vec<usize>| {
            let mut i = inj(1000, 10, Class::Header, 0x1011, true);
            i.offsets = offsets;
            let mut a = Attribution::strict(vec![i], &hdr());
            a.on_pcr(1000, 5000);
            a.on_signal(5020, Some(0x1031), Signal::ContinuityJump);
            a.on_media(5100, 0x1031);
            a.finish(10_000)
        };
        assert_eq!(
            sync_byte(vec![0]).attributed_events,
            1,
            "sync-byte header damage is framing-wide"
        );
        assert_eq!(
            sync_byte(vec![3]).attributed_events,
            0,
            "a CC rewrite is PID-local"
        );
    }

    /// A resync is the HARNESS's raw reader noticing, not the receiver.
    /// A garbage run detected only by a resync is counted
    /// apart from a drop the demuxer itself reported.
    #[test]
    fn detection_credit_splits_reader_resyncs_from_demux_events() {
        let mut a = Attribution::strict(
            vec![
                inj(1000, 10, Class::Garbage, 0x1011, true),
                inj(1000, 2010, Class::Drop, 0x1011, true),
                inj(1000, 4010, Class::Garbage, 0x1011, true),
            ],
            &hdr(),
        );
        a.on_pcr(1000, 5000);
        a.on_signal(5015, None, Signal::Resync); // garbage #1: reader only
        a.on_media(5100, 0x1011);
        a.on_signal(7015, Some(0x1011), Signal::ContinuityJump); // drop: demux
        a.on_media(7100, 0x1011);
        a.on_signal(9012, None, Signal::Resync); // garbage #2: reader …
        a.on_signal(9015, Some(0x1011), Signal::ContinuityJump); // … then demux too
        a.on_media(9100, 0x1011);
        let r = a.finish(20_000);
        assert_eq!(r.undetected_count, 0, "{r:?}");
        assert_eq!(r.detected_by_demux, 2, "{r:?}");
        assert_eq!(r.detected_by_reader_only, 1, "{r:?}");
        assert_eq!(
            r.detected_by_reader_only_per_class.get("garbage"),
            Some(&1),
            "{r:?}"
        );
    }

    /// The uncapped counters keep counting after the sample list stops.
    /// A verdict reading `unexplained_events.len()` would report 64 for
    /// any run above the cap, which is how a badly-broken run reads as a
    /// mildly-broken one.
    #[test]
    fn unexplained_counts_are_uncapped_while_the_sample_list_is_capped() {
        let mut a = Attribution::strict(vec![], &hdr());
        let n = MAX_SAMPLES as u64 * 10;
        for k in 0..n {
            a.on_signal(1000 + k, Some(0x1011), Signal::PsiChecksum);
        }
        let r = a.finish(100_000);
        assert_eq!(r.unexplained_events.len(), MAX_SAMPLES);
        assert_eq!(r.unexplained_nonconformant, n);
        assert_eq!(r.unexplained_total(), n);
    }

    /// The same cap/count split for the per-injection verdicts (I4):
    /// `undetected`/`unrecovered` are samples, `*_count` are the truth.
    #[test]
    fn undetected_and_unrecovered_lists_are_capped_and_their_counts_are_not() {
        let n = MAX_SAMPLES as u64 * 3;
        // Detectable injections, spaced past every window, none of which
        // ever produces an event or any media afterwards.
        let log: Vec<Injection> = (0..n)
            .map(|k| inj(1000, 10 + k * 2000, Class::Drop, 0x1011, true))
            .collect();
        let mut a = Attribution::strict(log, &hdr());
        a.on_pcr(1000, 0);
        // Walk the cursors past every window so the retirement path does
        // the counting, not just the final sweep.
        a.on_media(10 + n * 2000 + 10_000, 0x2222);
        let r = a.finish(10 + n * 2000 + 20_000);

        assert_eq!(r.undetected_count, n);
        assert_eq!(r.unrecovered_count, n);
        assert_eq!(r.undetected.len(), MAX_SAMPLES);
        assert_eq!(r.unrecovered.len(), MAX_SAMPLES);
    }

    /// C1, the memory half: the engine must stop holding the log.
    ///
    /// A 72-hour soak logs a quarter of a million injections, each
    /// carrying `before`/`after` byte images, against a receive process
    /// whose own `rss_slope_*_recv` verdict gates at 200 KiB/h. Retained
    /// state has to be bounded by the window the cursors are actually
    /// working in, not by the length of the run.
    #[test]
    fn a_long_run_retires_injections_instead_of_accumulating_them() {
        let n = 20_000u64;
        let mut a = Attribution::strict(vec![], &hdr());
        let mut peak = 0usize;
        for k in 0..n {
            let at = 10 + k * 2000;
            a.append(vec![inj(1000, at, Class::Drop, 0x1011, true)]);
            // A live receiver feeds every PCR it decodes, which is what
            // turns a logged coordinate into a receiver position — and
            // therefore what lets the resolution cursor move on.
            a.on_pcr(1000, 0);
            // One event per injection, inside its own window, which is
            // what walks the scan cursors forward.
            a.on_signal(at + 5, Some(0x1011), Signal::ContinuityJump);
            a.on_media(at + 10, 0x1011);
            peak = peak.max(a.retained());
        }
        assert!(
            peak <= PRUNE_BATCH + 16,
            "retained {peak} injections at peak against a {PRUNE_BATCH}-injection prune batch"
        );

        // …and the verdict is unchanged by the pruning: every one of them
        // was detected and recovered.
        let r = a.finish(10 + n * 2000 + 10_000);
        assert_eq!(r.injected, n);
        assert_eq!(r.resolved, n);
        assert_eq!(r.undetected_count, 0, "{:?}", r.undetected);
        assert_eq!(r.unrecovered_count, 0, "{:?}", r.unrecovered);
    }

    /// Pruning must not change a verdict. The same event stream is judged
    /// twice — once long enough to retire most of the log, once short
    /// enough that nothing retires — and the two reports must agree on
    /// every counter.
    #[test]
    fn retiring_injections_does_not_change_the_verdict() {
        let judge = |n: u64| {
            let log: Vec<Injection> = (0..n)
                .map(|k| inj(1000, 10 + k * 2000, Class::Drop, 0x1011, true))
                .collect();
            let mut a = Attribution::strict(log, &hdr());
            a.on_pcr(1000, 0);
            for k in 0..n {
                let at = 10 + k * 2000;
                // Every third injection is left with no event at all, so
                // both the passing and the failing paths are exercised.
                if k % 3 != 0 {
                    a.on_signal(at + 5, Some(0x1011), Signal::ContinuityJump);
                }
                a.on_media(at + 10, 0x1011);
            }
            a.finish(10 + n * 2000 + 10_000)
        };
        // Below the batch nothing is ever retired; well above it, most of
        // the log is. Both must produce the arithmetically exact verdict.
        for n in [100u64, PRUNE_BATCH as u64 * 3] {
            let r = judge(n);
            assert_eq!(r.injected, n, "n={n}: {r:?}");
            assert_eq!(r.resolved, n, "n={n}: {r:?}");
            assert_eq!(r.detectable, n, "n={n}: {r:?}");
            // Exactly the every-third injections that got no event.
            assert_eq!(r.undetected_count, n.div_ceil(3), "n={n}: {r:?}");
            assert_eq!(r.unrecovered_count, 0, "n={n}: {r:?}");
        }
    }

    #[test]
    fn attribution_explains_events_inside_the_window_and_flags_the_rest() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Header, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000); // injection resolves to receiver ordinal 5010
        a.on_signal(5020, Some(0x1011), Signal::ContinuityJump); // explained
        a.on_media(5100, 0x1011); // recovered
        a.on_signal(9000, Some(0x1011), Signal::ContinuityJump); // unexplained
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 1);
        assert_eq!(r.attributed_discontinuities, 1);
        assert_eq!(r.unexplained_events.len(), 1);
        assert!(r.undetected.is_empty() && r.unrecovered.is_empty());
        assert_eq!(r.resolved, 1);
    }

    /// Transport-loss excusal, the Lossy half. An unexplained
    /// `ContinuityJump` says packets went missing, which on a live
    /// impaired link is the link doing its job — it is already counted in
    /// the verifier's own `discontinuities`, and it is not corruption
    /// evidence. `Strict` keeps failing on the identical input, because a
    /// file has no transport to lose anything.
    #[test]
    fn lossy_excuses_an_unexplained_continuity_jump_and_strict_does_not() {
        let build = |lossy: bool| {
            let log = vec![inj(1000, 10, Class::Header, 0x1011, true)];
            let mut a = if lossy {
                Attribution::lossy(log, &hdr())
            } else {
                Attribution::strict(log, &hdr())
            };
            a.on_pcr(1000, 5000); // resolves to receiver ordinal 5010
            a.on_signal(5020, Some(0x1011), Signal::ContinuityJump); // explained
            a.on_media(5100, 0x1011);
            // Far outside every window: nothing in the log explains it.
            a.on_signal(9000, Some(0x1011), Signal::ContinuityJump);
            a
        };

        let lossy = build(true).finish(10_000);
        assert!(
            lossy.unexplained_events.is_empty(),
            "a transport gap is not corruption evidence: {:?}",
            lossy.unexplained_events
        );
        assert_eq!(lossy.unexplained_transport_loss, 1);
        // The raw tallies still reconcile: the event was counted, just not
        // held against the tap.
        assert_eq!(lossy.events, 2);
        assert_eq!(lossy.attributed_events, 1);

        let strict = build(false).finish(10_000);
        assert_eq!(strict.unexplained_events.len(), 1);
        assert_eq!(strict.unexplained_discontinuities, 1);
        assert_eq!(strict.unexplained_transport_loss, 0);
    }

    /// The other families are NOT excused: a dropped packet cannot forge a
    /// bad PSI CRC or a malformed PES header, so an unexplained one of
    /// those still fails in Lossy exactly as in Strict.
    #[test]
    fn lossy_still_fails_an_unexplained_nonconformance_or_resync() {
        for sig in [
            Signal::PsiChecksum,
            Signal::MalformedPes,
            Signal::OtherNonConformant,
            Signal::Resync,
        ] {
            let mut a =
                Attribution::lossy(vec![inj(1000, 10, Class::Header, 0x1011, true)], &hdr());
            a.on_pcr(1000, 5000);
            a.on_signal(9000, Some(0x1011), sig);
            let r = a.finish(10_000);
            assert_eq!(
                r.unexplained_events.len(),
                1,
                "{sig:?} must survive transport-loss excusal"
            );
            assert_eq!(r.unexplained_transport_loss, 0, "{sig:?}");
        }
    }

    /// A forward PCR jump of two seconds, the shape a span of lost packets
    /// leaves behind.
    const PCR_FORWARD: Signal = Signal::PcrAnomaly { delta: 54_000_000 };

    /// A lossy or strict attribution over `log` whose receiver reported
    /// one program with its time base on the video PID.
    fn with_video_pcr(lossy: bool, log: Vec<Injection>) -> Attribution {
        let mut a = if lossy {
            Attribution::lossy(log, &hdr())
        } else {
            Attribution::strict(log, &hdr())
        };
        a.on_program_map(1, 0x1000, 0x1011);
        a
    }

    /// A forward PCR jump that coincides with a continuity gap on its own
    /// PID is that gap's timestamp signature — packets went missing for
    /// longer than the anomaly threshold — not corruption. Under lossy
    /// judgement it is transport loss; strict has no transport and charges
    /// it. Run 3 (2026-09-17): ~17 such jumps on the srt video PID, 1–3 per
    /// outage window, every one beside an excused continuity jump.
    #[test]
    fn a_pcr_jump_beside_a_continuity_gap_is_transport_loss_only_under_lossy() {
        // Far-away, never-resolved injection so the log is non-empty.
        let bystander = || vec![inj(1000, 0, Class::Drop, 0x1011, true)];
        for (lossy, want_excused) in [(true, 1u64), (false, 0u64)] {
            let mut a = with_video_pcr(lossy, bystander());
            // Gap first, jump inside the window after it.
            a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
            a.on_signal(5000 + ATTRIBUTION_WINDOW, Some(0x1011), PCR_FORWARD);
            let r = a.finish(10_000);
            assert_eq!(
                r.pcr_anomalies_excused, want_excused,
                "lossy={lossy}: {r:?}"
            );
            assert_eq!(
                r.unexplained_nonconformant,
                1 - want_excused,
                "lossy={lossy}: {r:?}"
            );
        }

        // Same packet, jump QUEUED FIRST (tst_core's check_pcr runs before
        // check_continuity): the gap evidence arrives one event late.
        let mut a = with_video_pcr(true, bystander());
        a.on_signal(5000, Some(0x1011), PCR_FORWARD);
        a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (1, 0),
            "{r:?}"
        );

        // A gap on a DIFFERENT PID is not this PID's signature.
        let mut a = with_video_pcr(true, bystander());
        a.on_signal(5000, Some(0x1100), Signal::ContinuityJump);
        a.on_signal(5010, Some(0x1011), PCR_FORWARD);
        assert_eq!(a.finish(10_000).unexplained_nonconformant, 1);

        // Beyond the window, before or after: a finding.
        let mut a = with_video_pcr(true, bystander());
        a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
        a.on_signal(5001 + ATTRIBUTION_WINDOW, Some(0x1011), PCR_FORWARD);
        assert_eq!(a.finish(10_000).unexplained_nonconformant, 1);
        let mut a = with_video_pcr(true, bystander());
        a.on_signal(5000, Some(0x1011), PCR_FORWARD);
        a.on_signal(
            5001 + ATTRIBUTION_WINDOW,
            Some(0x1011),
            Signal::ContinuityJump,
        );
        let r = a.finish(10_000);
        assert_eq!(r.unexplained_nonconformant, 1, "{r:?}");
        assert!(
            r.unexplained_events
                .iter()
                .any(|e| e.starts_with("PcrAnomaly { delta: 54000000 } on pid Some(4113)")),
            "a charged pending anomaly keeps its sample text: {:?}",
            r.unexplained_events
        );

        // A pending anomaly at the end of the capture is charged by finish,
        // never dropped.
        let mut a = with_video_pcr(true, bystander());
        a.on_signal(9990, Some(0x1011), PCR_FORWARD);
        let r = a.finish(10_000);
        assert_eq!((r.events, r.unexplained_nonconformant), (1, 1), "{r:?}");
    }

    /// The shape run 3 measured, 1–3 anomalies per outage window: every
    /// anomaly beside a gap of its own, so every one is excused. One gap,
    /// one anomaly is a bound on what ONE loss explains, not on the run.
    #[test]
    fn each_pcr_jump_beside_its_own_gap_is_excused() {
        let mut a = with_video_pcr(true, vec![inj(1000, 0, Class::Drop, 0x1011, true)]);
        for at in [5000, 5040, 5090] {
            // The order tst_core emits them in, on one packet.
            a.on_signal(at, Some(0x1011), PCR_FORWARD);
            a.on_signal(at, Some(0x1011), Signal::ContinuityJump);
        }
        // And the other order: the gap a few packets before its PCR.
        a.on_signal(5200, Some(0x1011), Signal::ContinuityJump);
        a.on_signal(5205, Some(0x1011), PCR_FORWARD);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (4, 0),
            "{r:?}"
        );
    }

    /// One continuity gap is one span of lost packets. The first PCR after
    /// it carries the whole jump and the demuxer re-bases on it, so the gap
    /// is the cause of that one anomaly and of no later one: a further
    /// anomaly on the PID, with no further gap, is a clock misbehaving on
    /// packets that all arrived.
    #[test]
    fn one_gap_excuses_at_most_one_pcr_anomaly() {
        let mut a = with_video_pcr(true, vec![inj(1000, 0, Class::Drop, 0x1011, true)]);
        a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
        a.on_signal(5010, Some(0x1011), PCR_FORWARD);
        a.on_signal(5020, Some(0x1011), PCR_FORWARD);
        a.on_signal(5030, Some(0x1011), PCR_FORWARD);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (1, 2),
            "{r:?}"
        );

        // The same bound when the anomaly arrives first and the gap on its
        // packet claims it: the gap is spent, and the next anomaly is not
        // excused against it.
        let mut a = with_video_pcr(true, vec![inj(1000, 0, Class::Drop, 0x1011, true)]);
        a.on_signal(5000, Some(0x1011), PCR_FORWARD);
        a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
        a.on_signal(5010, Some(0x1011), PCR_FORWARD);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (1, 1),
            "{r:?}"
        );
    }

    /// Packets that go missing take their share of the timeline with
    /// them, so loss moves the clock FORWARD. A PCR behind its predecessor
    /// is never a gap's signature, however close the gap: before it, or on
    /// the same packet.
    #[test]
    fn a_backward_pcr_jump_beside_a_gap_is_a_finding() {
        let back = Signal::PcrAnomaly { delta: -27_000_000 };
        let mut a = with_video_pcr(true, vec![inj(1000, 0, Class::Drop, 0x1011, true)]);
        a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
        a.on_signal(5010, Some(0x1011), back);
        a.on_signal(6000, Some(0x1011), back);
        a.on_signal(6000, Some(0x1011), Signal::ContinuityJump);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (0, 2),
            "{r:?}"
        );
    }

    /// The demuxer keeps a PCR timeline only for a PID some PMT declares
    /// as `PCR_PID`, so it can never honestly report a `PcrAnomaly` on
    /// `REWRITTEN_PID`. A PID rewrite explains a packet APPEARING there —
    /// a non-conformance of that one packet, say — not a timeline being
    /// kept for it, so it must not be able to claim the event in either
    /// tier.
    #[test]
    fn a_pcr_anomaly_on_a_pid_no_pmt_declares_is_never_attributed() {
        let mut moved = inj(1000, 10, Class::Header, 0x1011, true);
        moved.offsets = vec![1, 2];
        for lossy in [false, true] {
            let mut a = with_video_pcr(lossy, vec![moved.clone()]);
            a.on_pcr(1000, 5000); // resolves at 5010
            a.on_signal(5010, Some(REWRITTEN_PID), PCR_FORWARD);
            a.on_media(5100, 0x1011);
            let r = a.finish(10_000);
            assert_eq!(
                (r.attributed_events, r.unexplained_nonconformant),
                (0, 1),
                "lossy={lossy}: a PCR anomaly on a PID no PMT declares is a library finding: {r:?}"
            );

            // Nor excused: a gap on that PID does not make it loss.
            let mut a = with_video_pcr(lossy, vec![]);
            a.on_signal(5000, Some(REWRITTEN_PID), Signal::ContinuityJump);
            a.on_signal(5010, Some(REWRITTEN_PID), PCR_FORWARD);
            let r = a.finish(10_000);
            assert_eq!(
                (r.pcr_anomalies_excused, r.unexplained_nonconformant),
                (0, 1),
                "lossy={lossy}: {r:?}"
            );

            // The same injection still explains an anomaly on the PID it
            // left, which IS declared, and a non-conformance on the PID it
            // moved the packet to, which needs no timeline.
            let mut a = with_video_pcr(lossy, vec![moved.clone()]);
            a.on_pcr(1000, 5000);
            a.on_signal(5010, Some(REWRITTEN_PID), Signal::OtherNonConformant);
            a.on_signal(5012, Some(0x1011), PCR_FORWARD);
            let r = a.finish(10_000);
            assert_eq!(
                (r.attributed_events, r.unexplained_nonconformant),
                (2, 0),
                "lossy={lossy}: {r:?}"
            );
        }

        // Before any PMT is reported nothing is declared at all.
        let mut a = Attribution::lossy(vec![], &hdr());
        a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
        a.on_signal(5010, Some(0x1011), PCR_FORWARD);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (0, 1),
            "{r:?}"
        );
    }

    /// Everything the sender logged between the last media the receiver
    /// saw and a reconnect marker never arrived — its PCR anchor included,
    /// so it resolves approximately, against the first base the rebuilt
    /// connection delivers. An injection placed inside that gap this way
    /// is lost in transit, not undetected — for the PAT/PMT shapes that no continuity jump can
    /// ever excuse (the demuxer reports none on a PSI PID), and for the
    /// media shapes a 90 s outage strands far outside a 500-packet window.
    /// Run 3 (2026-09-17): all 12 `undetected` on the srt leg sat at the
    /// 6-hour outage boundaries.
    #[test]
    fn an_injection_resolved_inside_a_reconnect_gap_is_lost_in_transit() {
        // A PAT flip anchored at PCR base 1000, +10 packets.
        let mut pat_flip = inj(1000, 10, Class::PsiFlip, 0, true);
        pat_flip.psi = true;
        let run = |lossy: bool| {
            let mut a = if lossy {
                Attribution::lossy(vec![pat_flip.clone()], &hdr())
            } else {
                Attribution::strict(vec![pat_flip.clone()], &hdr())
            };
            a.on_media(4000, 0x1011); // last media before the link died
            a.on_reconnect(4900); // the marker
            // Base 1000 went down with the link. The first base the
            // rebuilt connection delivers is a later one, so the injection
            // resolves approximately, at 5010.
            a.on_pcr(1500, 5000);
            a.on_media(5100, 0x1011);
            a.finish(10_000)
        };
        let r = run(true);
        assert_eq!(
            (
                r.reconnects_seen,
                r.lost_in_reconnect_gap,
                r.undetected_lost
            ),
            (1, 1, 1),
            "{r:?}"
        );
        assert_eq!(r.undetected_count, 0, "{r:?}");
        let r = run(false);
        assert_eq!(
            (
                r.reconnects_seen,
                r.lost_in_reconnect_gap,
                r.undetected_count
            ),
            (1, 0, 1),
            "{r:?}"
        );

        // Two markers with no media between them (a rebuild that broke
        // again): both gaps open at the same last media, and an injection
        // between them is still lost.
        let mut a = Attribution::lossy(vec![pat_flip.clone()], &hdr());
        a.on_media(4000, 0x1011);
        a.on_reconnect(4500);
        a.on_reconnect(4900);
        a.on_pcr(1500, 5000);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            (r.reconnects_seen, r.lost_in_reconnect_gap),
            (2, 1),
            "{r:?}"
        );

        // The anchor arrived, and the link died between it and the
        // injection: resolved exactly, but from BEFORE the marker, so
        // nothing says the injection itself came through.
        let mut a = Attribution::lossy(vec![pat_flip.clone()], &hdr());
        a.on_pcr(1000, 3995); // resolves at 4005
        a.on_media(4000, 0x1011);
        a.on_reconnect(4900);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            (r.lost_in_reconnect_gap, r.undetected_count),
            (1, 0),
            "{r:?}"
        );

        // Resolved AFTER the gap closes (marker + window + slack): judged
        // on its own merits — here, undetected.
        let mut a = Attribution::lossy(vec![pat_flip.clone()], &hdr());
        a.on_media(4000, 0x1011);
        a.on_reconnect(4000);
        a.on_pcr(1000, 4000 + ATTRIBUTION_WINDOW + 200);
        a.on_media(6000, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            (r.lost_in_reconnect_gap, r.undetected_count),
            (0, 1),
            "{r:?}"
        );
    }

    /// An unexplained event's sample names the nearest resolved injection,
    /// so a report can be read offline for "what was nearby" — the
    /// question run 3's six PMT checksum failures could not answer.
    #[test]
    fn an_unexplained_sample_names_the_nearest_injection() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Dup, 0x1011, false)], &hdr());
        a.on_pcr(1000, 5000); // resolves at 5010
        a.on_signal(7000, Some(0x1000), Signal::PsiChecksum);
        let r = a.finish(10_000);
        assert_eq!(
            r.unexplained_events,
            vec![
                "PsiChecksum on pid Some(4096) at packet 7000 (nearest injection: dup on pid \
                 0x1011 at packet 5010, 1990 packets before)"
                    .to_string()
            ],
        );
    }

    /// The excusal is evidence-driven, not blanket: with NO continuity
    /// jump in its window, an undetected injection still fails in Lossy.
    /// Without this the whole corruption suite would go vacuous.
    #[test]
    fn lossy_without_a_cc_jump_in_the_window_still_reports_undetected() {
        let mut a = Attribution::lossy(vec![inj(1000, 10, Class::PsiFlip, 0, true)], &hdr());
        a.on_pcr(1000, 5000);
        // A jump far outside the window explains nothing about it.
        a.on_signal(9000, Some(0x1011), Signal::ContinuityJump);
        let r = a.finish(10_000);
        assert_eq!(r.undetected.len(), 1, "{r:?}");
        assert_eq!(r.undetected_lost, 0);
    }

    /// An injection's OWN continuity jump is its own damage surfacing, not
    /// evidence that the network ate it, so it must not excuse that
    /// injection's recovery obligation. `Drop` is the class this protects:
    /// its only observable IS a continuity jump, so a self-excusing rule
    /// would let every drop arrive pre-excused and never have to show the
    /// stream recovering.
    #[test]
    fn an_injections_own_cc_jump_does_not_excuse_its_recovery() {
        let mut a = Attribution::lossy(vec![inj(1000, 10, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000); // resolves to 5010
        // Its own jump, inside its own window, attributed to it.
        a.on_signal(5100, Some(0x1011), Signal::ContinuityJump);
        // No media on 0x1011 afterwards, so it never recovers.
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 1, "the jump IS attributed: {r:?}");
        assert_eq!(
            r.unrecovered.len(),
            1,
            "a drop keeps its full recovery obligation: {r:?}"
        );
        assert_eq!(r.unrecovered_lost, 0, "{r:?}");
    }

    /// The other half of the same rule: a jump from a DIFFERENT injection
    /// still excuses. `explainer` attributes a jump to the newest covering
    /// injection that can cause it, so an older one whose window also
    /// covers it is excused — which needs overlapping windows, the case a
    /// real tap config forbids (`min_gap >= 2 * ATTRIBUTION_WINDOW`) but
    /// an `approx` resolution's widened window can still produce.
    ///
    /// Both injections sit on the PSI PID because the excusal is
    /// PID-constrained too: a gap on a PID an injection never touched says
    /// nothing about whether ITS packet went missing, so
    /// only a jump the older injection could itself have suffered counts.
    #[test]
    fn a_foreign_cc_jump_in_the_window_still_excuses() {
        // Two injections 100 packets apart: deliberately closer than
        // `min_gap` so their windows overlap and the newer one can own a
        // jump that also falls inside the older one's window.
        let build = |lossy: bool| {
            let log = vec![
                inj(1000, 10, Class::PsiFlip, 0, true),
                inj(1000, 110, Class::Drop, 0, true),
            ];
            let mut a = if lossy {
                Attribution::lossy(log, &hdr())
            } else {
                Attribution::strict(log, &hdr())
            };
            a.on_pcr(1000, 5000); // resolve to 5010 and 5110
            // Inside BOTH windows and on a PID both injections damaged;
            // `explainer` gives it to the newer (the Drop, the only one of
            // the two whose class expects a jump), so the PsiFlip sees it
            // as foreign.
            a.on_signal(5200, Some(0), Signal::ContinuityJump);
            a
        };

        // The psi_flip is detectable and expects a PsiChecksum event it
        // never got, so it is BOTH undetected and unrecovered — one
        // fixture covering both excused counters.
        let lossy = build(true).finish(10_000);
        assert_eq!(
            (lossy.undetected_lost, lossy.unrecovered_lost),
            (1, 1),
            "the psi_flip is excused by the drop's foreign jump: {lossy:?}"
        );
        assert!(
            lossy.undetected.is_empty(),
            "nothing else was undetected: {lossy:?}"
        );
        assert_eq!(
            lossy.unrecovered.len(),
            1,
            "and the drop itself is NOT excused by its own jump: {lossy:?}"
        );
        assert!(
            lossy.unrecovered[0].contains("drop"),
            "the surviving one must be the drop: {lossy:?}"
        );

        // Strict excuses neither, as always.
        let strict = build(false).finish(10_000);
        assert_eq!(strict.undetected.len(), 1, "{strict:?}");
        assert_eq!(strict.unrecovered.len(), 2, "{strict:?}");
        assert_eq!((strict.undetected_lost, strict.unrecovered_lost), (0, 0));
    }

    /// `explains_damage` answers for a content oracle, records nothing,
    /// and covers exactly two cases: same PID any class, or Truncate/
    /// Garbage on any PID (both destroy framing multiplex-wide).
    #[test]
    fn explains_damage_covers_same_pid_any_class_and_framing_classes_anywhere() {
        let mut a = Attribution::strict(
            vec![
                inj(1000, 10, Class::BodyFlip, 0x1031, false),
                inj(2000, 10, Class::Truncate, 0x1011, true),
            ],
            &hdr(),
        );
        a.on_pcr(1000, 5000); // body flip on 0x1031 resolves to 5010
        a.on_pcr(2000, 7000); // truncate on 0x1011 resolves to 7010

        // Same PID, any class.
        assert!(a.explains_damage(5010, 0x1031));
        assert!(a.explains_damage(5010 + ATTRIBUTION_WINDOW, 0x1031));
        // A body flip on another PID explains nothing there.
        assert!(!a.explains_damage(5010, 0x1011));
        // Truncation breaks framing for the whole multiplex.
        assert!(a.explains_damage(7010, 0x1031));
        assert!(a.explains_damage(7010, 0x1011));
        // Past the window, nothing is explained.
        assert!(!a.explains_damage(7011 + ATTRIBUTION_WINDOW, 0x1031));

        // It recorded nothing: no events, no detection, no recovery.
        let r = a.finish(10_000);
        assert_eq!(r.events, 0);
        assert_eq!(r.attributed_events, 0);
    }

    #[test]
    fn attribution_flags_undetected_and_unrecovered_injections() {
        let mut a = Attribution::strict(
            vec![
                inj(1000, 10, Class::PsiFlip, 0, true),
                inj(2000, 0, Class::Dup, 0x1011, false),
            ],
            &hdr(),
        );
        a.on_pcr(1000, 5000);
        a.on_pcr(2000, 7000);
        a.on_media(7100, 0x1011); // dup recovered (no detection required)
        let r = a.finish(10_000);
        assert_eq!(r.undetected.len(), 1, "psi_flip had no PsiChecksum event");
        assert_eq!(
            r.unrecovered.len(),
            1,
            "psi_flip (pid 0) never saw media after it"
        );
    }

    #[test]
    fn attribution_resolves_an_unseen_pcr_base_to_the_next_seen_one_wrap_aware() {
        let near_wrap = (1u64 << 33) - 10;
        let mut a = Attribution::strict(
            vec![inj(near_wrap, 3, Class::Truncate, 0x1011, true)],
            &hdr(),
        );
        a.on_pcr(5, 8000); // wrapped past 2^33; first base "at or after" near_wrap
        a.on_signal(8010, None, Signal::Resync);
        a.on_media(8020, 0x1011);
        let r = a.finish(9000);
        assert_eq!(r.resolved, 1);
        assert_eq!(r.resyncs, 1);
        assert!(
            r.unexplained_events.is_empty() && r.undetected.is_empty() && r.unrecovered.is_empty()
        );
    }

    #[test]
    fn approximate_resolution_is_bounded_so_an_outage_strands_an_injection() {
        // Just inside the bound: the logged anchor base never arrived (the
        // corruption destroyed the packet carrying it), but the next base
        // is close enough to place the injection approximately.
        let mut a = Attribution::strict(vec![inj(2000, 0, Class::Header, 0x1011, true)], &hdr());
        a.on_pcr(2000 + MAX_APPROX_TICKS, 5000);
        a.on_signal(5010, Some(0x1011), Signal::ContinuityJump);
        a.on_media(5020, 0x1011);
        let r = a.finish(10_000);
        assert_eq!((r.resolved, r.unresolved), (1, 0));
        assert!(r.undetected.is_empty() && r.unrecovered.is_empty());

        // One tick further — a reconnect outage, through which the sender
        // kept logging and the receiver saw nothing. Resolving here would
        // pile every injection logged during the outage onto this one
        // ordinal and report all but one as undetected, so the injection
        // stays unresolved and is judged for nothing.
        let mut a = Attribution::strict(vec![inj(2000, 0, Class::Header, 0x1011, true)], &hdr());
        a.on_pcr(2000 + MAX_APPROX_TICKS + 1, 5000);
        let r = a.finish(10_000);
        assert_eq!((r.resolved, r.unresolved), (0, 1));
        assert!(r.undetected.is_empty() && r.unrecovered.is_empty());
    }

    #[test]
    fn a_stranded_injection_does_not_block_the_ones_after_it() {
        // The cursor must step over an injection it can never place, or
        // everything logged after the outage becomes unattributable too.
        let mut a = Attribution::strict(
            vec![
                inj(2000, 0, Class::Header, 0x1011, true),
                inj(2000 + 2 * MAX_APPROX_TICKS, 0, Class::Header, 0x1011, true),
            ],
            &hdr(),
        );
        a.on_pcr(2000 + 2 * MAX_APPROX_TICKS, 5000); // strands #0, resolves #1
        a.on_signal(5010, Some(0x1011), Signal::ContinuityJump);
        let r = a.finish(10_000);
        assert_eq!((r.resolved, r.unresolved), (1, 1));
        assert_eq!(
            r.attributed_events, 1,
            "the surviving injection is reachable"
        );
        assert!(r.undetected.is_empty());
    }

    /// A "PCR" decoded from a mis-framed packet (a framing injection's
    /// aftermath) is a random 33-bit number. Fed as an anchor it stranded
    /// every injection pending behind it. One implausible sample is held,
    /// not accepted; the real anchor that follows resolves the injection.
    #[test]
    fn a_wild_pcr_sample_between_an_injection_and_its_anchor_is_not_an_anchor() {
        let mut a = Attribution::lossy(
            vec![inj(5_000_000, 3, Class::Garbage, 0x1011, true)],
            &hdr(),
        );
        a.on_pcr(4_900_000, 100); // 1.1 s before the injection's base: accepted
        a.on_pcr(3_000_000_000, 150); // 9 h away in one step: held as a candidate
        a.on_pcr(5_000_000, 200); // the injection's own anchor
        a.on_signal(204, Some(0x1011), Signal::ContinuityJump);
        a.on_media(220, 0x1011);
        let r = a.finish(2_000);
        assert_eq!((r.resolved, r.unresolved), (1, 0));
        assert_eq!(r.attributed_events, 1);
        assert_eq!(r.anchors_rejected, 1);
    }

    /// A genuine clock discontinuity (the source restarted) is confirmed by
    /// the next sample agreeing with the candidate: both become anchors.
    #[test]
    fn a_clock_jump_confirmed_by_the_next_sample_re_anchors() {
        let mut a = Attribution::strict(
            vec![inj(2_001_000_000, 2, Class::Drop, 0x1011, true)],
            &hdr(),
        );
        a.on_pcr(1_000_000, 10);
        a.on_pcr(2_001_000_000, 50); // ≈ 6.2 h jump: held
        a.on_pcr(2_001_009_000, 60); // 100 ms later on the new clock: confirms
        a.on_signal(53, Some(0x1011), Signal::ContinuityJump);
        a.on_media(70, 0x1011);
        let r = a.finish(2_000);
        assert_eq!((r.resolved, r.unresolved), (1, 0));
        assert_eq!(r.attributed_events, 1);
        assert_eq!(
            r.anchors_rejected, 0,
            "a confirmed candidate was not rejected"
        );
    }

    /// Two implausible samples that also disagree with each other (two
    /// mis-framed packets in a row) anchor nothing; the plausible third one
    /// resolves normally.
    #[test]
    fn two_disagreeing_wild_samples_anchor_nothing() {
        let mut a = Attribution::lossy(vec![inj(5_000_000, 3, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(4_900_000, 100);
        a.on_pcr(3_000_000_000, 150);
        a.on_pcr(7_000_000_000, 160);
        a.on_pcr(5_000_000, 200);
        a.on_signal(204, Some(0x1011), Signal::ContinuityJump);
        a.on_media(220, 0x1011);
        let r = a.finish(2_000);
        assert_eq!((r.resolved, r.unresolved), (1, 0));
        assert_eq!(r.attributed_events, 1);
        assert_eq!(r.anchors_rejected, 2);
    }

    /// A receiver that ingests injections logged long before it listened
    /// (history from before a restart, or a unit test feeding one) must not
    /// let the oldest block the live ones: a 14-hour-old anchor is past the
    /// 33-bit PCR half-range, so the naive "ahead ⇒ still in the future"
    /// test read it as future and stopped the cursor in front of it.
    #[test]
    fn old_restart_history_must_not_block_live_injections() {
        let current = 14 * 3600 * 90_000u64;
        let mut a = Attribution::lossy(
            vec![
                inj(1_000, 2, Class::Drop, 0x1011, true),
                inj(current, 3, Class::Drop, 0x1011, true),
            ],
            &hdr(),
        );
        a.on_pcr(current, 10);
        a.on_signal(14, Some(0x1011), Signal::ContinuityJump);
        a.on_media(20, 0x1011);
        let report = a.finish(2_000);
        assert_eq!(report.resolved, 1, "current injection must be placed");
        assert_eq!(
            report.unresolved, 1,
            "the ancient one is stranded, not waited for"
        );
        assert_eq!(report.attributed_events, 1);
    }

    #[test]
    fn expected_signal_sets_cover_the_non_conformance_shapes() {
        // An adaptation-field-length overrun (a `header` sub-kind) is
        // reported as a plain non-conformance, not a resync, so it must
        // still count as having noticed the injection.
        let mut a = Attribution::strict(vec![inj(1000, 0, Class::Header, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5010, Some(0x1011), Signal::OtherNonConformant);
        a.on_media(5020, 0x1011);
        let r = a.finish(10_000);
        assert!(r.undetected.is_empty(), "{:?}", r.undetected);
        assert_eq!(r.attributed_nonconformant, 1);

        // A flipped pointer field or section length surfaces as a
        // table-id/section-length non-conformance before the CRC is
        // reached.
        let mut a = Attribution::strict(vec![inj(1000, 0, Class::PsiFlip, 0, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5010, Some(0), Signal::OtherNonConformant);
        a.on_media(5020, 0);
        let r = a.finish(10_000);
        assert!(r.undetected.is_empty(), "{:?}", r.undetected);

        // But the sets are not wide open: a dropped packet is a continuity
        // jump and nothing else, so a resync must not be mistaken for
        // having noticed it.
        let mut a = Attribution::strict(vec![inj(1000, 0, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5010, None, Signal::Resync);
        a.on_media(5020, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            r.undetected.len(),
            1,
            "a resync does not prove a drop was seen"
        );
    }

    #[test]
    fn attribution_reports_an_injection_whose_window_never_arrived_as_unresolved() {
        let a = Attribution::strict(vec![inj(1000, 0, Class::Header, 0x1011, true)], &hdr());
        let r = a.finish(100);
        assert_eq!(r.unresolved, 1);
        assert!(
            r.undetected.is_empty(),
            "an unresolved injection is not judged"
        );
    }

    /// The reconnect excusal is for injections that never arrived. This
    /// one did: its own anchor base arrives, exactly, AFTER the marker,
    /// and media flows on both sides of its resolved position with no
    /// continuity jump anywhere. A demuxer that said nothing about the
    /// damaged PAT has missed it, inside the gap's window or not.
    #[test]
    fn an_injection_resolved_exactly_after_a_reconnect_marker_is_not_lost() {
        let mut pat_flip = inj(1000, 10, Class::PsiFlip, 0, true);
        pat_flip.psi = true;
        // 4900 is the first packet after the marker: a marker is stamped
        // with the count of packets received, a PCR with its 0-based
        // ordinal.
        for anchor_at in [5000, 4900] {
            let mut a = Attribution::lossy(vec![pat_flip.clone()], &hdr());
            a.on_media(4000, 0x1011); // last media before the link died
            a.on_reconnect(4900); // the marker
            a.on_pcr(1000, anchor_at); // the injection's OWN anchor, exact
            a.on_media(anchor_at + 5, 0x1011); // media just before the injected packet...
            a.on_media(anchor_at + 15, 0x1011); // ...and just after it
            a.on_media(anchor_at + 100, 0x1011);
            let r = a.finish(10_000);
            assert_eq!(
                (
                    r.undetected_count,
                    r.lost_in_reconnect_gap,
                    r.undetected_lost
                ),
                (1, 0, 0),
                "anchor at {anchor_at}: an injection anchored exactly on a post-reconnect PCR \
                 arrived and must be detected: {r:?}"
            );
        }
    }

    /// A reconnect resets the demuxer, and until it has re-acquired what
    /// a class is noticed by, an injection that arrived still could not
    /// have been noticed. Each shape twice: placed before the media that
    /// shows the demuxer able (lost in the gap), and after it (held).
    #[test]
    fn a_post_reconnect_injection_is_held_only_once_the_demuxer_could_notice_it() {
        const VIDEO: u16 = 0x1011;
        const KLV: u16 = 0x1100;
        // Marker 4900, exact anchor 5000, injection at 5010, on `inj`'s
        // PID; `media_before` is fed between the anchor and the injection.
        let run = |inj: &Injection, media_before: &[u16]| {
            let mut a = Attribution::lossy(vec![inj.clone()], &hdr());
            a.on_media(4000, VIDEO); // before the marker: says nothing about after it
            a.on_media(4000, KLV);
            a.on_reconnect(4900);
            a.on_pcr(1000, 5000);
            for &pid in media_before {
                a.on_media(5005, pid);
            }
            a.on_media(5015, VIDEO); // after the injection: too late to show anything
            a.on_media(5015, KLV);
            let r = a.finish(10_000);
            (r.undetected_count, r.lost_in_reconnect_gap)
        };
        const HELD: (u64, u64) = (1, 0);
        const LOST: (u64, u64) = (0, 1);

        // Noticed as a continuity jump on its own PID: needs a previous
        // counter THERE. Media on the other PID is not that.
        let drop = inj(1000, 10, Class::Drop, KLV, true);
        let mut pid_rewrite = inj(1000, 10, Class::Header, KLV, true);
        pid_rewrite.offsets = vec![1, 2];
        let mut cc_rewrite = inj(1000, 10, Class::Header, KLV, true);
        cc_rewrite.offsets = vec![3];
        for (name, i) in [
            ("drop", &drop),
            ("pid rewrite", &pid_rewrite),
            ("cc rewrite", &cc_rewrite),
        ] {
            assert_eq!(run(i, &[]), LOST, "{name}, no media");
            assert_eq!(run(i, &[VIDEO]), LOST, "{name}, media on another PID");
            assert_eq!(run(i, &[KLV]), HELD, "{name}, media on its own PID");
        }

        // Noticed without per-PID history: media on any PID shows the
        // PAT and a PMT read and the packets in sync.
        let mut pat_flip = inj(1000, 10, Class::PsiFlip, 0, true);
        pat_flip.psi = true;
        let mut pmt_body_flip = inj(1000, 10, Class::BodyFlip, 0x1000, true);
        pmt_body_flip.psi = true;
        let mut af_overrun = inj(1000, 10, Class::Header, KLV, true);
        af_overrun.offsets = vec![4];
        let mut sync_byte = inj(1000, 10, Class::Header, KLV, true);
        sync_byte.offsets = vec![0];
        let truncate = inj(1000, 10, Class::Truncate, KLV, true);
        let garbage = inj(1000, 10, Class::Garbage, KLV, true);
        for (name, i) in [
            ("pat flip", &pat_flip),
            ("pmt body flip", &pmt_body_flip),
            ("adaptation-field overrun", &af_overrun),
            ("sync byte", &sync_byte),
            ("truncate", &truncate),
            ("garbage", &garbage),
        ] {
            assert_eq!(run(i, &[]), LOST, "{name}, no media");
            assert_eq!(run(i, &[VIDEO]), HELD, "{name}, media on any PID");
        }

        // A second reconnect resets the demuxer again: media after the
        // first marker shows nothing about the state after the second.
        let mut a = Attribution::lossy(vec![drop.clone()], &hdr());
        a.on_reconnect(4900);
        a.on_media(4950, KLV);
        a.on_reconnect(4990);
        a.on_pcr(1000, 5000);
        a.on_media(5015, KLV);
        let r = a.finish(10_000);
        assert_eq!((r.undetected_count, r.lost_in_reconnect_gap), LOST, "{r:?}");
    }

    /// A gap belongs to one reconnect epoch. Two reconnects in quick
    /// succession leave the older gap's window reaching past the newer
    /// marker, and the older gap saw no media of its own — so if it still
    /// had a say there, a second reconnect would switch detection off for
    /// everything the newer epoch demonstrably delivered.
    #[test]
    fn an_older_reconnect_gap_does_not_excuse_what_the_newest_epoch_holds() {
        let mut pat_flip = inj(1000, 10, Class::PsiFlip, 0, true);
        pat_flip.psi = true;
        let mut a = Attribution::lossy(vec![pat_flip.clone()], &hdr());
        a.on_media(4000, 0x1011);
        a.on_reconnect(4900);
        a.on_reconnect(4950);
        a.on_pcr(1000, 5000); // exact, after the newest marker: resolves at 5010
        a.on_media(5005, 0x1011); // the reset demuxer is parsing again
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            (r.undetected_count, r.lost_in_reconnect_gap),
            (1, 0),
            "arrived in the newest epoch, after its media, and nothing was reported: {r:?}"
        );

        // The twin that IS lost: placed between the two markers, in an
        // epoch that ended before the demuxer produced any media.
        let mut a = Attribution::lossy(vec![pat_flip], &hdr());
        a.on_media(4000, 0x1011);
        a.on_reconnect(4900);
        a.on_pcr(1000, 4905); // exact, after the first marker: resolves at 4915
        a.on_reconnect(4950);
        a.on_media(5005, 0x1011);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            (r.undetected_count, r.lost_in_reconnect_gap),
            (0, 1),
            "{r:?}"
        );
    }

    /// The declared `PCR_PID`s are the demuxer's CURRENT topology. A
    /// reconnect resets the demuxer, so a PID declared before it is
    /// declared no longer, and a `PcrAnomaly` there means a timeline
    /// outlived its declaration: never excused, never attributed.
    #[test]
    fn a_pcr_pid_declared_before_a_reconnect_is_not_declared_after_it() {
        let mut on_video = inj(1000, 10, Class::Header, 0x1011, true);
        on_video.offsets = vec![4];
        for lossy in [false, true] {
            // Program 1 before the reconnect, only program 2 after it.
            let run = |log: Vec<Injection>| {
                let mut a = with_video_pcr(lossy, log);
                a.on_reconnect(4000);
                a.on_program_map(2, 0x1001, 0x1100);
                a
            };

            // Beside a gap nothing else claims.
            let mut a = run(vec![]);
            a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
            a.on_signal(5010, Some(0x1011), PCR_FORWARD);
            let r = a.finish(10_000);
            assert_eq!(
                (r.pcr_anomalies_excused, r.unexplained_nonconformant),
                (0, 1),
                "lossy={lossy}: {r:?}"
            );

            // Inside the window of an injection that could otherwise
            // explain it.
            let mut a = run(vec![on_video.clone()]);
            a.on_pcr(1000, 5000); // resolves at 5010
            a.on_media(5005, 0x1100);
            a.on_signal(5012, Some(0x1011), PCR_FORWARD);
            let r = a.finish(10_000);
            assert_eq!(
                (r.attributed_events, r.unexplained_nonconformant),
                (0, 1),
                "lossy={lossy}: {r:?}"
            );
        }

        // The healthy twin: the anomaly is on the PID the new topology
        // declares, beside its own gap.
        let mut a = with_video_pcr(true, vec![]);
        a.on_reconnect(4000);
        a.on_program_map(2, 0x1001, 0x1100);
        a.on_signal(5000, Some(0x1100), Signal::ContinuityJump);
        a.on_signal(5010, Some(0x1100), PCR_FORWARD);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (1, 0),
            "{r:?}"
        );

        // After a reconnect nothing is declared until a program map says
        // so — the same fail-closed rule as at the start of a capture.
        let mut a = with_video_pcr(true, vec![]);
        a.on_reconnect(4000);
        a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
        a.on_signal(5010, Some(0x1011), PCR_FORWARD);
        a.on_program_map(1, 0x1000, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (0, 1),
            "{r:?}"
        );
    }

    /// Without a reconnect the declarations follow the program maps: a
    /// later map for the same program replaces its `PCR_PID`, and a map
    /// for a different program on the same PMT PID replaces the program
    /// (the demuxer keeps one program per PMT PID).
    #[test]
    fn a_program_map_replaces_the_declaration_it_supersedes() {
        let excused_on = |a: &mut Attribution, at: u64, pid: u16| {
            a.on_signal(at, Some(pid), Signal::ContinuityJump);
            a.on_signal(at + 10, Some(pid), PCR_FORWARD);
        };

        // Program 1 moves its time base from 0x1011 to 0x1100.
        let mut a = with_video_pcr(true, vec![]);
        a.on_program_map(1, 0x1000, 0x1100);
        excused_on(&mut a, 5000, 0x1011);
        excused_on(&mut a, 6000, 0x1100);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (1, 1),
            "{r:?}"
        );

        // Program 2 takes over program 1's PMT PID.
        let mut a = with_video_pcr(true, vec![]);
        a.on_program_map(2, 0x1000, 0x1100);
        excused_on(&mut a, 5000, 0x1011);
        excused_on(&mut a, 6000, 0x1100);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (1, 1),
            "{r:?}"
        );

        // A second program on its own PMT PID removes nothing.
        let mut a = with_video_pcr(true, vec![]);
        a.on_program_map(2, 0x1001, 0x1100);
        excused_on(&mut a, 5000, 0x1011);
        excused_on(&mut a, 6000, 0x1100);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (2, 0),
            "{r:?}"
        );
    }

    /// A continuity gap is evidence about the connection it was seen on.
    /// After a reconnect the demuxer holds no PCR baseline from before it,
    /// so a jump it reports is between two PCRs of the NEW connection and
    /// a gap from the old one is not its cause.
    #[test]
    fn a_continuity_gap_from_before_a_reconnect_excuses_no_pcr_anomaly_after_it() {
        let mut a = with_video_pcr(true, vec![]);
        a.on_signal(3990, Some(0x1011), Signal::ContinuityJump);
        a.on_reconnect(4000);
        a.on_program_map(1, 0x1000, 0x1011);
        a.on_signal(4010, Some(0x1011), PCR_FORWARD);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (0, 1),
            "{r:?}"
        );
    }
}
