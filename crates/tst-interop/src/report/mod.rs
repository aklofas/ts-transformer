//! `report merge` / `report render`: turn per-cell interop results into
//! the published evidence artifact.
//!
//! The orchestrator (`run-matrix.sh`) writes one JSON file
//! per cell into a directory. [`merge`] reads all of them, checks each
//! `FAIL` against a hand-parsed expectations file, and writes a single
//! `results.json` ([`Results`]). [`render`] turns that `results.json`
//! into a markdown report.
//!
//! **The load-bearing property: an unexpected failure can never be
//! silently absorbed.** A `FAIL` only becomes non-fatal
//! ([`Verdict::ExpectedUnsupported`]) when it matches a specific,
//! human-authored entry in the expectations file. Everything else that
//! fails stays [`Verdict::Fail`], which is what [`merge`]'s caller checks
//! (`results.summary.fail > 0`) to decide the process exit code — but
//! that isn't the only way a run fails: a stale `expected_unsupported`
//! row (one whose cell actually `PASS`ed — see `results.summary.
//! stale_expectations`) fails the run too (exit 1), and `merge` itself
//! returns an `Err` (exit 2, no `results.json` written at all) when the
//! produced cells don't exactly match the declared `--inventory`
//! multiset (see [`check_inventory`]).
//!
//! Expectations are read from a small hand-rolled TOML-shaped format (see
//! [`parse_expectations`]) rather than a real TOML crate — this crate
//! deliberately takes no `toml` dependency (tst-integration and tst-core's
//! optional feature do) for a machine-written file with an exact, fixed schema.

use serde::{Deserialize, Serialize};

use crate::report_types::CellMetrics;

/// Axis-group row count above which [`render_markdown`] wraps the group
/// in a collapsed `<details>` block instead of a bare `##` heading.
const AXIS_DETAILS_THRESHOLD: usize = 15;

/// Outcome an orchestrator observed for one cell, before expectations are
/// applied. See [`RawCell`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RawVerdict {
    Pass,
    Fail,
    SkippedToolMissing,
}

/// One per-cell JSON file as written by the orchestrator into
/// `--cells-dir`. Field shapes mirror the orchestrator's contract
/// verbatim — `metrics` is
/// `null` for a `SkippedToolMissing` cell and for any cell whose peer
/// tool produced no parseable capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawCell {
    pub id: String,
    pub profile: String,
    pub peer: String,
    pub direction: String,
    pub tier: String,
    pub verdict: RawVerdict,
    #[serde(default)]
    pub failures: Vec<String>,
    pub metrics: Option<CellMetrics>,
    pub log: String,
}

/// Final, expectations-applied verdict for one cell — the value
/// [`merge`]'s caller acts on. Exactly these four values; `known_flaky`
/// (see [`MergedCell::known_flaky`]) is a display-only refinement of
/// `ExpectedUnsupported`, not a fifth verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    Pass,
    Fail,
    ExpectedUnsupported,
    SkippedToolMissing,
}

/// One cell after expectations matching, as written into `results.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedCell {
    pub id: String,
    pub profile: String,
    pub peer: String,
    pub direction: String,
    pub tier: String,
    pub verdict: Verdict,
    /// Set iff `verdict == ExpectedUnsupported` and the matching
    /// expectation's declared verdict was `known_flaky` rather than
    /// `expected_unsupported` — [`render_markdown`] labels such a row
    /// `KNOWN-FLAKY` instead of `EXPECTED-UNSUPPORTED`.
    pub known_flaky: bool,
    pub failures: Vec<String>,
    /// The lines of `failures` no expectation explained. Empty unless
    /// `verdict == Fail`: one unexplained line keeps the whole cell a
    /// `FAIL`, however many other lines a row excused.
    #[serde(default)]
    pub unexplained_failures: Vec<String>,
    /// One entry per line of `failures`, in order: the expectation row
    /// that explained that line, or `None`. Lets a reader of the
    /// evidence see which row excused which line when a cell needed
    /// several. Empty for a `PASS`, a skip, or a `FAIL` that printed no
    /// line.
    #[serde(default)]
    pub explained_by: Vec<Option<ExcusedBy>>,
    pub metrics: Option<CellMetrics>,
    pub log: String,
    /// The matching expectation's `reason`, if `verdict == ExpectedUnsupported`.
    pub expectation_reason: Option<String>,
    /// The matching expectation's optional `ref`, if any.
    pub expectation_ref: Option<String>,
}

