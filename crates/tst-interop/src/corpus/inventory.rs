//! The inventory stage: hash every `.ts` under a directory, probe it
//! through the demuxer, classify its PMT shape and merge identical content
//! found under different names.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tst_core::codec::VideoCodec;
use tst_core::klv::st0601;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, DemuxerConfig, SamplePayload, StreamKind};
use tst_test_helpers::rawts::{self, classify_packet};

use super::shape::classify;
use super::{FEED_CHUNK, RunDir, Sha256Hex};

/// Bytes read from the start of a capture for the PMT shape. A PMT repeats
/// within a fraction of a second, so the prefix always holds one.
const PMT_PREFIX: usize = 4 * 1024 * 1024;
/// A 33-bit PCR base wraps after this many 90 kHz ticks.
const PCR_BASE_WRAP: u64 = 1 << 33;
const PCR_BASE_HZ: f64 = 90_000.0;

/// One elementary stream of the capture's first program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamEntry {
    pub pid: u16,
    pub stream_type: u8,
    pub klva: bool,
    /// `"video"`, `"audio"`, `"klv-sync"`, `"klv-async"` or `"unknown"`,
    /// from the stream kind the demuxer declares for the PID.
    pub kind: String,
}

/// One distinct capture, by content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub sha256: Sha256Hex,
    /// Every path with this content; the only path field in the tool.
    pub paths: Vec<PathBuf>,
    pub bytes: u64,
    /// Last PCR minus first PCR, in seconds; 0.0 with fewer than two PCRs.
    pub duration_s: f64,
    /// `ShapeClass::key()`.
    pub shape: String,
    pub pcr_pid: Option<u16>,
    pub programs: u32,
    pub streams: Vec<StreamEntry>,
    /// Sorted, deduped, e.g. `["h264"]`.
    pub codecs: Vec<String>,
    /// Sorted, deduped: `"st0601"` when a KLV PES decodes as ST 0601,
    /// `"unknown"` otherwise.
    pub klv_sets: Vec<String>,
    /// Largest video sample payload seen.
    pub largest_au_bytes: u64,
    /// Set when no PMT was found or the demuxer returned an error; the
    /// file still has an entry.
    pub probe_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    pub schema_version: u32,
    /// Sorted by sha256.
    pub files: Vec<FileEntry>,
}

fn kind_name(kind: &StreamKind) -> &'static str {
    match kind {
        StreamKind::Video(_) => "video",
        StreamKind::Audio(_) => "audio",
        StreamKind::KlvSync { .. } => "klv-sync",
        StreamKind::KlvAsync => "klv-async",
        _ => "unknown",
    }
}

fn codec_name(c: VideoCodec) -> &'static str {
    match c {
        VideoCodec::H264 => "h264",
        VideoCodec::H265 => "h265",
        VideoCodec::H266 => "h266",
        VideoCodec::Av1 => "av1",
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn collect_ts(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in rd {
        let entry = entry.map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| format!("stat {}: {e}", path.display()))?;
        // A symlinked directory is skipped so a link loop cannot recurse forever.
        if file_type.is_dir() {
            collect_ts(&path, out)?;
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("ts"))
        {
            out.push(path);
        }
    }
    Ok(())
}

/// First and last PCR base of one PID, and whether its clock stepped back.
///
/// A single backwards step counts as one 33-bit wrap. That is right for a
/// capture that crosses the wrap and over-counts a glitch, which is fine
/// for a duration estimate.
#[derive(Default)]
struct PcrSpan {
    first: u64,
    last: u64,
    wrapped: bool,
}

/// Per-PID PCR spans read packet by packet, carrying a partial packet
/// across chunks. PIDs are never combined.
#[derive(Default)]
struct PcrScan {
    carry: Vec<u8>,
    spans: BTreeMap<u16, PcrSpan>,
    first_pid: Option<u16>,
}

impl PcrScan {
    fn feed(&mut self, chunk: &[u8]) -> Result<(), String> {
        self.carry.extend_from_slice(chunk);
        let whole = self.carry.len() - self.carry.len() % 188;
        for pkt in self.carry[..whole].chunks_exact(188) {
            let pkt: &[u8; 188] = pkt.try_into().expect("chunk is one packet");
            let info = classify_packet(pkt)?;
            let Some(base) = info.pcr_base else { continue };
            self.first_pid.get_or_insert(info.pid);
            let span = self.spans.entry(info.pid).or_insert(PcrSpan {
                first: base,
                last: base,
                wrapped: false,
            });
            if base < span.last {
                span.wrapped = true;
            }
            span.last = base;
        }
        self.carry.drain(..whole);
        Ok(())
    }

    /// Seconds spanned by the declared PCR PID, else the first PID seen
    /// carrying a PCR; 0.0 with fewer than two PCRs.
    fn duration_s(&self, declared: Option<u16>) -> f64 {
        let span = declared
            .and_then(|pid| self.spans.get(&pid))
            .or_else(|| self.first_pid.and_then(|pid| self.spans.get(&pid)));
        match span {
            Some(s) if s.first != s.last || s.wrapped => {
                let ticks = if s.wrapped {
                    s.last + PCR_BASE_WRAP - s.first
                } else {
                    s.last - s.first
                };
                ticks as f64 / PCR_BASE_HZ
            }
            _ => 0.0,
        }
    }
}

