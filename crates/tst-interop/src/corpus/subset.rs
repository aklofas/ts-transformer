//! The replay subset: which captures are replayed, under a wall-clock
//! budget. Pure selection over the inventory; the only I/O is writing and
//! reading the plan.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::inventory::{FileEntry, Inventory};
use super::{PREFIX_SECONDS, REPLAY_PICK_MAX_BYTES, RunDir, Sha256Hex};

/// One capture chosen for replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pick {
    pub sha256: Sha256Hex,
    /// `"rare:codec:<v>"`, `"rare:klv:<v>"`, `"shape:<key>"`, `"largest"` or `"longest"`.
    pub why: String,
    /// Set when the capture is longer than the prefix and only its first seconds are replayed.
    pub prefix_seconds: Option<u32>,
    /// Seconds of replay this pick costs.
    pub seconds: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayPlan {
    pub schema_version: u32,
    pub budget_seconds: u32,
    /// Recorded as given; the replay stage validates them.
    pub paths: Vec<String>,
    /// Rare-coverage picks, then shape picks, then largest/longest.
    pub picks: Vec<Pick>,
    /// Shas dropped to fit the budget, sorted.
    pub trimmed: Vec<Sha256Hex>,
    /// Shas chosen but not replayed, with the reason, sorted by sha.
    pub skipped: Vec<(Sha256Hex, String)>,
    pub total_seconds: f64,
    /// The most bytes any kept pick replays ([`predicted_bytes`]): what the
    /// replay holds in memory at its peak.
    pub predicted_peak_bytes: u64,
}

/// Shape keys in selection order.
const SHAPES: [&str; 4] = ["a", "b", "c", "other"];
/// A codec or KLV set carried by fewer files than this is rare.
const RARE_BELOW: usize = 3;

/// The bytes a replay of `f` sends: the whole file, or the share of it the
/// prefix covers.
pub fn predicted_bytes(f: &FileEntry) -> u64 {
    let share = (f64::from(PREFIX_SECONDS) / f.duration_s).min(1.0);
    (f.bytes as f64 * share).ceil() as u64
}

fn make_pick(f: &FileEntry, why: String) -> Pick {
    let long = f.duration_s > f64::from(PREFIX_SECONDS);
    Pick {
        sha256: f.sha256.clone(),
        why,
        prefix_seconds: long.then_some(PREFIX_SECONDS),
        seconds: f.duration_s.min(f64::from(PREFIX_SECONDS)),
    }
}

/// The best of `files` under `key` (the greatest value), ties to the smallest sha.
fn best<'a>(
    files: impl Iterator<Item = &'a FileEntry>,
    key: impl Fn(&FileEntry) -> f64,
) -> Option<&'a FileEntry> {
    files.fold(None, |acc: Option<&FileEntry>, f| match acc {
        None => Some(f),
        Some(a) => {
            if key(f) > key(a) || (key(f) == key(a) && f.sha256 < a.sha256) {
                Some(f)
            } else {
                Some(a)
            }
        }
    })
}

