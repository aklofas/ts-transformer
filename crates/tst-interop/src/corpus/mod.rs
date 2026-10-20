//! `tst-interop corpus`: sweeps a private directory of real MPEG-TS
//! captures. Nothing here reads, names or copies a capture on its own;
//! the items below are the vocabulary the sweep stages share.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod annotations;
pub mod digest;
pub mod cli;
pub mod invariants;
pub mod judge;
pub mod inventory;
pub mod pace;
pub mod pcr;
pub mod receive;
pub mod replay;
pub mod report;
pub mod shape;
pub mod subset;
pub mod sweep;

/// A SHA-256 digest as 64 lowercase hex characters. `Display` prints the hex.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sha256Hex(pub String);

impl fmt::Display for Sha256Hex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The output directory of one corpus run. The files inside are
/// `inventory.json`, `digests/`, `sweep.json`, `replay-plan.json`,
/// `replay.json`, `replay-rss.csv` and `logs/`, all joined onto `root`.
#[derive(Debug, Clone)]
pub struct RunDir {
    pub root: PathBuf,
}

/// Bytes fed to a demuxer per call when sweeping a capture.
pub const FEED_CHUNK: usize = 188 * 1024;
/// Seconds of each capture replayed as a prefix.
pub const PREFIX_SECONDS: u32 = 600;
/// Total wall-clock seconds the replay stage may spend.
pub const REPLAY_BUDGET_SECONDS: u32 = 7200;
/// Seconds without progress before a replay is declared stalled.
pub const STALL_SECONDS: u64 = 10;
/// The most bytes one pick may replay: a pick is held in memory once
/// while every path sends it, so a larger one is skipped, not replayed.
pub const REPLAY_PICK_MAX_BYTES: u64 = 4 << 30;
/// The transport paths a replay exercises.
pub const PATHS: [&str; 5] = ["srt", "rist", "udp", "tcp", "rtsp-publish"];