/// Hash and probe one file in a single read.
fn probe(path: &Path) -> Result<FileEntry, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut demuxer = Demuxer::with_config(DemuxerConfig::default());
    let mut pcr = PcrScan::default();
    let mut prefix: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; FEED_CHUNK];
    let mut total: u64 = 0;
    let mut probe_error: Option<String> = None;
    let mut kinds: BTreeMap<u16, &'static str> = BTreeMap::new();
    let mut programs: BTreeSet<u16> = BTreeSet::new();
    let mut codecs: BTreeSet<String> = BTreeSet::new();
    let mut klv_sets: BTreeSet<String> = BTreeSet::new();
    let mut largest_au: u64 = 0;
    let mut feeding = true;

    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        total += n as u64;
        hasher.update(chunk);
        if prefix.len() < PMT_PREFIX {
            let take = (PMT_PREFIX - prefix.len()).min(n);
            prefix.extend_from_slice(&chunk[..take]);
        }
        if !feeding {
            continue;
        }
        if let Err(e) = pcr.feed(chunk) {
            probe_error.get_or_insert(format!("wire reader: {e}"));
            feeding = false;
            continue;
        }
        if let Err(e) = demuxer.feed(chunk) {
            probe_error.get_or_insert(format!("demuxer: {e}"));
            feeding = false;
            continue;
        }
        while let Some(ev) = demuxer.next_event() {
            match ev {
                DemuxEvent::ProgramMap(pm) => {
                    programs.insert(pm.program_number);
                    for s in &pm.streams {
                        kinds.entry(s.pid).or_insert(kind_name(&s.kind));
                    }
                }
                DemuxEvent::Sample {
                    payload: SamplePayload::Video { codec, raw, .. },
                    ..
                } => {
                    codecs.insert(codec_name(codec).to_string());
                    largest_au = largest_au.max(raw.len() as u64);
                }
                DemuxEvent::Metadata { payload, .. } => {
                    let set = if st0601::decode(&payload).is_ok() {
                        "st0601"
                    } else {
                        "unknown"
                    };
                    klv_sets.insert(set.to_string());
                }
                _ => {}
            }
        }
    }

    let parsed = match rawts::try_parsed(&prefix) {
        Ok(p) => p,
        Err(e) => {
            probe_error.get_or_insert(format!("wire reader: {e}"));
            Default::default()
        }
    };
    if parsed.pmt_pid.is_none() || parsed.streams.is_empty() {
        probe_error = Some(match probe_error {
            Some(e) => format!("no PMT found; {e}"),
            None => "no PMT found".to_string(),
        });
    }
    let duration_s = pcr.duration_s(parsed.pcr_pid);
    let sha = Sha256Hex(hex(&hasher.finalize()));
    Ok(FileEntry {
        sha256: sha,
        paths: vec![path.to_path_buf()],
        bytes: total,
        duration_s,
        shape: classify(&parsed).key().to_string(),
        pcr_pid: parsed.pcr_pid,
        programs: programs.len() as u32,
        streams: parsed
            .streams
            .iter()
            .map(|s| StreamEntry {
                pid: s.pid,
                stream_type: s.stream_type,
                klva: s.klva,
                kind: kinds.get(&s.pid).copied().unwrap_or("unknown").to_string(),
            })
            .collect(),
        codecs: codecs.into_iter().collect(),
        klv_sets: klv_sets.into_iter().collect(),
        largest_au_bytes: largest_au,
        probe_error,
    })
}

/// Walk `dir` for `*.ts` files recursively and inventory them in parallel.
pub fn build(dir: &Path) -> Result<Inventory, String> {
    let mut paths = Vec::new();
    collect_ts(dir, &mut paths)?;
    paths.sort();
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Result<FileEntry, String>>> = Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(paths.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = paths.get(i) else { break };
                    let r = probe(path);
                    results.lock().unwrap().push(r);
                }
            });
        }
    });
    let mut merged: BTreeMap<String, FileEntry> = BTreeMap::new();
    for r in results.into_inner().unwrap() {
        let entry = r?;
        match merged.get_mut(&entry.sha256.0) {
            Some(existing) => existing.paths.extend(entry.paths),
            None => {
                merged.insert(entry.sha256.0.clone(), entry);
            }
        }
    }
    let mut files: Vec<FileEntry> = merged.into_values().collect();
    for f in &mut files {
        f.paths.sort();
    }
    Ok(Inventory {
        schema_version: 1,
        files,
    })
}

fn inventory_path(run: &RunDir) -> PathBuf {
    run.root.join("inventory.json")
}

pub fn write(inv: &Inventory, run: &RunDir) -> Result<(), String> {
    std::fs::create_dir_all(&run.root)
        .map_err(|e| format!("create {}: {e}", run.root.display()))?;
    let text = serde_json::to_string_pretty(inv).map_err(|e| format!("inventory json: {e}"))?;
    let path = inventory_path(run);
    std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

pub fn read(run: &RunDir) -> Result<Inventory, String> {
    let path = inventory_path(run);
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}