/// The expectation row that explained one failure line: its `reason`,
/// its optional `ref`, and the `failure_contains` text that selected it
/// (`None` for a row matching every line of its cell).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcusedBy {
    pub reason: String,
    pub reference: Option<String>,
    pub failure_contains: Option<String>,
}

/// One expectation whose matched cell turned out to `PASS` — the
/// expectation no longer reproduces and should probably be removed.
/// Only `expected_unsupported` expectations can go stale this way; a
/// `known_flaky` expectation matching a passing cell is normal (a flaky
/// failure that didn't reproduce this run) and is never reported here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleExpectation {
    pub cell: String,
    pub profile: String,
    pub reason: String,
}

/// Verdict tallies plus the stale-expectation list, over one merged run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub total: usize,
    pub pass: usize,
    /// Cells whose `FAIL` matched no expectation — see the module doc's
    /// load-bearing property. `merge`'s CLI caller (`main.rs`) exits
    /// nonzero (1) if this is nonzero OR `stale_expectations` is
    /// non-empty — either alone is enough to fail the run, so this
    /// field is not by itself the whole exit-code story (an inventory
    /// mismatch fails even earlier, via `merge`'s own `Err`, exit 2,
    /// before a `Summary` is even produced).
    pub fail: usize,
    pub expected_unsupported: usize,
    pub skipped_tool_missing: usize,
    pub stale_expectations: Vec<StaleExpectation>,
}

/// The full `results.json` document: [`merge`]'s output and
/// [`render`]'s input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Results {
    /// The `--meta` file's contents, embedded verbatim (run date, host,
    /// tool versions — the orchestrator's shape, not this module's).
    pub meta: serde_json::Value,
    pub cells: Vec<MergedCell>,
    pub summary: Summary,
    /// Set by [`merge`] from the `--inventory` file it validated the run
    /// against. `None` only for a `Results` built directly by
    /// [`build_results`] rather than through `merge`'s file-driven
    /// wrapper (e.g. in tests) — a real `results.json` written by `merge`
    /// always has this set. `#[serde(default)]` so an older `results.json`
    /// written before this field existed still deserializes.
    #[serde(default)]
    pub inventory: Option<InventorySummary>,
}

/// What the merged report records about its inventory. Deliberately
/// smaller than [`Inventory`] — it omits `cells` (the merge already
/// proved they match, one-for-one, `raw_cells`; re-listing them here
/// would just duplicate `results.cells`), `cells_glob`/`profiles` (raw
/// orchestration inputs, not evidence-page-relevant), and `tools` (a
/// spec §5.1 deviation from `Inventory`'s own shape: `results.meta.tools`
/// — embedded verbatim from `merge`'s `--meta` file — already carries
/// the tool versions, so this struct doesn't duplicate them a second
/// time under a different path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventorySummary {
    pub shape: String,
    pub declared_cells: usize,
    /// The `[census] full_cells` the expectations file declared. Defaults
    /// to 0 so a `results.json` written before the census was recorded
    /// still deserializes.
    #[serde(default)]
    pub expected_full_cells: usize,
    pub allowed_skips: Vec<String>,
}

// ---------------------------------------------------------------------
// `report merge` / `report render` — the interop-matrix half
// ---------------------------------------------------------------------

pub(crate) mod csv;
mod matrix;
pub use matrix::{
    ExpectVerdict, Expectation, Inventory, InventoryCell, SHAPE_FULL, SHAPE_SUBSET,
    append_github_summary, build_results, check_inventory, merge, parse_census, parse_expectations,
    parse_inventory, render, render_markdown,
};

/// `report step` / `report stress` — the stress harness's verdicts; see `report/stress.rs`.
pub mod stress;

