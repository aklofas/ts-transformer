//! `report merge` / `report render`: turn per-cell interop results into
//! the published evidence artifact.
//!
//! The orchestrator (a later task's `run-matrix.sh`) writes one JSON file
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

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

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
/// verbatim (see the task brief's Interfaces block) — `metrics` is
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
    pub metrics: Option<CellMetrics>,
    pub log: String,
    /// The matching expectation's `reason`, if `verdict == ExpectedUnsupported`.
    pub expectation_reason: Option<String>,
    /// The matching expectation's optional `ref`, if any.
    pub expectation_ref: Option<String>,
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

/// One `[[expect]]` block from the expectations file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expectation {
    /// Exact cell id, or a pattern ending in `*` (prefix match).
    pub cell: String,
    pub profile: String,
    pub verdict: ExpectVerdict,
    pub reason: String,
    pub reference: Option<String>,
    /// Substring narrowing: when set, this expectation only matches a
    /// `FAIL` whose failures text contains it (see the private
    /// `find_expectation` helper's doc comment for the exact matching
    /// rule). Never applied to a `PASS` staleness lookup — an
    /// expectation is checked for staleness by `(cell, profile)` alone,
    /// regardless of what failure text it was originally written to
    /// match. Mandatory on an `ExpectVerdict::ExpectedUnsupported` row
    /// (enforced by `ExpectBuilder::finish` — a documented gap must
    /// name the failure text it absorbs); optional on a `KnownFlaky`
    /// row, which by definition has no single mechanism string.
    pub failure_contains: Option<String>,
}

/// The two verdicts an expectation can declare. See
/// [`MergedCell::known_flaky`] for how `KnownFlaky` differs from
/// `ExpectedUnsupported` in the merged output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectVerdict {
    ExpectedUnsupported,
    KnownFlaky,
}

// ---------------------------------------------------------------------
// Expectations parsing
// ---------------------------------------------------------------------

/// Accumulates one `[[expect]]` block's `key = "value"` lines before
/// [`ExpectBuilder::finish`] validates it into an [`Expectation`].
#[derive(Default)]
struct ExpectBuilder {
    cell: Option<String>,
    profile: Option<String>,
    verdict: Option<String>,
    reason: Option<String>,
    reference: Option<String>,
    failure_contains: Option<String>,
}

impl ExpectBuilder {
    fn set(&mut self, key: &str, value: String, line_no: usize) -> Result<(), String> {
        match key {
            "cell" => {
                validate_cell_pattern(&value, line_no)?;
                self.cell = Some(value);
            }
            "profile" => self.profile = Some(value),
            "verdict" => self.verdict = Some(value),
            "reason" => self.reason = Some(value),
            "ref" => self.reference = Some(value),
            "failure_contains" => self.failure_contains = Some(value),
            other => {
                return Err(format!(
                    "expectations line {line_no}: unknown key `{other}`"
                ));
            }
        }
        Ok(())
    }

    /// Validate the accumulated block, reporting missing-required-key
    /// errors against `end_line` (the line the block closed at — either
    /// the next `[[expect]]` or end of file).
    fn finish(self, end_line: usize) -> Result<Expectation, String> {
        let cell = self.cell.ok_or_else(|| {
            format!("expectations block ending at line {end_line}: missing required key `cell`")
        })?;
        let profile = self.profile.ok_or_else(|| {
            format!("expectations block ending at line {end_line}: missing required key `profile`")
        })?;
        let verdict_str = self.verdict.ok_or_else(|| {
            format!("expectations block ending at line {end_line}: missing required key `verdict`")
        })?;
        let verdict = match verdict_str.as_str() {
            "expected_unsupported" => ExpectVerdict::ExpectedUnsupported,
            "known_flaky" => ExpectVerdict::KnownFlaky,
            other => {
                return Err(format!(
                    "expectations block ending at line {end_line}: unknown verdict `{other}` \
                     (want `expected_unsupported` or `known_flaky`)"
                ));
            }
        };
        let reason = self.reason.ok_or_else(|| {
            format!("expectations block ending at line {end_line}: missing required key `reason`")
        })?;
        if verdict == ExpectVerdict::ExpectedUnsupported && self.failure_contains.is_none() {
            return Err(format!(
                "expectations block ending at line {end_line}: expected_unsupported rows must \
                 carry failure_contains (name the failure text this row absorbs)"
            ));
        }
        Ok(Expectation {
            cell,
            profile,
            verdict,
            reason,
            reference: self.reference,
            failure_contains: self.failure_contains,
        })
    }
}

/// A `cell` pattern may only use `*` as its final character (a trailing
/// prefix-match glob). Reject it anywhere else, at parse time, so a typo
/// like `decode/*/foo` fails loudly here rather than silently matching
/// nothing (or everything) at merge time.
fn validate_cell_pattern(value: &str, line_no: usize) -> Result<(), String> {
    if let Some(pos) = value.find('*') {
        if pos != value.len() - 1 {
            return Err(format!(
                "expectations line {line_no}: `*` only allowed as the final character of \
                 `cell`, got: {value:?}"
            ));
        }
    }
    Ok(())
}

/// Parse one non-blank, non-comment line as `key = "value"`. Keys are
/// bare lowercase/underscore identifiers; values must be exactly one
/// quoted string with no embedded `"` — nothing else on the line.
fn parse_kv_line(line: &str, line_no: usize) -> Result<(String, String), String> {
    let (key, rest) = line.split_once('=').ok_or_else(|| {
        format!("expectations line {line_no}: expected `key = \"value\"`, got: {line:?}")
    })?;
    let key = key.trim();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
        return Err(format!("expectations line {line_no}: invalid key {key:?}"));
    }
    let rest = rest.trim();
    if rest.len() < 2 || !rest.starts_with('"') || !rest.ends_with('"') {
        return Err(format!(
            "expectations line {line_no}: value must be a quoted string, got: {rest:?}"
        ));
    }
    let inner = &rest[1..rest.len() - 1];
    if inner.contains('"') {
        return Err(format!(
            "expectations line {line_no}: unexpected `\"` inside value: {rest:?}"
        ));
    }
    Ok((key.to_string(), inner.to_string()))
}

/// Parse the expectations file format: `#`-comment lines, blank lines,
/// `[[expect]]` block markers, and `key = "value"` lines within a block.
/// Anything else is a loud parse error naming the offending line number.
pub fn parse_expectations(text: &str) -> Result<Vec<Expectation>, String> {
    let mut out = Vec::new();
    let mut current: Option<ExpectBuilder> = None;
    let mut last_line = 0;

    for (idx, raw_line) in text.lines().enumerate() {
        let line_no = idx + 1;
        last_line = line_no;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[expect]]" {
            if let Some(builder) = current.take() {
                out.push(builder.finish(line_no)?);
            }
            current = Some(ExpectBuilder::default());
            continue;
        }
        let (key, value) = parse_kv_line(line, line_no)?;
        match current.as_mut() {
            Some(builder) => builder.set(&key, value, line_no)?,
            None => {
                return Err(format!(
                    "expectations line {line_no}: `{key} = ...` outside of an [[expect]] block"
                ));
            }
        }
    }
    if let Some(builder) = current.take() {
        out.push(builder.finish(last_line)?);
    }
    reject_ambiguous(&out)?;
    Ok(out)
}

/// Can `a` and `b` (exact ids or trailing-`*` prefix globs) both match
/// one cell id?
fn patterns_overlap(a: &str, b: &str) -> bool {
    match (a.strip_suffix('*'), b.strip_suffix('*')) {
        (None, None) => a == b,
        (Some(pa), None) => b.starts_with(pa),
        (None, Some(pb)) => a.starts_with(pb),
        (Some(pa), Some(pb)) => pa.starts_with(pb) || pb.starts_with(pa),
    }
}

