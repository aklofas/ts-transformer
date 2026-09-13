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
//! (`results.summary.fail > 0`) to decide the process exit code — there
//! is no other path to a passing run.
//!
//! Expectations are read from a small hand-rolled TOML-shaped format (see
//! [`parse_expectations`]) rather than a real TOML crate — the workspace
//! has no `toml` dependency in normal deps and this crate doesn't widen
//! that set for a machine-written file with an exact, fixed schema.

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
    /// load-bearing property. `merge`'s caller exits nonzero iff this is
    /// nonzero.
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
/// `profile` both match. Ambiguous multi-match expectations files are
/// the expectations author's problem, not merge's — file order wins.
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
                        stale.push(StaleExpectation {
                            cell: exp.cell.clone(),
                            profile: exp.profile.clone(),
                            reason: exp.reason.clone(),
                        });
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
/// plus the knobs that shaped it. `shape` is `full-157` only when no
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

/// What the merged report records about its inventory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventorySummary {
    pub shape: String,
    pub declared_cells: usize,
    pub allowed_skips: Vec<String>,
}

pub const SHAPE_FULL: &str = "full-157";
pub const SHAPE_SUBSET: &str = "subset";

pub fn parse_inventory(text: &str) -> Result<Inventory, String> {
    let inv: Inventory = serde_json::from_str(text).map_err(|e| format!("inventory.json: {e}"))?;
    if inv.shape != SHAPE_FULL && inv.shape != SHAPE_SUBSET {
        return Err(format!(
            "inventory.json: shape must be {SHAPE_FULL:?} or {SHAPE_SUBSET:?}, got {:?}",
            inv.shape
        ));
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
    (label, escape_markdown_table_cell(&notes))
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
                "- `{}` (profile `{}`): {} — matched cell now PASSes; consider removing.",
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

/// `report soak`: turn one multi-day soak run's raw artifacts (an RSS
/// time series plus each leg's proxy/recv/send evidence) into
/// `soak-results.json` — the endurance half of this arc's published
/// evidence (`merge`/`render`, above, is the interop-matrix half).
///
/// `scripts/interop/soak.sh` runs two concurrent legs over the same
/// wall-clock window: `srt` (through an impaired proxy with scheduled
/// outage windows, sender wrapped via `send::run_managed` so it
/// reconnects) and `rist` (through a second impaired proxy with NO
/// outage — sustained impairment only; see this module's "Known
/// telemetry limitations" section for why only one leg carries the
/// outage/reconnect assertion). [`soak::build_soak_results`] is the
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
///   dry-run during this arc's own fix wave observed a SECOND rebuild
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
/// - **Outage-leg drop-rate excess (watch item, not a bug in itself).**
///   The continuous-impairment-only model above is a slight
///   underestimate on a leg that also carries outage windows: a
///   `ManagedTransport` sender backing off through an outage doesn't
///   sit perfectly silent — each reconnect attempt's handshake can fire
///   before the sender's own `Broken` detection has caught up to the
///   outage clearing (or before it's caught up to the outage
///   *starting*), and any such attempt that lands while the proxy's
///   outage window is still active gets counted as `dropped` same as
///   continuous loss, even though `expected_drop_fraction` never
///   modeled it. Per outage window this is a handful of packets — at
///   72h scale (roughly a dozen windows over the default schedule)
///   that accumulates to a small, genuinely expected excess on the
///   order of ~0.02-0.2 percentage points over the continuous-only
///   expectation, which can occasionally eat into (or on a bad draw,
///   slightly exceed) the `DROP_RATE_TOLERANCE_FLOOR` (0.1pp) the
///   non-outage leg never has to absorb. If
///   `drop_rate_consistent_with_impairment_<leg>` fails specifically on
///   the outage-bearing leg and by a small margin, treat this as the
///   first hypothesis to rule out (compare against the reconnect count
///   and the size of the excess) before escalating it as a library
///   regression.
///
/// # RSS-growth harness artifact (`--no-klv-digest`)
///
/// Task 14's own 1h smoke run found real, linear (not one-time-step)
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
pub mod soak {
    use std::collections::BTreeMap;
    use std::path::Path;

    use serde::{Deserialize, Serialize};

    use crate::proxy::ProxyStats;
    use crate::report_types::{CellMetrics, VerifyReport};

    /// Absolute ceiling on the warmup excluded from the RSS regression,
    /// in seconds. The actual warmup is `min(MAX_WARMUP_S,
    /// config.expected_duration_s * config.warmup_fraction)` — 30
    /// minutes for a full-length run, or a fraction of the DECLARED run
    /// length for a shorter one (a `--hours 1` smoke's own post-warmup
    /// window would otherwise be near-empty against a flat 30-minute
    /// floor) — whichever is smaller. Derived from the declared config
    /// rather than the observed sample span so a truncated run doesn't
    /// also shrink its own warmup exclusion.
    const MAX_WARMUP_S: f64 = 30.0 * 60.0;

    const KNOWN_LEGS: [&str; 2] = ["srt", "rist"];
    const KNOWN_PROCESSES: [&str; 3] = ["send", "proxy", "recv"];

    /// Every worker `soak.sh` reaps into `exits.json` (the sampler is
    /// killed by the script itself and is deliberately not a worker
    /// here). The `worker_exits` verdict below only REQUIRES a role's
    /// entry when its leg is present in `legs` (a single-leg srt-only
    /// run, e.g. a local smoke test, never launches the `rist-*` three)
    /// — but a key outside this whole set, for ANY run, is always an
    /// unconditional failure, never silently ignored.
    const WORKER_ROLES: [&str; 6] = [
        "srt-send",
        "srt-proxy",
        "srt-recv",
        "rist-send",
        "rist-proxy",
        "rist-recv",
    ];
    /// Fraction of the cadence-implied post-warmup sample count a
    /// process must actually have.
    const MIN_SAMPLE_COVERAGE: f64 = 0.9;
    /// Largest tolerated gap between consecutive samples, in cadences.
    const MAX_GAP_CADENCES: f64 = 3.0;

    /// One `elapsed_s,leg,process,pid,rss_kb` row from `soak.sh`'s RSS
    /// sampler. `rss_kb: None` means `/proc/<pid>/status` couldn't be
    /// read at this tick — the sampler's own crash signal (see
    /// [`SoakResults::process_exits`]); a healthy run never produces one
    /// at all. NOT because the sampler outlives every tracked process —
    /// the opposite is true and deliberately so: `soak.sh`'s sampler
    /// stops `SAMPLER_END_SLACK_S` (35s) before its own nominal
    /// deadline, specifically so it never samples a process that has
    /// ALREADY exited on schedule (both proxies' own `--run-seconds`
    /// budgets, sized from their own, earlier launch instants, can
    /// otherwise elapse a few real seconds before the sampler's next
    /// tick — confirmed empirically, ~6.5s for the srt leg, ~2.5s for
    /// rist). A `None` here means a process died UNEXPECTEDLY, mid-run,
    /// not that the run reached its end.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct RssSample {
        pub elapsed_s: f64,
        pub leg: String,
        pub process: String,
        pub pid: u32,
        pub rss_kb: Option<u64>,
    }

    /// Parse `soak.sh`'s `rss.csv`: an `elapsed_s,leg,process,pid,rss_kb`
    /// header line followed by one row per sample tick per process.
    /// `rss_kb` may be empty (process gone at this tick). Loudly errors
    /// (naming the offending line) on anything else — a malformed or
    /// truncated soak artifact must never silently read as "no data,
    /// every check vacuously passes."
    pub fn parse_rss_csv(text: &str) -> Result<Vec<RssSample>, String> {
        let mut lines = text.lines().enumerate();
        let Some((_, header)) = lines.next() else {
            return Err("rss.csv: empty file (expected a header line)".to_string());
        };
        if header.trim() != "elapsed_s,leg,process,pid,rss_kb" {
            return Err(format!(
                "rss.csv line 1: expected header `elapsed_s,leg,process,pid,rss_kb`, got: {header:?}"
            ));
        }

        let mut samples = Vec::new();
        for (idx, raw_line) in lines {
            let line_no = idx + 1;
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split(',').collect();
            if fields.len() != 5 {
                return Err(format!(
                    "rss.csv line {line_no}: expected 5 comma-separated fields, got {}: {line:?}",
                    fields.len()
                ));
            }
            let elapsed_s: f64 = fields[0].parse().map_err(|_| {
                format!("rss.csv line {line_no}: invalid elapsed_s {:?}", fields[0])
            })?;
            if !elapsed_s.is_finite() || elapsed_s < 0.0 {
                return Err(format!(
                    "rss.csv line {line_no}: elapsed_s must be finite and non-negative, got {elapsed_s}"
                ));
            }
            let leg = fields[1].to_string();
            if !KNOWN_LEGS.contains(&leg.as_str()) {
                return Err(format!(
                    "rss.csv line {line_no}: unknown leg {leg:?} (want one of {KNOWN_LEGS:?})"
                ));
            }
            let process = fields[2].to_string();
            if !KNOWN_PROCESSES.contains(&process.as_str()) {
                return Err(format!(
                    "rss.csv line {line_no}: unknown process {process:?} (want one of {KNOWN_PROCESSES:?})"
                ));
            }
            let pid: u32 = fields[3]
                .parse()
                .map_err(|_| format!("rss.csv line {line_no}: invalid pid {:?}", fields[3]))?;
            let rss_kb = if fields[4].is_empty() {
                None
            } else {
                Some(fields[4].parse::<u64>().map_err(|_| {
                    format!("rss.csv line {line_no}: invalid rss_kb {:?}", fields[4])
                })?)
            };
            samples.push(RssSample {
                elapsed_s,
                leg,
                process,
                pid,
                rss_kb,
            });
        }
        Ok(samples)
    }

    /// `soak.sh`'s declared run parameters, written to `soak-config.json`
    /// at launch — BEFORE any evidence exists — so `report soak` judges the
    /// run against what was configured, never against what the artifacts
    /// happen to contain (release-gate audit RLS-B09).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SoakConfig {
        pub expected_duration_s: f64,
        pub rss_cadence_s: f64,
        /// `warmup_s = min(MAX_WARMUP_S, expected_duration_s * warmup_fraction)`.
        pub warmup_fraction: f64,
        /// The sampler deliberately stops this many seconds before the
        /// nominal deadline (`soak.sh`'s `SAMPLER_END_SLACK_S`), so the
        /// achievable RSS span is `expected_duration_s - sampler_end_slack_s`.
        pub sampler_end_slack_s: f64,
        /// Worker role -> exit status that is expected for this run
        /// (normally empty; a drill that kills a role on purpose lists it).
        #[serde(default)]
        pub expected_worker_exits: BTreeMap<String, i32>,
    }

    pub fn parse_soak_config(text: &str) -> Result<SoakConfig, String> {
        let cfg: SoakConfig =
            serde_json::from_str(text).map_err(|e| format!("soak-config.json: {e}"))?;
        if !(cfg.expected_duration_s.is_finite() && cfg.expected_duration_s > 0.0) {
            return Err(format!(
                "soak-config.json: expected_duration_s must be finite and > 0, got {}",
                cfg.expected_duration_s
            ));
        }
        if !(cfg.rss_cadence_s.is_finite() && cfg.rss_cadence_s > 0.0) {
            return Err(format!(
                "soak-config.json: rss_cadence_s must be finite and > 0, got {}",
                cfg.rss_cadence_s
            ));
        }
        if !(cfg.warmup_fraction.is_finite() && (0.0..1.0).contains(&cfg.warmup_fraction)) {
            return Err(format!(
                "soak-config.json: warmup_fraction must be in [0, 1), got {}",
                cfg.warmup_fraction
            ));
        }
        if !(cfg.sampler_end_slack_s.is_finite() && cfg.sampler_end_slack_s >= 0.0) {
            return Err(format!(
                "soak-config.json: sampler_end_slack_s must be finite and >= 0, got {}",
                cfg.sampler_end_slack_s
            ));
        }
        if cfg.sampler_end_slack_s >= cfg.expected_duration_s {
            return Err(format!(
                "soak-config.json: sampler_end_slack_s ({}) must be less than expected_duration_s ({}) — \
                 otherwise the achievable RSS span is non-positive before a single sample is examined",
                cfg.sampler_end_slack_s, cfg.expected_duration_s
            ));
        }
        // `duration_coverage`'s own passing bound (see `build_soak_results`)
        // is `run_duration_s >= expected_duration_s - sampler_end_slack_s -
        // 2*rss_cadence_s`. If that right-hand side is <= 0 every observed
        // span (which is always >= 0) passes vacuously — a short `--hours`
        // combined with the fixed 35s sampler slack can reach this (e.g.
        // 72s / cadence 30 / slack 35: 72 - 35 - 60 = -23). Caught here, at
        // config-declaration time, rather than only showing up as a
        // silent, uninformative pass.
        let min_span_s =
            cfg.expected_duration_s - cfg.sampler_end_slack_s - 2.0 * cfg.rss_cadence_s;
        if min_span_s <= 0.0 {
            return Err(format!(
                "soak-config.json: expected_duration_s ({}) minus sampler_end_slack_s ({}) minus \
                 2 * rss_cadence_s ({}) = {min_span_s} <= 0 — every run would pass duration_coverage \
                 vacuously (any observed span is >= 0)",
                cfg.expected_duration_s, cfg.sampler_end_slack_s, cfg.rss_cadence_s
            ));
        }
        // Same vacuous-pass hazard on the sample-coverage side:
        // `rss_sample_coverage_*` needs >= 2 distinct timestamps to
        // compute a gap at all (see `build_soak_results`'s `distinct_ts >=
        // 2` check) — below that, `min_samples` (90% of `expected_samples`,
        // itself possibly 0 or 1) can never meaningfully gate anything, so
        // reject a config whose OWN implied expected-sample count already
        // can't reach 2.
        let warmup_s = MAX_WARMUP_S.min(cfg.expected_duration_s * cfg.warmup_fraction);
        let expected_samples = ((cfg.expected_duration_s - cfg.sampler_end_slack_s - warmup_s)
            / cfg.rss_cadence_s)
            .floor();
        if expected_samples < 2.0 {
            return Err(format!(
                "soak-config.json: implied post-warmup sample count is {expected_samples} (< 2) from \
                 expected_duration_s ({}), sampler_end_slack_s ({}), warmup {warmup_s}s (min(1800, \
                 expected_duration_s * warmup_fraction)), and rss_cadence_s ({}) — no run of this \
                 length could ever pass rss_sample_coverage",
                cfg.expected_duration_s, cfg.sampler_end_slack_s, cfg.rss_cadence_s
            ));
        }
        for role in cfg.expected_worker_exits.keys() {
            if !WORKER_ROLES.contains(&role.as_str()) {
                return Err(format!(
                    "soak-config.json: expected_worker_exits has unknown role {role:?} (want one of {WORKER_ROLES:?})"
                ));
            }
        }
        Ok(cfg)
    }

    /// `soak.sh`'s `exits.json`: `{ "<role>": <exit status>, ... }` for every
    /// worker it reaped (the six leg processes; never the sampler, which the
    /// script itself kills on schedule).
    pub fn parse_worker_exits(text: &str) -> Result<BTreeMap<String, i32>, String> {
        serde_json::from_str(text).map_err(|e| format!("exits.json: {e}"))
    }

    /// Ordinary-least-squares slope of `points` (x, y) pairs — used here
    /// as KB of RSS per second of elapsed wall-clock time. Returns `0.0`
    /// for fewer than 2 points or a degenerate (all-same-x) input rather
    /// than dividing by zero.
    fn linear_regression_slope(points: &[(f64, f64)]) -> f64 {
        let n = points.len() as f64;
        if n < 2.0 {
            return 0.0;
        }
        let sum_x: f64 = points.iter().map(|(x, _)| x).sum();
        let sum_y: f64 = points.iter().map(|(_, y)| y).sum();
        let sum_xy: f64 = points.iter().map(|(x, y)| x * y).sum();
        let sum_xx: f64 = points.iter().map(|(x, _)| x * x).sum();
        let denom = n * sum_xx - sum_x * sum_x;
        if denom.abs() < f64::EPSILON {
            return 0.0;
        }
        (n * sum_xy - sum_x * sum_y) / denom
    }

    /// Number of outage windows (`impair::Engine::in_outage`'s zero-based
    /// `[k*period, k*period+dur)` numbering, measured from the PROXY's
    /// own launch instant) whose START falls within the period send/recv
    /// are actually active.
    ///
    /// `duration_s` is `run_duration_s` — the active-traffic period's own
    /// length, NOT measured from the proxy's `t=0` but from when
    /// send/recv themselves start. `soak.sh` deliberately launches the
    /// proxy `outage_dur_s + 30` seconds ahead of send/recv (its own
    /// `SRT_PROXY_WARMUP_S` constant — mirrored here as `head_start_s`,
    /// two independent literals for one concept; keep them in sync if
    /// either changes) specifically so scheduled window `k=0` — which
    /// always starts at the proxy's `t=0`, before send/recv exist — can
    /// never reach real traffic. The pre-fix version of this function
    /// measured from the proxy's own `t=0` and so always counted window
    /// 0 for any nonzero `duration_s`, even though it's structurally
    /// impossible for send/recv to ever observe it (caught in PR review:
    /// a 1h smoke run, whose active period never reaches window 1 either,
    /// was reporting "1 window expected" as if window 0 were a real,
    /// reachable event). Fixed by counting windows whose start `k*period_s`
    /// falls within `[head_start_s, head_start_s + duration_s)` instead —
    /// window 0's start (`t=0`) is always before `head_start_s`, so it's
    /// now correctly excluded.
    fn expected_outage_windows(
        duration_s: f64,
        outage_period_s: Option<u64>,
        outage_dur_s: u64,
    ) -> u64 {
        let Some(period_s) = outage_period_s else {
            return 0;
        };
        if period_s == 0 || duration_s <= 0.0 {
            return 0;
        }
        let period_s = period_s as f64;
        let head_start_s = outage_dur_s as f64 + 30.0;
        let active_end_s = head_start_s + duration_s;
        // Smallest k with k*period_s >= head_start_s (first window whose
        // start can reach the active period), and the smallest k with
        // k*period_s >= active_end_s (first window whose start is past
        // it) — the count of windows in between is the answer.
        let first_reachable = (head_start_s / period_s).ceil() as u64;
        let first_past_end = (active_end_s / period_s).ceil() as u64;
        first_past_end.saturating_sub(first_reachable)
    }

    /// Fraction of packets the proxy's CONTINUOUS configured impairment
    /// (`loss_pct`) predicts it will drop. Deliberately does NOT add an
    /// outage-coverage term — see the module doc's "Known telemetry
    /// limitations" section for the empirical finding on why an
    /// outage's true impact shows up as reduced sender throughput, not
    /// proxy-visible drops, once a `ManagedTransport` sender is in the
    /// picture.
    fn expected_drop_fraction(loss_pct: f64) -> f64 {
        loss_pct / 100.0
    }

    /// Standard-error multiplier for [`drop_rate_tolerance`]'s binomial
    /// term. ~6 standard errors of a binomial proportion is generous
    /// enough that ordinary sampling noise at any packet volume this
    /// soak actually pushes (thousands to tens of millions over a 72h
    /// run) won't false-positive, while still catching a genuine
    /// multiple-of-the-configured-rate regression (e.g. observed 4%
    /// against a configured 2%) instead of silently absorbing it — the
    /// flat `max(3pp, expected*30%)` band this replaced did NOT scale
    /// down with volume, so a real 2x regression at high packet counts
    /// could sit comfortably inside a fixed few-percentage-point band
    /// forever, no matter how many packets confirmed it.
    const DROP_RATE_TOLERANCE_SIGMA: f64 = 6.0;

    /// Absolute floor on the tolerance band, applied regardless of the
    /// binomial term above. Exists only to keep
    /// [`drop_rate_tolerance`]'s output finite and sane in the
    /// degenerate `n == 0` case (a genuine 0-packet division would
    /// otherwise propagate `inf`/`NaN` into `LegResult::
    /// drop_fraction_tolerance`'s JSON, which `serde_json` can't
    /// serialize) — deliberately much smaller than the old flat 3
    /// percentage-point floor so it doesn't itself become the binding
    /// constraint at realistic-to-large packet counts, where the
    /// binomial term alone is already far tighter.
    const DROP_RATE_TOLERANCE_FLOOR: f64 = 0.001; // 0.1 percentage points

    /// Tolerance band for the observed-vs-expected drop-fraction
    /// comparison: `DROP_RATE_TOLERANCE_SIGMA` standard errors of a
    /// binomial proportion with success probability `expected` over `n`
    /// trials (`n` = total packets the proxy decided on), floored at
    /// [`DROP_RATE_TOLERANCE_FLOOR`]. Scales down as `n` grows — unlike
    /// the flat percentage band this replaced, a 72h run's much larger
    /// packet count tightens the check rather than leaving it exactly
    /// as loose as a five-second interop-matrix cell would need.
    fn drop_rate_tolerance(expected: f64, n: u64) -> f64 {
        if n == 0 {
            return DROP_RATE_TOLERANCE_FLOOR;
        }
        let variance = (expected * (1.0 - expected) / n as f64).max(0.0);
        let sigma = variance.sqrt();
        (DROP_RATE_TOLERANCE_SIGMA * sigma).max(DROP_RATE_TOLERANCE_FLOOR)
    }

    /// One (leg, process) RSS linear-regression result.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct RssSlope {
        pub leg: String,
        pub process: String,
        pub slope_kb_per_hour: f64,
        pub samples_used: usize,
    }

    /// Per-(leg, process) post-warmup sampling coverage.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct RssCoverage {
        pub leg: String,
        pub process: String,
        pub expected_samples: u64,
        pub observed_samples: u64,
        pub largest_gap_s: f64,
        pub pass: bool,
    }

    /// One (leg, process) that disappeared from `/proc` mid-run — see
    /// [`RssSample::rss_kb`].
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ProcessExit {
        pub leg: String,
        pub process: String,
        pub elapsed_s: f64,
        pub pid: u32,
    }

    /// One named check in [`SoakResults::verdicts`].
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SoakVerdict {
        pub name: String,
        pub pass: bool,
        /// A provisional verdict is recorded for evidence but never
        /// makes [`SoakResults::overall_pass`] false — see the module
        /// doc's "Known telemetry limitations" section.
        pub provisional: bool,
        pub detail: String,
    }

    /// Computed numbers for one leg (`"srt"` or `"rist"`).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct LegResult {
        pub leg: String,
        pub outage_period_s: Option<u64>,
        pub outage_dur_s: u64,
        pub expected_outage_windows: u64,
        pub proxy_forwarded: u64,
        pub proxy_dropped: u64,
        pub loss_pct: f64,
        pub observed_drop_fraction: f64,
        pub expected_drop_fraction: f64,
        pub drop_fraction_tolerance: f64,
        pub recv_pass: bool,
        pub recv_failures: Vec<String>,
        pub send_video_aus: u64,
        pub recv_video_aus: u64,
    }

    /// The full `soak-results.json` document: [`build_soak_results`]'s
    /// output and [`run`]'s return value.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SoakResults {
        pub run_duration_s: f64,
        pub expected_duration_s: f64,
        pub warmup_s: f64,
        pub rss_slope_threshold_kb_per_hour: Option<f64>,
        pub rss_slopes: Vec<RssSlope>,
        pub coverage: Vec<RssCoverage>,
        pub process_exits: Vec<ProcessExit>,
        pub worker_exits: BTreeMap<String, i32>,
        pub legs: Vec<LegResult>,
        pub verdicts: Vec<SoakVerdict>,
        /// `true` iff every non-[`SoakVerdict::provisional`] verdict
        /// passed.
        pub overall_pass: bool,
        pub limitations: Vec<String>,
    }

    /// One leg's raw evidence artifacts — `soak.sh` writes these as
    /// `proxy --stats-json`'s output, `recv --json`'s output, and
    /// `send --json`'s output respectively.
    #[derive(Debug, Clone)]
    pub struct LegArtifacts {
        pub proxy_stats: ProxyStats,
        pub recv_report: VerifyReport,
        pub send_metrics: CellMetrics,
        /// `None` for the `rist` leg (sustained impairment only, no
        /// scheduled outage — see the module doc).
        pub outage_period_s: Option<u64>,
    }

    /// Pure input to [`build_soak_results`].
    pub struct SoakInputs {
        pub rss_samples: Vec<RssSample>,
        /// `(leg name, artifacts)` pairs — `"srt"` is always present;
        /// `"rist"` is included whenever that leg's three report files
        /// were supplied.
        pub legs: Vec<(String, LegArtifacts)>,
        pub rss_slope_threshold_kb_per_hour: Option<f64>,
        /// The run's declared parameters (`soak-config.json`) — judged
        /// against, never derived from, the artifacts on disk.
        pub config: SoakConfig,
        /// Worker role -> exit status, from `exits.json`.
        pub worker_exits: BTreeMap<String, i32>,
    }

    /// Compute every verdict from already-parsed inputs — no I/O. [`run`]
    /// is the file-driven wrapper around this.
    ///
    /// # Errors
    ///
    /// Errors (rather than returning a clean, vacuously-passing
    /// [`SoakResults`]) if `rss_samples` is completely empty — a
    /// header-only `rss.csv` most likely means the sampler crashed
    /// before writing a single tick, or a `--rss` path typo pointed at
    /// the wrong file; either way, this must never silently read as a
    /// data-free "the soak was fine" report (mirrors [`super::merge`]'s own
    /// "no cells" hard error for the same reason). A `rss.csv` that HAS
    /// some rows but is missing an entire expected `(leg, process)`
    /// combination (or has rows for it that are all pre-warmup) is a
    /// softer problem — handled instead by an explicit, non-
    /// [`SoakVerdict::provisional`], FAILING `rss_data_present_<leg>_
    /// <process>` verdict per combination, so `overall_pass` still goes
    /// false but the rest of the report (the combinations that DO have
    /// data) stays readable.
    pub fn build_soak_results(inputs: SoakInputs) -> Result<SoakResults, String> {
        let SoakInputs {
            rss_samples,
            legs,
            rss_slope_threshold_kb_per_hour,
            config,
            worker_exits,
        } = inputs;

        if rss_samples.is_empty() {
            return Err(
                "rss.csv has zero data rows — refusing to write a clean pass for a data-free \
                 soak run (the RSS sampler most likely crashed before its first tick, or --rss \
                 points at the wrong file)"
                    .to_string(),
            );
        }

        // `rss_samples` is non-empty here (the `is_empty()` hard-error
        // above already returned), so min_t/max_t are always assigned a
        // real elapsed_s from the loop below — no separate empty-case
        // branch needed.
        let run_duration_s = {
            let mut min_t = f64::INFINITY;
            let mut max_t = f64::NEG_INFINITY;
            for s in &rss_samples {
                min_t = min_t.min(s.elapsed_s);
                max_t = max_t.max(s.elapsed_s);
            }
            (max_t - min_t).max(0.0)
        };
        let warmup_s = MAX_WARMUP_S.min(config.expected_duration_s * config.warmup_fraction);

        // The run's own declared achievable window: `soak.sh`'s sampler
        // deliberately stops `sampler_end_slack_s` before the nominal
        // deadline (see `RssSample`'s doc), so the longest RSS span a
        // healthy run can ever produce is `expected_duration_s -
        // sampler_end_slack_s`, not `expected_duration_s` itself.
        let achievable_span_s = config.expected_duration_s - config.sampler_end_slack_s;
        let mut verdicts = Vec::new();
        {
            // Two cadences of slack: the sampler's first tick lands at
            // t≈0 and its last up to one tick short of the achievable
            // end.
            let min_span_s = achievable_span_s - 2.0 * config.rss_cadence_s;
            verdicts.push(SoakVerdict {
                name: "duration_coverage".to_string(),
                pass: run_duration_s >= min_span_s,
                provisional: false,
                detail: format!(
                    "observed RSS span {run_duration_s:.0}s against a configured {:.0}s run \
                     (achievable {achievable_span_s:.0}s after the sampler's {:.0}s end slack; \
                     minimum accepted {min_span_s:.0}s)",
                    config.expected_duration_s, config.sampler_end_slack_s
                ),
            });
        }

        let process_exits: Vec<ProcessExit> = rss_samples
            .iter()
            .filter(|s| s.rss_kb.is_none())
            .map(|s| ProcessExit {
                leg: s.leg.clone(),
                process: s.process.clone(),
                elapsed_s: s.elapsed_s,
                pid: s.pid,
            })
            .collect();

        // Post-warmup (leg, process) -> (elapsed_s, rss_kb) points.
        // BTreeMap keeps iteration (and so `rss_slopes`' output order)
        // sorted by (leg, process) without a separate sort step. Each
        // group's own points are sorted by elapsed time below —
        // `rss_samples` isn't guaranteed to arrive in timestamp order,
        // and the gap computation over `windows(2)` needs it to be.
        let mut groups: BTreeMap<(String, String), Vec<(f64, f64)>> = BTreeMap::new();
        for s in &rss_samples {
            if s.elapsed_s < warmup_s {
                continue;
            }
            if let Some(kb) = s.rss_kb {
                groups
                    .entry((s.leg.clone(), s.process.clone()))
                    .or_default()
                    .push((s.elapsed_s, kb as f64));
            }
        }
        for points in groups.values_mut() {
            points.sort_by(|a, b| a.0.total_cmp(&b.0));
        }
        // Every `(leg, process)` combination this run is SUPPOSED to
        // have RSS evidence for — one entry per `KNOWN_PROCESSES` value
        // for each leg actually present in `legs`. Compared against
        // `groups`'s keys (which ones actually have >=1 post-warmup,
        // rss_kb-present sample) BEFORE `groups` is consumed below, so a
        // whole missing process (or one whose only samples are all
        // pre-warmup, or all crashed/`rss_kb: None`) gets its own
        // FAILING verdict instead of just silently absent from
        // `rss_slopes` — see [`build_soak_results`]'s `# Errors` section
        // for why a total data absence needs a harder signal than "this
        // vec happens to be shorter than expected."
        let mut missing_data: Vec<(String, String)> = Vec::new();
        for (leg_name, _) in &legs {
            for process in KNOWN_PROCESSES {
                if !groups.contains_key(&(leg_name.clone(), process.to_string())) {
                    missing_data.push((leg_name.clone(), process.to_string()));
                }
            }
        }

        // Per-(leg, process) sampling-density check, computed BEFORE
        // slopes: a group with too few post-warmup samples (or too
        // large a gap between consecutive ones) can't support a
        // trustworthy regression, so it's excluded from `rss_slopes`
        // below rather than silently reporting a slope built from a
        // couple of stray points.
        let expected_samples = ((achievable_span_s - warmup_s) / config.rss_cadence_s)
            .floor()
            .max(0.0) as u64;
        let min_samples = (expected_samples as f64 * MIN_SAMPLE_COVERAGE).ceil() as u64;
        let max_gap_s = MAX_GAP_CADENCES * config.rss_cadence_s;
        let mut coverage: Vec<RssCoverage> = Vec::new();
        for ((leg, process), points) in &groups {
            let mut largest_gap_s = 0.0f64;
            for w in points.windows(2) {
                largest_gap_s = largest_gap_s.max(w[1].0 - w[0].0);
            }
            let distinct_ts = points
                .iter()
                .map(|(t, _)| t.to_bits())
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            let pass =
                distinct_ts >= 2 && points.len() as u64 >= min_samples && largest_gap_s < max_gap_s;
            coverage.push(RssCoverage {
                leg: leg.clone(),
                process: process.clone(),
                expected_samples,
                observed_samples: points.len() as u64,
                largest_gap_s,
                pass,
            });
            verdicts.push(SoakVerdict {
                name: format!("rss_sample_coverage_{leg}_{process}"),
                pass,
                provisional: false,
                detail: format!(
                    "{} post-warmup sample(s) ({} distinct timestamps) against {expected_samples} \
                     expected at a {:.0}s cadence (minimum {min_samples}); largest gap {largest_gap_s:.0}s \
                     (must be strictly under {max_gap_s:.0}s)",
                    points.len(),
                    distinct_ts,
                    config.rss_cadence_s
                ),
            });
        }

        // Only groups whose coverage passed get a regression computed
        // over them — a slope from a handful of sparse or gappy points
        // isn't evidence of anything.
        let covered: std::collections::BTreeSet<(String, String)> = coverage
            .iter()
            .filter(|c| c.pass)
            .map(|c| (c.leg.clone(), c.process.clone()))
            .collect();
        let rss_slopes: Vec<RssSlope> = groups
            .into_iter()
            .filter(|(key, _)| covered.contains(key))
            .map(|((leg, process), points)| {
                debug_assert!(
                    points.len() >= 2,
                    "a coverage-passed group must have at least 2 samples"
                );
                let samples_used = points.len();
                let slope_kb_per_hour = linear_regression_slope(&points) * 3600.0;
                RssSlope {
                    leg,
                    process,
                    slope_kb_per_hour,
                    samples_used,
                }
            })
            .collect();

        for (leg, process) in &missing_data {
            verdicts.push(SoakVerdict {
                name: format!("rss_data_present_{leg}_{process}"),
                pass: false,
                provisional: false,
                detail: format!(
                    "no post-warmup RSS samples for {leg}/{process} — either this process never \
                     appears in rss.csv at all, or every sample for it fell before the \
                     {warmup_s:.0}s warmup cutoff (crashed/never-sampled processes show up here, \
                     not just in zero_process_exits)"
                ),
            });
        }
        for slope in &rss_slopes {
            let provisional = rss_slope_threshold_kb_per_hour.is_none();
            let pass = match rss_slope_threshold_kb_per_hour {
                Some(threshold) => slope.slope_kb_per_hour <= threshold,
                None => true,
            };
            verdicts.push(SoakVerdict {
                name: format!("rss_slope_{}_{}", slope.leg, slope.process),
                pass,
                provisional,
                detail: format!(
                    "{:.1} KiB/h over {} post-warmup sample(s)",
                    slope.slope_kb_per_hour, slope.samples_used
                ),
            });
        }

        verdicts.push(SoakVerdict {
            name: "zero_process_exits".to_string(),
            pass: process_exits.is_empty(),
            provisional: false,
            detail: if process_exits.is_empty() {
                "no process disappeared from /proc before the runner's own shutdown".to_string()
            } else {
                format!(
                    "{} unexpected exit(s): {:?}",
                    process_exits.len(),
                    process_exits
                )
            },
        });

        // `worker_exits` judges the DECLARED `exits.json` this function
        // was handed — it never runs at all if `run()`'s earlier
        // `read_leg_artifacts` call already hard-errored because a killed
        // worker never wrote its own per-leg report file (send-report/
        // recv-report/proxy-stats). Both paths end in a nonzero process
        // exit, but only this verdict names which role and status failed;
        // the hard-error path is a bare "No such file" (see
        // `SoakResults::limitations`'s last entry).
        //
        // A single-leg (srt-only) run — e.g. a local smoke test, or any
        // invocation that omits the three `--rist-*` flags per
        // `run`'s own doc comment — never launches the `rist-*` trio at
        // all, so only the roles whose leg is actually present in
        // `legs` are required. The same scoping `missing_data` above
        // already applies to RSS/leg checks.
        let required_roles = WORKER_ROLES.iter().copied().filter(|role| {
            legs.iter()
                .any(|(leg_name, _)| role.starts_with(leg_name.as_str()))
        });

        let mut exit_problems: Vec<String> = Vec::new();
        for role in required_roles {
            match worker_exits.get(role) {
                None => exit_problems.push(format!("{role}: no exit status recorded")),
                Some(&status) => {
                    let expected = config.expected_worker_exits.get(role).copied().unwrap_or(0);
                    if status != expected {
                        exit_problems.push(format!("{role}: exit {status} (expected {expected})"));
                    }
                }
            }
        }
        for (role, &status) in &worker_exits {
            if !WORKER_ROLES.contains(&role.as_str()) {
                exit_problems.push(format!(
                    "unknown role {role} in exits.json (status {status})"
                ));
            }
        }
        verdicts.push(SoakVerdict {
            name: "worker_exits".to_string(),
            pass: exit_problems.is_empty(),
            provisional: false,
            detail: if exit_problems.is_empty() {
                "every worker exited with its expected status".to_string()
            } else {
                exit_problems.join("; ")
            },
        });

        let mut leg_results = Vec::new();
        for (leg_name, artifacts) in &legs {
            let outage_dur_s = artifacts.proxy_stats.config.outage_dur_s;
            let loss_pct = artifacts.proxy_stats.config.loss_pct;
            let outage_windows =
                expected_outage_windows(run_duration_s, artifacts.outage_period_s, outage_dur_s);
            let total = artifacts.proxy_stats.forwarded + artifacts.proxy_stats.dropped;
            let expected = expected_drop_fraction(loss_pct);
            let tolerance = drop_rate_tolerance(expected, total);
            let observed = if total == 0 {
                0.0
            } else {
                artifacts.proxy_stats.dropped as f64 / total as f64
            };
            let drop_pass = total != 0 && (observed - expected).abs() <= tolerance;

            verdicts.push(SoakVerdict {
                name: format!("drop_rate_consistent_with_impairment_{leg_name}"),
                pass: drop_pass,
                provisional: false,
                detail: if total == 0 {
                    format!("{leg_name}: proxy observed zero packets — no traffic crossed it")
                } else {
                    format!(
                        "{leg_name}: observed {:.2}%, expected {:.2}% ± {:.2}pp ({} forwarded, {} dropped)",
                        observed * 100.0,
                        expected * 100.0,
                        tolerance * 100.0,
                        artifacts.proxy_stats.forwarded,
                        artifacts.proxy_stats.dropped
                    )
                },
            });

            let observed_reconnects = artifacts.recv_report.reconnects;
            verdicts.push(SoakVerdict {
                name: format!("reconnect_count_matches_outage_count_{leg_name}"),
                pass: observed_reconnects.is_none_or(|n| n == outage_windows),
                provisional: true,
                detail: match observed_reconnects {
                    Some(observed) => format!(
                        "{leg_name}: recv --managed observed {observed} successful receive-side \
                         transport rebuild(s) (`ManagedRecvTransport::reconnects_count`) against \
                         {outage_windows} expected outage window(s) over {:.1}h. That counter \
                         counts every successful factory rebuild, not outage windows directly — \
                         a single outage window CAN drive more than one rebuild if the \
                         freshly-reconnected transport breaks again before the outage fully \
                         clears (observed empirically during this fix wave's own short-outage \
                         dry-run testing at an 8s outage duration; not yet confirmed either way \
                         at the 90s outage duration this run schedule actually uses). Recorded \
                         for comparison, still provisional until enough real runs establish the \
                         actual rebuild-to-window ratio at production outage duration.",
                        run_duration_s / 3600.0
                    ),
                    None => format!(
                        "{leg_name}: expected {outage_windows} outage window(s) over {:.1}h; \
                         recv wasn't run with --managed on this leg (no reconnect counter \
                         available) — recorded, not verified",
                        run_duration_s / 3600.0
                    ),
                },
            });

            verdicts.push(SoakVerdict {
                name: format!("recv_invariants_{leg_name}"),
                pass: artifacts.recv_report.pass,
                provisional: false,
                detail: if artifacts.recv_report.pass {
                    "final tallies within expected loss bounds".to_string()
                } else {
                    artifacts.recv_report.failures.join("; ")
                },
            });

            leg_results.push(LegResult {
                leg: leg_name.clone(),
                outage_period_s: artifacts.outage_period_s,
                outage_dur_s,
                expected_outage_windows: outage_windows,
                proxy_forwarded: artifacts.proxy_stats.forwarded,
                proxy_dropped: artifacts.proxy_stats.dropped,
                loss_pct,
                observed_drop_fraction: observed,
                expected_drop_fraction: expected,
                drop_fraction_tolerance: tolerance,
                recv_pass: artifacts.recv_report.pass,
                recv_failures: artifacts.recv_report.failures.clone(),
                send_video_aus: artifacts.send_metrics.video_aus,
                recv_video_aus: artifacts.recv_report.metrics.video_aus,
            });
        }

        let overall_pass = verdicts.iter().all(|v| v.provisional || v.pass);

        Ok(SoakResults {
            run_duration_s,
            expected_duration_s: config.expected_duration_s,
            warmup_s,
            rss_slope_threshold_kb_per_hour,
            rss_slopes,
            coverage,
            process_exits,
            worker_exits,
            legs: leg_results,
            verdicts,
            overall_pass,
            limitations: vec![
                "ManagedTransport (send side) exposes no reconnect-cycle counter \
                 (deferred-features: 'Reconnect counters on ManagedTransport stats'); \
                 recv --managed's ManagedRecvTransport::reconnects_count IS a real observed \
                 count, but a single outage window has not been confirmed to always drive \
                 exactly one rebuild (a short 8s-outage dry-run observed a second rebuild \
                 shortly after the first, before the real 90s-outage-duration ratio could be \
                 established over enough runs), so reconnect_count_matches_outage_count_* \
                 verdicts stay provisional and never gate the run even when a real count is \
                 present."
                    .to_string(),
                "recv's VerifyReport carries no per-event delivery-gap timestamps; \
                 drop_rate_consistent_with_impairment_* verdicts approximate 'gaps confined to \
                 outage windows' via an aggregate drop-rate check against the proxy's cumulative \
                 counters instead of a per-event ±30s timestamp localization."
                    .to_string(),
                "On an outage-bearing leg, each outage window contributes a small \
                 proxy-visible drop-rate excess beyond the continuous-impairment-only model \
                 (reconnect handshake attempts that land while the outage is still active get \
                 counted as dropped) — roughly 0.02-0.2 percentage points at 72h scale against \
                 this run's 0.1pp tolerance floor. A small drop_rate_consistent_with_impairment \
                 excess on that leg specifically is this known, benign model artifact to rule \
                 out first, not automatically a library regression."
                    .to_string(),
                "A worker killed before it writes its own per-leg report artifact (send-report/\
                 recv-report/proxy-stats) makes report soak hard-error (exit 2) instead of \
                 reaching this function and producing a worker_exits verdict at all — both \
                 outcomes are nonzero, but only the latter names which role and status failed."
                    .to_string(),
            ],
        })
    }

    fn read_to_string(path: &Path) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))
    }

    fn read_leg_artifacts(
        proxy_stats_path: &Path,
        recv_report_path: &Path,
        send_report_path: &Path,
        outage_period_s: Option<u64>,
    ) -> Result<LegArtifacts, String> {
        let proxy_stats: ProxyStats = serde_json::from_str(&read_to_string(proxy_stats_path)?)
            .map_err(|e| format!("parse {}: {e}", proxy_stats_path.display()))?;
        let recv_report: VerifyReport = serde_json::from_str(&read_to_string(recv_report_path)?)
            .map_err(|e| format!("parse {}: {e}", recv_report_path.display()))?;
        let send_metrics: CellMetrics = serde_json::from_str(&read_to_string(send_report_path)?)
            .map_err(|e| format!("parse {}: {e}", send_report_path.display()))?;
        Ok(LegArtifacts {
            proxy_stats,
            recv_report,
            send_metrics,
            outage_period_s,
        })
    }

    /// `report soak`'s file-driven entry point: read every artifact
    /// path, build [`SoakInputs`], and write [`build_soak_results`]'s
    /// output to `out_path`. The `rist` leg is entirely optional — pass
    /// `None` to omit it (the `srt` leg alone is enough for a local
    /// smoke run; the full 72h run supplies both).
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        rss_path: &Path,
        config_path: &Path,
        exits_path: &Path,
        srt_proxy_stats_path: &Path,
        srt_recv_report_path: &Path,
        srt_send_report_path: &Path,
        srt_outage_period_s: u64,
        rist: Option<(&Path, &Path, &Path)>,
        rss_slope_threshold_kb_per_hour: Option<f64>,
        out_path: &Path,
    ) -> Result<SoakResults, String> {
        let rss_samples = parse_rss_csv(&read_to_string(rss_path)?)?;
        let config = parse_soak_config(&read_to_string(config_path)?)?;
        let worker_exits = parse_worker_exits(&read_to_string(exits_path)?)?;

        let mut legs = vec![(
            "srt".to_string(),
            read_leg_artifacts(
                srt_proxy_stats_path,
                srt_recv_report_path,
                srt_send_report_path,
                Some(srt_outage_period_s),
            )?,
        )];

        if let Some((proxy_stats_path, recv_report_path, send_report_path)) = rist {
            legs.push((
                "rist".to_string(),
                read_leg_artifacts(proxy_stats_path, recv_report_path, send_report_path, None)?,
            ));
        }

        let results = build_soak_results(SoakInputs {
            rss_samples,
            legs,
            rss_slope_threshold_kb_per_hour,
            config,
            worker_exits,
        })?;

        let json = serde_json::to_string_pretty(&results).expect("SoakResults always serializes");
        std::fs::write(out_path, json).map_err(|e| format!("write {}: {e}", out_path.display()))?;
        Ok(results)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn proxy_stats(
            forwarded: u64,
            dropped: u64,
            loss_pct: f64,
            outage_period_s: Option<u64>,
            outage_dur_s: u64,
        ) -> ProxyStats {
            ProxyStats {
                forwarded,
                dropped,
                duped: 0,
                reordered: 0,
                seed: 1,
                config: crate::proxy::ConfigEcho {
                    loss_pct,
                    dup_pct: 0.0,
                    reorder_pct: 0.0,
                    reorder_hold: 0,
                    jitter_ms_max: 0,
                    base_delay_ms: 0,
                    outage_period_s,
                    outage_dur_s,
                },
            }
        }

        fn cell_metrics(video_aus: u64) -> CellMetrics {
            CellMetrics {
                video_aus,
                keyframes: video_aus / 30,
                klv_records: 0,
                klv_set_sha256: Some(String::new()),
                audio_frames: 0,
                programs_seen: 1,
                pts_monotonic: true,
                misp_sei_seen: false,
                bytes: 0,
                stream_sha256: String::new(),
            }
        }

        fn passing_recv_report(video_aus: u64) -> VerifyReport {
            VerifyReport {
                pass: true,
                failures: Vec::new(),
                metrics: cell_metrics(video_aus),
                reconnects: None,
            }
        }

        fn rss_row(leg: &str, process: &str, elapsed_s: f64, rss_kb: Option<u64>) -> RssSample {
            RssSample {
                elapsed_s,
                leg: leg.to_string(),
                process: process.to_string(),
                pid: 1000,
                rss_kb,
            }
        }

        fn one_leg(artifacts: LegArtifacts) -> Vec<(String, LegArtifacts)> {
            vec![("srt".to_string(), artifacts)]
        }

        /// A config whose expected duration matches a synthetic series that
        /// spans `span_s` at `cadence_s` — the sampler-slack term is 0 so
        /// `span == expected` is the fully-covered case.
        fn cfg(span_s: f64, cadence_s: f64) -> SoakConfig {
            SoakConfig {
                expected_duration_s: span_s,
                rss_cadence_s: cadence_s,
                warmup_fraction: 1.0 / 6.0,
                sampler_end_slack_s: 0.0,
                expected_worker_exits: BTreeMap::new(),
            }
        }

        fn six_clean_exits() -> BTreeMap<String, i32> {
            [
                "srt-send",
                "srt-proxy",
                "srt-recv",
                "rist-send",
                "rist-proxy",
                "rist-recv",
            ]
            .into_iter()
            .map(|r| (r.to_string(), 0))
            .collect()
        }

        /// 3 processes x `n` samples every `cadence_s`, all flat.
        fn flat_series(n: usize, cadence_s: f64) -> Vec<RssSample> {
            let mut s = Vec::new();
            for i in 0..n {
                let t = i as f64 * cadence_s;
                s.push(rss_row("srt", "send", t, Some(50_000)));
                s.push(rss_row("srt", "proxy", t, Some(20_000)));
                s.push(rss_row("srt", "recv", t, Some(50_000)));
            }
            s
        }

        fn verdict<'a>(r: &'a SoakResults, name: &str) -> &'a SoakVerdict {
            r.verdicts
                .iter()
                .find(|v| v.name == name)
                .unwrap_or_else(|| panic!("verdict {name} must be present: {:?}", r.verdicts))
        }

        fn inputs(
            samples: Vec<RssSample>,
            config: SoakConfig,
            exits: BTreeMap<String, i32>,
        ) -> SoakInputs {
            SoakInputs {
                rss_samples: samples,
                legs: one_leg(LegArtifacts {
                    proxy_stats: proxy_stats(1000, 0, 0.0, None, 0),
                    recv_report: passing_recv_report(1000),
                    send_metrics: cell_metrics(1000),
                    outage_period_s: None,
                }),
                rss_slope_threshold_kb_per_hour: None,
                config,
                worker_exits: exits,
            }
        }

        #[test]
        fn fully_covered_run_passes_every_completeness_verdict() {
            // 121 samples at 30s = 3600s span, expected 3600.
            let r = build_soak_results(inputs(
                flat_series(121, 30.0),
                cfg(3600.0, 30.0),
                six_clean_exits(),
            ))
            .unwrap();
            assert!(verdict(&r, "duration_coverage").pass);
            assert!(verdict(&r, "rss_sample_coverage_srt_send").pass);
            assert!(verdict(&r, "worker_exits").pass);
            assert!(r.overall_pass, "{:?}", r.verdicts);
        }

        #[test]
        fn run_truncated_at_forty_percent_fails_duration_coverage() {
            // Series spans 1440s of an expected 3600s.
            let r = build_soak_results(inputs(
                flat_series(49, 30.0),
                cfg(3600.0, 30.0),
                six_clean_exits(),
            ))
            .unwrap();
            let v = verdict(&r, "duration_coverage");
            assert!(!v.pass, "{}", v.detail);
            assert!(!v.provisional);
            assert!(!r.overall_pass);
            assert_eq!(r.expected_duration_s, 3600.0);
        }

        #[test]
        fn one_sample_per_process_fails_sample_coverage_and_skips_slope() {
            let mut samples = flat_series(1, 30.0);
            // Put the one sample past warmup so it is the only post-warmup point.
            for s in &mut samples {
                s.elapsed_s = 3000.0;
            }
            let r =
                build_soak_results(inputs(samples, cfg(3600.0, 30.0), six_clean_exits())).unwrap();
            let v = verdict(&r, "rss_sample_coverage_srt_send");
            assert!(!v.pass, "{}", v.detail);
            assert!(
                r.rss_slopes.is_empty(),
                "slope must not be computed from a single sample: {:?}",
                r.rss_slopes
            );
            assert!(r.verdicts.iter().all(|v| !v.name.starts_with("rss_slope_")));
            assert!(!r.overall_pass);
        }

        #[test]
        fn ten_minute_gap_in_an_otherwise_complete_series_fails_sample_coverage() {
            let mut samples = flat_series(121, 30.0);
            // Drop every sample in (1800, 2400) — a 600s hole after warmup.
            samples.retain(|s| !(s.elapsed_s > 1800.0 && s.elapsed_s < 2400.0));
            let r =
                build_soak_results(inputs(samples, cfg(3600.0, 30.0), six_clean_exits())).unwrap();
            let v = verdict(&r, "rss_sample_coverage_srt_send");
            assert!(!v.pass, "{}", v.detail);
            assert!(v.detail.contains("largest gap"), "{}", v.detail);
            let c = r.coverage.iter().find(|c| c.process == "send").unwrap();
            assert!(c.largest_gap_s >= 600.0);
        }

        #[test]
        fn nonzero_worker_exit_fails_worker_exits_verdict() {
            let mut exits = six_clean_exits();
            exits.insert("srt-send".to_string(), 137);
            let r = build_soak_results(inputs(flat_series(121, 30.0), cfg(3600.0, 30.0), exits))
                .unwrap();
            let v = verdict(&r, "worker_exits");
            assert!(!v.pass);
            assert!(
                v.detail.contains("srt-send") && v.detail.contains("137"),
                "{}",
                v.detail
            );
            assert!(!r.overall_pass);
        }

        #[test]
        fn expected_nonzero_exit_listed_in_config_passes() {
            let mut exits = six_clean_exits();
            exits.insert("srt-send".to_string(), 137);
            let mut config = cfg(3600.0, 30.0);
            config
                .expected_worker_exits
                .insert("srt-send".to_string(), 137);
            let r = build_soak_results(inputs(flat_series(121, 30.0), config, exits)).unwrap();
            assert!(verdict(&r, "worker_exits").pass);
        }

        #[test]
        fn missing_worker_role_in_exits_fails() {
            // `inputs()` builds a single-leg (srt-only) run via `one_leg`,
            // so the removed role must be one of the srt-* three — a
            // missing rist-* role is legitimately not required here (see
            // `single_leg_run_requires_only_that_legs_roles` below).
            let mut exits = six_clean_exits();
            exits.remove("srt-recv");
            let r = build_soak_results(inputs(flat_series(121, 30.0), cfg(3600.0, 30.0), exits))
                .unwrap();
            let v = verdict(&r, "worker_exits");
            assert!(!v.pass && v.detail.contains("srt-recv"), "{}", v.detail);
        }

        /// A single-leg (srt-only) run — e.g. a local smoke test — never
        /// launches the `rist-*` trio at all, so `worker_exits` must not
        /// require their presence in `exits.json`; only the srt-* three
        /// (the leg actually present in `legs`) are required.
        #[test]
        fn single_leg_run_requires_only_that_legs_roles() {
            let exits: BTreeMap<String, i32> = [("srt-send", 0), ("srt-proxy", 0), ("srt-recv", 0)]
                .into_iter()
                .map(|(r, s)| (r.to_string(), s))
                .collect();
            let r = build_soak_results(inputs(flat_series(121, 30.0), cfg(3600.0, 30.0), exits))
                .unwrap();
            let v = verdict(&r, "worker_exits");
            assert!(v.pass, "{}", v.detail);
        }

        /// A key in `exits.json` that isn't one of the six `WORKER_ROLES`
        /// must fail `worker_exits` rather than being silently ignored —
        /// otherwise an unknown role's nonzero status (a renamed worker, a
        /// typo, a future seventh role) never surfaces anywhere.
        #[test]
        fn unknown_role_in_exits_json_fails_worker_exits() {
            let mut exits = six_clean_exits();
            exits.insert("bogus-role".to_string(), 137);
            let r = build_soak_results(inputs(flat_series(121, 30.0), cfg(3600.0, 30.0), exits))
                .unwrap();
            let v = verdict(&r, "worker_exits");
            assert!(
                !v.pass && v.detail.contains("bogus-role") && v.detail.contains("137"),
                "{}",
                v.detail
            );
            assert!(!r.overall_pass);
        }

        // (a) flat RSS -> pass (and, with no threshold set, provisional).
        // Samples cover all 3 processes (not just "send") so the
        // per-combination data-presence check (see `missing_data`) has
        // nothing to flag here — this test is about the regression
        // math, not that check.
        #[test]
        fn flat_rss_slope_passes() {
            let mut samples: Vec<RssSample> = Vec::new();
            for i in 0..20 {
                let t = i as f64 * 60.0;
                samples.push(rss_row("srt", "send", t, Some(50_000)));
                samples.push(rss_row("srt", "proxy", t, Some(20_000)));
                samples.push(rss_row("srt", "recv", t, Some(50_000)));
            }
            let results = build_soak_results(SoakInputs {
                rss_samples: samples,
                legs: one_leg(LegArtifacts {
                    proxy_stats: proxy_stats(1000, 0, 0.0, None, 0),
                    recv_report: passing_recv_report(1000),
                    send_metrics: cell_metrics(1000),
                    outage_period_s: None,
                }),
                rss_slope_threshold_kb_per_hour: None,
                config: cfg(1140.0, 60.0),
                worker_exits: six_clean_exits(),
            })
            .expect("non-empty rss_samples must not error");

            let slope = results
                .rss_slopes
                .iter()
                .find(|s| s.leg == "srt" && s.process == "send")
                .expect("send slope must be present");
            assert!(
                slope.slope_kb_per_hour.abs() < 1.0,
                "flat RSS must regress to ~0 slope, got {}",
                slope.slope_kb_per_hour
            );
            let v = results
                .verdicts
                .iter()
                .find(|v| v.name == "rss_slope_srt_send")
                .expect("verdict must be present");
            assert!(v.pass);
            assert!(v.provisional, "no threshold given -> provisional");
            assert!(results.overall_pass);
        }

        // (b) 1 MiB/h ramp -> slope recorded; passes with no threshold,
        // fails once a threshold below it is set.
        #[test]
        fn ramping_rss_slope_recorded_and_gated_by_threshold() {
            let mut samples: Vec<RssSample> = Vec::new();
            for i in 0..600 {
                let elapsed_s = i as f64 * 60.0;
                let recv_kb = 50_000 + (elapsed_s / 3600.0 * 1024.0) as u64;
                // send/proxy stay flat — this test is about recv's slope
                // and threshold-gating, but every process still needs
                // >=1 post-warmup sample or the new data-presence check
                // (see `missing_data`) would flag them as missing.
                samples.push(rss_row("srt", "recv", elapsed_s, Some(recv_kb)));
                samples.push(rss_row("srt", "send", elapsed_s, Some(50_000)));
                samples.push(rss_row("srt", "proxy", elapsed_s, Some(20_000)));
            }
            let base_inputs = || SoakInputs {
                rss_samples: samples.clone(),
                legs: one_leg(LegArtifacts {
                    proxy_stats: proxy_stats(1000, 0, 0.0, None, 0),
                    recv_report: passing_recv_report(1000),
                    send_metrics: cell_metrics(1000),
                    outage_period_s: None,
                }),
                rss_slope_threshold_kb_per_hour: None,
                config: cfg(35940.0, 60.0),
                worker_exits: six_clean_exits(),
            };

            let no_threshold =
                build_soak_results(base_inputs()).expect("non-empty rss_samples must not error");
            let slope = no_threshold
                .rss_slopes
                .iter()
                .find(|s| s.process == "recv")
                .expect("recv slope must be present");
            assert!(
                (slope.slope_kb_per_hour - 1024.0).abs() < 20.0,
                "expected ~1024 KiB/h, got {}",
                slope.slope_kb_per_hour
            );
            let v = no_threshold
                .verdicts
                .iter()
                .find(|v| v.name == "rss_slope_srt_recv")
                .unwrap();
            assert!(v.pass, "no threshold set -> never fails");
            assert!(v.provisional);

            let mut with_threshold = base_inputs();
            with_threshold.rss_slope_threshold_kb_per_hour = Some(512.0);
            let results =
                build_soak_results(with_threshold).expect("non-empty rss_samples must not error");
            let v = results
                .verdicts
                .iter()
                .find(|v| v.name == "rss_slope_srt_recv")
                .unwrap();
            assert!(
                !v.pass,
                "a ~1024 KiB/h slope must fail a 512 KiB/h threshold"
            );
            assert!(!v.provisional);
            assert!(!results.overall_pass);
        }

        // (c) an unexplained gap (drop rate the configured schedule
        // can't account for) fails the drop-rate check — see the module
        // doc's "Known telemetry limitations" section for why this
        // stands in for a per-event gap-timing check.
        #[test]
        fn unexplained_excess_drop_fails_the_drop_rate_check() {
            let samples = vec![rss_row("srt", "send", 0.0, Some(1000))];
            let results = build_soak_results(SoakInputs {
                rss_samples: samples,
                legs: one_leg(LegArtifacts {
                    // No outage configured, loss_pct=0 -> expected drop
                    // fraction ~0, but the proxy actually dropped half of
                    // everything.
                    proxy_stats: proxy_stats(500, 500, 0.0, None, 0),
                    recv_report: passing_recv_report(500),
                    send_metrics: cell_metrics(1000),
                    outage_period_s: None,
                }),
                rss_slope_threshold_kb_per_hour: None,
                config: cfg(0.0, 30.0),
                worker_exits: six_clean_exits(),
            })
            .expect("non-empty rss_samples must not error");

            let v = results
                .verdicts
                .iter()
                .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
                .unwrap();
            assert!(
                !v.pass,
                "an unexplained 50% drop rate against ~0% expected must fail"
            );
            assert!(!v.provisional);
            assert!(!results.overall_pass);
        }

        // Positive control for (c): a run whose observed drop rate
        // matches what the proxy's CONTINUOUS configured loss_pct
        // predicts must pass — deliberately independent of the outage
        // schedule (an outage window's true impact on a managed sender
        // is reduced throughput, not proxy-visible drops; see the
        // module doc's "Known telemetry limitations" section for the
        // empirical finding behind that design choice).
        #[test]
        fn drop_rate_matching_configured_outage_passes() {
            let total = 100_000u64;
            let dropped = (total as f64 * 0.02).round() as u64;
            // All 3 processes present at a real 300s cadence (not just
            // "send", and not just the two endpoints) — this test
            // asserts `overall_pass`, so it must clear both the
            // per-combination data-presence check AND the new
            // duration/sample-coverage checks too.
            let mut samples = Vec::new();
            for i in 0..=12 {
                let t = i as f64 * 300.0;
                samples.push(rss_row("srt", "send", t, Some(1000)));
                samples.push(rss_row("srt", "proxy", t, Some(1000)));
                samples.push(rss_row("srt", "recv", t, Some(1000)));
            }
            let results = build_soak_results(SoakInputs {
                rss_samples: samples,
                legs: one_leg(LegArtifacts {
                    proxy_stats: proxy_stats(total - dropped, dropped, 2.0, Some(3600), 360),
                    recv_report: passing_recv_report(total),
                    send_metrics: cell_metrics(total),
                    outage_period_s: Some(3600),
                }),
                rss_slope_threshold_kb_per_hour: None,
                config: cfg(3600.0, 300.0),
                worker_exits: six_clean_exits(),
            })
            .expect("non-empty rss_samples must not error");

            let v = results
                .verdicts
                .iter()
                .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
                .unwrap();
            assert!(
                v.pass,
                "drop rate matching the configured outage+loss schedule must pass: {}",
                v.detail
            );
            assert!(results.overall_pass);
        }

        /// (Copilot PR-review fix regression) `expected_outage_windows`
        /// must not count window 0: `soak.sh` deliberately launches the
        /// proxy `outage_dur_s + 30` seconds ahead of send/recv
        /// specifically so window 0 (which always starts at the proxy's
        /// own `t=0`) never overlaps traffic that hasn't started yet.
        /// The 1h smoke's own real shape (outage_period_s=21600,
        /// outage_dur_s=90, ~3542s of active traffic) never reaches
        /// window 1 either (that starts at t=21600 in the proxy's frame,
        /// long after the ~3572s active period this shape covers) — so
        /// the correct expectation is 0, not 1 (the pre-fix answer,
        /// which always counted window 0 for any nonzero duration even
        /// though it's structurally unreachable).
        #[test]
        fn expected_outage_windows_excludes_the_unreachable_warmup_window() {
            let samples = vec![
                rss_row("srt", "send", 0.0, Some(1000)),
                rss_row("srt", "send", 3542.0, Some(1000)),
            ];
            let results = build_soak_results(SoakInputs {
                rss_samples: samples,
                legs: one_leg(LegArtifacts {
                    proxy_stats: proxy_stats(98_000, 2_000, 2.0, Some(21600), 90),
                    recv_report: passing_recv_report(1000),
                    send_metrics: cell_metrics(1000),
                    outage_period_s: Some(21600),
                }),
                rss_slope_threshold_kb_per_hour: None,
                config: cfg(3542.0, 30.0),
                worker_exits: six_clean_exits(),
            })
            .expect("non-empty rss_samples must not error");

            assert_eq!(
                results.legs[0].expected_outage_windows, 0,
                "a 1h-smoke-shaped run must never reach window 0 (proxy-only, before \
                 send/recv start) or window 1 (t=21600, long after this run ends)"
            );
        }

        /// The opposite direction, so the fix isn't just trivially always
        /// zero: a run long enough to actually reach several scheduled
        /// windows must count them correctly, still excluding window 0.
        /// 72h at a 6h period reaches windows 1..=12 (window 12 starts at
        /// t=259200, comfortably inside the ~259320s active period the
        /// 120s head-start leaves room for; window 13 at t=280800 does
        /// not).
        #[test]
        fn expected_outage_windows_counts_real_72h_schedule_correctly() {
            let samples = vec![
                rss_row("srt", "send", 0.0, Some(1000)),
                rss_row("srt", "send", 259200.0, Some(1000)),
            ];
            let results = build_soak_results(SoakInputs {
                rss_samples: samples,
                legs: one_leg(LegArtifacts {
                    proxy_stats: proxy_stats(98_000, 2_000, 2.0, Some(21600), 90),
                    recv_report: passing_recv_report(1000),
                    send_metrics: cell_metrics(1000),
                    outage_period_s: Some(21600),
                }),
                rss_slope_threshold_kb_per_hour: None,
                config: cfg(259200.0, 30.0),
                worker_exits: six_clean_exits(),
            })
            .expect("non-empty rss_samples must not error");

            assert_eq!(
                results.legs[0].expected_outage_windows, 12,
                "72h at a 6h period must count windows 1..=12 (never window 0, the \
                 proxy-only warmup window)"
            );
        }

        /// (Important fix regression) The tolerance must SCALE DOWN with
        /// packet volume: a genuine ~2x drop-rate regression (4%
        /// observed against a 2% configured `loss_pct`) must FAIL at a
        /// large `n` (the scale a 72h run actually reaches — hundreds
        /// of thousands to tens of millions of packets), where 2
        /// percentage points of absolute deviation is many standard
        /// errors of noise, not something a flat volume-blind
        /// percentage band could ever catch.
        #[test]
        fn large_n_two_x_drop_rate_regression_fails() {
            let total = 10_000_000u64;
            let dropped = (total as f64 * 0.04).round() as u64; // observed 4%, configured 2%
            let samples = vec![
                rss_row("srt", "send", 0.0, Some(1000)),
                rss_row("srt", "proxy", 0.0, Some(1000)),
                rss_row("srt", "recv", 0.0, Some(1000)),
            ];
            let results = build_soak_results(SoakInputs {
                rss_samples: samples,
                legs: one_leg(LegArtifacts {
                    proxy_stats: proxy_stats(total - dropped, dropped, 2.0, None, 0),
                    recv_report: passing_recv_report(total),
                    send_metrics: cell_metrics(total),
                    outage_period_s: None,
                }),
                rss_slope_threshold_kb_per_hour: None,
                config: cfg(0.0, 30.0),
                worker_exits: six_clean_exits(),
            })
            .expect("non-empty rss_samples must not error");

            let v = results
                .verdicts
                .iter()
                .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
                .unwrap();
            assert!(
                !v.pass,
                "a 4%-observed/2%-configured drop rate at n=10,000,000 must fail: {}",
                v.detail
            );
            assert!(!v.provisional);
            assert!(!results.overall_pass);
        }

        /// (Important fix regression) The SAME 2x relative deviation
        /// (4% observed against 2% configured) at a SMALL `n` must
        /// still pass — at low packet counts that deviation is
        /// unremarkable sampling noise, not evidence of a real
        /// regression, and the tolerance (whether via the binomial term
        /// or the small absolute floor) must reflect that rather than
        /// flagging every short/thin cell as broken.
        #[test]
        fn small_n_two_x_drop_rate_deviation_still_passes() {
            let total = 50u64;
            let dropped = 2u64; // 4% observed vs 2% configured, same ratio as the large-n test above
            // A short but dense RSS series (this test asserts
            // `overall_pass`, so it must also clear duration/sample
            // coverage — not just the drop-rate check it's actually
            // about).
            let mut samples = Vec::new();
            for i in 0..=3 {
                let t = i as f64 * 60.0;
                samples.push(rss_row("srt", "send", t, Some(1000)));
                samples.push(rss_row("srt", "proxy", t, Some(1000)));
                samples.push(rss_row("srt", "recv", t, Some(1000)));
            }
            let results = build_soak_results(SoakInputs {
                rss_samples: samples,
                legs: one_leg(LegArtifacts {
                    proxy_stats: proxy_stats(total - dropped, dropped, 2.0, None, 0),
                    recv_report: passing_recv_report(total),
                    send_metrics: cell_metrics(total),
                    outage_period_s: None,
                }),
                rss_slope_threshold_kb_per_hour: None,
                config: cfg(180.0, 60.0),
                worker_exits: six_clean_exits(),
            })
            .expect("non-empty rss_samples must not error");

            let v = results
                .verdicts
                .iter()
                .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
                .unwrap();
            assert!(
                v.pass,
                "the same 4%-vs-2% deviation at n=50 must pass (sampling noise, not a \
                 regression): {}",
                v.detail
            );
            assert!(results.overall_pass);
        }

        /// The `n == 0` degenerate case must still produce a FINITE
        /// tolerance (never NaN/infinity, which `serde_json` can't
        /// serialize) even though `drop_pass` is already forced false
        /// by the `total != 0` guard regardless of the tolerance value.
        #[test]
        fn zero_packets_tolerance_is_finite_not_nan_or_infinite() {
            let samples = vec![
                rss_row("srt", "send", 0.0, Some(1000)),
                rss_row("srt", "proxy", 0.0, Some(1000)),
                rss_row("srt", "recv", 0.0, Some(1000)),
            ];
            let results = build_soak_results(SoakInputs {
                rss_samples: samples,
                legs: one_leg(LegArtifacts {
                    proxy_stats: proxy_stats(0, 0, 2.0, None, 0),
                    recv_report: passing_recv_report(0),
                    send_metrics: cell_metrics(0),
                    outage_period_s: None,
                }),
                rss_slope_threshold_kb_per_hour: None,
                config: cfg(0.0, 30.0),
                worker_exits: six_clean_exits(),
            })
            .expect("non-empty rss_samples must not error");

            let leg = &results.legs[0];
            assert!(leg.drop_fraction_tolerance.is_finite());
            let v = results
                .verdicts
                .iter()
                .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
                .unwrap();
            assert!(!v.pass, "zero packets crossing the proxy must still fail");
        }

        // (d) malformed CSV -> loud, line-naming error.
        #[test]
        fn malformed_csv_is_a_loud_error() {
            let err =
                parse_rss_csv("elapsed_s,leg,process,pid,rss_kb\n0,srt,send,not-a-pid,1000\n")
                    .expect_err("non-numeric pid must be rejected");
            assert!(err.contains("line 2"), "{err}");

            let err = parse_rss_csv("elapsed_s,leg,process,pid,rss_kb\n0,mars,send,1000,1000\n")
                .expect_err("unknown leg must be rejected");
            assert!(err.contains("unknown leg"), "{err}");

            let err = parse_rss_csv("elapsed_s,leg,process,pid,rss_kb\n0,srt,send,1000\n")
                .expect_err("wrong field count must be rejected");
            assert!(err.contains("5 comma-separated fields"), "{err}");

            let err =
                parse_rss_csv("not,the,right,header\n").expect_err("wrong header must be rejected");
            assert!(err.contains("header"), "{err}");
        }

        #[test]
        fn zero_process_exits_when_every_row_has_rss() {
            let samples = vec![
                rss_row("srt", "send", 0.0, Some(1000)),
                rss_row("srt", "send", 30.0, Some(1000)),
            ];
            let results = build_soak_results(SoakInputs {
                rss_samples: samples,
                legs: Vec::new(),
                rss_slope_threshold_kb_per_hour: None,
                config: cfg(30.0, 30.0),
                worker_exits: six_clean_exits(),
            })
            .expect("non-empty rss_samples must not error");
            let v = results
                .verdicts
                .iter()
                .find(|v| v.name == "zero_process_exits")
                .unwrap();
            assert!(v.pass);
            assert!(results.process_exits.is_empty());
        }

        #[test]
        fn a_missing_rss_kb_row_is_recorded_as_a_process_exit_and_fails() {
            let samples = vec![
                rss_row("srt", "recv", 0.0, Some(1000)),
                rss_row("srt", "recv", 30.0, None),
            ];
            let results = build_soak_results(SoakInputs {
                rss_samples: samples,
                legs: Vec::new(),
                rss_slope_threshold_kb_per_hour: None,
                config: cfg(30.0, 30.0),
                worker_exits: six_clean_exits(),
            })
            .expect("non-empty rss_samples must not error");
            assert_eq!(results.process_exits.len(), 1);
            assert_eq!(results.process_exits[0].elapsed_s, 30.0);
            let v = results
                .verdicts
                .iter()
                .find(|v| v.name == "zero_process_exits")
                .unwrap();
            assert!(!v.pass);
            assert!(!results.overall_pass);
        }

        // File-driven round trip, mirroring `merge_and_render_end_to_end_via_tempdir`'s
        // convention above.
        #[test]
        fn run_reads_files_and_writes_soak_results_json() {
            let dir = std::env::temp_dir().join(format!(
                "tst-interop-soak-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("time moves forward")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");

            let rss_path = dir.join("rss.csv");
            std::fs::write(
                &rss_path,
                "elapsed_s,leg,process,pid,rss_kb\n0,srt,send,1,1000\n30,srt,send,2,1000\n",
            )
            .expect("write rss.csv");

            let config_path = dir.join("soak-config.json");
            std::fs::write(
                &config_path,
                serde_json::to_string(&cfg(180.0, 30.0)).unwrap(),
            )
            .expect("write soak-config.json");
            let exits_path = dir.join("exits.json");
            std::fs::write(
                &exits_path,
                serde_json::to_string(&six_clean_exits()).unwrap(),
            )
            .expect("write exits.json");

            let proxy_path = dir.join("proxy-stats.json");
            std::fs::write(
                &proxy_path,
                serde_json::to_string(&proxy_stats(1000, 0, 0.0, None, 0)).unwrap(),
            )
            .expect("write proxy stats");
            let recv_path = dir.join("recv-report.json");
            std::fs::write(
                &recv_path,
                serde_json::to_string(&passing_recv_report(1000)).unwrap(),
            )
            .expect("write recv report");
            let send_path = dir.join("send-report.json");
            std::fs::write(
                &send_path,
                serde_json::to_string(&cell_metrics(1000)).unwrap(),
            )
            .expect("write send report");

            let out_path = dir.join("soak-results.json");
            let results = run(
                &rss_path,
                &config_path,
                &exits_path,
                &proxy_path,
                &recv_path,
                &send_path,
                0,
                None,
                None,
                &out_path,
            )
            .expect("run must succeed");

            assert!(out_path.exists());
            assert_eq!(results.legs.len(), 1);
            assert_eq!(results.legs[0].leg, "srt");
            let written =
                std::fs::read_to_string(&out_path).expect("read written soak-results.json");
            assert!(written.contains("\"overall_pass\""));

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// (Critical fix regression) A header-only `rss.csv` (zero data
        /// rows — the exact shape a crashed-before-first-tick sampler
        /// would produce) must be a hard error through the FULL
        /// file-driven path, not just a quietly-empty `rss_samples` in
        /// the pure function — `run` must neither write `--out` nor
        /// return a clean, vacuously-passing report.
        #[test]
        fn header_only_rss_csv_is_a_hard_error_through_run() {
            let dir = std::env::temp_dir().join(format!(
                "tst-interop-soak-header-only-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("time moves forward")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");

            let rss_path = dir.join("rss.csv");
            std::fs::write(&rss_path, "elapsed_s,leg,process,pid,rss_kb\n")
                .expect("write header-only rss.csv");

            let config_path = dir.join("soak-config.json");
            std::fs::write(
                &config_path,
                serde_json::to_string(&cfg(180.0, 30.0)).unwrap(),
            )
            .expect("write soak-config.json");
            let exits_path = dir.join("exits.json");
            std::fs::write(
                &exits_path,
                serde_json::to_string(&six_clean_exits()).unwrap(),
            )
            .expect("write exits.json");

            let proxy_path = dir.join("proxy-stats.json");
            std::fs::write(
                &proxy_path,
                serde_json::to_string(&proxy_stats(1000, 0, 0.0, None, 0)).unwrap(),
            )
            .expect("write proxy stats");
            let recv_path = dir.join("recv-report.json");
            std::fs::write(
                &recv_path,
                serde_json::to_string(&passing_recv_report(1000)).unwrap(),
            )
            .expect("write recv report");
            let send_path = dir.join("send-report.json");
            std::fs::write(
                &send_path,
                serde_json::to_string(&cell_metrics(1000)).unwrap(),
            )
            .expect("write send report");

            let out_path = dir.join("soak-results.json");
            let err = run(
                &rss_path,
                &config_path,
                &exits_path,
                &proxy_path,
                &recv_path,
                &send_path,
                0,
                None,
                None,
                &out_path,
            )
            .expect_err("a header-only rss.csv must be a hard error, not a clean pass");
            assert!(
                err.contains("zero data rows"),
                "error must name the problem: {err}"
            );
            assert!(
                !out_path.exists(),
                "run must not write --out on this error path"
            );

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// `report soak` judges a run against a DECLARED config — if
        /// that file is missing, there is nothing to judge against, so
        /// `run` must error (naming the missing path) and write no
        /// `--out`, exactly like the other precondition failures above.
        #[test]
        fn run_without_config_file_is_a_hard_error() {
            let dir = std::env::temp_dir().join(format!(
                "tst-interop-soak-no-config-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("time moves forward")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");

            let rss_path = dir.join("rss.csv");
            std::fs::write(
                &rss_path,
                "elapsed_s,leg,process,pid,rss_kb\n0,srt,send,1,1000\n30,srt,send,2,1000\n",
            )
            .expect("write rss.csv");

            let exits_path = dir.join("exits.json");
            std::fs::write(
                &exits_path,
                serde_json::to_string(&six_clean_exits()).unwrap(),
            )
            .expect("write exits.json");

            let proxy_path = dir.join("proxy-stats.json");
            std::fs::write(
                &proxy_path,
                serde_json::to_string(&proxy_stats(1000, 0, 0.0, None, 0)).unwrap(),
            )
            .expect("write proxy stats");
            let recv_path = dir.join("recv-report.json");
            std::fs::write(
                &recv_path,
                serde_json::to_string(&passing_recv_report(1000)).unwrap(),
            )
            .expect("write recv report");
            let send_path = dir.join("send-report.json");
            std::fs::write(
                &send_path,
                serde_json::to_string(&cell_metrics(1000)).unwrap(),
            )
            .expect("write send report");

            let out_path = dir.join("soak-results.json");
            let err = run(
                &rss_path,
                &dir.join("missing-config.json"),
                &exits_path,
                &proxy_path,
                &recv_path,
                &send_path,
                21600,
                None,
                None,
                &out_path,
            )
            .unwrap_err();
            assert!(err.contains("missing-config.json"), "{err}");
            assert!(
                !out_path.exists(),
                "no results may be written on a config error"
            );

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Twin of the test above for `--exits`: `report soak` judges a
        /// run against DECLARED worker exit statuses too — if that file
        /// is missing, `run` must error (naming the missing path) and
        /// write no `--out`, exactly like a missing `--config`.
        #[test]
        fn run_without_exits_file_is_a_hard_error() {
            let dir = std::env::temp_dir().join(format!(
                "tst-interop-soak-no-exits-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("time moves forward")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");

            let rss_path = dir.join("rss.csv");
            std::fs::write(
                &rss_path,
                "elapsed_s,leg,process,pid,rss_kb\n0,srt,send,1,1000\n30,srt,send,2,1000\n",
            )
            .expect("write rss.csv");

            let config_path = dir.join("soak-config.json");
            std::fs::write(
                &config_path,
                r#"{"expected_duration_s": 3600, "rss_cadence_s": 30, "warmup_fraction": 0.1667,
                    "sampler_end_slack_s": 35, "expected_worker_exits": {}}"#,
            )
            .expect("write soak-config.json");

            let proxy_path = dir.join("proxy-stats.json");
            std::fs::write(
                &proxy_path,
                serde_json::to_string(&proxy_stats(1000, 0, 0.0, None, 0)).unwrap(),
            )
            .expect("write proxy stats");
            let recv_path = dir.join("recv-report.json");
            std::fs::write(
                &recv_path,
                serde_json::to_string(&passing_recv_report(1000)).unwrap(),
            )
            .expect("write recv report");
            let send_path = dir.join("send-report.json");
            std::fs::write(
                &send_path,
                serde_json::to_string(&cell_metrics(1000)).unwrap(),
            )
            .expect("write send report");

            let out_path = dir.join("soak-results.json");
            let err = run(
                &rss_path,
                &config_path,
                &dir.join("missing-exits.json"),
                &proxy_path,
                &recv_path,
                &send_path,
                21600,
                None,
                None,
                &out_path,
            )
            .unwrap_err();
            assert!(err.contains("missing-exits.json"), "{err}");
            assert!(
                !out_path.exists(),
                "no results may be written on an exits error"
            );

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// (Critical fix regression) `build_soak_results` itself, given
        /// a completely empty `rss_samples`, errors rather than
        /// returning a vacuously-passing `SoakResults` — the pure-
        /// function-level twin of the file-driven test above.
        #[test]
        fn empty_rss_samples_is_a_hard_error() {
            let err = build_soak_results(SoakInputs {
                rss_samples: Vec::new(),
                legs: one_leg(LegArtifacts {
                    proxy_stats: proxy_stats(1000, 0, 0.0, None, 0),
                    recv_report: passing_recv_report(1000),
                    send_metrics: cell_metrics(1000),
                    outage_period_s: None,
                }),
                rss_slope_threshold_kb_per_hour: None,
                config: cfg(0.0, 30.0),
                worker_exits: six_clean_exits(),
            })
            .expect_err("empty rss_samples must be a hard error, not a vacuous pass");
            assert!(err.contains("zero data rows"), "{err}");
        }

        /// (Critical fix regression) `rss.csv` has real data, but one
        /// entire expected process ("recv") never appears for the `srt`
        /// leg — a partial-sampler-death shape distinct from the
        /// completely-empty case above. Must produce a named, FAILING,
        /// non-provisional `rss_data_present_srt_recv` verdict (not just
        /// a silently-shorter `rss_slopes` list) and flip `overall_pass`
        /// false, while the two processes that DO have data still get
        /// their normal, passing verdicts.
        #[test]
        fn missing_one_process_entirely_fails_with_a_named_verdict() {
            let mut samples = Vec::new();
            for i in 0..20 {
                let t = i as f64 * 60.0;
                samples.push(rss_row("srt", "send", t, Some(50_000)));
                samples.push(rss_row("srt", "proxy", t, Some(20_000)));
                // "recv" never appears at all.
            }
            let results = build_soak_results(SoakInputs {
                rss_samples: samples,
                legs: one_leg(LegArtifacts {
                    proxy_stats: proxy_stats(1000, 0, 0.0, None, 0),
                    recv_report: passing_recv_report(1000),
                    send_metrics: cell_metrics(1000),
                    outage_period_s: None,
                }),
                rss_slope_threshold_kb_per_hour: None,
                config: cfg(1140.0, 60.0),
                worker_exits: six_clean_exits(),
            })
            .expect("non-empty rss_samples must not error");

            let missing = results
                .verdicts
                .iter()
                .find(|v| v.name == "rss_data_present_srt_recv")
                .expect("a named verdict must flag the entirely-missing process");
            assert!(!missing.pass);
            assert!(!missing.provisional);
            assert!(!results.overall_pass);

            // The two processes that DO have data are unaffected —
            // this isn't a blanket "any gap anywhere fails everything"
            // check, just the missing combination.
            let send_slope = results
                .verdicts
                .iter()
                .find(|v| v.name == "rss_slope_srt_send")
                .expect("send verdict must still be present");
            assert!(send_slope.pass);
            assert!(
                !results
                    .verdicts
                    .iter()
                    .any(|v| v.name.starts_with("rss_data_present_srt_send")),
                "a process that DOES have data must not also get a missing-data verdict"
            );
        }

        /// (Folds Minor 6; contract updated by the completeness-verdicts
        /// wave) Exactly one post-warmup sample per process — distinct
        /// from the entirely-missing case above: this data IS present,
        /// just too thin for a real regression. Under the OLD contract
        /// this regressed to a 0.0 slope; under the NEW one, a single
        /// post-warmup sample can't clear `rss_sample_coverage_*`
        /// (fewer than 2 distinct timestamps), so it's reported as
        /// missing evidence — no `rss_slope_*` verdict at all — rather
        /// than a (misleadingly confident-looking) flat 0.0 slope. A
        /// single present sample must still NOT trip the OLDER,
        /// separate `rss_data_present_*` missing-data check (that one
        /// is about zero samples, not "not enough for a confident
        /// slope").
        #[test]
        fn single_post_warmup_sample_is_missing_evidence_not_zero_slope() {
            // Two ticks per process: one at t=0 (pre-warmup, with a
            // declared 6000s run -> warmup_s=min(1800, 1000)=1000s,
            // excluding it) and one at t=6000 (post-warmup, the lone
            // surviving sample).
            let mut samples = Vec::new();
            for process in ["send", "proxy", "recv"] {
                samples.push(rss_row("srt", process, 0.0, Some(10_000)));
                samples.push(rss_row("srt", process, 6000.0, Some(10_500)));
            }
            let results = build_soak_results(SoakInputs {
                rss_samples: samples,
                legs: one_leg(LegArtifacts {
                    proxy_stats: proxy_stats(1000, 0, 0.0, None, 0),
                    recv_report: passing_recv_report(1000),
                    send_metrics: cell_metrics(1000),
                    outage_period_s: None,
                }),
                rss_slope_threshold_kb_per_hour: None,
                config: cfg(6000.0, 6000.0),
                worker_exits: six_clean_exits(),
            })
            .expect("non-empty rss_samples must not error");

            assert!(
                results.rss_slopes.is_empty(),
                "a single post-warmup sample must not produce a slope: {:?}",
                results.rss_slopes
            );
            for process in ["send", "proxy", "recv"] {
                let v = results
                    .verdicts
                    .iter()
                    .find(|v| v.name == format!("rss_sample_coverage_srt_{process}"))
                    .unwrap_or_else(|| panic!("coverage verdict for {process} must be present"));
                assert!(!v.pass, "{}", v.detail);
            }
            assert!(
                !results
                    .verdicts
                    .iter()
                    .any(|v| v.name.starts_with("rss_slope_")),
                "no rss_slope_* verdict must be emitted when coverage fails"
            );
            assert!(
                !results
                    .verdicts
                    .iter()
                    .any(|v| v.name.starts_with("rss_data_present_")),
                "a single present sample is not 'missing data'"
            );
            assert!(!results.overall_pass);
        }

        #[test]
        fn parses_soak_config_json() {
            let cfg = parse_soak_config(
                r#"{"expected_duration_s": 259200, "rss_cadence_s": 30, "warmup_fraction": 0.1667,
                    "sampler_end_slack_s": 35, "expected_worker_exits": {}}"#,
            )
            .expect("valid config parses");
            assert_eq!(cfg.expected_duration_s, 259200.0);
            assert_eq!(cfg.rss_cadence_s, 30.0);
            assert!(cfg.expected_worker_exits.is_empty());
        }

        #[test]
        fn soak_config_rejects_nonpositive_duration_or_cadence() {
            let e = parse_soak_config(
                r#"{"expected_duration_s": 0, "rss_cadence_s": 30, "warmup_fraction": 0.1,
                    "sampler_end_slack_s": 35, "expected_worker_exits": {}}"#,
            )
            .unwrap_err();
            assert!(e.contains("expected_duration_s"), "{e}");
            let e = parse_soak_config(
                r#"{"expected_duration_s": 100, "rss_cadence_s": -1, "warmup_fraction": 0.1,
                    "sampler_end_slack_s": 35, "expected_worker_exits": {}}"#,
            )
            .unwrap_err();
            assert!(e.contains("rss_cadence_s"), "{e}");
        }

        /// `sampler_end_slack_s >= expected_duration_s` makes the
        /// achievable RSS span non-positive (it is `expected_duration_s`
        /// minus the slack minus two cadences) before a single sample is
        /// even examined — every real run would fail `duration_coverage`
        /// unconditionally, so this is a config error, not a run outcome.
        #[test]
        fn soak_config_rejects_slack_that_consumes_the_run() {
            let e = parse_soak_config(
                r#"{"expected_duration_s": 180, "rss_cadence_s": 30, "warmup_fraction": 0.1,
                    "sampler_end_slack_s": 180, "expected_worker_exits": {}}"#,
            )
            .unwrap_err();
            assert!(
                e.contains("sampler_end_slack_s") && e.contains("expected_duration_s"),
                "{e}"
            );
            assert!(e.contains("180"), "{e}");
            // Strictly greater than is rejected too.
            let e = parse_soak_config(
                r#"{"expected_duration_s": 180, "rss_cadence_s": 30, "warmup_fraction": 0.1,
                    "sampler_end_slack_s": 200, "expected_worker_exits": {}}"#,
            )
            .unwrap_err();
            assert!(
                e.contains("sampler_end_slack_s") && e.contains("expected_duration_s"),
                "{e}"
            );
        }

        /// A config can pass the previous test's simple `slack <
        /// duration` check yet still leave `duration_coverage`'s minimum
        /// accepted span (`expected_duration_s - sampler_end_slack_s -
        /// 2*rss_cadence_s`) at or below zero, which passes vacuously —
        /// this is `--hours 0.02` (72s) at the real 30s/35s cadence/slack
        /// soak.sh always uses: 72 - 35 - 60 = -23.
        #[test]
        fn soak_config_rejects_run_shorter_than_slack_plus_two_cadences() {
            let e = parse_soak_config(
                r#"{"expected_duration_s": 72, "rss_cadence_s": 30, "warmup_fraction": 0.1,
                    "sampler_end_slack_s": 35, "expected_worker_exits": {}}"#,
            )
            .unwrap_err();
            assert!(
                e.contains("expected_duration_s")
                    && e.contains("sampler_end_slack_s")
                    && e.contains("rss_cadence_s"),
                "{e}"
            );
            assert!(e.contains("72"), "{e}");
            // The real smoke config (3600/30/0.1667/35) must still validate.
            parse_soak_config(
                r#"{"expected_duration_s": 3600, "rss_cadence_s": 30, "warmup_fraction": 0.1667,
                    "sampler_end_slack_s": 35, "expected_worker_exits": {}}"#,
            )
            .expect("the real smoke config must still validate");
            // The `cfg(1140.0, 60.0)` test helper's parameters, run through
            // the real parser, must still validate too.
            parse_soak_config(
                r#"{"expected_duration_s": 1140, "rss_cadence_s": 60, "warmup_fraction": 0.16666666666666666,
                    "sampler_end_slack_s": 0, "expected_worker_exits": {}}"#,
            )
            .expect("the cfg(1140.0, 60.0) helper's parameters must still validate");
        }

        /// A config can leave `min_span_s` comfortably positive yet still
        /// imply fewer than 2 post-warmup samples once the warmup window
        /// (`min(1800, expected_duration_s * warmup_fraction)`) and the
        /// cadence are accounted for — `rss_sample_coverage_*` needs >= 2
        /// distinct timestamps to compute a gap at all.
        #[test]
        fn soak_config_rejects_fewer_than_two_implied_post_warmup_samples() {
            // duration 1000, cadence 400, slack 0: min_span = 1000-0-800 =
            // 200 > 0 (passes the duration check), but warmup_s =
            // min(1800, 1000*0.5) = 500, so expected_samples =
            // floor((1000-0-500)/400) = 1 < 2.
            let e = parse_soak_config(
                r#"{"expected_duration_s": 1000, "rss_cadence_s": 400, "warmup_fraction": 0.5,
                    "sampler_end_slack_s": 0, "expected_worker_exits": {}}"#,
            )
            .unwrap_err();
            assert!(
                e.contains("expected_duration_s")
                    && e.contains("rss_cadence_s")
                    && e.contains("warmup"),
                "{e}"
            );
        }

        /// A key in `expected_worker_exits` outside the six `WORKER_ROLES`
        /// is a config typo (a renamed role, a misspelled one) that must
        /// be rejected at parse time rather than silently doing nothing —
        /// the entry would never match anything in `exits.json` either.
        #[test]
        fn soak_config_rejects_unknown_expected_role() {
            let e = parse_soak_config(
                r#"{"expected_duration_s": 3600, "rss_cadence_s": 30, "warmup_fraction": 0.1,
                    "sampler_end_slack_s": 35, "expected_worker_exits": {"bogus-role": 137}}"#,
            )
            .unwrap_err();
            assert!(e.contains("bogus-role"), "{e}");
        }

        #[test]
        fn parses_worker_exits_json() {
            let exits = parse_worker_exits(r#"{"srt-send": 0, "srt-recv": 1, "rist-proxy": 143}"#)
                .expect("valid exits parse");
            assert_eq!(exits["srt-recv"], 1);
            assert_eq!(exits.len(), 3);
            assert!(parse_worker_exits("{}").unwrap().is_empty());
            assert!(parse_worker_exits("not json").is_err());
        }
    }
}

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

- `decode/old` (profile `baseline`): historic gap, now fixed — matched cell now PASSes; consider removing.
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
