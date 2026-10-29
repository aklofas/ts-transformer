//! The per-file demux digest: what a capture demultiplexes to under three
//! demuxer configurations, with KLV records hashed in a canonical form.
//! Every later sweep and replay verdict compares against it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tst_core::error::DemuxErrorKind;
use tst_core::error::KlvDecodeError;
use tst_core::klv::length::read_ber;
use tst_core::klv::{UniversalLabel, st0601, st0605};
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, DemuxerConfig, DiscontinuityKind, MetadataKind, NonConformantIssue,
    SamplePayload, StrictMode,
};

use super::inventory::kind_name;
use super::pcr::PrefixCutter;
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
    /// The kinds of the payload's top-level items in order (`"st0605-pack"`,
    /// `"st0601-ls"`); empty unless every item decoded.
    pub items: Vec<String>,
    pub decoded: bool,
    pub unknown_reason: Option<String>,
    /// The payload ends inside an item: a record cut short, as packet loss
    /// leaves it, rather than one that is malformed or foreign.
    pub truncated: bool,
    /// Field-level errors the lenient ST 0601 decoder recorded while
    /// keeping the record.
    pub field_errors: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PidDigest {
    pub kind: String,
    pub samples: u64,
    pub metadata: u64,
    /// Discontinuities the demuxer reported on this PID other than a PES
    /// oversize: continuity jumps and discontinuity indicators, the ones
    /// the wire shows.
    pub discontinuities: u64,
    /// PES reassembly caps exceeded on this PID (per PID or in total): a
    /// discontinuity the demuxer reports that the wire cannot show.
    pub pes_oversize: u64,
    /// `NonConformantIssue` variant name to count.
    pub nonconformant: BTreeMap<String, u64>,
    /// First and last PTS of a payload that carried one.
    pub pts_first: Option<i64>,
    pub pts_last: Option<i64>,
    /// Payloads whose PES carried no PTS. The demuxer gives them a PTS of
    /// 0; they take no part in `pts_first`, `pts_last` or the order rule.
    pub pts_absent: u64,
    /// Times a PTS fell more than [`PTS_BACKWARD_TICKS`] below the running
    /// maximum after 33-bit unwrap; the maximum restarts from that PTS, so a
    /// timeline restart counts once.
    pub pts_backward_breaks: u64,
    /// KLV payloads on this PID that did not decode as ST 0601.
    pub klv_unknown: u64,
    /// Why the first of them did not.
    pub klv_unknown_reason: Option<String>,
    /// Payloads the demuxer handed out as unclassified samples; on a KLV
    /// PID these are payloads that carry neither an AU-cell header nor a
    /// SMPTE label.
    pub unknown_samples: u64,
    /// Running SHA-256 over every sample and metadata payload, in order.
    pub payload_sha256: Sha256Hex,
    /// Of `klv_unknown`, the records that ended inside an item. `None` only
    /// in a digest written before the field existed; see
    /// [`adopt_fields_the_baseline_predates`].
    #[serde(default)]
    pub klv_truncated: Option<u64>,
    /// ST 0601 field errors over this PID's decoded records. `None` as for
    /// `klv_truncated`.
    #[serde(default)]
    pub klv_field_errors: Option<u64>,
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
    /// `DemuxError` variant name to count.
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

/// The top-level KLV items of a payload: per item 16 UL bytes, a BER
/// length, then that many body bytes. Each item comes back whole, UL
/// included. A length that runs past the end, or trailing bytes too short
/// to hold a UL, is [`KlvDecodeError::Truncated`]: a payload is split
/// completely or not at all.
fn split_items(payload: &[u8]) -> Result<Vec<(UniversalLabel, &[u8])>, KlvDecodeError> {
    let mut items = Vec::new();
    let mut at = 0;
    // An empty payload holds no item, so it is short of a UL too.
    while at < payload.len() || items.is_empty() {
        let rest = &payload[at..];
        if rest.len() < 16 {
            return Err(KlvDecodeError::Truncated {
                offset: at,
                needed: 16,
                have: rest.len(),
            });
        }
        let mut ul = [0u8; 16];
        ul.copy_from_slice(&rest[..16]);
        let (len, body) = read_ber(&rest[16..])?;
        let body_at = rest.len() - body.len();
        if body.len() < len {
            return Err(KlvDecodeError::Truncated {
                offset: at + body_at,
                needed: len,
                have: body.len(),
            });
        }
        items.push((UniversalLabel::new(ul), &rest[..body_at + len]));
        at += body_at + len;
    }
    Ok(items)
}

/// The digest of one KLV payload, a sequence of top-level MISB items: an
/// ST 0605 Precision Time Stamp Pack, standalone or before the set, and
/// ST 0601 local sets. A set is decoded and re-encoded in the encoder's
/// tag order, so a producer's tag order never changes the hash; a pack has
/// one shape and is hashed as it is. A payload with an item that does not
/// split, decode or re-encode, or whose UL is neither, is hashed as it is
/// and says why in `unknown_reason`, naming the first failing item.
pub fn klv_record_digest(pid: u16, pts: i64, payload: &[u8]) -> KlvRecordDigest {
    let undecoded = |reason: String, truncated: bool| KlvRecordDigest {
        pid,
        pts,
        canonical_sha256: sha_hex(payload),
        tags: Vec::new(),
        items: Vec::new(),
        decoded: false,
        unknown_reason: Some(reason),
        truncated,
        field_errors: 0,
    };
    let items = match split_items(payload) {
        Ok(items) => items,
        // Only a length that runs past the payload is a cut record; inside a
        // set that splits whole, ST 0601 also reports a missing checksum as
        // truncation, which is a malformed record, not a lost packet.
        Err(e) => {
            let truncated = matches!(e, KlvDecodeError::Truncated { .. });
            return undecoded(e.to_string(), truncated);
        }
    };
    let mut canonical = Vec::with_capacity(payload.len());
    let mut kinds = Vec::with_capacity(items.len());
    let mut tags = std::collections::BTreeSet::new();
    let mut field_errors = 0u64;
    for (i, (ul, item)) in items.iter().enumerate() {
        let failed = |why: String| undecoded(format!("item {i} ({}): {why}", hex(&ul.0)), false);
        if *ul == UniversalLabel::PRECISION_TIMESTAMP_PACK_UL {
            if let Err(e) = st0605::decode(item) {
                return failed(e.to_string());
            }
            canonical.extend_from_slice(item);
            kinds.push("st0605-pack".to_string());
        } else if ul.is_st0601_family() {
            let rec = match st0601::decode(item) {
                Ok(rec) => rec,
                Err(e) => return failed(e.to_string()),
            };
            // `encode_to_vec` sizes its buffer from the record, so no retry is needed.
            match st0601::encode_to_vec(&rec) {
                Ok(bytes) => canonical.extend_from_slice(&bytes),
                Err(e) => return failed(format!("re-encode: {e}")),
            }
            tags.extend(crate::fixtures::observed_tags(&rec));
            field_errors += rec.field_errors.len() as u64;
            kinds.push("st0601-ls".to_string());
        } else {
            return failed("not a registered MISB item".to_string());
        }
    }
    KlvRecordDigest {
        pid,
        pts,
        canonical_sha256: sha_hex(&canonical),
        tags: tags.into_iter().collect(),
        items: kinds,
        decoded: true,
        unknown_reason: None,
        truncated: false,
        field_errors,
    }
}

/// Gives `baseline` the value `current` has for every field the baseline's
/// digest predates (read back as `None`), on every PID both hold under the
/// same configuration. A field added to the digest then never marks a
/// capture changed against a baseline written before it.
pub fn adopt_fields_the_baseline_predates(baseline: &mut FileDigest, current: &FileDigest) {
    for (key, base_cfg) in &mut baseline.configs {
        let Some(cur_cfg) = current.configs.get(key) else {
            continue;
        };
        for (pid, base) in &mut base_cfg.pids {
            let Some(cur) = cur_cfg.pids.get(pid) else {
                continue;
            };
            if base.klv_truncated.is_none() {
                base.klv_truncated = cur.klv_truncated;
            }
            if base.klv_field_errors.is_none() {
                base.klv_field_errors = cur.klv_field_errors;
            }
        }
    }
}

/// The variant name of an issue, without the values its text embeds: a
/// jittery PCR would otherwise give one key per delta.
fn issue_key(issue: &NonConformantIssue) -> &'static str {
    use NonConformantIssue as I;
    match issue {
        I::StreamTypeMismatchSyncOnAsyncPid => "StreamTypeMismatchSyncOnAsyncPid",
        I::StreamTypeMismatchAsyncOnSyncPid => "StreamTypeMismatchAsyncOnSyncPid",
        I::MissingMetadataDescriptor => "MissingMetadataDescriptor",
        I::PcrAnomaly { .. } => "PcrAnomaly",
        I::PsiChecksumMismatch { .. } => "PsiChecksumMismatch",
        I::PusiMidPes => "PusiMidPes",
        I::MalformedPes { .. } => "MalformedPes",
        I::PidReusedAcrossPrograms { .. } => "PidReusedAcrossPrograms",
        I::SubtitleMissingDescriptor { .. } => "SubtitleMissingDescriptor",
        I::SubtitleDescriptorAmbiguous { .. } => "SubtitleDescriptorAmbiguous",
        I::SubtitleDescriptorMalformed { .. } => "SubtitleDescriptorMalformed",
        I::Av1RegistrationMalformed { .. } => "Av1RegistrationMalformed",
        I::Av1ObuMissingSizeField { .. } => "Av1ObuMissingSizeField",
        I::Av1TileListNotAllowed { .. } => "Av1TileListNotAllowed",
        I::PsiOverlongSection { .. } => "PsiOverlongSection",
        I::TransportErrorPacket { .. } => "TransportErrorPacket",
        I::DvbSubDataIdentifier { .. } => "DvbSubDataIdentifier",
        I::PtsAnomaly { .. } => "PtsAnomaly",
        I::MissingRequiredPts { .. } => "MissingRequiredPts",
        I::PesHeaderMalformed { .. } => "PesHeaderMalformed",
        I::SubtitleAlignmentMissing { .. } => "SubtitleAlignmentMissing",
        I::PcrMalformed { .. } => "PcrMalformed",
        I::NalHeader { .. } => "NalHeader",
        I::Av1ObuHeader { .. } => "Av1ObuHeader",
        I::LatmFraming { .. } => "LatmFraming",
        I::PsiCcDiscontinuity { .. } => "PsiCcDiscontinuity",
        I::MultiCellAu { .. } => "MultiCellAu",
        I::CfiTolerated { .. } => "CfiTolerated",
        I::PsiMultiSectionUnsupported { .. } => "PsiMultiSectionUnsupported",
        I::Ac3SyncMissing { .. } => "Ac3SyncMissing",
        I::Av1WrongStreamId { .. } => "Av1WrongStreamId",
        I::Av1MissingTsObuFraming { .. } => "Av1MissingTsObuFraming",
        I::PmtProgramNumberMismatch { .. } => "PmtProgramNumberMismatch",
        I::UnsupportedScrambling { .. } => "UnsupportedScrambling",
        I::AdaptationFieldMalformed { .. } => "AdaptationFieldMalformed",
        I::ZeroLengthPesNonVideo { .. } => "ZeroLengthPesNonVideo",
        I::PsiSyntax { .. } => "PsiSyntax",
        I::Other(_) => "Other",
    }
}