/// Reject a table where two rows could both claim the same (cell,
/// profile) — first-match-wins would then silently decide which
/// documented mechanism a failure gets attributed to. Two rows may
/// share a cell only when each carries a DIFFERENT `failure_contains`.
fn reject_ambiguous(expectations: &[Expectation]) -> Result<(), String> {
    for (i, a) in expectations.iter().enumerate() {
        for b in &expectations[i + 1..] {
            if a.profile != b.profile || !patterns_overlap(&a.cell, &b.cell) {
                continue;
            }
            let distinct = matches!((&a.failure_contains, &b.failure_contains),
                (Some(x), Some(y)) if x != y);
            if !distinct {
                return Err(format!(
                    "ambiguous expectations: cell {:?} and cell {:?} (profile {:?}) can both match one \
                     cell; give each a distinct failure_contains or remove one",
                    a.cell, b.cell, a.profile
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Matching + merge
// ---------------------------------------------------------------------

/// Does `pattern` (an [`Expectation::cell`], already validated by
/// [`validate_cell_pattern`]) match cell id `id`? A trailing `*` is a
/// prefix match; anything else is a literal match.
fn cell_pattern_matches(pattern: &str, id: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => id.starts_with(prefix),
        None => pattern == id,
    }
}

/// First expectation (in file order) whose `cell` pattern and exact
/// `profile` both match. Ambiguity is NOT resolved by file order here —
/// [`parse_expectations`]'s [`reject_ambiguous`] pass already rejected
/// (hard parse error) any table where two rows share a `profile` and
/// overlapping `cell` patterns, unless both carry a distinct
/// `failure_contains`. So by the time this function runs, two rows can
/// only share a (cell, profile) when `failure_text` (the `FAIL`-matching
/// call site) picks between them by substring — file order only matters
/// as an arbitrary tie-break on the `PASS`-staleness call site
/// (`failure_text: None`), where `failure_contains` is never applied and
/// both surviving rows would otherwise match identically.
///
/// `failure_text` distinguishes the two call sites in [`build_results`]:
/// - `Some(joined_failures)` (the `FAIL`-matching path): an expectation
///   with `failure_contains` set only matches if `failure_text` contains
///   that substring — a `FAIL` on the same `(cell, profile)` whose text
///   does *not* contain it is skipped in favor of a later candidate (or,
///   if none match, surfaces as an unexpected `FAIL`, never silently
///   absorbed by a same-cell expectation written for a *different*
///   failure mode).
/// - `None` (the `PASS`-staleness-check path): `failure_contains` is
///   never applied — staleness is a property of the whole `(cell,
///   profile)` pair, independent of which specific failure text the
///   expectation was originally written against.
///
/// An expectation with no `failure_contains` matches either way, exactly
/// as before this key existed.
fn find_expectation<'a>(
    expectations: &'a [Expectation],
    cell_id: &str,
    cell_profile: &str,
    failure_text: Option<&str>,
) -> Option<&'a Expectation> {
    expectations.iter().find(|e| {
        e.profile == cell_profile
            && cell_pattern_matches(&e.cell, cell_id)
            && match (&e.failure_contains, failure_text) {
                (Some(substr), Some(text)) => text.contains(substr.as_str()),
                (Some(_), None) | (None, _) => true,
            }
    })
}

/// Apply `expectations` to `raw_cells`, producing the merged, tallied
/// [`Results`]. Pure (no I/O) — [`merge`] is the file-driven wrapper
/// around this.
pub fn build_results(
    raw_cells: Vec<RawCell>,
    expectations: &[Expectation],
    meta: serde_json::Value,
) -> Results {
    let mut cells = Vec::with_capacity(raw_cells.len());
    let mut stale = Vec::new();

    for raw in raw_cells {
        let (verdict, known_flaky, expectation_reason, expectation_ref) = match raw.verdict {
            RawVerdict::SkippedToolMissing => (Verdict::SkippedToolMissing, false, None, None),
            RawVerdict::Pass => {
                // Staleness is checked by (cell, profile) alone —
                // `failure_contains` never applies here (see
                // find_expectation's doc comment).
                let matched = find_expectation(expectations, &raw.id, &raw.profile, None);
                if let Some(exp) = matched {
                    if exp.verdict == ExpectVerdict::ExpectedUnsupported {
                        // A glob row (e.g. "decode/*") can match more
                        // than one raw cell id at the same profile — if
                        // several of them PASS, that's the same
                        // expectations-file ROW going stale once, not
                        // once per matching cell. Dedupe by the row's
                        // own (cell pattern, profile) identity.
                        let already_flagged = stale.iter().any(|s: &StaleExpectation| {
                            s.cell == exp.cell && s.profile == exp.profile
                        });
                        if !already_flagged {
                            stale.push(StaleExpectation {
                                cell: exp.cell.clone(),
                                profile: exp.profile.clone(),
                                reason: exp.reason.clone(),
                            });
                        }
                    }
                    // A `known_flaky` expectation matching a PASS is
                    // normal (see the module + type docs) — no stale
                    // entry either way.
                }
                (Verdict::Pass, false, None, None)
            }
            RawVerdict::Fail => {
                let failure_text = raw.failures.join("; ");
                let matched =
                    find_expectation(expectations, &raw.id, &raw.profile, Some(&failure_text));
                match matched {
                    Some(exp) => (
                        Verdict::ExpectedUnsupported,
                        exp.verdict == ExpectVerdict::KnownFlaky,
                        Some(exp.reason.clone()),
                        exp.reference.clone(),
                    ),
                    None => (Verdict::Fail, false, None, None),
                }
            }
        };

        cells.push(MergedCell {
            id: raw.id,
            profile: raw.profile,
            peer: raw.peer,
            direction: raw.direction,
            tier: raw.tier,
            verdict,
            known_flaky,
            failures: raw.failures,
            metrics: raw.metrics,
            log: raw.log,
            expectation_reason,
            expectation_ref,
        });
    }

    let summary = Summary {
        total: cells.len(),
        pass: cells.iter().filter(|c| c.verdict == Verdict::Pass).count(),
        fail: cells.iter().filter(|c| c.verdict == Verdict::Fail).count(),
        expected_unsupported: cells
            .iter()
            .filter(|c| c.verdict == Verdict::ExpectedUnsupported)
            .count(),
        skipped_tool_missing: cells
            .iter()
            .filter(|c| c.verdict == Verdict::SkippedToolMissing)
            .count(),
        stale_expectations: stale,
    };

    Results {
        meta,
        cells,
        summary,
        inventory: None,
    }
}

/// Read every `*.json` file in `dir` (sorted by filename, for
/// deterministic output) and parse each as a [`RawCell`].
///
/// A directory-entry read error propagates rather than being skipped:
/// an unreadable entry could hide a written cell, and a silently
/// shrunken census is exactly the "absorbed failure" shape the module
/// doc rules out (same property as the zero-cells guard in [`merge`]).
fn read_cells_dir(dir: &Path) -> Result<Vec<RawCell>, String> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("read_dir entry in {}: {e}", dir.display()))?;
        let p = entry.path();
        if p.extension().and_then(|ext| ext.to_str()) == Some("json") {
            paths.push(p);
        }
    }
    paths.sort();

    let mut cells = Vec::with_capacity(paths.len());
    for path in &paths {
        let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let cell: RawCell =
            serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
        cells.push(cell);
    }
    Ok(cells)
}

/// One (id, profile) pair `run-matrix.sh` DECLARED it would run, written
/// to `inventory.json` before the first cell executes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InventoryCell {
    pub id: String,
    pub profile: String,
}

/// `inventory.json`: the exact cell multiset a run intends to produce,
/// plus the knobs that shaped it. `shape` is `full-159` only when no
/// `--cells`/`--profiles` narrowing was given (the advertised census);
/// anything else is `subset` and the evidence page may not cite it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inventory {
    pub shape: String,
    pub seconds_per_cell: u64,
    pub cells_glob: String,
    pub profiles: Vec<String>,
    pub cells: Vec<InventoryCell>,
    /// Cell ids whose `SKIPPED_TOOL_MISSING` is tolerated (local runs on
    /// a box missing a peer). `interop.yml` never sets this.
    #[serde(default)]
    pub allowed_skips: Vec<String>,
    #[serde(default)]
    pub tools: serde_json::Value,
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
    pub allowed_skips: Vec<String>,
}

pub const SHAPE_FULL: &str = "full-159";
pub const SHAPE_SUBSET: &str = "subset";

/// The declared cell count a `full-159` inventory must have — the number
/// embedded in [`SHAPE_FULL`]'s own name, enforced here so a caller can't
/// claim the advertised full census while actually declaring some other
/// count under that label.
pub const FULL_SHAPE_CELL_COUNT: usize = 159;

pub fn parse_inventory(text: &str) -> Result<Inventory, String> {
    let inv: Inventory = serde_json::from_str(text).map_err(|e| format!("inventory.json: {e}"))?;
    if inv.shape != SHAPE_FULL && inv.shape != SHAPE_SUBSET {
        return Err(format!(
            "inventory.json: shape must be {SHAPE_FULL:?} or {SHAPE_SUBSET:?}, got {:?}",
            inv.shape
        ));
    }
    if inv.shape == SHAPE_FULL && inv.cells.len() != FULL_SHAPE_CELL_COUNT {
        return Err(format!(
            "inventory.json: shape {SHAPE_FULL:?} must declare exactly {FULL_SHAPE_CELL_COUNT} \
             cells, got {}",
            inv.cells.len()
        ));
    }
    // Every declared (id, profile) exactly once: the merge compares
    // multisets, so a duplicated declaration would be satisfied by a
    // duplicated result — 159 copies of one cell must never pass as
    // the full census just because there are 159 of them.
    let mut seen = std::collections::BTreeSet::new();
    for c in &inv.cells {
        if !seen.insert((c.id.as_str(), c.profile.as_str())) {
            return Err(format!(
                "inventory.json: duplicate declared cell {} ({})",
                c.id, c.profile
            ));
        }
        if crate::profiles::by_name(&c.profile).is_none() {
            return Err(format!(
                "inventory.json: cell {} names unknown profile {:?}",
                c.id, c.profile
            ));
        }
    }
    if inv.shape == SHAPE_FULL {
        let declared: std::collections::BTreeSet<&str> =
            inv.profiles.iter().map(String::as_str).collect();
        let canonical: std::collections::BTreeSet<&str> =
            crate::profiles::all().iter().map(|p| p.name).collect();
        if declared != canonical {
            return Err(format!(
                "inventory.json: shape {SHAPE_FULL:?} must list every profile {canonical:?}, got \
                 {declared:?}"
            ));
        }
    }
    Ok(inv)
}

