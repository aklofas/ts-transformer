//! Curated notes about a capture, kept outside the corpus as
//! `<dir>/<sha256>.toml`. They excuse findings a person has looked at.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::Sha256Hex;

/// Each key that excuses a rule carries the count a person reviewed beside
/// it, and the PIDs where the rule is per PID: an annotation waives that
/// much of that damage and no more, so a capture that gets worse, or is
/// damaged somewhere else, fails again.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Annotations {
    /// PIDs on a 0x06 stream that carry private data which is not MISB KLV.
    pub non_misb_private_pids: Vec<u16>,
    /// The most undecoded records or unknown samples excused on each PID
    /// of `non_misb_private_pids`.
    pub max_private_records: Option<u64>,
    /// The capture is known to restart or jump its timeline.
    pub known_discontinuity: bool,
    /// The most backward PTS breaks excused, in total over
    /// `discontinuity_pids`.
    pub max_pts_backward_breaks: Option<u64>,
    /// The PIDs whose backward PTS breaks `known_discontinuity` excuses.
    pub discontinuity_pids: Vec<u16>,
    /// A person has reviewed the capture's continuity-counter accounting
    /// and found the difference between the wire walk and the demuxer
    /// explained; the `accounting` rule is then recorded as excused.
    pub known_cc_mismatch: bool,
    /// The largest `|wire − demuxer|` difference excused.
    pub max_cc_mismatch: Option<u64>,
    /// A person has reviewed the capture's demuxer feed errors (a recorder
    /// dropout, say); the `feed-error` rule is then recorded as excused.
    pub known_feed_errors: bool,
    /// The most feed errors excused.
    pub max_feed_errors: Option<u64>,
    /// The capture lost packets and some KLV records are cut short; a KLV
    /// PID whose every undecoded record is a truncation is excused. A
    /// truncation is a record whose declared length runs past its payload;
    /// a missing checksum is malformed data and is never excused here.
    pub known_klv_truncation: bool,
    /// The most truncated records excused on each PID of
    /// `klv_truncation_pids`.
    pub max_klv_truncated: Option<u64>,
    /// The PIDs whose truncated records `known_klv_truncation` excuses.
    pub klv_truncation_pids: Vec<u16>,
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

/// The excusing key a bound or PID-list key belongs to.
fn bounded_key(key: &str) -> Option<&'static str> {
    Some(match key {
        "max_private_records" => "non_misb_private_pids",
        "max_pts_backward_breaks" | "discontinuity_pids" => "known_discontinuity",
        "max_cc_mismatch" => "known_cc_mismatch",
        "max_feed_errors" => "known_feed_errors",
        "max_klv_truncated" | "klv_truncation_pids" => "known_klv_truncation",
        _ => return None,
    })
}

