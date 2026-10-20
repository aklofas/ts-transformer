//! The verdict for one capture on one transport path, from the digest of
//! what was sent and the digest of what the receiver demuxed.

use serde::{Deserialize, Serialize};

use super::Sha256Hex;
use super::digest::{FileDigest, PidDigest};

/// How the receiving side of a replay ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReceiverOutcome {
    Completed,
    Error(String),
    Stalled,
    Exited,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileVerdict {
    pub sha256: Sha256Hex,
    /// The transport path this verdict is for.
    pub path: String,
    pub pass: bool,
    pub failures: Vec<String>,
}

/// The first field in which two PID digests differ, as a failure text.
fn pid_failure(pid: u16, want: &PidDigest, got: &PidDigest) -> Option<String> {
    if got.samples < want.samples {
        return Some(format!(
            "delivery incomplete: {} sample(s) short on pid {pid}",
            want.samples - got.samples
        ));
    }
    let fields: [(&str, bool); 12] = [
        ("kind", want.kind != got.kind),
        ("samples", want.samples != got.samples),
        ("metadata", want.metadata != got.metadata),
        (
            "discontinuities",
            want.discontinuities != got.discontinuities,
        ),
        ("nonconformant", want.nonconformant != got.nonconformant),
        ("pts_first", want.pts_first != got.pts_first),
        ("pts_last", want.pts_last != got.pts_last),
        (
            "pts_backward_breaks",
            want.pts_backward_breaks != got.pts_backward_breaks,
        ),
        ("klv_unknown", want.klv_unknown != got.klv_unknown),
        (
            "klv_unknown_reason",
            want.klv_unknown_reason != got.klv_unknown_reason,
        ),
        (
            "unknown_samples",
            want.unknown_samples != got.unknown_samples,
        ),
        ("payload_sha256", want.payload_sha256 != got.payload_sha256),
    ];
    fields
        .iter()
        .find(|(_, differs)| *differs)
        .map(|(name, _)| format!("pid {pid}: {name} differs"))
}

/// Judge one replay of `expected` over transport `path`. A receiver that did
/// not complete fails once, without a digest comparison. `srt`, `rist`, `udp`
/// and `tcp` compare every PID digest and the KLV record hash of the default
/// configuration; `rtsp-publish` re-muxes with its own PIDs and timing, so it
/// compares only the elementary-stream hash and the KLV record count.
pub fn judge(
    path: &str,
    expected: &FileDigest,
    received: Option<&FileDigest>,
    outcome: &ReceiverOutcome,
) -> FileVerdict {
    let failures = failures(path, expected, received, outcome);
    FileVerdict {
        sha256: expected.sha256.clone(),
        path: path.to_string(),
        pass: failures.is_empty(),
        failures,
    }
}

fn failures(
    path: &str,
    expected: &FileDigest,
    received: Option<&FileDigest>,
    outcome: &ReceiverOutcome,
) -> Vec<String> {
    match outcome {
        ReceiverOutcome::Completed => {}
        ReceiverOutcome::Stalled => return vec!["receiver stalled".into()],
        ReceiverOutcome::Exited => return vec!["receiver exited".into()],
        ReceiverOutcome::Error(e) => return vec![format!("receiver error: {e}")],
    }
    let Some(got) = received else {
        return vec!["no received digest".into()];
    };
    let (Some(want_cfg), Some(got_cfg)) =
        (expected.configs.get("default"), got.configs.get("default"))
    else {
        return vec!["digest has no default configuration".into()];
    };
    let mut out = Vec::new();
    if got_cfg.feed_errors > 0 {
        out.push("receiver demux error".to_string());
    }
    if path == "rtsp-publish" {
        if got.elementary_sha256 != expected.elementary_sha256 {
            out.push("elementary streams differ".into());
        }
        if got_cfg.klv_records != want_cfg.klv_records {
            out.push("klv record count differs".into());
        }
        return out;
    }
    let pid_failure = want_cfg
        .pids
        .iter()
        .find_map(|(pid, want)| match got_cfg.pids.get(pid) {
            None => Some(format!("pid {pid}: missing")),
            Some(g) => pid_failure(*pid, want, g),
        });
    let extra = got_cfg
        .pids
        .keys()
        .find(|p| !want_cfg.pids.contains_key(p))
        .map(|p| format!("pid {p}: unexpected"));
    out.extend(pid_failure.or(extra));
    if got_cfg.klv_records_sha256 != want_cfg.klv_records_sha256 {
        out.push("klv records differ".into());
    }
    out
}
