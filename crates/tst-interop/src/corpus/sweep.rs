//! The sweep stage: digest every distinct capture, compare each digest with
//! a baseline and record which changes carry a stated reason.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::digest::{self, CONFIG_KEYS, FileDigest};
use super::invariants::InvariantFailure;
use super::inventory::{self, FileEntry};
use super::{RunDir, Sha256Hex};

pub struct SweepArgs {
    pub dir: PathBuf,
    pub run: RunDir,
    pub baseline: Option<PathBuf>,
    pub accepts: Vec<(Sha256Hex, String)>,
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
    /// `git rev-parse HEAD` when run inside a repository, else `None`.
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

fn sha_of_bytes(bytes: &[u8]) -> Sha256Hex {
    Sha256Hex(
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// Digests one distinct capture. Errors name no path.
fn digest_entry(entry: &FileEntry) -> Result<FileDigest, String> {
    if let Some(e) = &entry.probe_error {
        return Err(e.clone());
    }
    let path = entry
        .paths
        .first()
        .ok_or_else(|| "inventory entry has no path".to_string())?;
    let bytes = std::fs::read(path).map_err(|e| format!("read failed: {}", e.kind()))?;
    let sha = sha_of_bytes(&bytes);
    if sha != entry.sha256 {
        return Err("content changed since the inventory".to_string());
    }
    digest::digest_bytes(&sha, &bytes, None)
}

fn same_but_version(a: &FileDigest, b: &FileDigest) -> bool {
    let mut a = a.clone();
    a.schema_version = b.schema_version;
    a == *b
}

pub fn run(args: &SweepArgs) -> Result<SweepSummary, String> {
    let inv = match inventory::read(&args.run) {
        Ok(inv) => inv,
        Err(_) => {
            let inv = inventory::build(&args.dir)?;
            inventory::write(&inv, &args.run)?;
            inv
        }
    };
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Result<FileDigest, String>)>> = Mutex::new(Vec::new());
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
                    let r = digest_entry(entry);
                    results.lock().unwrap().push((i, r));
                }
            });
        }
    });
    let mut by_index: BTreeMap<usize, Result<FileDigest, String>> =
        results.into_inner().unwrap().into_iter().collect();

    let mut used: Vec<bool> = vec![false; args.accepts.len()];
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
        per_file: Vec::new(),
        pass: false,
    };
    for (i, entry) in inv.files.iter().enumerate() {
        let outcome = match by_index.remove(&i).expect("every file was digested") {
            Err(e) => {
                summary.parse_failures += 1;
                FileOutcome::ParseFailure(e)
            }
            Ok(d) => {
                digest::store(&args.run, &d)?;
                let base = match &args.baseline {
                    None => None,
                    Some(dir) => {
                        let p = dir.join(format!("{}.json", entry.sha256));
                        if p.exists() {
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
            invariant_failures: Vec::new(),
        });
    }
    for (u, (sha, _)) in used.iter().zip(&args.accepts) {
        if !u && !summary.unused_accepts.contains(sha) {
            summary.unused_accepts.push(sha.clone());
        }
    }
    summary.pass = summary.changed_unexplained == 0
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