fn count(value: &str) -> Option<u64> {
    value.parse().ok()
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
/// `known_*` flags set true) needs a non-empty `reason`, its `max_*` bound
/// and, where it has one, a non-empty PID list; without one the error names
/// that key's line. A bound or PID list whose key excuses nothing is
/// refused the same way.
pub fn load(dir: &Path, sha: &Sha256Hex) -> Result<Annotations, String> {
    let path = dir.join(format!("{sha}.toml"));
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Annotations::default()),
        Err(e) => return Err(format!("cannot read annotations {sha}.toml: {}", e.kind())),
    };
    let mut ann = Annotations::default();
    // The first line whose key excuses a rule, and that key; then each
    // excusing key's own line, for the bound check.
    let mut excusing: Option<(usize, &str)> = None;
    let mut excusing_lines: Vec<(usize, &str)> = Vec::new();
    // The first line of each bound or PID-list key.
    let mut bound_lines: Vec<(usize, &str)> = Vec::new();
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
            "known_klv_truncation" => {
                ann.known_klv_truncation =
                    flag(value).ok_or_else(|| bad("expected true or false"))?;
            }
            "max_private_records"
            | "max_pts_backward_breaks"
            | "max_cc_mismatch"
            | "max_feed_errors"
            | "max_klv_truncated" => {
                let n = Some(count(value).ok_or_else(|| bad("expected a whole number"))?);
                match key {
                    "max_private_records" => ann.max_private_records = n,
                    "max_pts_backward_breaks" => ann.max_pts_backward_breaks = n,
                    "max_cc_mismatch" => ann.max_cc_mismatch = n,
                    "max_feed_errors" => ann.max_feed_errors = n,
                    _ => ann.max_klv_truncated = n,
                }
            }
            "discontinuity_pids" => {
                ann.discontinuity_pids =
                    pids(value).ok_or_else(|| bad("expected a list like [256, 257]"))?;
            }
            "klv_truncation_pids" => {
                ann.klv_truncation_pids =
                    pids(value).ok_or_else(|| bad("expected a list like [65, 66]"))?;
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
            "known_klv_truncation" => ann.known_klv_truncation,
            _ => false,
        };
        if excuses && excusing.is_none() {
            excusing = Some((i + 1, key));
        }
        if excuses && !excusing_lines.iter().any(|(_, k)| *k == key) {
            excusing_lines.push((i + 1, key));
        }
        if bounded_key(key).is_some() && !bound_lines.iter().any(|(_, k)| *k == key) {
            bound_lines.push((i + 1, key));
        }
    }
    if let Some((n, key)) = excusing
        && ann.reason.as_deref().is_none_or(str::is_empty)
    {
        return Err(format!(
            "annotations {sha}.toml line {n}: {key} excuses a rule and needs a reason = \"...\" line"
        ));
    }
    for (n, key) in excusing_lines {
        // Only the last value of a key counts, so a key set true and then
        // false excuses nothing.
        let (bound, has_bound, list) = match key {
            "non_misb_private_pids" if !ann.non_misb_private_pids.is_empty() => (
                "max_private_records",
                ann.max_private_records.is_some(),
                None,
            ),
            "known_discontinuity" if ann.known_discontinuity => (
                "max_pts_backward_breaks",
                ann.max_pts_backward_breaks.is_some(),
                Some(("discontinuity_pids", &ann.discontinuity_pids)),
            ),
            "known_cc_mismatch" if ann.known_cc_mismatch => {
                ("max_cc_mismatch", ann.max_cc_mismatch.is_some(), None)
            }
            "known_feed_errors" if ann.known_feed_errors => {
                ("max_feed_errors", ann.max_feed_errors.is_some(), None)
            }
            "known_klv_truncation" if ann.known_klv_truncation => (
                "max_klv_truncated",
                ann.max_klv_truncated.is_some(),
                Some(("klv_truncation_pids", &ann.klv_truncation_pids)),
            ),
            _ => continue,
        };
        let needs = |what: &str| {
            format!("annotations {sha}.toml line {n}: {key} excuses a rule and needs {what}")
        };
        if !has_bound {
            return Err(needs(&format!("{bound} = N")));
        }
        if let Some((list_key, pids)) = list
            && pids.is_empty()
        {
            return Err(needs(&format!("{list_key} = [N, ...]")));
        }
    }
    // A bound whose key excuses nothing is a typo; loading it silently
    // would hide that the annotation does not say what was meant.
    for (n, key) in bound_lines {
        let Some(bounds) = bounded_key(key) else {
            continue;
        };
        let set = match bounds {
            "non_misb_private_pids" => !ann.non_misb_private_pids.is_empty(),
            "known_discontinuity" => ann.known_discontinuity,
            "known_cc_mismatch" => ann.known_cc_mismatch,
            "known_feed_errors" => ann.known_feed_errors,
            _ => ann.known_klv_truncation,
        };
        if !set {
            return Err(format!(
                "annotations {sha}.toml line {n}: {key} bounds {bounds}, which excuses nothing"
            ));
        }
    }
    Ok(ann)
}
