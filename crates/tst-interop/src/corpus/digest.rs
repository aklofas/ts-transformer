//! The per-file demux digest: what a capture demultiplexes to under three
//! demuxer configurations, with KLV records hashed in a canonical form.
//! Every later sweep and replay verdict compares against it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tst_core::klv::st0601;
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, DemuxerConfig, MetadataKind, SamplePayload, StrictMode,
};

use super::inventory::kind_name;
use super::pcr::{PCR_BASE_HZ, PcrScan, align_offset, parse_prefix};
use super::{FEED_CHUNK, RunDir, Sha256Hex};

/// The configurations every capture is digested under, in the order run.
pub const CONFIG_KEYS: [&str; 3] = ["default", "unwrap", "strict_full"];

/// The demuxer configuration named by one of [`CONFIG_KEYS`].
///
/// # Panics
/// On a key that is not in [`CONFIG_KEYS`].
pub fn config_for(key: &str) -> DemuxerConfig {
    match key {
        "default" => DemuxerConfig::default(),
        "unwrap" => DemuxerConfig::builder().unwrap_timestamps(true).build(),
        "strict_full" => DemuxerConfig::builder().strict(StrictMode::Full).build(),
        other => panic!("no demuxer configuration named {other:?}"),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KlvRecordDigest {
    pub pid: u16,
    pub pts: i64,
    pub canonical_sha256: Sha256Hex,
    pub tags: Vec<u32>,
    pub decoded: bool,
    pub unknown_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PidDigest {
    pub kind: String,
    pub samples: u64,
    pub metadata: u64,
    pub discontinuities: u64,
    /// `NonConformantIssue` display text to count.
    pub nonconformant: BTreeMap<String, u64>,
    pub pts_first: Option<i64>,
    pub pts_last: Option<i64>,
    /// Running SHA-256 over every sample and metadata payload, in order.
    pub payload_sha256: Sha256Hex,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatsDigest {
    pub program_maps_seen: u64,
    pub pmt_versions_seen: u64,
    pub discontinuities: u64,
    pub nonconformant: u64,
    pub programs_seen: u32,
    pub unwrap_reanchors: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigDigest {
    pub feed_errors: u64,
    /// `DemuxError` display text to count.
    pub feed_error_kinds: BTreeMap<String, u64>,
    pub pids: BTreeMap<u16, PidDigest>,
    pub klv_records: u64,
    pub klv_decoded: u64,
    pub klv_unknown: u64,
    /// SHA-256 over the `canonical_sha256` hex strings of every KLV record,
    /// in event order.
    pub klv_records_sha256: Sha256Hex,
    pub stats: StatsDigest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileDigest {
    pub schema_version: u32,
    pub sha256: Sha256Hex,
    pub bytes: u64,
    pub prefix_seconds: Option<u32>,
    /// Keyed by [`CONFIG_KEYS`].
    pub configs: BTreeMap<String, ConfigDigest>,
    pub elementary_sha256: Sha256Hex,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha_hex(bytes: &[u8]) -> Sha256Hex {
    Sha256Hex(hex(&Sha256::digest(bytes)))
}

/// The digest of one KLV payload: its ST 0601 decode, re-encoded in the
/// encoder's tag order and hashed, so a producer's tag order never changes
/// the hash. A payload that does not decode, or whose decoded record does
/// not re-encode, is hashed as it is and says why in `unknown_reason`.
pub fn klv_record_digest(pid: u16, pts: i64, payload: &[u8]) -> KlvRecordDigest {
    let undecoded = |reason: String| KlvRecordDigest {
        pid,
        pts,
        canonical_sha256: sha_hex(payload),
        tags: Vec::new(),
        decoded: false,
        unknown_reason: Some(reason),
    };
    match st0601::decode(payload) {
        // `encode_to_vec` sizes its buffer from the record, so no retry is needed.
        Ok(rec) => match st0601::encode_to_vec(&rec) {
            Ok(canonical) => KlvRecordDigest {
                pid,
                pts,
                canonical_sha256: sha_hex(&canonical),
                tags: crate::fixtures::observed_tags(&rec).into_iter().collect(),
                decoded: true,
                unknown_reason: None,
            },
            Err(e) => undecoded(format!("re-encode: {e}")),
        },
        Err(e) => undecoded(e.to_string()),
    }
}

struct PidAcc {
    digest: PidDigest,
    hasher: Sha256,
}

/// Everything one demuxer pass collects.
struct Pass {
    feed_errors: u64,
    feed_error_kinds: BTreeMap<String, u64>,
    pids: BTreeMap<u16, PidAcc>,
    klv_records: u64,
    klv_decoded: u64,
    klv_unknown: u64,
    klv_hasher: Sha256,
}

impl Pass {
    fn new() -> Self {
        Self {
            feed_errors: 0,
            feed_error_kinds: BTreeMap::new(),
            pids: BTreeMap::new(),
            klv_records: 0,
            klv_decoded: 0,
            klv_unknown: 0,
            klv_hasher: Sha256::new(),
        }
    }

    fn pid(&mut self, pid: u16, kind: &'static str) -> &mut PidAcc {
        let acc = self.pids.entry(pid).or_insert_with(|| PidAcc {
            digest: PidDigest {
                kind: kind.to_string(),
                samples: 0,
                metadata: 0,
                discontinuities: 0,
                nonconformant: BTreeMap::new(),
                pts_first: None,
                pts_last: None,
                payload_sha256: Sha256Hex(String::new()),
            },
            hasher: Sha256::new(),
        });
        // A PID first seen before its PMT has no kind yet.
        if acc.digest.kind == "unknown" && kind != "unknown" {
            acc.digest.kind = kind.to_string();
        }
        acc
    }

    fn drain(&mut self, demuxer: &mut Demuxer) {
        while let Some(ev) = demuxer.next_event() {
            match ev {
                DemuxEvent::Sample {
                    stream,
                    pts,
                    payload,
                    ..
                } => {
                    let acc = self.pid(stream.pid, kind_name(&stream.kind));
                    let bytes: &[u8] = match &payload {
                        SamplePayload::Video { raw, .. } => raw,
                        SamplePayload::Audio { frames, .. } => frames,
                        SamplePayload::Subtitle { payload, .. } => payload,
                        SamplePayload::Unknown { raw, .. } => raw,
                    };
                    acc.hasher.update(bytes);
                    acc.digest.samples += 1;
                    acc.digest.pts_first.get_or_insert(pts.as_ticks());
                    acc.digest.pts_last = Some(pts.as_ticks());
                }
                DemuxEvent::Metadata {
                    stream,
                    pts,
                    kind,
                    payload,
                } => {
                    let acc = self.pid(stream.pid, kind_name(&stream.kind));
                    acc.hasher.update(&payload);
                    acc.digest.metadata += 1;
                    acc.digest.pts_first.get_or_insert(pts.as_ticks());
                    acc.digest.pts_last = Some(pts.as_ticks());
                    if matches!(
                        kind,
                        MetadataKind::KlvSyncAuCell { .. } | MetadataKind::KlvAsync
                    ) {
                        let rec = klv_record_digest(stream.pid, pts.as_ticks(), &payload);
                        self.klv_records += 1;
                        if rec.decoded {
                            self.klv_decoded += 1;
                        } else {
                            self.klv_unknown += 1;
                        }
                        self.klv_hasher.update(rec.canonical_sha256.0.as_bytes());
                    }
                }
                DemuxEvent::Discontinuity { stream, .. } => {
                    self.pid(stream.pid, kind_name(&stream.kind))
                        .digest
                        .discontinuities += 1;
                }
                DemuxEvent::NonConformant { stream, issue } => {
                    *self
                        .pid(stream.pid, kind_name(&stream.kind))
                        .digest
                        .nonconformant
                        .entry(issue.to_string())
                        .or_default() += 1;
                }
                _ => {}
            }
        }
    }
}

fn digest_config(key: &str, bytes: &[u8]) -> ConfigDigest {
    let mut demuxer = Demuxer::with_config(config_for(key));
    let mut pass = Pass::new();
    for chunk in bytes.chunks(FEED_CHUNK) {
        // A rejection leaves the demuxer usable and its issue queued: count
        // it, drain, and carry on with the next chunk.
        if let Err(e) = demuxer.feed(chunk) {
            pass.feed_errors += 1;
            *pass.feed_error_kinds.entry(e.to_string()).or_default() += 1;
        }
        pass.drain(&mut demuxer);
    }
    demuxer.flush();
    pass.drain(&mut demuxer);
    let stats = demuxer.stats();
    ConfigDigest {
        feed_errors: pass.feed_errors,
        feed_error_kinds: pass.feed_error_kinds,
        pids: pass
            .pids
            .into_iter()
            .map(|(pid, mut acc)| {
                acc.digest.payload_sha256 = Sha256Hex(hex(&acc.hasher.finalize()));
                (pid, acc.digest)
            })
            .collect(),
        klv_records: pass.klv_records,
        klv_decoded: pass.klv_decoded,
        klv_unknown: pass.klv_unknown,
        klv_records_sha256: Sha256Hex(hex(&pass.klv_hasher.finalize())),
        stats: StatsDigest {
            program_maps_seen: stats.program_maps_seen,
            pmt_versions_seen: stats.pmt_versions_seen,
            discontinuities: stats.discontinuities,
            nonconformant: stats.nonconformant,
            programs_seen: stats.programs_seen,
            unwrap_reanchors: stats.unwrap_reanchors,
        },
    }
}

/// Offset in `bytes` where a prefix of `seconds` ends: the first packet
/// whose PCR, on the PMT's PCR PID (else the first PID carrying one), lies
/// more than `seconds` after that PID's first PCR. `bytes.len()` when the
/// capture is shorter.
fn prefix_end(bytes: &[u8], seconds: u32) -> usize {
    let pcr_pid = parse_prefix(&bytes[align_offset(bytes)..])
        .ok()
        .and_then(|p| p.pcr_pid);
    let mut scan = PcrScan::with_cut(pcr_pid, u64::from(seconds) * PCR_BASE_HZ);
    for chunk in bytes.chunks(FEED_CHUNK) {
        scan.feed(chunk);
        if let Some(at) = scan.cut_at() {
            return at as usize;
        }
    }
    bytes.len()
}

/// Digests `bytes` (whose SHA-256 is `sha`) under every configuration. With
/// `prefix_seconds`, only the capture's first that many seconds are fed.
pub fn digest_bytes(
    sha: &Sha256Hex,
    bytes: &[u8],
    prefix_seconds: Option<u32>,
) -> Result<FileDigest, String> {
    let fed = match prefix_seconds {
        Some(s) => &bytes[..prefix_end(bytes, s)],
        None => bytes,
    };
    let configs: BTreeMap<String, ConfigDigest> = CONFIG_KEYS
        .iter()
        .map(|key| (key.to_string(), digest_config(key, fed)))
        .collect();
    let elementary_sha256 = elementary_sha256(&configs["default"]);
    Ok(FileDigest {
        schema_version: 1,
        sha256: sha.clone(),
        bytes: bytes.len() as u64,
        prefix_seconds,
        configs,
        elementary_sha256,
    })
}

/// SHA-256 over what a capture carries independent of how it is packetised:
/// per PID in PID order the kind, sample count, metadata count and payload
/// hash, then the KLV records hash. PIDs, timestamps and timing issues are
/// left out, so a re-mux of the same elementary streams hashes the same.
pub fn elementary_sha256(default: &ConfigDigest) -> Sha256Hex {
    let mut h = Sha256::new();
    for pid in default.pids.values() {
        h.update(pid.kind.as_bytes());
        h.update([0]);
        h.update(pid.samples.to_le_bytes());
        h.update(pid.metadata.to_le_bytes());
        h.update(pid.payload_sha256.0.as_bytes());
    }
    h.update(default.klv_records_sha256.0.as_bytes());
    Sha256Hex(hex(&h.finalize()))
}

fn digest_path(run: &RunDir, sha: &Sha256Hex) -> PathBuf {
    run.root.join("digests").join(format!("{sha}.json"))
}

pub fn load(run: &RunDir, sha: &Sha256Hex) -> Result<FileDigest, String> {
    let path = digest_path(run, sha);
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

pub fn store(run: &RunDir, d: &FileDigest) -> Result<(), String> {
    let path = digest_path(run, &d.sha256);
    let dir = run.root.join("digests");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let text = serde_json::to_string_pretty(d).map_err(|e| format!("digest json: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))
}