/// The variant name of a feed error, without the values its text embeds,
/// so rewording an error message changes no digest.
fn error_key(kind: DemuxErrorKind) -> &'static str {
    match kind {
        DemuxErrorKind::Unrecoverable => "Unrecoverable",
        DemuxErrorKind::StrictRejection => "StrictRejection",
        DemuxErrorKind::MalformedPes => "MalformedPes",
        DemuxErrorKind::SyncBufExhausted => "SyncBufExhausted",
    }
}

/// A PTS this far (one second at 90 kHz) below the running maximum is a
/// backward break.
pub const PTS_BACKWARD_TICKS: i64 = 90_000;
const PTS_MODULUS: i64 = 1 << 33;

struct PidAcc {
    digest: PidDigest,
    hasher: Sha256,
    /// Last PTS modulo 2^33 and the unwrapped timeline built from it.
    pts_clock: Option<(i64, i64)>,
    pts_max: i64,
}

impl PidAcc {
    /// Records the PTS of one payload. The demuxer reports a PES without a
    /// PTS as 0, the value it never stores as a PES's own; such a payload
    /// is counted in `pts_absent` and judged by no timestamp rule.
    fn note_pts(&mut self, ticks: i64) {
        if ticks == 0 {
            self.digest.pts_absent += 1;
            return;
        }
        self.digest.pts_first.get_or_insert(ticks);
        self.digest.pts_last = Some(ticks);
        let raw = ticks.rem_euclid(PTS_MODULUS);
        let at = match self.pts_clock {
            None => {
                self.pts_max = raw;
                raw
            }
            Some((prev_raw, prev_at)) => {
                let mut delta = raw - prev_raw;
                if delta > PTS_MODULUS / 2 {
                    delta -= PTS_MODULUS;
                } else if delta <= -PTS_MODULUS / 2 {
                    delta += PTS_MODULUS;
                }
                prev_at + delta
            }
        };
        self.pts_clock = Some((raw, at));
        if at < self.pts_max - PTS_BACKWARD_TICKS {
            self.digest.pts_backward_breaks += 1;
            self.pts_max = at;
        } else {
            self.pts_max = self.pts_max.max(at);
        }
    }
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
                pes_oversize: 0,
                nonconformant: BTreeMap::new(),
                pts_first: None,
                pts_last: None,
                pts_absent: 0,
                pts_backward_breaks: 0,
                klv_unknown: 0,
                klv_unknown_reason: None,
                unknown_samples: 0,
                payload_sha256: Sha256Hex(String::new()),
                klv_truncated: Some(0),
                klv_field_errors: Some(0),
            },
            hasher: Sha256::new(),
            pts_clock: None,
            pts_max: 0,
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
                    if matches!(payload, SamplePayload::Unknown { .. }) {
                        acc.digest.unknown_samples += 1;
                    }
                    acc.note_pts(pts.as_ticks());
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
                    acc.note_pts(pts.as_ticks());
                    if matches!(
                        kind,
                        MetadataKind::KlvSyncAuCell { .. } | MetadataKind::KlvAsync
                    ) {
                        let rec = klv_record_digest(stream.pid, pts.as_ticks(), &payload);
                        self.klv_records += 1;
                        if rec.decoded {
                            self.klv_decoded += 1;
                            if rec.field_errors > 0 {
                                let d = &mut self.pid(stream.pid, kind_name(&stream.kind)).digest;
                                *d.klv_field_errors.get_or_insert(0) += rec.field_errors;
                            }
                        } else {
                            self.klv_unknown += 1;
                            let d = &mut self.pid(stream.pid, kind_name(&stream.kind)).digest;
                            d.klv_unknown += 1;
                            if rec.truncated {
                                *d.klv_truncated.get_or_insert(0) += 1;
                            }
                            if d.klv_unknown_reason.is_none() {
                                d.klv_unknown_reason = rec.unknown_reason.clone();
                            }
                        }
                        self.klv_hasher.update(rec.canonical_sha256.0.as_bytes());
                    }
                }
                DemuxEvent::Discontinuity { stream, kind } => {
                    let d = &mut self.pid(stream.pid, kind_name(&stream.kind)).digest;
                    match kind {
                        DiscontinuityKind::PesOversize { .. }
                        | DiscontinuityKind::PesTotalOversize => {
                            d.pes_oversize += 1;
                        }
                        _ => d.discontinuities += 1,
                    }
                }
                DemuxEvent::NonConformant { stream, issue } => {
                    *self
                        .pid(stream.pid, kind_name(&stream.kind))
                        .digest
                        .nonconformant
                        .entry(issue_key(&issue).to_string())
                        .or_default() += 1;
                }
                _ => {}
            }
        }
    }
}

