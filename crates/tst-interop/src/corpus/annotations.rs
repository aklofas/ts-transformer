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
    pub reason: Option<String>,
}

fn pids(value: &str) -> Option<Vec<u16>> {
    let inner = value.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner.split(',').map(|n| n.trim().parse().ok()).collect()
}

fn text(value: &str) -> Option<String> {
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('"')).then(|| inner.to_string())
}

/// Reads the annotations for `sha` from `dir`; an absent file is the
/// default. Flat `key = value` lines and `#` comments only; any other line
/// is an error naming its number.
pub fn load(dir: &Path, sha: &Sha256Hex) -> Result<Annotations, String> {
    let path = dir.join(format!("{sha}.toml"));
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Annotations::default()),
        Err(e) => return Err(format!("cannot read annotations {sha}.toml: {}", e.kind())),
    };
    let mut ann = Annotations::default();
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
                ann.known_discontinuity = match value {
                    "true" => true,
                    "false" => false,
                    _ => return Err(bad("expected true or false")),
                };
            }
            "reason" => {
                ann.reason = Some(text(value).ok_or_else(|| bad("expected a quoted string"))?);
            }
            _ => return Err(bad("unknown key")),
        }
    }
    Ok(ann)
}