/// Compare the produced cells against the declared inventory as an exact
/// multiset: every declared (id, profile) exactly once, nothing else, and
/// no `SKIPPED_TOOL_MISSING` outside `allowed_skips`. Reports EVERY
/// problem in one error so a broken run is diagnosed in one read.
pub fn check_inventory(raw_cells: &[RawCell], inv: &Inventory) -> Result<(), String> {
    let mut declared: BTreeMap<InventoryCell, usize> = BTreeMap::new();
    for c in &inv.cells {
        *declared.entry(c.clone()).or_insert(0) += 1;
    }
    let mut produced: BTreeMap<InventoryCell, usize> = BTreeMap::new();
    for c in raw_cells {
        *produced
            .entry(InventoryCell {
                id: c.id.clone(),
                profile: c.profile.clone(),
            })
            .or_insert(0) += 1;
    }
    let mut problems = Vec::new();
    for (cell, &n) in &declared {
        match produced.get(cell).copied().unwrap_or(0) {
            0 => problems.push(format!("missing: {} ({})", cell.id, cell.profile)),
            m if m < n => problems.push(format!(
                "missing: {} ({}) x{} (declared {n}, produced {m})",
                cell.id,
                cell.profile,
                n - m
            )),
            m if m > n => problems.push(format!(
                "duplicate: {} ({}) x{m} (declared {n})",
                cell.id, cell.profile
            )),
            _ => {}
        }
    }
    for cell in produced.keys() {
        if !declared.contains_key(cell) {
            problems.push(format!("extra: {} ({})", cell.id, cell.profile));
        }
    }
    for c in raw_cells {
        if c.verdict == RawVerdict::SkippedToolMissing
            && !inv.allowed_skips.iter().any(|s| s == &c.id)
        {
            problems.push(format!("undeclared skip: {} ({})", c.id, c.profile));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "inventory mismatch ({} declared, {} produced): {}",
            inv.cells.len(),
            raw_cells.len(),
            problems.join("; ")
        ))
    }
}

/// `report merge --cells-dir DIR --expectations FILE --meta FILE
/// --inventory FILE --out results.json`: read every per-cell JSON file
/// in `cells_dir`, apply `expectations_path`'s expectations, embed
/// `meta_path`'s contents verbatim, validate the produced cells against
/// `inventory_path`'s declared multiset (see [`check_inventory`]), and
/// write the result to `out_path`. Returns the same [`Results`] that was
/// written — the caller decides the process exit code from
/// `results.summary.fail` and `results.summary.stale_expectations` (see
/// the module doc).
///
/// Errors (rather than returning an empty, trivially-all-PASS
/// [`Results`]) if `cells_dir` contains zero `*.json` files, or if the
/// produced cells don't exactly match `inventory_path`'s declared
/// multiset — the same "never silently absorbed" property the module
/// doc describes for individual failures also has to hold for the
/// degenerate case of an orchestrator that crashed before writing any
/// cell at all, a `--cells-dir` typo, or a run that silently dropped or
/// duplicated cells. Neither error writes `out_path`.
pub fn merge(
    cells_dir: &Path,
    expectations_path: &Path,
    meta_path: &Path,
    inventory_path: &Path,
    out_path: &Path,
) -> Result<Results, String> {
    let expectations_text = fs::read_to_string(expectations_path)
        .map_err(|e| format!("read {}: {e}", expectations_path.display()))?;
    let expectations = parse_expectations(&expectations_text)
        .map_err(|e| format!("{}: {e}", expectations_path.display()))?;

    let meta_text =
        fs::read_to_string(meta_path).map_err(|e| format!("read {}: {e}", meta_path.display()))?;
    let meta: serde_json::Value = serde_json::from_str(&meta_text)
        .map_err(|e| format!("parse {}: {e}", meta_path.display()))?;

    let inventory_text = fs::read_to_string(inventory_path)
        .map_err(|e| format!("read {}: {e}", inventory_path.display()))?;
    let inventory = parse_inventory(&inventory_text)?;

    let raw_cells = read_cells_dir(cells_dir)?;
    if raw_cells.is_empty() {
        return Err(format!(
            "no *.json cell files found in {} — refusing to write a clean all-PASS report for \
             zero cells (an empty --cells-dir most likely means the orchestrator crashed \
             before writing any cell, or the path is wrong)",
            cells_dir.display()
        ));
    }
    check_inventory(&raw_cells, &inventory)?;

    let mut results = build_results(raw_cells, &expectations, meta);
    results.inventory = Some(InventorySummary {
        shape: inventory.shape.clone(),
        declared_cells: inventory.cells.len(),
        allowed_skips: inventory.allowed_skips.clone(),
    });

    let json = serde_json::to_string_pretty(&results).expect("Results always serializes");
    fs::write(out_path, json).map_err(|e| format!("write {}: {e}", out_path.display()))?;

    Ok(results)
}

// ---------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------

/// Escape a value about to be interpolated into a single GFM table
/// cell. A literal `|` in a `FAIL` failure message or an
/// `expectations.toml` `reason` field would otherwise read as a new
/// column boundary, and an embedded newline would split one logical
/// row across several visual lines — both corrupt not just the
/// published markdown file but the CI step-summary render it's pasted
/// into verbatim. Backslash-escape `|` (GFM's own escape for a literal
/// pipe) and fold newlines into `<br>` (GFM's own in-cell-multiline
/// convention) so the table stays well-formed regardless of what a
/// peer tool's raw error text or a hand-written reason string
/// contains.
fn escape_markdown_table_cell(s: &str) -> String {
    s.replace('|', "\\|")
        .replace("\r\n", "\n")
        .replace('\n', "<br>")
}

/// Verdict cell text and notes-column text for one [`MergedCell`].
fn render_verdict(cell: &MergedCell) -> (&'static str, String) {
    let (label, notes) = match cell.verdict {
        Verdict::Pass => ("✅ PASS", String::new()),
        Verdict::Fail => ("❌ FAIL", cell.failures.join("; ")),
        Verdict::ExpectedUnsupported => {
            let label = if cell.known_flaky {
                "⚠️ KNOWN-FLAKY"
            } else {
                "⚠️ EXPECTED-UNSUPPORTED"
            };
            let mut notes = cell.expectation_reason.clone().unwrap_or_default();
            if let Some(r) = &cell.expectation_ref {
                notes.push_str(&format!(" ({r})"));
            }
            (label, notes)
        }
        Verdict::SkippedToolMissing => ("⏭ SKIPPED", cell.failures.join("; ")),
    };
    let notes = match (declared_relaxation(cell), notes.is_empty()) {
        (None, _) => notes,
        (Some(d), true) => d,
        (Some(d), false) => format!("{d}; {notes}"),
    };
    (label, escape_markdown_table_cell(&notes))
}

/// The relaxation a cell's receiver ran under, as the row declares it:
/// the profile the capture was judged against when it is not the one
/// the cell is recorded under, and the verdicts skipped. `None` for a
/// cell judged in full against its own profile.
fn declared_relaxation(cell: &MergedCell) -> Option<String> {
    let m = cell.metrics.as_ref()?;
    let mut parts = Vec::new();
    if let Some(p) = m.judged_profile.as_deref().filter(|p| *p != cell.profile) {
        parts.push(format!("judged against profile `{p}`"));
    }
    // Code spans, so a name such as `program_<n>_wire_media` is not read
    // as an HTML tag by the renderer the table is pasted into.
    if let Some(s) = m.skipped_oracles.as_deref().filter(|s| !s.is_empty()) {
        let names: Vec<String> = s.iter().map(|o| format!("`{o}`")).collect();
        parts.push(format!("skipped oracles: {}", names.join(", ")));
    }
    (!parts.is_empty()).then(|| format!("declared: {}", parts.join("; ")))
}

/// Render `results` as the published markdown evidence page: a `**Meta**`
/// header (sorted `results.meta` keys), a `**Summary**` counts block, one
/// verdict table per axis (the cell id's segment before its first `/`,
/// axes in alphabetical order; a group with more than
/// `AXIS_DETAILS_THRESHOLD` rows collapses into a `<details>` block),
/// and — only if nonempty — a `**Stale expectations**` warnings section.
pub fn render_markdown(results: &Results) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    writeln!(out, "# Interop Report").unwrap();
    if let Some(inv) = &results.inventory {
        writeln!(
            out,
            "Inventory: {} ({} declared cells)",
            inv.shape, inv.declared_cells
        )
        .unwrap();
    }
    writeln!(out).unwrap();

    writeln!(out, "**Meta**").unwrap();
    writeln!(out).unwrap();
    if let serde_json::Value::Object(map) = &results.meta {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for k in keys {
            let rendered = match &map[k] {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            writeln!(out, "- {k}: {rendered}").unwrap();
        }
    }
    writeln!(out).unwrap();

    writeln!(out, "**Summary**").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "- Total: {}", results.summary.total).unwrap();
    writeln!(out, "- PASS: {}", results.summary.pass).unwrap();
    writeln!(out, "- FAIL: {}", results.summary.fail).unwrap();
    writeln!(
        out,
        "- EXPECTED-UNSUPPORTED: {}",
        results.summary.expected_unsupported
    )
    .unwrap();
    writeln!(out, "- SKIPPED: {}", results.summary.skipped_tool_missing).unwrap();
    writeln!(out).unwrap();

    let mut axes: BTreeMap<&str, Vec<&MergedCell>> = BTreeMap::new();
    for cell in &results.cells {
        let axis = cell
            .id
            .split('/')
            .next()
            .expect("str::split always yields at least one item");
        axes.entry(axis).or_default().push(cell);
    }

    for (axis, rows) in &axes {
        let wrap = rows.len() > AXIS_DETAILS_THRESHOLD;
        if wrap {
            writeln!(
                out,
                "<details><summary>{axis} ({} rows)</summary>",
                rows.len()
            )
            .unwrap();
            writeln!(out).unwrap();
        } else {
            writeln!(out, "## {axis}").unwrap();
            writeln!(out).unwrap();
        }
        writeln!(
            out,
            "| Cell | Profile | Peer | Direction | Tier | Verdict | Notes |"
        )
        .unwrap();
        writeln!(out, "| --- | --- | --- | --- | --- | --- | --- |").unwrap();
        for cell in rows {
            let (label, notes) = render_verdict(cell);
            writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} | {} |",
                cell.id, cell.profile, cell.peer, cell.direction, cell.tier, label, notes
            )
            .unwrap();
        }
        writeln!(out).unwrap();
        if wrap {
            writeln!(out, "</details>").unwrap();
            writeln!(out).unwrap();
        }
    }

    if !results.summary.stale_expectations.is_empty() {
        writeln!(out, "**Stale expectations**").unwrap();
        writeln!(out).unwrap();
        for s in &results.summary.stale_expectations {
            writeln!(
                out,
                "- `{}` (profile `{}`): {} — STALE, merge FAILED: matched cell now PASSes; remove this row.",
                s.cell, s.profile, s.reason
            )
            .unwrap();
        }
    }

    out
}