/// Choose the replay subset. Deterministic for a given inventory content
/// whatever the order of its files. A file that qualifies for several
/// reasons keeps the first, in the order rare codec, rare KLV set, shape,
/// largest, longest.
///
/// Rare picks alone may exceed the budget and are never trimmed; the plan
/// then overruns, and `total_seconds` says so.
///
/// A file without a duration (fewer than two PCRs: nothing to pace by) is
/// never chosen. A chosen file whose [`predicted_bytes`] exceed
/// [`REPLAY_PICK_MAX_BYTES`] is recorded in `skipped` and costs no budget.
pub fn select(inv: &Inventory, paths: &[String], budget_seconds: u32) -> ReplayPlan {
    let mut files: Vec<&FileEntry> = inv
        .files
        .iter()
        .filter(|f| f.probe_error.is_none() && f.duration_s > 0.0)
        .collect();
    files.sort_by(|a, b| a.sha256.cmp(&b.sha256));

    let mut picks: Vec<Pick> = Vec::new();
    let mut picked: BTreeSet<Sha256Hex> = BTreeSet::new();
    let mut skipped: Vec<(Sha256Hex, String)> = Vec::new();
    let mut predicted: BTreeMap<Sha256Hex, u64> = BTreeMap::new();
    let mut push = |f: &FileEntry, why: String, picks: &mut Vec<Pick>| {
        if picked.insert(f.sha256.clone()) {
            let bytes = predicted_bytes(f);
            if bytes > REPLAY_PICK_MAX_BYTES {
                skipped.push((f.sha256.clone(), "over the pick size cap".into()));
            } else {
                predicted.insert(f.sha256.clone(), bytes);
                picks.push(make_pick(f, why));
            }
        }
    };

    // (1) rare coverage, codecs then KLV sets, values in sorted order.
    type Values = fn(&FileEntry) -> &Vec<String>;
    let groups: [(&str, Values); 2] = [("codec", |f| &f.codecs), ("klv", |f| &f.klv_sets)];
    for (label, values) in groups {
        let mut carriers: BTreeMap<&str, Vec<&FileEntry>> = BTreeMap::new();
        for f in &files {
            for v in values(f).iter().collect::<BTreeSet<_>>() {
                carriers.entry(v.as_str()).or_default().push(f);
            }
        }
        for (v, carrying) in carriers {
            if carrying.len() < RARE_BELOW {
                for f in carrying {
                    push(f, format!("rare:{label}:{v}"), &mut picks);
                }
            }
        }
    }
    let rare_count = picks.len();

    // (2) the longest capture of each shape present.
    for key in SHAPES {
        if let Some(f) = best(files.iter().copied().filter(|f| f.shape == key), |f| {
            f.duration_s
        }) {
            push(f, format!("shape:{key}"), &mut picks);
        }
    }

    // (3) the largest and the longest overall.
    if let Some(f) = best(files.iter().copied(), |f| f.bytes as f64) {
        push(f, "largest".into(), &mut picks);
    }
    if let Some(f) = best(files.iter().copied(), |f| f.duration_s) {
        push(f, "longest".into(), &mut picks);
    }

    // Budget: drop from the end, never a rare pick.
    let mut trimmed = Vec::new();
    while picks.iter().map(|p| p.seconds).sum::<f64>() > f64::from(budget_seconds)
        && picks.len() > rare_count
    {
        let p = picks.pop().expect("length checked");
        trimmed.push(p.sha256);
    }
    trimmed.sort();
    skipped.sort();
    let total_seconds = picks.iter().map(|p| p.seconds).sum();
    let predicted_peak_bytes = picks
        .iter()
        .map(|p| predicted[&p.sha256])
        .max()
        .unwrap_or(0);

    ReplayPlan {
        schema_version: 1,
        budget_seconds,
        paths: paths.to_vec(),
        picks,
        trimmed,
        skipped,
        total_seconds,
        predicted_peak_bytes,
    }
}

fn plan_path(run: &RunDir) -> PathBuf {
    run.root.join("replay-plan.json")
}

pub fn write(plan: &ReplayPlan, run: &RunDir) -> Result<(), String> {
    std::fs::create_dir_all(&run.root)
        .map_err(|e| format!("cannot create the run directory: {}", e.kind()))?;
    let text = serde_json::to_string_pretty(plan).map_err(|e| format!("replay plan json: {e}"))?;
    std::fs::write(plan_path(run), text).map_err(|e| {
        format!(
            "cannot write replay-plan.json in the run directory: {}",
            e.kind()
        )
    })
}

pub fn read(run: &RunDir) -> Result<ReplayPlan, String> {
    let text = std::fs::read_to_string(plan_path(run)).map_err(|e| {
        format!(
            "cannot read replay-plan.json in the run directory: {}",
            e.kind()
        )
    })?;
    serde_json::from_str(&text).map_err(|e| format!("cannot parse replay-plan.json: {e}"))
}
