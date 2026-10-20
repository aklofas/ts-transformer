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

use super::pcr::{PMT_PREFIX, PcrScan, parse_prefix};
use super::shape::classify;
use super::{FEED_CHUNK, RunDir, Sha256Hex};

/// One elementary stream of the capture's first program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamEntry {
    pub pid: u16,
    pub stream_type: u8,
    pub klva: bool,
    /// `"video"`, `"audio"`, `"subtitle"`, `"klv-sync"`, `"klv-async"` or `"unknown"`,
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
    /// The PCR span of the declared PCR PID in seconds, its discontinuities
    /// left out (see [`super::pcr::PcrScan`]); 0.0 with fewer than two PCRs.
    pub duration_s: f64,
    /// PCR resets and jumps over one second on that PID.
    #[serde(default)]
    pub pcr_discontinuities: u64,
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

pub(super) fn kind_name(kind: &StreamKind) -> &'static str {
    match kind {
        StreamKind::Video(_) => "video",
        StreamKind::Audio(_) => "audio",
        StreamKind::Subtitle(_) => "subtitle",
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
    let rd = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read the corpus directory: {}", e.kind()))?;
    for entry in rd {
        let entry = entry.map_err(|e| format!("cannot read the corpus directory: {}", e.kind()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| format!("cannot stat a corpus entry: {}", e.kind()))?;
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

/// Hash and probe one file in a single read.
fn probe(path: &Path) -> Result<FileEntry, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("open failed: {}", e.kind()))?;
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
            .map_err(|e| format!("read failed: {}", e.kind()))?;
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
        pcr.feed(chunk);
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

    let parsed = parse_prefix(&prefix);
    if parsed.pmt_pid.is_none() || parsed.streams.is_empty() {
        probe_error = Some(match probe_error {
            Some(e) => format!("no PMT found; {e}"),
            None => "no PMT found".to_string(),
        });
    }
    let duration_s = pcr.duration_s(parsed.pcr_pid);
    let pcr_discontinuities = pcr.discontinuities(parsed.pcr_pid);
    let sha = Sha256Hex(hex(&hasher.finalize()));
    Ok(FileEntry {
        sha256: sha,
        paths: vec![path.to_path_buf()],
        bytes: total,
        duration_s,
        pcr_discontinuities,
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

/// The entry of a file whose probe panicked: its hash and size only, with
/// the panic as its probe error, so the sweep counts it as a parse failure
/// named by its hash and the rest of the corpus is still read.
fn panicked_entry(path: &Path) -> Result<FileEntry, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("open failed: {}", e.kind()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; FEED_CHUNK];
    let mut total: u64 = 0;
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read failed: {}", e.kind()))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok(FileEntry {
        sha256: Sha256Hex(hex(&hasher.finalize())),
        paths: vec![path.to_path_buf()],
        bytes: total,
        duration_s: 0.0,
        pcr_discontinuities: 0,
        shape: "other".to_string(),
        pcr_pid: None,
        programs: 0,
        streams: Vec::new(),
        codecs: Vec::new(),
        klv_sets: Vec::new(),
        largest_au_bytes: 0,
        probe_error: Some("the probe panicked".to_string()),
    })
}

/// Walk `dir` for `*.ts` files recursively and inventory them in parallel.
/// A directory with no `.ts` file is an error: an empty or unmounted corpus
/// must not pass.
pub fn build(dir: &Path) -> Result<Inventory, String> {
    let mut paths = Vec::new();
    collect_ts(dir, &mut paths)?;
    if paths.is_empty() {
        return Err("no captures found: the corpus directory holds no .ts file".to_string());
    }
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
                    // A panic in one probe is that file's failure, never the run's.
                    let r = std::panic::catch_unwind(|| probe(path))
                        .unwrap_or_else(|_| panicked_entry(path))
                        .map_err(|e| format!("file {} of {}: {e}", i + 1, paths.len()));
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

/// Whether the run directory already holds an `inventory.json`.
pub fn exists(run: &RunDir) -> bool {
    inventory_path(run).exists()
}

pub fn write(inv: &Inventory, run: &RunDir) -> Result<(), String> {
    std::fs::create_dir_all(&run.root)
        .map_err(|e| format!("cannot create the run directory: {}", e.kind()))?;
    let text = serde_json::to_string_pretty(inv).map_err(|e| format!("inventory json: {e}"))?;
    std::fs::write(inventory_path(run), text).map_err(|e| {
        format!(
            "cannot write inventory.json in the run directory: {}",
            e.kind()
        )
    })
}

pub fn read(run: &RunDir) -> Result<Inventory, String> {
    let text = std::fs::read_to_string(inventory_path(run)).map_err(|e| {
        format!(
            "cannot read inventory.json in the run directory: {}",
            e.kind()
        )
    })?;
    serde_json::from_str(&text).map_err(|e| format!("cannot parse inventory.json: {e}"))
}