/// `report render --in results.json --out results.md`: read a
/// [`Results`] JSON document from `in_path`, render it via
/// [`render_markdown`], and write the markdown to `out_path`. Returns
/// the rendered markdown (the `--github-summary` CLI flag reuses it via
/// [`append_github_summary`] without a re-read).
pub fn render(in_path: &Path, out_path: &Path) -> Result<String, String> {
    let text =
        fs::read_to_string(in_path).map_err(|e| format!("read {}: {e}", in_path.display()))?;
    let results: Results =
        serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", in_path.display()))?;
    let md = render_markdown(&results);
    fs::write(out_path, &md).map_err(|e| format!("write {}: {e}", out_path.display()))?;
    Ok(md)
}

/// Append `markdown` to the file at `path`, creating it if it doesn't
/// exist yet. Split out from [`render`] so `--github-summary`'s
/// `GITHUB_STEP_SUMMARY` env-var lookup (which this module does not do
/// itself, keeping it testable without touching real process state)
/// stays in the CLI layer, while the append behavior itself stays
/// testable here against an ordinary temp file.
pub fn append_github_summary(path: &Path, markdown: &str) -> Result<(), String> {
    use std::io::Write as _;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    file.write_all(markdown.as_bytes())
        .map_err(|e| format!("append {}: {e}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------
// `report soak`
// ---------------------------------------------------------------------

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
/// and corruption verdicts are reported not applicable for it).
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
///   `soak`'s internal `expected_drop_fraction` helper instead models
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

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_cell(id: &str, profile: &str, verdict: RawVerdict) -> RawCell {
        RawCell {
            id: id.to_string(),
            profile: profile.to_string(),
            peer: "ffmpeg".to_string(),
            direction: "recv".to_string(),
            tier: "remux".to_string(),
            verdict,
            failures: Vec::new(),
            metrics: None,
            log: format!("{id}.log"),
        }
    }

    fn inventory(cells: &[(&str, &str)], allowed_skips: &[&str]) -> Inventory {
        Inventory {
            shape: "subset".to_string(),
            seconds_per_cell: 10,
            cells_glob: "*".to_string(),
            profiles: vec!["baseline".to_string()],
            cells: cells
                .iter()
                .map(|(id, p)| InventoryCell {
                    id: id.to_string(),
                    profile: p.to_string(),
                })
                .collect(),
            allowed_skips: allowed_skips.iter().map(|s| s.to_string()).collect(),
            tools: serde_json::json!({}),
        }
    }

    #[test]
    fn inventory_exact_match_passes() {
        let cells = vec![
            raw_cell("udp/us-to-tsp", "baseline", RawVerdict::Pass),
            raw_cell("udp/tsp-to-us", "baseline", RawVerdict::Fail),
        ];
        let inv = inventory(
            &[("udp/us-to-tsp", "baseline"), ("udp/tsp-to-us", "baseline")],
            &[],
        );
        check_inventory(&cells, &inv).expect("exact multiset must pass");
    }

    #[test]
    fn inventory_missing_cell_is_an_error_naming_it() {
        let cells = vec![raw_cell("udp/us-to-tsp", "baseline", RawVerdict::Pass)];
        let inv = inventory(
            &[("udp/us-to-tsp", "baseline"), ("udp/tsp-to-us", "baseline")],
            &[],
        );
        let e = check_inventory(&cells, &inv).unwrap_err();
        assert!(e.contains("missing") && e.contains("udp/tsp-to-us"), "{e}");
    }

    #[test]
    fn inventory_duplicate_cell_is_an_error() {
        let cells = vec![
            raw_cell("udp/us-to-tsp", "baseline", RawVerdict::Pass),
            raw_cell("udp/us-to-tsp", "baseline", RawVerdict::Pass),
        ];
        let inv = inventory(&[("udp/us-to-tsp", "baseline")], &[]);
        let e = check_inventory(&cells, &inv).unwrap_err();
        assert!(
            e.contains("duplicate") && e.contains("udp/us-to-tsp"),
            "{e}"
        );
    }

    #[test]
    fn inventory_underproduced_multiplicity_is_an_error() {
        let cells = vec![raw_cell("udp/us-to-tsp", "baseline", RawVerdict::Pass)];
        let inv = inventory(
            &[("udp/us-to-tsp", "baseline"), ("udp/us-to-tsp", "baseline")],
            &[],
        );
        let e = check_inventory(&cells, &inv).unwrap_err();
        assert!(e.contains("missing") && e.contains("udp/us-to-tsp"), "{e}");
    }

    #[test]
    fn inventory_extra_cell_is_an_error() {
        let cells = vec![
            raw_cell("udp/us-to-tsp", "baseline", RawVerdict::Pass),
            raw_cell("srt/us-to-tsp", "baseline", RawVerdict::Pass),
        ];
        let inv = inventory(&[("udp/us-to-tsp", "baseline")], &[]);
        let e = check_inventory(&cells, &inv).unwrap_err();
        assert!(e.contains("extra") && e.contains("srt/us-to-tsp"), "{e}");
    }

    #[test]
    fn inventory_same_id_different_profile_is_a_distinct_cell() {
        let cells = vec![raw_cell("decode/mpv/audio", "audio", RawVerdict::Pass)];
        let inv = inventory(&[("decode/mpv/audio", "baseline")], &[]);
        let e = check_inventory(&cells, &inv).unwrap_err();
        assert!(e.contains("missing") && e.contains("extra"), "{e}");
    }

    #[test]
    fn undeclared_skip_is_an_error_but_an_allowed_skip_passes() {
        let cells = vec![raw_cell(
            "decode/mpv/baseline",
            "baseline",
            RawVerdict::SkippedToolMissing,
        )];
        let inv = inventory(&[("decode/mpv/baseline", "baseline")], &[]);
        let e = check_inventory(&cells, &inv).unwrap_err();
        assert!(
            e.contains("undeclared skip") && e.contains("decode/mpv/baseline"),
            "{e}"
        );
        let inv = inventory(
            &[("decode/mpv/baseline", "baseline")],
            &["decode/mpv/baseline"],
        );
        check_inventory(&cells, &inv).expect("declared skip passes");
    }

    #[test]
    fn parse_inventory_rejects_unknown_shape() {
        let e = parse_inventory(
            r#"{"shape":"bogus","seconds_per_cell":10,"cells_glob":"*","profiles":[],"cells":[],"allowed_skips":[],"tools":{}}"#,
        )
        .unwrap_err();
        assert!(e.contains("shape"), "{e}");
    }

    #[test]
    fn parse_inventory_rejects_full_shape_with_wrong_count() {
        let e = parse_inventory(
            r#"{"shape":"full-159","seconds_per_cell":10,"cells_glob":"*","profiles":[],"cells":[{"id":"a","profile":"baseline"}],"allowed_skips":[],"tools":{}}"#,
        )
        .unwrap_err();
        assert!(e.contains("159"), "{e}");
        assert!(e.contains('1'), "{e}"); // the actual declared count, 1, should be named
    }

    fn inventory_json(shape: &str, cells: &[(&str, &str)], profiles: &[&str]) -> String {
        let cells: Vec<String> = cells
            .iter()
            .map(|(id, p)| format!(r#"{{"id":"{id}","profile":"{p}"}}"#))
            .collect();
        let profiles: Vec<String> = profiles.iter().map(|p| format!("\"{p}\"")).collect();
        format!(
            r#"{{"shape":"{shape}","seconds_per_cell":10,"cells_glob":"*","profiles":[{}],"cells":[{}],"allowed_skips":[],"tools":{{}}}}"#,
            profiles.join(","),
            cells.join(",")
        )
    }

    /// 159 declarations of ONE key satisfied the full shape's cardinality
    /// check, and 159 produced copies of it would have matched the
    /// multiset.
    #[test]
    fn parse_inventory_rejects_duplicate_declarations() {
        let same = vec![("udp/us-to-tsp", "baseline"); 159];
        let e = parse_inventory(&inventory_json("full-159", &same, &["baseline"])).unwrap_err();
        assert!(
            e.contains("duplicate declared cell") && e.contains("udp/us-to-tsp (baseline)"),
            "{e}"
        );
        // Subsets are held to uniqueness too.
        let e = parse_inventory(&inventory_json(
            "subset",
            &[("udp/us-to-tsp", "baseline"), ("udp/us-to-tsp", "baseline")],
            &["baseline"],
        ))
        .unwrap_err();
        assert!(e.contains("duplicate declared cell"), "{e}");
        // Same id, different profile, is two cells.
        parse_inventory(&inventory_json(
            "subset",
            &[
                ("decode/mpv/audio", "audio"),
                ("decode/mpv/audio", "baseline"),
            ],
            &["audio", "baseline"],
        ))
        .expect("distinct (id, profile) pairs");
    }

    #[test]
    fn parse_inventory_rejects_unknown_profiles_and_a_partial_full_shape() {
        let e = parse_inventory(&inventory_json(
            "subset",
            &[("udp/us-to-tsp", "bogus")],
            &["bogus"],
        ))
        .unwrap_err();
        assert!(e.contains("unknown profile") && e.contains("bogus"), "{e}");
        // 159 distinct keys but a full shape that only lists one profile.
        let ids: Vec<String> = (0..159).map(|i| format!("udp/cell-{i}")).collect();
        let cells: Vec<(&str, &str)> = ids.iter().map(|s| (s.as_str(), "baseline")).collect();
        let e = parse_inventory(&inventory_json("full-159", &cells, &["baseline"])).unwrap_err();
        assert!(e.contains("must list every profile"), "{e}");
    }

    fn expectation(cell: &str, profile: &str, verdict: ExpectVerdict, reason: &str) -> Expectation {
        Expectation {
            cell: cell.to_string(),
            profile: profile.to_string(),
            verdict,
            reason: reason.to_string(),
            reference: None,
            failure_contains: None,
        }
    }

    fn raw_cell_with_failures(
        id: &str,
        profile: &str,
        verdict: RawVerdict,
        failures: &[&str],
    ) -> RawCell {
        RawCell {
            failures: failures.iter().map(|s| s.to_string()).collect(),
            ..raw_cell(id, profile, verdict)
        }
    }

    fn expectation_with_failure_contains(
        cell: &str,
        profile: &str,
        verdict: ExpectVerdict,
        reason: &str,
        failure_contains: &str,
    ) -> Expectation {
        Expectation {
            failure_contains: Some(failure_contains.to_string()),
            ..expectation(cell, profile, verdict, reason)
        }
    }

    // (a) all-PASS cells -> exit-0 summary (summary.fail == 0).
    #[test]
    fn all_pass_cells_yield_zero_fail() {
        let raw = vec![
            raw_cell("decode/ffmpeg", "baseline", RawVerdict::Pass),
            raw_cell("decode/mpv", "baseline", RawVerdict::Pass),
        ];
        let results = build_results(raw, &[], serde_json::json!({}));

        assert_eq!(results.summary.fail, 0);
        assert_eq!(results.summary.pass, 2);
        assert_eq!(results.summary.total, 2);
        assert!(results.summary.stale_expectations.is_empty());
    }

    // (b) one unexpected FAIL -> nonzero + named (findable by id in the
    // merged cells with verdict Fail).
    #[test]
    fn unexpected_fail_is_nonzero_and_named() {
        let raw = vec![
            raw_cell("decode/ffmpeg", "baseline", RawVerdict::Pass),
            raw_cell("srt/us-to-them", "baseline", RawVerdict::Fail),
        ];
        let results = build_results(raw, &[], serde_json::json!({}));

        assert_eq!(results.summary.fail, 1);
        let failed = results
            .cells
            .iter()
            .find(|c| c.id == "srt/us-to-them")
            .expect("failed cell must be present in the merged output");
        assert_eq!(failed.verdict, Verdict::Fail);
    }

    // (c) FAIL matched by expectation -> run passes, cell verdict
    // EXPECTED_UNSUPPORTED.
    #[test]
    fn matched_fail_becomes_expected_unsupported() {
        let raw = vec![raw_cell("decode/mpv", "baseline", RawVerdict::Fail)];
        let exp = expectation(
            "decode/mpv",
            "baseline",
            ExpectVerdict::ExpectedUnsupported,
            "mpv lacks async KLV support",
        );
        let results = build_results(raw, &[exp], serde_json::json!({}));

        assert_eq!(results.summary.fail, 0);
        assert_eq!(results.summary.expected_unsupported, 1);
        assert_eq!(results.cells[0].verdict, Verdict::ExpectedUnsupported);
        assert!(!results.cells[0].known_flaky);
        assert_eq!(
            results.cells[0].expectation_reason.as_deref(),
            Some("mpv lacks async KLV support")
        );
    }

    // --- failure_contains ---

    // A FAIL whose joined failures text contains the expectation's
    // `failure_contains` substring matches, exactly like a plain
    // (cell, profile)-only expectation would.
    #[test]
    fn failure_contains_matches_when_substring_present() {
        let raw = vec![raw_cell_with_failures(
            "srt-live/tsp-to-us",
            "baseline",
            RawVerdict::Fail,
            &["byte-transparent tier: stream_sha256 mismatch (source abc, received def)"],
        )];
        let exp = expectation_with_failure_contains(
            "srt-live/tsp-to-us",
            "baseline",
            ExpectVerdict::ExpectedUnsupported,
            "SRT-specific tail loss",
            "stream_sha256 mismatch",
        );
        let results = build_results(raw, &[exp], serde_json::json!({}));

        assert_eq!(results.summary.fail, 0);
        assert_eq!(results.summary.expected_unsupported, 1);
        assert_eq!(results.cells[0].verdict, Verdict::ExpectedUnsupported);
    }

    // A FAIL on the same (cell, profile) but a DIFFERENT failure mode
    // (text doesn't contain the substring) must NOT be silently
    // absorbed — this is the whole point of the key: mechanism-blind
    // (cell, profile)-only matching would have hidden this as
    // ExpectedUnsupported; failure_contains keeps it a real FAIL.
    #[test]
    fn failure_contains_non_match_stays_fail() {
        let raw = vec![raw_cell_with_failures(
            "srt-live/tsp-to-us",
            "baseline",
            RawVerdict::Fail,
            &["recv FAILed: video AUs: got 0, want >= 168"],
        )];
        let exp = expectation_with_failure_contains(
            "srt-live/tsp-to-us",
            "baseline",
            ExpectVerdict::ExpectedUnsupported,
            "SRT-specific tail loss",
            "stream_sha256 mismatch",
        );
        let results = build_results(raw, &[exp], serde_json::json!({}));

        assert_eq!(results.summary.fail, 1);
        assert_eq!(results.summary.expected_unsupported, 0);
        assert_eq!(results.cells[0].verdict, Verdict::Fail);
    }

    // An expectation with no `failure_contains` key set (the common
    // case, and every pre-existing row) matches a FAIL regardless of
    // its failure text — unchanged behavior from before this key
    // existed.
    #[test]
    fn absent_failure_contains_matches_any_failure_text() {
        let raw = vec![raw_cell_with_failures(
            "decode/mpv",
            "baseline",
            RawVerdict::Fail,
            &["anything at all, unrelated to the expectation's own reason text"],
        )];
        let exp = expectation(
            "decode/mpv",
            "baseline",
            ExpectVerdict::ExpectedUnsupported,
            "mpv lacks async KLV support",
        );
        let results = build_results(raw, &[exp], serde_json::json!({}));

        assert_eq!(results.summary.fail, 0);
        assert_eq!(results.cells[0].verdict, Verdict::ExpectedUnsupported);
    }

    // A `failure_contains` expectation must still catch staleness when
    // its cell PASSes — the substring constraint only narrows FAIL
    // matching, never a PASS staleness check (there's no failure text
    // to test the substring against on a PASS in the first place).
    #[test]
    fn failure_contains_expectation_is_still_stale_on_pass() {
        let raw = vec![raw_cell("srt-live/tsp-to-us", "baseline", RawVerdict::Pass)];
        let exp = expectation_with_failure_contains(
            "srt-live/tsp-to-us",
            "baseline",
            ExpectVerdict::ExpectedUnsupported,
            "SRT-specific tail loss",
            "stream_sha256 mismatch",
        );
        let results = build_results(raw, &[exp], serde_json::json!({}));

        assert_eq!(results.summary.stale_expectations.len(), 1);
        assert_eq!(results.cells[0].verdict, Verdict::Pass);
    }

    // Two candidates for the SAME (cell, profile): the first has a
    // `failure_contains` that does NOT match this FAIL's text, the
    // second is unconstrained. `find_expectation`'s single-predicate
    // `Iterator::find` must skip the non-matching first candidate and
    // resolve via the second, rather than stopping at (and shadowing
    // behind) the first just because cell+profile matched — pins this
    // against a future two-phase (match cell+profile first, filter by
    // failure_contains second) refactor that could silently break it.
    #[test]
    fn non_matching_failure_contains_falls_through_to_next_candidate() {
        let raw = vec![raw_cell_with_failures(
            "srt-live/ffmpeg-to-us",
            "baseline",
            RawVerdict::Fail,
            &["recv FAILed: KLV records: got 0, want >= 56 (10 Hz x 8s x 70% slack)"],
        )];
        let decoy = expectation_with_failure_contains(
            "srt-live/ffmpeg-to-us",
            "baseline",
            ExpectVerdict::ExpectedUnsupported,
            "wrong mechanism — should never be selected",
            "stream_sha256 mismatch",
        );
        let real = expectation(
            "srt-live/ffmpeg-to-us",
            "baseline",
            ExpectVerdict::ExpectedUnsupported,
            "KLV payload truncation",
        );
        let results = build_results(raw, &[decoy, real], serde_json::json!({}));

        assert_eq!(results.summary.fail, 0);
        assert_eq!(results.cells[0].verdict, Verdict::ExpectedUnsupported);
        assert_eq!(
            results.cells[0].expectation_reason.as_deref(),
            Some("KLV payload truncation"),
            "must resolve via the second (matching) candidate, not the first (non-matching, cell+profile-only) one"
        );
    }

    // (d) expectation whose cell PASSes -> stale_expectations entry.
    #[test]
    fn passing_cell_with_expected_unsupported_expectation_is_stale() {
        let raw = vec![raw_cell("decode/mpv", "baseline", RawVerdict::Pass)];
        let exp = expectation(
            "decode/mpv",
            "baseline",
            ExpectVerdict::ExpectedUnsupported,
            "mpv lacks async KLV support",
        );
        let results = build_results(raw, &[exp], serde_json::json!({}));

        assert_eq!(results.summary.stale_expectations.len(), 1);
        assert_eq!(results.summary.stale_expectations[0].cell, "decode/mpv");
        assert_eq!(results.cells[0].verdict, Verdict::Pass);
    }

    #[test]
    fn glob_row_matching_two_passing_cells_is_one_stale_entry_not_two() {
        let raw = vec![
            raw_cell("decode/mpv", "baseline", RawVerdict::Pass),
            raw_cell("decode/ffmpeg", "baseline", RawVerdict::Pass),
        ];
        let exp = expectation(
            "decode/*",
            "baseline",
            ExpectVerdict::ExpectedUnsupported,
            "decode gap",
        );
        let results = build_results(raw, &[exp], serde_json::json!({}));

        // Both decode/mpv and decode/ffmpeg PASS and both match the same
        // "decode/*" row — that's one row gone stale, not two.
        assert_eq!(
            results.summary.stale_expectations.len(),
            1,
            "{:?}",
            results.summary.stale_expectations
        );
        assert_eq!(results.summary.stale_expectations[0].cell, "decode/*");
    }

    // (e) glob decode/* matches decode/mpv.
    #[test]
    fn glob_pattern_matches_prefix() {
        let raw = vec![raw_cell("decode/mpv", "baseline", RawVerdict::Fail)];
        let exp = expectation(
            "decode/*",
            "baseline",
            ExpectVerdict::ExpectedUnsupported,
            "decode gap",
        );
        let results = build_results(raw, &[exp], serde_json::json!({}));

        assert_eq!(results.cells[0].verdict, Verdict::ExpectedUnsupported);
        assert_eq!(results.summary.fail, 0);
    }

    // (f) SKIPPED_TOOL_MISSING counted, never fatal, never stale-matched
    // (even when a matching expectation exists).
    #[test]
    fn skipped_tool_missing_is_never_fatal_or_stale() {
        let raw = vec![raw_cell(
            "decode/mpv",
            "baseline",
            RawVerdict::SkippedToolMissing,
        )];
        let exp = expectation(
            "decode/mpv",
            "baseline",
            ExpectVerdict::ExpectedUnsupported,
            "would-be-stale reason",
        );
        let results = build_results(raw, &[exp], serde_json::json!({}));

        assert_eq!(results.summary.fail, 0);
        assert_eq!(results.summary.skipped_tool_missing, 1);
        assert_eq!(results.cells[0].verdict, Verdict::SkippedToolMissing);
        assert!(results.summary.stale_expectations.is_empty());
    }

    #[test]
    fn known_flaky_fail_is_expected_unsupported_and_flagged() {
        let raw = vec![raw_cell("srt/flaky", "baseline", RawVerdict::Fail)];
        let exp = expectation(
            "srt/flaky",
            "baseline",
            ExpectVerdict::KnownFlaky,
            "intermittent timeout, see TICKET-9",
        );
        let results = build_results(raw, &[exp], serde_json::json!({}));

        assert_eq!(results.summary.fail, 0);
        assert_eq!(results.cells[0].verdict, Verdict::ExpectedUnsupported);
        assert!(results.cells[0].known_flaky);
    }

    #[test]
    fn known_flaky_pass_is_normal_never_stale() {
        let raw = vec![raw_cell("srt/flaky", "baseline", RawVerdict::Pass)];
        let exp = expectation(
            "srt/flaky",
            "baseline",
            ExpectVerdict::KnownFlaky,
            "intermittent timeout, see TICKET-9",
        );
        let results = build_results(raw, &[exp], serde_json::json!({}));

        assert_eq!(results.cells[0].verdict, Verdict::Pass);
        assert!(
            results.summary.stale_expectations.is_empty(),
            "a known_flaky expectation matching a PASS must never be reported stale"
        );
    }

    #[test]
    fn profile_mismatch_does_not_match() {
        let raw = vec![raw_cell("decode/mpv", "hevc", RawVerdict::Fail)];
        let exp = expectation(
            "decode/mpv",
            "baseline",
            ExpectVerdict::ExpectedUnsupported,
            "wrong profile",
        );
        let results = build_results(raw, &[exp], serde_json::json!({}));

        assert_eq!(results.cells[0].verdict, Verdict::Fail);
        assert_eq!(results.summary.fail, 1);
    }

    #[test]
    fn cell_pattern_matches_exact_and_trailing_glob() {
        assert!(cell_pattern_matches("decode/mpv", "decode/mpv"));
        assert!(!cell_pattern_matches("decode/mpv", "decode/mpv2"));
        assert!(cell_pattern_matches("decode/*", "decode/mpv"));
        assert!(cell_pattern_matches("decode/*", "decode/"));
        assert!(!cell_pattern_matches("decode/*", "encode/mpv"));
    }

    /// A cell judged against another profile with oracles skipped
    /// declares both on its own row: the keys `run-matrix.sh` adds to
    /// the cell's metrics survive `report merge` and `report render`
    /// prints them, on a PASS and beside a FAIL's reasons.
    #[test]
    fn a_relaxed_cell_declares_its_judged_profile_and_skipped_oracles() {
        let cell_json = |verdict: &str, failures: &str| {
            format!(
                r#"{{"id": "rtsp-publish/gst-push-es-klv", "profile": "baseline",
                    "peer": "gst-launch-1.0", "direction": "recv", "tier": "remux",
                    "verdict": "{verdict}", "failures": [{failures}],
                    "metrics": {{"video_aus": 240, "keyframes": 8, "klv_records": 240,
                        "klv_set_sha256": "aa", "audio_frames": 0, "programs_seen": 1,
                        "pts_monotonic": true, "misp_sei_seen": false, "bytes": 1,
                        "stream_sha256": "00", "judged_profile": "klv-sync",
                        "skipped_oracles": ["pmt_stream_type_", "KLV carriage"]}},
                    "log": "logs/x.log"}}"#
            )
        };
        let raw: RawCell = serde_json::from_str(&cell_json("PASS", "")).unwrap();
        let results = build_results(vec![raw], &[], serde_json::json!({}));
        let m = results.cells[0].metrics.as_ref().unwrap();
        assert_eq!(m.judged_profile.as_deref(), Some("klv-sync"));
        let round: Results =
            serde_json::from_str(&serde_json::to_string(&results).unwrap()).unwrap();
        let md = render_markdown(&round);
        assert!(
            md.contains(
                "| ✅ PASS | declared: judged against profile `klv-sync`; skipped oracles: \
                 `pmt_stream_type_`, `KLV carriage` |"
            ),
            "{md}"
        );

        let raw: RawCell =
            serde_json::from_str(&cell_json("FAIL", r#""KLV set digest mismatch""#)).unwrap();
        let md = render_markdown(&build_results(vec![raw], &[], serde_json::json!({})));
        assert!(
            md.contains(
                "skipped oracles: `pmt_stream_type_`, `KLV carriage`; KLV set digest mismatch"
            ),
            "{md}"
        );
    }

    // (g) render golden: small fixed Results -> exact expected markdown.
    #[test]
    fn render_golden() {
        let results = Results {
            meta: serde_json::json!({"host": "ci-runner-1", "profile_seconds": 30}),
            cells: vec![
                MergedCell {
                    id: "decode/ffmpeg".to_string(),
                    profile: "baseline".to_string(),
                    peer: "ffmpeg".to_string(),
                    direction: "recv".to_string(),
                    tier: "remux".to_string(),
                    verdict: Verdict::Pass,
                    known_flaky: false,
                    failures: Vec::new(),
                    metrics: None,
                    log: "decode-ffmpeg.log".to_string(),
                    expectation_reason: None,
                    expectation_ref: None,
                },
                MergedCell {
                    id: "decode/mpv".to_string(),
                    profile: "baseline".to_string(),
                    peer: "mpv".to_string(),
                    direction: "recv".to_string(),
                    tier: "remux".to_string(),
                    verdict: Verdict::ExpectedUnsupported,
                    known_flaky: false,
                    failures: Vec::new(),
                    metrics: None,
                    log: "decode-mpv.log".to_string(),
                    expectation_reason: Some("mpv lacks async KLV support".to_string()),
                    expectation_ref: Some("TICKET-123".to_string()),
                },
                MergedCell {
                    id: "srt/us-to-ffmpeg".to_string(),
                    profile: "baseline".to_string(),
                    peer: "ffmpeg".to_string(),
                    direction: "send".to_string(),
                    tier: "wire".to_string(),
                    verdict: Verdict::Fail,
                    known_flaky: false,
                    failures: vec!["timeout waiting for connect".to_string()],
                    metrics: None,
                    log: "srt-us-to-ffmpeg.log".to_string(),
                    expectation_reason: None,
                    expectation_ref: None,
                },
            ],
            summary: Summary {
                total: 3,
                pass: 1,
                fail: 1,
                expected_unsupported: 1,
                skipped_tool_missing: 0,
                stale_expectations: vec![StaleExpectation {
                    cell: "decode/old".to_string(),
                    profile: "baseline".to_string(),
                    reason: "historic gap, now fixed".to_string(),
                }],
            },
            inventory: None,
        };

        let golden = "\
# Interop Report

**Meta**

- host: ci-runner-1
- profile_seconds: 30

**Summary**

- Total: 3
- PASS: 1
- FAIL: 1
- EXPECTED-UNSUPPORTED: 1
- SKIPPED: 0

## decode

| Cell | Profile | Peer | Direction | Tier | Verdict | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| decode/ffmpeg | baseline | ffmpeg | recv | remux | ✅ PASS |  |
| decode/mpv | baseline | mpv | recv | remux | ⚠️ EXPECTED-UNSUPPORTED | mpv lacks async KLV support (TICKET-123) |

## srt

| Cell | Profile | Peer | Direction | Tier | Verdict | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| srt/us-to-ffmpeg | baseline | ffmpeg | send | wire | ❌ FAIL | timeout waiting for connect |

**Stale expectations**

- `decode/old` (profile `baseline`): historic gap, now fixed — STALE, merge FAILED: matched cell now PASSes; remove this row.
";

        let rendered = render_markdown(&results);
        assert_eq!(rendered, golden, "rendered markdown:\n{rendered}");

        // A `Some` inventory renders its own line — the `None` case above
        // must stay byte-identical to the golden text.
        let with_inventory = Results {
            inventory: Some(InventorySummary {
                shape: SHAPE_SUBSET.to_string(),
                declared_cells: 3,
                allowed_skips: Vec::new(),
            }),
            ..results.clone()
        };
        let rendered_with_inventory = render_markdown(&with_inventory);
        assert!(
            rendered_with_inventory.contains("Inventory: subset (3 declared cells)"),
            "expected an Inventory line when inventory is Some:\n{rendered_with_inventory}"
        );
    }

    #[test]
    fn axis_group_wraps_in_details_beyond_threshold() {
        let mut cells = Vec::new();
        for i in 0..(AXIS_DETAILS_THRESHOLD + 1) {
            cells.push(MergedCell {
                id: format!("decode/tool{i}"),
                profile: "baseline".to_string(),
                peer: format!("tool{i}"),
                direction: "recv".to_string(),
                tier: "remux".to_string(),
                verdict: Verdict::Pass,
                known_flaky: false,
                failures: Vec::new(),
                metrics: None,
                log: format!("tool{i}.log"),
                expectation_reason: None,
                expectation_ref: None,
            });
        }
        let results = Results {
            meta: serde_json::json!({}),
            summary: Summary {
                total: cells.len(),
                pass: cells.len(),
                fail: 0,
                expected_unsupported: 0,
                skipped_tool_missing: 0,
                stale_expectations: Vec::new(),
            },
            cells,
            inventory: None,
        };

        let rendered = render_markdown(&results);
        assert!(
            rendered.contains("<details><summary>decode (16 rows)</summary>"),
            "expected a <details> wrapper for a 16-row group:\n{rendered}"
        );
        assert!(rendered.contains("</details>"));
    }

    #[test]
    fn axis_group_at_threshold_does_not_wrap() {
        let mut cells = Vec::new();
        for i in 0..AXIS_DETAILS_THRESHOLD {
            cells.push(MergedCell {
                id: format!("decode/tool{i}"),
                profile: "baseline".to_string(),
                peer: format!("tool{i}"),
                direction: "recv".to_string(),
                tier: "remux".to_string(),
                verdict: Verdict::Pass,
                known_flaky: false,
                failures: Vec::new(),
                metrics: None,
                log: format!("tool{i}.log"),
                expectation_reason: None,
                expectation_ref: None,
            });
        }
        let results = Results {
            meta: serde_json::json!({}),
            summary: Summary {
                total: cells.len(),
                pass: cells.len(),
                fail: 0,
                expected_unsupported: 0,
                skipped_tool_missing: 0,
                stale_expectations: Vec::new(),
            },
            cells,
            inventory: None,
        };

        let rendered = render_markdown(&results);
        assert!(!rendered.contains("<details>"));
        assert!(rendered.contains("## decode"));
    }

    // A literal `|` or newline in a failure message must not corrupt the
    // GFM table it's interpolated into (see `escape_markdown_table_cell`)
    // — a peer tool's own error text (e.g. a multi-line stderr capture,
    // or a message that itself quotes a `|`-delimited value) is
    // untrusted input to this renderer and must come out as one
    // well-formed table row, not a broken column count or a split row.
    #[test]
    fn failure_text_with_pipe_and_newline_is_escaped_in_the_table() {
        let raw = vec![raw_cell_with_failures(
            "decode/ffmpeg",
            "baseline",
            RawVerdict::Fail,
            &["stderr: `a | b`\nsecond line"],
        )];
        let results = build_results(raw, &[], serde_json::json!({}));

        let rendered = render_markdown(&results);
        // An unescaped embedded newline would split this row across two
        // `lines()` entries, so finding exactly one confirms it stayed a
        // single logical line.
        let row = rendered
            .lines()
            .find(|l| l.starts_with("| decode/ffmpeg "))
            .expect("the cell's row must be present as a single line");
        assert!(
            row.contains("a \\| b"),
            "the literal pipe must be backslash-escaped, not a bare column break:\n{row}"
        );
        assert!(
            row.contains("<br>"),
            "the embedded newline must fold to GFM's <br>:\n{row}"
        );
    }

    // --- Expectations TOML parser ---

    #[test]
    fn parses_a_minimal_valid_expectations_file() {
        let text = "\
# a comment
[[expect]]
cell = \"decode/*\"
profile = \"baseline\"
verdict = \"expected_unsupported\"
reason = \"mpv lacks async KLV support\"
ref = \"TICKET-123\"
failure_contains = \"no async KLV\"

[[expect]]
cell = \"srt/flaky\"
profile = \"baseline\"
verdict = \"known_flaky\"
reason = \"intermittent timeout\"
";
        let parsed = parse_expectations(text).expect("valid file must parse");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].cell, "decode/*");
        assert_eq!(parsed[0].verdict, ExpectVerdict::ExpectedUnsupported);
        assert_eq!(parsed[0].reference.as_deref(), Some("TICKET-123"));
        assert_eq!(parsed[1].verdict, ExpectVerdict::KnownFlaky);
        assert_eq!(parsed[1].reference, None);
        assert_eq!(parsed[0].failure_contains.as_deref(), Some("no async KLV"));
        assert_eq!(parsed[1].failure_contains, None);
    }

    #[test]
    fn expected_unsupported_row_without_failure_contains_is_rejected() {
        let text = "[[expect]]\ncell = \"decode/mpv\"\nprofile = \"baseline\"\n\
                    verdict = \"expected_unsupported\"\nreason = \"mpv lacks async KLV support\"\n";
        let err = parse_expectations(text).unwrap_err();
        assert!(err.contains("failure_contains"), "{err}");
    }

    #[test]
    fn known_flaky_row_may_omit_failure_contains() {
        let text = "[[expect]]\ncell = \"srt/flaky\"\nprofile = \"baseline\"\n\
                    verdict = \"known_flaky\"\nreason = \"intermittent timeout\"\n";
        let parsed =
            parse_expectations(text).expect("known_flaky rows have no single mechanism string");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].failure_contains, None);
    }

    #[test]
    fn parses_the_optional_failure_contains_key() {
        let text = "\
[[expect]]
cell = \"srt-live/tsp-to-us\"
profile = \"baseline\"
verdict = \"expected_unsupported\"
reason = \"SRT-specific tail loss\"
failure_contains = \"stream_sha256 mismatch\"
";
        let parsed = parse_expectations(text).expect("valid file must parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[0].failure_contains.as_deref(),
            Some("stream_sha256 mismatch")
        );
    }

    #[test]
    fn empty_and_all_comment_file_parses_to_no_expectations() {
        let text = "# nothing but comments\n\n# another\n";
        assert_eq!(parse_expectations(text).expect("must parse"), vec![]);
        assert_eq!(parse_expectations("").expect("must parse"), vec![]);
    }

    #[test]
    fn rejects_star_not_at_end_of_cell_pattern() {
        let text = "[[expect]]\ncell = \"decode/*/foo\"\nprofile = \"baseline\"\nverdict = \"expected_unsupported\"\nreason = \"x\"\n";
        let err = parse_expectations(text).unwrap_err();
        assert!(err.contains("line 2"), "error should name the line: {err}");
        assert!(err.contains('*'));
    }

    #[test]
    fn rejects_unquoted_value() {
        let text = "[[expect]]\ncell = decode/mpv\n";
        let err = parse_expectations(text).unwrap_err();
        assert!(err.contains("line 2"), "error should name the line: {err}");
        assert!(err.contains("quoted"));
    }

    #[test]
    fn rejects_value_with_embedded_quote() {
        let text = "[[expect]]\ncell = \"decode/\"mpv\"\"\n";
        let err = parse_expectations(text).unwrap_err();
        assert!(err.contains("line 2"), "error should name the line: {err}");
    }

    #[test]
    fn rejects_unknown_key() {
        let text = "[[expect]]\nbogus = \"x\"\n";
        let err = parse_expectations(text).unwrap_err();
        assert!(err.contains("unknown key"), "{err}");
    }

    #[test]
    fn rejects_kv_line_outside_a_block() {
        let text = "cell = \"decode/mpv\"\n";
        let err = parse_expectations(text).unwrap_err();
        assert!(err.contains("outside of an [[expect]] block"), "{err}");
    }

    #[test]
    fn rejects_missing_required_key() {
        let text = "[[expect]]\ncell = \"decode/mpv\"\nprofile = \"baseline\"\nverdict = \"expected_unsupported\"\n";
        let err = parse_expectations(text).unwrap_err();
        assert!(err.contains("missing required key `reason`"), "{err}");
    }

    #[test]
    fn rejects_unknown_verdict_value() {
        let text = "[[expect]]\ncell = \"decode/mpv\"\nprofile = \"baseline\"\nverdict = \"bogus\"\nreason = \"x\"\n";
        let err = parse_expectations(text).unwrap_err();
        assert!(err.contains("unknown verdict"), "{err}");
    }

    #[test]
    fn rejects_garbage_line() {
        let text = "[[expect]]\nnot a key value line at all\n";
        let err = parse_expectations(text).unwrap_err();
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn two_rows_on_the_same_cell_without_distinct_failure_contains_are_rejected() {
        let text = "[[expect]]\ncell = \"decode/mpv/audio\"\nprofile = \"audio\"\nverdict = \"expected_unsupported\"\nreason = \"a\"\nfailure_contains = \"x\"\n\n\
                    [[expect]]\ncell = \"decode/mpv/*\"\nprofile = \"audio\"\nverdict = \"expected_unsupported\"\nreason = \"b\"\nfailure_contains = \"x\"\n";
        let e = parse_expectations(text).unwrap_err();
        assert!(
            e.contains("ambiguous") && e.contains("decode/mpv/audio") && e.contains("decode/mpv/*"),
            "{e}"
        );
    }

    #[test]
    fn two_rows_on_the_same_cell_with_distinct_failure_contains_are_allowed() {
        let text = "[[expect]]\ncell = \"srt/us-to-tsp\"\nprofile = \"baseline\"\nverdict = \"expected_unsupported\"\nreason = \"a\"\nfailure_contains = \"tail loss\"\n\n\
                    [[expect]]\ncell = \"srt/us-to-tsp\"\nprofile = \"baseline\"\nverdict = \"expected_unsupported\"\nreason = \"b\"\nfailure_contains = \"KLV\"\n";
        assert_eq!(parse_expectations(text).unwrap().len(), 2);
    }

    #[test]
    fn same_cell_different_profile_is_not_ambiguous() {
        let text = "[[expect]]\ncell = \"decode/vlc/audio\"\nprofile = \"audio\"\nverdict = \"expected_unsupported\"\nreason = \"a\"\nfailure_contains = \"x\"\n\n\
                    [[expect]]\ncell = \"decode/vlc/audio\"\nprofile = \"baseline\"\nverdict = \"expected_unsupported\"\nreason = \"b\"\nfailure_contains = \"y\"\n";
        assert_eq!(parse_expectations(text).unwrap().len(), 2);
    }

    #[test]
    fn patterns_overlap_rules() {
        assert!(patterns_overlap("a/b", "a/b"));
        assert!(!patterns_overlap("a/b", "a/c"));
        assert!(patterns_overlap("a/*", "a/b"));
        assert!(patterns_overlap("a/b/*", "a/*"));
        assert!(!patterns_overlap("a/*", "b/*"));
    }

    #[test]
    fn live_expectations_table_parses() {
        let text = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/interop/expectations.toml"
        ));
        let parsed = parse_expectations(text).expect("the live expectations table must parse");
        assert!(
            parsed.len() >= 65,
            "expected at least 65 rows, got {}",
            parsed.len()
        );
    }

    // --- File-driven merge/render round trip ---

    #[test]
    fn merge_and_render_end_to_end_via_tempdir() {
        let dir = std::env::temp_dir().join(format!(
            "tst-interop-report-merge-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time moves forward")
                .as_nanos()
        ));
        let cells_dir = dir.join("cells");
        fs::create_dir_all(&cells_dir).expect("create cells dir");

        fs::write(
            cells_dir.join("a.json"),
            serde_json::to_string(&raw_cell("decode/ffmpeg", "baseline", RawVerdict::Pass))
                .unwrap(),
        )
        .expect("write cell a");
        fs::write(
            cells_dir.join("b.json"),
            serde_json::to_string(&raw_cell_with_failures(
                "decode/mpv",
                "baseline",
                RawVerdict::Fail,
                &["mpv lacks async KLV support"],
            ))
            .unwrap(),
        )
        .expect("write cell b");

        let expectations_path = dir.join("expectations.toml");
        fs::write(
            &expectations_path,
            "[[expect]]\ncell = \"decode/mpv\"\nprofile = \"baseline\"\nverdict = \"expected_unsupported\"\nreason = \"gap\"\nfailure_contains = \"async KLV support\"\n",
        )
        .expect("write expectations");

        let meta_path = dir.join("meta.json");
        fs::write(&meta_path, r#"{"host": "test-host"}"#).expect("write meta");

        let inventory_path = dir.join("inventory.json");
        fs::write(
            &inventory_path,
            serde_json::to_string(&inventory(
                &[("decode/ffmpeg", "baseline"), ("decode/mpv", "baseline")],
                &[],
            ))
            .unwrap(),
        )
        .expect("write inventory");

        let out_path = dir.join("results.json");
        let results = merge(
            &cells_dir,
            &expectations_path,
            &meta_path,
            &inventory_path,
            &out_path,
        )
        .expect("merge must succeed");

        assert_eq!(results.summary.fail, 0);
        assert_eq!(results.summary.pass, 1);
        assert_eq!(results.summary.expected_unsupported, 1);
        assert_eq!(results.inventory.as_ref().unwrap().declared_cells, 2);
        assert!(out_path.exists());

        let md_path = dir.join("results.md");
        let md = render(&out_path, &md_path).expect("render must succeed");
        assert!(md.contains("# Interop Report"));
        assert!(
            fs::read_to_string(&md_path)
                .expect("read md")
                .contains("decode/ffmpeg")
        );

        let summary_path = dir.join("summary.md");
        append_github_summary(&summary_path, &md).expect("append must succeed");
        append_github_summary(&summary_path, "\nmore\n").expect("second append must succeed");
        let appended = fs::read_to_string(&summary_path).expect("read summary");
        assert!(appended.starts_with("# Interop Report"));
        assert!(appended.trim_end().ends_with("more"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// An empty `--cells-dir` must never produce a clean, all-PASS
    /// `results.json` — that's the worst failure mode for this tool (a
    /// crashed orchestrator, or a `--cells-dir` typo, silently rendering
    /// as green evidence). `merge` must error instead of returning an
    /// empty-but-successful `Results`, and must not write `--out`.
    #[test]
    fn merge_with_empty_cells_dir_is_a_hard_error() {
        let dir = std::env::temp_dir().join(format!(
            "tst-interop-report-merge-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time moves forward")
                .as_nanos()
        ));
        let cells_dir = dir.join("cells");
        fs::create_dir_all(&cells_dir).expect("create empty cells dir");
        // A non-`.json` file must not be mistaken for a cell either.
        fs::write(cells_dir.join("README.txt"), "not a cell").expect("write stray file");

        let expectations_path = dir.join("expectations.toml");
        fs::write(&expectations_path, "# no expectations\n").expect("write expectations");

        let meta_path = dir.join("meta.json");
        fs::write(&meta_path, r#"{"host": "test-host"}"#).expect("write meta");

        let inventory_path = dir.join("inventory.json");
        fs::write(
            &inventory_path,
            serde_json::to_string(&inventory(&[], &[])).unwrap(),
        )
        .expect("write inventory");

        let out_path = dir.join("results.json");
        let err = merge(
            &cells_dir,
            &expectations_path,
            &meta_path,
            &inventory_path,
            &out_path,
        )
        .expect_err("merge over zero cell files must be an error, not a clean empty report");

        assert!(
            err.contains(&cells_dir.display().to_string()),
            "error must name the empty directory: {err}"
        );
        assert!(
            !out_path.exists(),
            "merge must not write --out on this error path"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// An inventory mismatch is a hard `merge` error, same shape as the
    /// empty-`--cells-dir` case above: no `--out` written, no silently
    /// short-produced report.
    #[test]
    fn merge_with_inventory_mismatch_is_a_hard_error_and_writes_nothing() {
        let dir = std::env::temp_dir().join(format!(
            "tst-interop-report-merge-inv-mismatch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time moves forward")
                .as_nanos()
        ));
        let cells_dir = dir.join("cells");
        fs::create_dir_all(&cells_dir).expect("create cells dir");

        // ONE produced cell...
        fs::write(
            cells_dir.join("a.json"),
            serde_json::to_string(&raw_cell("decode/ffmpeg", "baseline", RawVerdict::Pass))
                .unwrap(),
        )
        .expect("write cell a");

        let expectations_path = dir.join("expectations.toml");
        fs::write(&expectations_path, "# no expectations\n").expect("write expectations");

        let meta_path = dir.join("meta.json");
        fs::write(&meta_path, r#"{"host": "test-host"}"#).expect("write meta");

        // ...but an inventory declaring TWO.
        let inventory_path = dir.join("inventory.json");
        fs::write(
            &inventory_path,
            serde_json::to_string(&inventory(
                &[("decode/ffmpeg", "baseline"), ("decode/mpv", "baseline")],
                &[],
            ))
            .unwrap(),
        )
        .expect("write inventory");

        let out_path = dir.join("results.json");
        let e = merge(
            &cells_dir,
            &expectations_path,
            &meta_path,
            &inventory_path,
            &out_path,
        )
        .unwrap_err();
        assert!(
            e.contains("inventory mismatch") && e.contains("missing"),
            "{e}"
        );
        assert!(!out_path.exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