/// `report soak`: turn one multi-day soak run's raw artifacts (an RSS
/// time series plus each leg's proxy/recv/send evidence) into
/// `soak-results.json` — the endurance half of the published interop
/// evidence (`merge`/`render`, above, is the interop-matrix half).
///
/// `scripts/interop/soak.sh` runs concurrent legs over the same
/// wall-clock window: `srt` (through an impaired proxy with scheduled
/// outage windows, sender wrapped via `send::run_managed` so it
/// reconnects), `rist` (through a second impaired proxy with NO
/// outage — sustained impairment only; see this module's "Known
/// telemetry limitations" section for why only one leg carries the
/// outage/reconnect assertion) and `rtsp-publish` (a publish leg: an
/// external RTSP publisher RECORDing into `recv`'s publish mount, its
/// session ended and re-ANNOUNCEd on a declared period, judged by
/// `publisher_generations_<leg>` from the mount's own count of ended
/// publishers; no proxy, no tst-interop sender, so the proxy, reconnect
/// and corruption verdicts are reported not applicable for it),
/// `rtsp-publish-es` (the same publish leg pushing elementary H.264 +
/// KLV tracks that the mount re-muxes) and `whep` (a headless browser
/// playing a WebRTC mount served for the whole run, judged from the
/// viewer's per-connection report and the server's counters).
/// [`soak::build_soak_results`] is the
/// pure verdict engine; [`soak::run`] is the file-driven CLI wrapper
/// `main.rs`'s `report soak` calls.
///
/// # Known telemetry limitations
///
/// Two of the verdicts this module would ideally check exactly are
/// instead approximated — every [`soak::SoakResults`] documents both in
/// its `limitations` field:
///
/// - **Reconnect count.** `recv --managed`'s `VerifyReport.reconnects`
///   (populated from `ManagedRecvTransport::reconnects_count` — see
///   `recv.rs`'s `run_managed`) IS a real, observed count of successful
///   receive-side transport rebuilds, unlike the send side
///   (`ManagedTransport` exposes no equivalent counter today —
///   `docs/project/deferred-features.md`'s "Reconnect counters on
///   ManagedTransport stats" entry covers the send-side gap only) —
///   `recv`'s `VerifyReport` still carries no per-event delivery-gap
///   timestamps, so exact gap TIMING still can't be checked. `soak`'s
///   internal `expected_outage_windows` helper computes how many outage
///   windows *should* have started within the observed run (from the
///   configured schedule alone, never from anything observed), and the
///   `reconnect_count_matches_outage_count_<leg>` verdict compares that
///   expectation against the real observed count when one is available
///   (a leg whose recv wasn't run `--managed` falls back to the old
///   recorded-not-verified message). The verdict stays `provisional` —
///   informational, never gating [`soak::SoakResults::overall_pass`] —
///   even when a real count is available: empirically, a single outage
///   window does not always drive exactly one rebuild (a short 8s-outage
///   dry-run observed a SECOND rebuild
///   cycle shortly after the first succeeded, before the one scheduled
///   outage window had even repeated), and the real 90s-outage-duration
///   ratio hasn't been confirmed by enough runs yet to assert a hard
///   1:1 pass/fail. See the verdict's own `detail` string for exactly
///   what was observed on a given run.
/// - **Delivery-gap timing.** For the same reason, "gaps only inside
///   outage windows, ±30s" can't be checked at per-event resolution.
///   `soak`'s internal `renewal_model` helper instead models
///   the drop rate the proxy's CONTINUOUS impairment (`loss_pct`)
///   predicts, deliberately
///   EXCLUDING the outage windows from that expectation — verified
///   empirically while dry-running this module against the real
///   binaries (a `period=3s,dur=1s` outage over a 10s managed-send
///   window measured a 21% proxy-observed drop rate, not the ~41% a
///   naive "outage-covered-time is 100%-dropped" model predicts): a
///   `ManagedTransport` sender that detects `Broken` mid-outage stops
///   sending while it backs off and retries, so most of an outage
///   window's duration shows up as the sender NOT TRANSMITTING, not as
///   the proxy dropping packets that were sent — the proxy's
///   `dropped`/`forwarded` counters simply can't see time the sender
///   spent silent. The `drop_rate_consistent_with_impairment_<leg>`
///   verdict therefore checks only the continuous-impairment
///   expectation; a real gap outside that (a bug, not configured
///   impairment or an outage-driven reconnect) still shows up as
///   unexplained excess drop and fails it. Outage/reconnect correctness
///   itself is evidenced by `recv_invariants_<leg>` (the final tallies
///   already reflect however well the managed sender recovered) and the
///   provisional reconnect-count verdict above. Unlike the reconnect
///   check, this one is NOT provisional: given the inputs available,
///   it's fully computable and always enforced.
///   Outage-window drops themselves are no longer part of that
///   comparison at all: a `ManagedTransport` sender backing off through
///   an outage doesn't sit perfectly silent — each reconnect attempt's
///   handshake can fire before the sender's own `Broken` detection has
///   caught up to the outage clearing (or starting) — and any such
///   packet landing while the window is active used to be counted as
///   `dropped` like continuous loss, against an expectation that never
///   modeled it, for a genuinely expected ~0.02-0.2 percentage-point
///   excess on the outage-bearing leg only. The proxy now records those
///   drops apart (`proxy::PhaseCounters::outage_dropped`) and this
///   verdict subtracts them from both the observed drops and the packet
///   total, so the leg with outages is held to exactly the same
///   continuous-loss expectation as the one without.
///
/// # RSS-growth harness artifact (`--no-klv-digest`)
///
/// A 1h soak smoke run found real, linear (not one-time-step)
/// RSS growth in every `send`/`recv` process — `proxy` (a plain UDP
/// relay untouched by `tst-srt`/`tst-rist`) stayed flat at 0.0 KiB/h
/// the whole run, while `send`/`recv` on both legs ranged ~3.6-5.7
/// MiB/h. Root-caused (order-of-magnitude verified, not profiler-
/// confirmed) to `send.rs`'s `send_over_transport` and this crate's
/// `verify::Tally` both accumulating one hex digest `String` per KLV
/// record for the ENTIRE run — needed because `verify::klv_set_hash`'s
/// order-insensitive fingerprint sorts every digest before hashing the
/// concatenation, so nothing can be freed until the whole capture ends.
/// At 10 Hz KLV that's ~4 MiB/hour of purely this test HARNESS's own
/// bookkeeping, unrelated to the `tst-core`/`tst-pipeline`/`tst-srt`/
/// `tst-rist` library code the soak exists to measure — left unbounded,
/// a 72h run would accumulate on the order of 300 MiB of it, easily
/// swamping (or hiding a regression inside) the real library-
/// attributable signal this RSS-slope mechanism is supposed to surface.
///
/// **Mechanism now confirmed as designed around**: `send`/`recv --no-
/// klv-digest` (see their own doc comments in `send.rs`/`recv.rs`)
/// skips the accumulation entirely, at the cost of
/// `CellMetrics::klv_set_sha256` coming back `None` — an explicit,
/// deliberate trade a multi-day soak makes (it never needed that hash;
/// `soak.sh` passes the flag on every process it launches), while the
/// short interop-matrix cells `run-matrix.sh` drives keep tracking it
/// (their transparent-tier byte-comparisons need it, and their runs are
/// seconds long, so the accumulation never mattered there in the first
/// place). **Open watch item, not yet explained**: RIST's send/recv
/// pair ran measurably higher than SRT's near-identical pair (~5.7/4.0
/// vs. ~3.7/3.7 MiB/h) despite pushing the identical profile — the
/// `klv_digests` mechanism above predicts roughly EQUAL growth for both
/// legs, so there's an unaccounted-for residual specific to RIST
/// (plausibly librist's own recovery-buffer growth, plausibly just
/// single-run noise) that `--no-klv-digest` doesn't explain and this
/// fix doesn't address — worth comparing against the real 72h VM run's
/// numbers once `--no-klv-digest` is in use there.
pub mod soak;