/// One demuxer configuration fed piece by piece: the single per-configuration
/// core every digest entry runs.
struct ConfigRun {
    demuxer: Demuxer,
    pass: Pass,
}

impl ConfigRun {
    fn new(key: &str) -> Self {
        Self {
            demuxer: Demuxer::with_config(config_for(key)),
            pass: Pass::new(),
        }
    }

    fn feed(&mut self, piece: &[u8]) {
        // A rejection returns at once with the rest of the input still
        // buffered and the issue queued. Empty feeds resume it, one
        // rejection per call, so every one is counted exactly once whatever
        // the chunking; each consumes a packet, so the loop ends.
        let mut input = piece;
        while let Err(e) = self.demuxer.feed(input) {
            self.pass.feed_errors += 1;
            *self
                .pass
                .feed_error_kinds
                .entry(error_key(e.kind()).to_string())
                .or_default() += 1;
            input = &[];
        }
        self.pass.drain(&mut self.demuxer);
    }

    fn finish(mut self) -> ConfigDigest {
        self.demuxer.flush();
        self.pass.drain(&mut self.demuxer);
        let stats = self.demuxer.stats();
        let pass = self.pass;
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
}

/// The digest of `bytes` under one configuration, fed `chunk` bytes at a
/// time. The result does not depend on `chunk`.
pub fn digest_config(key: &str, bytes: &[u8], chunk: usize) -> ConfigDigest {
    let mut run = ConfigRun::new(key);
    bytes.chunks(chunk).for_each(|piece| run.feed(piece));
    run.finish()
}

/// A capture digested as its bytes arrive, under every configuration at
/// once, without holding the capture: the SHA-256, the byte count, the
/// prefix cut and the three demuxers advance in lockstep per chunk.
/// [`digest_bytes`] and [`digest_file`] run this same code, and so does a
/// receiver that digests what a replay delivered.
///
/// With `prefix_seconds`, bytes are fed to the demuxers only up to the
/// packet whose PCR span first exceeds that many seconds (the packet itself
/// excluded), the cut [`super::pcr::prefix_end`] makes over a whole file: both read the
/// PCR PID from the first [`super::pcr::PMT_PREFIX`] bytes and cut where the span is
/// strictly greater than `seconds` x 90 000 ticks. The SHA-256 and the byte
/// count still cover every byte fed in, as for a file whose prefix is
/// digested. At most about [`super::pcr::PMT_PREFIX`] bytes are held back to find the PMT.
pub struct Incremental {
    prefix_seconds: Option<u32>,
    sha: Sha256,
    total: u64,
    runs: Vec<ConfigRun>,
    cutter: Option<PrefixCutter>,
}

impl Incremental {
    pub fn new(prefix_seconds: Option<u32>) -> Self {
        Self {
            prefix_seconds,
            sha: Sha256::new(),
            total: 0,
            runs: CONFIG_KEYS.iter().map(|k| ConfigRun::new(k)).collect(),
            cutter: prefix_seconds.map(PrefixCutter::new),
        }
    }

