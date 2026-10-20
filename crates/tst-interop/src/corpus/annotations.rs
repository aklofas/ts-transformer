//! Curated notes about a capture, kept outside the corpus as
//! `<dir>/<sha256>.toml`. They excuse findings a person has looked at.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::Sha256Hex;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Annotations {
    /// PIDs on a 0x06 stream that carry private data which is not MISB KLV.
    pub non_misb_private_pids: Vec<u16>,
    /// The capture is known to restart or jump its timeline.
    pub known_discontinuity: bool,
    /// A person has reviewed the capture's continuity-counter accounting
    /// and found the difference between the wire walk and the demuxer
    /// explained; the `accounting` rule is then recorded as excused.
    pub known_cc_mismatch: bool,
    /// A person has reviewed the capture's demuxer feed errors (a recorder
    /// dropout, say); the `feed-error` rule is then recorded as excused.
    pub known_feed_errors: bool,
    /// Why; required whenever a key above excuses a rule.
    pub reason: Option<String>,
}

fn pids(value: &str) -> Option<Vec<u16>> {
    let inner = value.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner.split(',').map(|n| n.trim().parse().ok()).collect()
}

fn flag(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn text(value: &str) -> Option<String> {
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('"')).then(|| inner.to_string())
}

/// Reads the annotations for `sha` from `dir`; an absent file is the
/// default. Flat `key = value` lines and `#` comments only; any other line
/// is an error naming its number. A key given twice takes its last value;
/// a comment after a value is part of the value and so is rejected. A key
/// that excuses a rule (a non-empty `non_misb_private_pids`, or one of the
/// `known_*` flags set true) needs a non-empty `reason`; without one the
/// error names that key's line.
pub fn load(dir: &Path, sha: &Sha256Hex) -> Result<Annotations, String> {
    let path = dir.join(format!("{sha}.toml"));
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Annotations::default()),
        Err(e) => return Err(format!("cannot read annotations {sha}.toml: {}", e.kind())),
    };
    let mut ann = Annotations::default();
    // The first line whose key excuses a rule, and that key.
    let mut excusing: Option<(usize, &str)> = None;
    for (i, raw) in body.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = |why: &str| format!("annotations {sha}.toml line {}: {why}", i + 1);
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| bad("expected key = value"))?;
        let (key, value) = (key.trim(), value.trim());
        match key {
            "non_misb_private_pids" => {
                ann.non_misb_private_pids =
                    pids(value).ok_or_else(|| bad("expected a list like [4097, 4101]"))?;
            }
            "known_discontinuity" => {
                ann.known_discontinuity =
                    flag(value).ok_or_else(|| bad("expected true or false"))?;
            }
            "known_cc_mismatch" => {
                ann.known_cc_mismatch = flag(value).ok_or_else(|| bad("expected true or false"))?;
            }
            "known_feed_errors" => {
                ann.known_feed_errors = flag(value).ok_or_else(|| bad("expected true or false"))?;
            }
            "reason" => {
                ann.reason = Some(text(value).ok_or_else(|| bad("expected a quoted string"))?);
            }
            _ => return Err(bad("unknown key")),
        }
        let excuses = match key {
            "non_misb_private_pids" => !ann.non_misb_private_pids.is_empty(),
            "known_discontinuity" => ann.known_discontinuity,
            "known_cc_mismatch" => ann.known_cc_mismatch,
            "known_feed_errors" => ann.known_feed_errors,
            _ => false,
        };
        if excuses && excusing.is_none() {
            excusing = Some((i + 1, key));
        }
    }
    if let Some((n, key)) = excusing
        && ann.reason.as_deref().is_none_or(str::is_empty)
    {
        return Err(format!(
            "annotations {sha}.toml line {n}: {key} excuses a rule and needs a reason = \"...\" line"
        ));
    }
    Ok(ann)
}
