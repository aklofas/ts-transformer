//! The sweep stage: digest every distinct capture, compare each digest with
//! a baseline and record which changes carry a stated reason.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::annotations;
use super::digest::{self, CONFIG_KEYS, FileDigest};
use super::invariants::{self, InvariantFailure, WireWalk};
use super::inventory::{self, FileEntry};
use super::{RunDir, Sha256Hex};

/// What one worker made of one file: its digest and its wire walk.
type Digested = Result<(FileDigest, WireWalk), String>;

pub struct SweepArgs {
    pub dir: PathBuf,
    pub run: RunDir,
    pub baseline: Option<PathBuf>,
    pub accepts: Vec<(Sha256Hex, String)>,
    /// Directory of `<sha256>.toml` annotations; none when absent.
    pub annotations: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FileOutcome {
    Identical,
    Changed { explained: Option<String> },
    NewNoBaseline,
    ParseFailure(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepFile {
    pub sha256: Sha256Hex,
    pub outcome: FileOutcome,
    pub invariant_failures: Vec<InvariantFailure>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepSummary {
    pub schema_version: u32,
    /// `git rev-parse HEAD` of the repository the tool ran from (its working
    /// directory, not the corpus), `None` outside one.
    pub tree: Option<String>,
    pub configs: Vec<String>,
    pub files: u64,
    pub identical: u64,
    pub changed_explained: u64,
    pub changed_unexplained: u64,
    pub new_no_baseline: u64,
    pub parse_failures: u64,
    pub invariant_failures: u64,
    pub unused_accepts: Vec<Sha256Hex>,
    /// Files whose digest the `--baseline` directory held; `None` without a
    /// baseline. A baseline that matches no file fails the sweep.
    #[serde(default)]
    pub baseline_matched: Option<u64>,
    pub per_file: Vec<SweepFile>,
    pub pass: bool,
}

fn git_head() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Digests one distinct capture by streaming it, then walks its wire.
/// Errors name no path.
fn digest_entry(entry: &FileEntry) -> Result<(FileDigest, WireWalk), String> {
    if let Some(e) = &entry.probe_error {
        return Err(e.clone());
    }
    let path = entry
        .paths
        .first()
        .ok_or_else(|| "inventory entry has no path".to_string())?;
    let d = digest::digest_file(Some(&entry.sha256), path, None)?;
    Ok((d, invariants::walk_file(path)?))
}

/// `current` equals `baseline` up to the schema version and the fields the
/// baseline predates.
fn same_but_version(current: &FileDigest, baseline: &FileDigest) -> bool {
    let mut b = baseline.clone();
    b.schema_version = current.schema_version;
    digest::adopt_fields_the_baseline_predates(&mut b, current);
    *current == b
}

/// Files a sweep invalidates: the replay's results and its subset plan.
const DOWNSTREAM_OUTPUTS: [&str; 3] = ["replay.json", "replay-rss.csv", "replay-plan.json"];

/// SHA-256 over every whole-capture `digests/*.json` file of the run, in
/// sorted file-name order. The replay's prefix digests are its outputs, not
/// its inputs, so they are left out: per file the name, then the byte length (u64 little-endian), then
/// the bytes, so renaming a file or moving bytes between files changes it.
/// It identifies the sweep's digest set. A replay records it when it starts
/// and the report recomputes it; a new sweep also deletes the old replay's
/// files, so a replay of one sweep never stands for another.
pub fn digests_fingerprint(run: &RunDir) -> Result<Sha256Hex, String> {
    let dir = run.root.join("digests");
    let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(&dir)
        .map_err(|e| format!("read digests directory: {}", e.kind()))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .filter(|n| {
            n.to_str()
                .is_some_and(|n| n.ends_with(".json") && !n.contains(".prefix"))
        })
        .collect();
    names.sort();
    let mut h = Sha256::new();
    for n in names {
        let bytes =
            std::fs::read(dir.join(&n)).map_err(|e| format!("read a digest file: {}", e.kind()))?;
        h.update(n.to_string_lossy().as_bytes());
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    Ok(Sha256Hex(
        h.finalize().iter().map(|b| format!("{b:02x}")).collect(),
    ))
}

pub fn run(args: &SweepArgs) -> Result<SweepSummary, String> {
    if let Some(b) = &args.baseline {
        let own = args.run.root.join("digests");
        let same =
            b == &own || matches!((b.canonicalize(), own.canonicalize()), (Ok(x), Ok(y)) if x == y);
        if same {
            return Err(
                "--baseline must not be the run's own digests directory; a sweep would overwrite it"
                    .to_string(),
            );
        }
        // A baseline that is not there would turn every file into a new one
        // and pass: the oracle silently off.
        if !b.is_dir() {
            return Err("--baseline must name an existing directory".to_string());
        }
    }
    // Only an absent inventory is rebuilt; an unreadable one is an error.
    let inv = if inventory::exists(&args.run) {
        inventory::read(&args.run)?
    } else {
        let inv = inventory::build(&args.dir)?;
        inventory::write(&inv, &args.run)?;
        inv
    };
    // Annotations are read before any file is digested, so a typo costs
    // nothing.
    let mut notes: BTreeMap<String, annotations::Annotations> = BTreeMap::new();
    if let Some(dir) = &args.annotations {
        if !dir.is_dir() {
            return Err("--annotations must name an existing directory".to_string());
        }
        for entry in &inv.files {
            notes.insert(
                entry.sha256.0.clone(),
                annotations::load(dir, &entry.sha256)?,
            );
        }
    }
    // A new sweep invalidates what was replayed from the old one.
    for name in DOWNSTREAM_OUTPUTS {
        match std::fs::remove_file(args.run.root.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot remove a stale {name}: {}", e.kind())),
        }
    }
    // The prefix digests belong to the replay that was just invalidated.
    let digests = args.run.root.join("digests");
    if let Ok(rd) = std::fs::read_dir(&digests) {
        for e in rd.filter_map(|e| e.ok()) {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.contains(".prefix") && name.ends_with(".json") {
                std::fs::remove_file(e.path())
                    .map_err(|e| format!("cannot remove a stale prefix digest: {}", e.kind()))?;
            }
        }
    }
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Digested)>> = Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(inv.files.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(entry) = inv.files.get(i) else { break };
                    // A panic is that file's parse failure; the sweep goes on.
                    let r = std::panic::catch_unwind(|| digest_entry(entry))
                        .unwrap_or_else(|_| Err("the digest panicked".to_string()));
                    results.lock().unwrap().push((i, r));
                }
            });
        }
    });
    let mut by_index: BTreeMap<usize, Digested> =
        results.into_inner().unwrap().into_iter().collect();

    let mut used: Vec<bool> = vec![false; args.accepts.len()];
    let mut baseline_matched: u64 = 0;
    let mut summary = SweepSummary {
        schema_version: 1,
        tree: git_head(),
        configs: CONFIG_KEYS.iter().map(|k| k.to_string()).collect(),
        files: inv.files.len() as u64,
        identical: 0,
        changed_explained: 0,
        changed_unexplained: 0,
        new_no_baseline: 0,
        parse_failures: 0,
        invariant_failures: 0,
        unused_accepts: Vec::new(),
        baseline_matched: None,
        per_file: Vec::new(),
        pass: false,
    };
    for (i, entry) in inv.files.iter().enumerate() {
        let mut failures = Vec::new();
        let outcome = match by_index.remove(&i).expect("every file was digested") {
            Err(e) => {
                summary.parse_failures += 1;
                FileOutcome::ParseFailure(e)
            }
            Ok((d, walked)) => {
                let ann = notes.get(&entry.sha256.0).cloned().unwrap_or_default();
                failures = invariants::check(&d.configs["default"], &walked, &ann);
                if failures.iter().any(|f| !f.excused) {
                    summary.invariant_failures += 1;
                }
                let base = match &args.baseline {
                    None => None,
                    Some(dir) => {
                        let p = dir.join(format!("{}.json", entry.sha256));
                        if p.exists() {
                            baseline_matched += 1;
                            let text = std::fs::read_to_string(&p).map_err(|e| {
                                format!("read baseline {}: {}", entry.sha256, e.kind())
                            })?;
                            let b: FileDigest = serde_json::from_str(&text)
                                .map_err(|e| format!("parse baseline {}: {e}", entry.sha256))?;
                            Some(Some(b))
                        } else {
                            Some(None)
                        }
                    }
                };
                digest::store(&args.run, &d)?;
                match base {
                    None | Some(None) => {
                        summary.new_no_baseline += 1;
                        FileOutcome::NewNoBaseline
                    }
                    Some(Some(b)) if same_but_version(&d, &b) => {
                        summary.identical += 1;
                        FileOutcome::Identical
                    }
                    Some(Some(_)) => {
                        let why = args
                            .accepts
                            .iter()
                            .position(|(s, _)| *s == entry.sha256)
                            .map(|p| {
                                used[p] = true;
                                args.accepts[p].1.clone()
                            });
                        if why.is_some() {
                            summary.changed_explained += 1;
                        } else {
                            summary.changed_unexplained += 1;
                        }
                        FileOutcome::Changed { explained: why }
                    }
                }
            }
        };
        summary.per_file.push(SweepFile {
            sha256: entry.sha256.clone(),
            outcome,
            invariant_failures: failures,
        });
    }
    for (u, (sha, _)) in used.iter().zip(&args.accepts) {
        if !u && !summary.unused_accepts.contains(sha) {
            summary.unused_accepts.push(sha.clone());
        }
    }
    summary.unused_accepts.sort_by(|a, b| a.0.cmp(&b.0));
    // A baseline that names none of these captures is the wrong baseline;
    // a first run takes no --baseline at all.
    summary.baseline_matched = args.baseline.as_ref().map(|_| baseline_matched);
    summary.pass = summary.baseline_matched != Some(0)
        && summary.changed_unexplained == 0
        && summary.parse_failures == 0
        && summary.invariant_failures == 0
        && summary.unused_accepts.is_empty();
    std::fs::create_dir_all(&args.run.root)
        .map_err(|e| format!("create run directory: {}", e.kind()))?;
    let text = serde_json::to_string_pretty(&summary).map_err(|e| format!("sweep json: {e}"))?;
    std::fs::write(args.run.root.join("sweep.json"), text)
        .map_err(|e| format!("write sweep.json: {}", e.kind()))?;
    Ok(summary)
}