    /// Bytes fed in so far, cut or not.
    pub fn bytes_fed(&self) -> u64 {
        self.total
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.sha.update(chunk);
        self.total += chunk.len() as u64;
        let runs = &mut self.runs;
        let mut to_runs = |piece: &[u8]| runs.iter_mut().for_each(|r| r.feed(piece));
        match &mut self.cutter {
            Some(c) => c.push(chunk, &mut to_runs),
            None => to_runs(chunk),
        }
    }

    pub fn finish(mut self) -> Result<FileDigest, String> {
        let runs = &mut self.runs;
        if let Some(c) = &mut self.cutter {
            c.finish(&mut |piece: &[u8]| runs.iter_mut().for_each(|r| r.feed(piece)));
        }
        let configs: BTreeMap<String, ConfigDigest> = CONFIG_KEYS
            .iter()
            .zip(self.runs)
            .map(|(key, run)| (key.to_string(), run.finish()))
            .collect();
        Ok(finish(
            Sha256Hex(hex(&self.sha.finalize())),
            self.total,
            self.prefix_seconds,
            configs,
        ))
    }
}

fn finish(
    sha: Sha256Hex,
    bytes: u64,
    prefix_seconds: Option<u32>,
    configs: BTreeMap<String, ConfigDigest>,
) -> FileDigest {
    let elementary_sha256 = elementary_sha256(&configs["default"]);
    FileDigest {
        schema_version: 1,
        sha256: sha,
        bytes,
        prefix_seconds,
        configs,
        elementary_sha256,
    }
}

/// Digests `bytes` (whose SHA-256 is `sha`) under every configuration. With
/// `prefix_seconds`, only the capture's first that many seconds are fed.
pub fn digest_bytes(
    sha: &Sha256Hex,
    bytes: &[u8],
    prefix_seconds: Option<u32>,
) -> Result<FileDigest, String> {
    let mut inc = Incremental::new(prefix_seconds);
    bytes.chunks(FEED_CHUNK).for_each(|piece| inc.feed(piece));
    let mut d = inc.finish()?;
    d.sha256 = sha.clone();
    Ok(d)
}

/// Fills `buf` from `f`; fewer bytes only at end of file.
fn fill(f: &mut std::fs::File, buf: &mut [u8]) -> Result<usize, String> {
    use std::io::Read;
    let mut filled = 0;
    while filled < buf.len() {
        let n = f
            .read(&mut buf[filled..])
            .map_err(|e| format!("read failed: {}", e.kind()))?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// The streaming twin of [`digest_bytes`]: reads the file chunk by chunk
/// into an [`Incremental`], never holding it in memory. A known `sha` must
/// match the file's SHA-256. Errors name no path.
pub fn digest_file(
    sha: Option<&Sha256Hex>,
    path: &std::path::Path,
    prefix_seconds: Option<u32>,
) -> Result<FileDigest, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("open failed: {}", e.kind()))?;
    let mut inc = Incremental::new(prefix_seconds);
    let mut buf = vec![0u8; FEED_CHUNK];
    loop {
        let n = fill(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        inc.feed(&buf[..n]);
    }
    let d = inc.finish()?;
    if let Some(expected) = sha
        && *expected != d.sha256
    {
        return Err("content changed since the inventory".to_string());
    }
    Ok(d)
}

/// SHA-256 over what a capture carries independent of how it is packetised.
/// Only PIDs that delivered a sample or a metadata payload count, ordered by
/// `(kind, payload hash)`, each contributing its kind, sample count,
/// metadata count and payload hash; then the KLV records hash. PID numbers,
/// timestamps and timing issues are left out, so a re-mux of the same
/// elementary streams onto other PIDs hashes the same.
pub fn elementary_sha256(default: &ConfigDigest) -> Sha256Hex {
    let mut carrying: Vec<&PidDigest> = default
        .pids
        .values()
        .filter(|p| p.samples > 0 || p.metadata > 0)
        .collect();
    carrying.sort_by(|a, b| {
        (&a.kind, &a.payload_sha256.0, a.samples, a.metadata).cmp(&(
            &b.kind,
            &b.payload_sha256.0,
            b.samples,
            b.metadata,
        ))
    });
    let mut h = Sha256::new();
    for pid in carrying {
        h.update(pid.kind.as_bytes());
        h.update([0]);
        h.update(pid.samples.to_le_bytes());
        h.update(pid.metadata.to_le_bytes());
        h.update(pid.payload_sha256.0.as_bytes());
    }
    h.update(default.klv_records_sha256.0.as_bytes());
    Sha256Hex(hex(&h.finalize()))
}

/// The file name a digest is stored under in a `digests/` directory:
/// `<sha256>.json` for a whole capture, `<sha256>.prefix<seconds>.json` for
/// the digest of its first `seconds`.
pub fn file_name(sha: &Sha256Hex, prefix_seconds: Option<u32>) -> String {
    match prefix_seconds {
        None => format!("{sha}.json"),
        Some(s) => format!("{sha}.prefix{s}.json"),
    }
}

fn digest_path(run: &RunDir, sha: &Sha256Hex) -> PathBuf {
    run.root.join("digests").join(file_name(sha, None))
}

pub fn load(run: &RunDir, sha: &Sha256Hex) -> Result<FileDigest, String> {
    let text = std::fs::read_to_string(digest_path(run, sha))
        .map_err(|e| format!("cannot read digest {sha}.json: {}", e.kind()))?;
    serde_json::from_str(&text).map_err(|e| format!("cannot parse digest {sha}.json: {e}"))
}

/// Writes `d` under [`file_name`] of its sha and prefix.
pub fn store(run: &RunDir, d: &FileDigest) -> Result<(), String> {
    let dir = run.root.join("digests");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create digests in the run directory: {}", e.kind()))?;
    let text = serde_json::to_string_pretty(d).map_err(|e| format!("digest json: {e}"))?;
    let name = file_name(&d.sha256, d.prefix_seconds);
    std::fs::write(dir.join(&name), text).map_err(|e| {
        format!(
            "cannot write digest {name} in the run directory: {}",
            e.kind()
        )
    })
}
