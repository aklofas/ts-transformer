//! Invariants every capture must meet without any baseline: the demuxer saw
//! no feed error, KLV payloads decode, the demuxer's discontinuity count
//! agrees with an independent walk of the wire, PTS never jumps back, and
//! a PID the PMT declares as KLV and the wire carries PES on shows KLV
//! metadata in the digest.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};
use tst_core::mpegts::demux::low_level::{
    RawDescriptor, has_klva_registration, parse_pat, parse_pmt,
};
use tst_core::mpegts::descriptors::{find_descriptor_tag, find_format_identifier};
use tst_core::mpegts::sync::TsSync;

use super::FEED_CHUNK;
use super::annotations::Annotations;
use super::digest::ConfigDigest;

/// One broken invariant. `rule` is `"feed-error"`, `"klv-unknown"`,
/// `"klv-missing"`, `"accounting"` or `"pts-order"`; it is a `String` so a
/// report can be read back from disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvariantFailure {
    pub rule: String,
    pub pid: Option<u16>,
    pub detail: String,
    /// A reviewed annotation excuses it: it is recorded, and its detail
    /// starts with `excused:` and the annotation's reason, but it does not
    /// fail the capture.
    #[serde(default)]
    pub excused: bool,
}

/// What a packet-level walk of the bytes saw.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WireWalk {
    pub packets: u64,
    pub null_packets: u64,
    /// Discontinuities counted by the demuxer's own rule: on a PID a PMT has
    /// declared, a continuity jump without the discontinuity indicator, or
    /// a payload packet carrying the indicator.
    pub cc_errors: u64,
    /// Per PID, packets opening a PES (`00 00 01` after the unit start)
    /// while the PMT declares the PID a stream the demuxer classifies as
    /// KLV (see `is_klv_carrier`). A PID a PMT change retires keeps the
    /// count it reached.
    pub klv_pes_starts: BTreeMap<u16, u64>,
}

struct PidCc {
    cc: u8,
    dup: bool,
    raw: [u8; 188],
}

/// The demuxer's PSI section assembler: one section at a time per PID,
/// capped at 4096 bytes.
#[derive(Default)]
struct Assembler {
    buf: Vec<u8>,
    total: Option<usize>,
}

impl Assembler {
    fn append(&mut self, bytes: &[u8]) -> Option<Vec<u8>> {
        if self.buf.len() + bytes.len() > 4096 {
            self.reset();
            return None;
        }
        self.buf.extend_from_slice(bytes);
        if self.total.is_none() && self.buf.len() >= 3 {
            let total = 3 + ((usize::from(self.buf[1] & 0x0F) << 8) | usize::from(self.buf[2]));
            if total > 4096 {
                self.reset();
                return None;
            }
            self.total = Some(total);
        }
        let total = self.total?;
        if self.buf.len() < total {
            return None;
        }
        let rest = self.buf.split_off(total);
        self.total = None;
        Some(std::mem::replace(&mut self.buf, rest))
    }

    fn start(&mut self, bytes: &[u8]) -> Option<Vec<u8>> {
        self.reset();
        self.append(bytes)
    }

    fn cont(&mut self, bytes: &[u8]) -> Option<Vec<u8>> {
        if self.buf.is_empty() {
            return None;
        }
        self.append(bytes)
    }

    /// Another complete section already buffered, if any.
    fn next(&mut self) -> Option<Vec<u8>> {
        if self.buf.first() == Some(&0xFF) {
            self.reset();
            return None;
        }
        if self.buf.is_empty() {
            return None;
        }
        self.append(&[])
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.total = None;
    }
}

/// One program the PAT announced and what its PMT declared.
struct Program {
    number: u16,
    version: Option<u8>,
    /// Elementary PIDs with the kind signature the PMT gave them.
    streams: Vec<Stream>,
}

struct Stream {
    pid: u16,
    sig: Vec<u8>,
    /// The PMT declares it a KLV carrier, by [`is_klv_carrier`].
    klv: bool,
}

/// Whether the demuxer classifies a PMT stream as KLV (sync or async).
/// A copy of its cascade, kept here so the wire walk does not depend on
/// the demuxer it checks; a parity test pins the two together.
///
/// - 0x15 is KLV unless its `metadata_descriptor` positively names another
///   format (ID3 timed metadata is the common case).
/// - 0x06 is KLV only through a KLVA registration, and only when nothing
///   the demuxer ranks first claims it: an AV01 registration, a DVB
///   subtitling or teletext descriptor, or a VTTC or GA94 registration.
fn is_klv_carrier(stream_type: u8, descriptors: &[RawDescriptor]) -> bool {
    match stream_type {
        0x15 => !names_non_klv_metadata(descriptors),
        0x06 => {
            !find_format_identifier(descriptors, b"AV01")
                && ![0x59, 0x56, 0x46]
                    .iter()
                    .any(|t| find_descriptor_tag(descriptors, *t))
                && !find_format_identifier(descriptors, b"VTTC")
                && !find_format_identifier(descriptors, b"GA94")
                && has_klva_registration(descriptors)
        }
        _ => false,
    }
}

/// True when the first `metadata_descriptor` (tag 0x26) parses through its
/// fixed fields (H.222.0 Table 2-89) and names `metadata_format` 0x10 or
/// 0x11, or 0xFF with an identifier other than `KLVA`. A missing or
/// truncated descriptor names nothing, so the stream stays KLV.
fn names_non_klv_metadata(descriptors: &[RawDescriptor]) -> bool {
    descriptors
        .iter()
        .find(|d| d.tag == 0x26)
        .and_then(|d| metadata_format(&d.data))
        .is_some_and(|(format, id)| match format {
            0x10 | 0x11 => true,
            0xFF => id != Some(*b"KLVA"),
            _ => false,
        })
}

/// `metadata_format` and, for 0xFF, `metadata_format_identifier` of a
/// `metadata_descriptor` body; `None` when the body is shorter than the
/// fixed fields it declares.
fn metadata_format(body: &[u8]) -> Option<(u8, Option<[u8; 4]>)> {
    let app_format = u16::from_be_bytes([*body.first()?, *body.get(1)?]);
    let mut pos = if app_format == 0xFFFF { 6 } else { 2 };
    let format = *body.get(pos)?;
    pos += 1;
    let id = if format == 0xFF {
        let id: [u8; 4] = body.get(pos..pos + 4)?.try_into().ok()?;
        pos += 4;
        Some(id)
    } else {
        None
    };
    // metadata_service_id, then the flags byte.
    pos += 1;
    let flags = *body.get(pos)?;
    pos += 1;
    if flags & 0x10 != 0 {
        pos += 1 + usize::from(*body.get(pos)?);
    }
    match flags >> 5 {
        0b001 | 0b011 | 0b101 | 0b110 => pos += 1 + usize::from(*body.get(pos)?),
        0b100 => pos += 1,
        _ => {}
    }
    (body.len() >= pos).then_some((format, id))
}

/// The demuxer's sync re-acquisition rule: after a byte that is not where a
/// packet should start, a candidate sync byte needs 5 of the 7 strides from
/// it to carry the sync byte.
const REACQUIRE: TsSync = TsSync::n_of_m(5, 7);

/// The incremental form of [`walk`]. Feed it bytes in any chunking.
#[derive(Default)]
pub struct Walker {
    carry: Vec<u8>,
    /// Sync was lost (or never held): the next candidate must pass
    /// [`REACQUIRE`], as in the demuxer.
    resyncing: bool,
    state: HashMap<u16, PidCc>,
    assemblers: HashMap<u16, Assembler>,
    pat_version: Option<u8>,
    programs: BTreeMap<u16, Program>,
    declared: BTreeSet<u16>,
    klv: BTreeSet<u16>,
    out: WireWalk,
}

/// The demuxer's rule for a masked duplicate: identical, or identical but
/// for the PCR field when both packets carry one.
fn same_but_pcr(a: &[u8; 188], b: &[u8; 188]) -> bool {
    if a == b {
        return true;
    }
    let has_pcr = |p: &[u8; 188]| (p[3] >> 4) & 2 != 0 && p[4] >= 7 && p[5] & 0x10 != 0;
    has_pcr(a) && has_pcr(b) && a[..6] == b[..6] && a[12..] == b[12..]
}

impl Walker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquires packets the way the demuxer's `feed` does: a sync byte
    /// where a packet is due is taken as it is (at the very start too); a
    /// byte that is not one starts a hunt, and the first candidate after it
    /// must meet the 5-of-7 rule. A candidate the buffered bytes cannot
    /// settle waits for the next feed.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.carry.extend_from_slice(bytes);
        let mut at = 0;
        while self.carry.len() - at >= 188 {
            if self.carry[at] != 0x47 {
                self.resyncing = true;
                at += 1;
                continue;
            }
            if self.resyncing {
                match REACQUIRE.verify(&self.carry, at) {
                    Some(true) => self.resyncing = false,
                    Some(false) => {
                        at += 1;
                        continue;
                    }
                    None => break,
                }
            }
            let mut pkt = [0u8; 188];
            pkt.copy_from_slice(&self.carry[at..at + 188]);
            self.packet(&pkt);
            at += 188;
        }
        self.carry.drain(..at);
    }

    pub fn finish(self) -> WireWalk {
        self.out
    }

    fn rebuild_declared(&mut self) {
        let streams = || self.programs.values().flat_map(|p| p.streams.iter());
        self.declared = streams().map(|s| s.pid).collect();
        self.klv = streams().filter(|s| s.klv).map(|s| s.pid).collect();
    }

    /// Forgets the continuity state of PIDs a topology change retired.
    fn retire(&mut self, pids: &[u16]) {
        for pid in pids {
            self.state.remove(pid);
        }
    }

    fn pat(&mut self, section: &[u8]) {
        let Ok(pat) = parse_pat(section) else { return };
        if Some(pat.version) == self.pat_version {
            return;
        }
        self.pat_version = Some(pat.version);
        let announced: BTreeMap<u16, u16> = pat
            .programs
            .iter()
            .filter(|e| e.program_number != 0)
            .map(|e| (e.pid, e.program_number))
            .collect();
        let gone: Vec<u16> = self
            .programs
            .iter()
            .filter(|(pid, p)| announced.get(pid) != Some(&p.number))
            .map(|(pid, _)| *pid)
            .collect();
        for pmt_pid in gone {
            if let Some(p) = self.programs.remove(&pmt_pid) {
                let pids: Vec<u16> = p.streams.iter().map(|s| s.pid).collect();
                self.retire(&pids);
            }
            self.assemblers.remove(&pmt_pid);
            self.state.remove(&pmt_pid);
        }
        for (pid, number) in announced {
            self.programs.entry(pid).or_insert(Program {
                number,
                version: None,
                streams: Vec::new(),
            });
            self.assemblers.entry(pid).or_default();
        }
        self.rebuild_declared();
    }

    fn pmt(&mut self, pmt_pid: u16, section: &[u8]) {
        let Ok(pmt) = parse_pmt(section) else { return };
        let Some(program) = self.programs.get(&pmt_pid) else {
            return;
        };
        if pmt.program_number != program.number || Some(pmt.version) == program.version {
            return;
        }
        // First program to declare a PID owns it.
        let mut streams: Vec<Stream> = Vec::new();
        for s in &pmt.streams {
            let taken = self.programs.iter().any(|(p, other)| {
                *p != pmt_pid && other.streams.iter().any(|e| e.pid == s.elementary_pid)
            });
            if !taken {
                let mut sig = vec![s.stream_type];
                sig.extend(s.descriptors.iter().map(|d| d.tag));
                let klv = is_klv_carrier(s.stream_type, &s.descriptors);
                streams.push(Stream {
                    pid: s.elementary_pid,
                    sig,
                    klv,
                });
            }
        }
        // A PID the old version owned that is gone, or kept under another
        // kind, loses its continuity state.
        let mut reset = Vec::new();
        for old in &program.streams {
            match streams.iter().find(|s| s.pid == old.pid) {
                None => reset.push(old.pid),
                Some(new) if new.sig != old.sig => reset.push(old.pid),
                Some(_) => {}
            }
        }
        let program = self.programs.get_mut(&pmt_pid).expect("checked above");
        program.version = Some(pmt.version);
        program.streams = streams;
        self.retire(&reset);
        self.rebuild_declared();
    }

    fn section(&mut self, pid: u16, section: &[u8]) {
        if pid == 0 {
            self.pat(section);
        } else {
            self.pmt(pid, section);
        }
    }

    /// Routes one PSI packet the way the demuxer's section assembler does.
    fn psi(&mut self, pid: u16, payload: &[u8], pusi: bool, jumped: bool) {
        let asm = self.assemblers.entry(pid).or_default();
        if !pusi && jumped {
            asm.reset();
            return;
        }
        let mut done = Vec::new();
        if pusi {
            let Some(&pointer) = payload.first() else {
                return;
            };
            let pointer = usize::from(pointer);
            if 1 + pointer > payload.len() {
                return;
            }
            if pointer > 0 {
                done.extend(asm.cont(&payload[1..1 + pointer]));
            }
            done.extend(asm.start(&payload[1 + pointer..]));
        } else {
            done.extend(asm.cont(payload));
        }
        while let Some(more) = self.assemblers.get_mut(&pid).and_then(Assembler::next) {
            done.push(more);
        }
        for s in done {
            self.section(pid, &s);
        }
    }

    fn packet(&mut self, p: &[u8; 188]) {
        self.out.packets += 1;
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        if pid == 0x1FFF {
            self.out.null_packets += 1;
            return;
        }
        // Dropped before the demuxer looks at continuity: transport error,
        // scrambled, or an adaptation field that overruns the packet.
        let afc = (p[3] >> 4) & 3;
        if p[1] & 0x80 != 0 || p[3] >> 6 != 0 || (afc & 2 != 0 && 5 + usize::from(p[4]) > 188) {
            return;
        }
        // Packets without payload neither advance nor judge the counter.
        if afc & 1 == 0 {
            return;
        }
        let payload_off = if afc & 2 != 0 {
            5 + usize::from(p[4])
        } else {
            4
        };
        let cc = p[3] & 0x0F;
        let indicator = afc & 2 != 0 && p[4] >= 1 && p[5] & 0x80 != 0;
        let counted = self.declared.contains(&pid);
        let mut jumped = false;
        match self.state.get_mut(&pid) {
            None => {
                self.state.insert(
                    pid,
                    PidCc {
                        cc,
                        dup: false,
                        raw: *p,
                    },
                );
                if indicator && counted {
                    self.out.cc_errors += 1;
                }
            }
            Some(prev) => {
                if cc == prev.cc && !indicator && same_but_pcr(&prev.raw, p) {
                    if prev.dup {
                        // A third identical packet is a jump; the stored
                        // state stays.
                        jumped = true;
                        if counted {
                            self.out.cc_errors += 1;
                        }
                    } else {
                        // A tolerated duplicate is not routed anywhere.
                        prev.dup = true;
                        return;
                    }
                } else {
                    prev.dup = false;
                    if cc != (prev.cc + 1) & 0x0F && !indicator {
                        jumped = true;
                        if counted {
                            self.out.cc_errors += 1;
                        }
                    }
                    if indicator && counted {
                        self.out.cc_errors += 1;
                    }
                    prev.cc = cc;
                    prev.raw = *p;
                }
            }
        }
        let payload = if payload_off < 188 {
            &p[payload_off..]
        } else {
            &[][..]
        };
        let pusi = p[1] & 0x40 != 0;
        // Counted after the duplicate check, so a tolerated duplicate of a
        // PES start counts once.
        if pusi && self.klv.contains(&pid) && payload.starts_with(&[0, 0, 1]) {
            *self.out.klv_pes_starts.entry(pid).or_default() += 1;
        }
        if pid == 0 || self.programs.contains_key(&pid) {
            self.psi(pid, payload, pusi, jumped);
        }
    }
}

/// Walks `bytes` packet by packet, following the demuxer's own rules for
/// which PIDs count: PAT and PMT sections are reassembled across packets
/// and adopted only as the demuxer adopts them (checksum, current/next,
/// program number against the PAT, version change, a retired or re-typed
/// PID losing its counter state), and acquiring packets by the demuxer's
/// sync rule. The one thing the wire cannot show is the demuxer's PES
/// oversize discontinuity (4 MiB per PID by default); the digest counts it
/// apart (`pes_oversize`) and [`check`] leaves it out. Multi-section PATs
/// are not followed.
pub fn walk(bytes: &[u8]) -> WireWalk {
    let mut w = Walker::new();
    w.feed(bytes);
    w.finish()
}

/// The streaming twin of [`walk`]. Errors name no path.
pub fn walk_file(path: &Path) -> Result<WireWalk, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("open failed: {}", e.kind()))?;
    let mut w = Walker::new();
    let mut buf = vec![0u8; FEED_CHUNK];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("read failed: {}", e.kind()))?;
        if n == 0 {
            return Ok(w.finish());
        }
        w.feed(&buf[..n]);
    }
}

fn push<'a>(
    out: &'a mut Vec<InvariantFailure>,
    rule: &str,
    pid: Option<u16>,
    detail: String,
) -> &'a mut InvariantFailure {
    out.push(InvariantFailure {
        rule: rule.to_string(),
        pid,
        detail,
        excused: false,
    });
    out.last_mut().expect("just pushed")
}

/// One rule's observation weighed against an annotation: excused with the
/// reviewed bound when `waived` holds it and `count` is within it, failed
/// naming the bound when `count` is over it, failed plainly otherwise.
struct Observed<'a> {
    rule: &'a str,
    pid: Option<u16>,
    count: u64,
    /// What `count` counts, as it reads after the number.
    what: String,
}

impl Observed<'_> {
    fn record(self, out: &mut Vec<InvariantFailure>, ann: &Annotations, bound: Option<u64>) {
        let Observed {
            rule,
            pid,
            count,
            what,
        } = self;
        match bound {
            Some(max) if count <= max => {
                let why = ann.reason.as_deref().unwrap_or("");
                push(
                    out,
                    rule,
                    pid,
                    format!("excused: {why}; {count} {what} (bound {max})"),
                )
                .excused = true;
            }
            Some(max) => {
                push(
                    out,
                    rule,
                    pid,
                    format!("{count} {what} exceeds the reviewed bound {max}"),
                );
            }
            None => {
                push(out, rule, pid, format!("{count} {what}"));
            }
        }
    }

    /// Fails on a PID the annotation does not list.
    fn unlisted(self, out: &mut Vec<InvariantFailure>, pid: u16) {
        push(
            out,
            self.rule,
            self.pid,
            format!(
                "{} {} on pid {pid}, which the annotation does not list",
                self.count, self.what
            ),
        );
    }
}

/// The invariants broken by a capture whose `default`-configuration digest
/// and wire walk are given. `ann` excuses what a person has reviewed, up to
/// the count reviewed and only where it was seen; every waived observation
/// is still recorded, as an excused row.
pub fn check(default: &ConfigDigest, walk: &WireWalk, ann: &Annotations) -> Vec<InvariantFailure> {
    let mut out = Vec::new();
    if default.feed_errors > 0 {
        Observed {
            rule: "feed-error",
            pid: None,
            count: default.feed_errors,
            what: "feed errors".to_string(),
        }
        .record(
            &mut out,
            ann,
            ann.max_feed_errors.filter(|_| ann.known_feed_errors),
        );
    }
    for (pid, d) in &default.pids {
        let klv = d.kind == "klv-sync" || d.kind == "klv-async";
        if !klv {
            continue;
        }
        if ann.non_misb_private_pids.contains(pid) {
            let count = d.klv_unknown + d.unknown_samples;
            if count > 0 {
                Observed {
                    rule: "klv-unknown",
                    pid: Some(*pid),
                    count,
                    what: "private record(s)".to_string(),
                }
                .record(&mut out, ann, ann.max_private_records);
            }
            continue;
        }
        if d.klv_unknown > 0 {
            let truncated = d.klv_truncated.unwrap_or(0);
            if ann.known_klv_truncation && d.klv_unknown == truncated {
                let seen = Observed {
                    rule: "klv-unknown",
                    pid: Some(*pid),
                    count: truncated,
                    what: "truncated record(s)".to_string(),
                };
                if ann.klv_truncation_pids.contains(pid) {
                    seen.record(&mut out, ann, ann.max_klv_truncated);
                } else {
                    seen.unlisted(&mut out, *pid);
                }
            } else {
                push(
                    &mut out,
                    "klv-unknown",
                    Some(*pid),
                    d.klv_unknown_reason.clone().unwrap_or_default(),
                );
            }
        }
        // Label-less payloads are judged apart from undecoded records: an
        // excused truncation on the PID says nothing about them, and no key
        // excuses them on a KLV PID.
        if d.unknown_samples > 0 {
            push(
                &mut out,
                "klv-unknown",
                Some(*pid),
                format!("{} unknown sample(s)", d.unknown_samples),
            );
        }
    }
    // KLV the wire carries must reach the digest as KLV: a demuxer that
    // drops it, or reclassifies the PID, fails here.
    for (pid, &pes) in &walk.klv_pes_starts {
        let state = match default.pids.get(pid) {
            None => "absent".to_string(),
            Some(d) if d.kind != "klv-sync" && d.kind != "klv-async" => {
                format!("kind {}", d.kind)
            }
            Some(d) if d.metadata == 0 => "0 metadata events".to_string(),
            Some(_) => continue,
        };
        let seen = Observed {
            rule: "klv-missing",
            pid: Some(*pid),
            count: pes,
            what: format!("KLV PES on the wire, digest {state}"),
        };
        let bound = ann
            .max_private_records
            .filter(|_| ann.non_misb_private_pids.contains(pid));
        seen.record(&mut out, ann, bound);
    }
    if walk.packets == 0 {
        push(&mut out, "accounting", None, "no packets".to_string());
    }
    // A PES oversize is a demuxer discontinuity the wire cannot show.
    let oversize: u64 = default.pids.values().map(|d| d.pes_oversize).sum();
    let demuxed = default.stats.discontinuities.saturating_sub(oversize);
    if walk.cc_errors != demuxed {
        let detail = format!(
            "cc errors {} vs demuxer discontinuities {demuxed} ({oversize} PES oversize left out)",
            walk.cc_errors
        );
        if ann.known_cc_mismatch {
            Observed {
                rule: "accounting",
                pid: None,
                count: walk.cc_errors.abs_diff(demuxed),
                what: format!(
                    "continuity difference(s), cc errors {} vs demuxer discontinuities {demuxed} with {oversize} PES oversize left out",
                    walk.cc_errors
                ),
            }
            .record(&mut out, ann, ann.max_cc_mismatch);
        } else {
            push(&mut out, "accounting", None, detail);
        }
    }
    let breaks = |pid: &u16, d: &super::digest::PidDigest| Observed {
        rule: "pts-order",
        pid: Some(*pid),
        count: d.pts_backward_breaks,
        what: "backward PTS breaks".to_string(),
    };
    let broken = default
        .pids
        .iter()
        .filter(|(_, d)| d.pts_backward_breaks > 0);
    if ann.known_discontinuity {
        // The bound is a total over the listed PIDs: a restart breaks every
        // stream once, and a person reviews the capture, not each PID.
        let total: u64 = broken
            .clone()
            .filter(|(pid, _)| ann.discontinuity_pids.contains(pid))
            .map(|(_, d)| d.pts_backward_breaks)
            .sum();
        let within = ann.max_pts_backward_breaks.filter(|max| total <= *max);
        for (pid, d) in broken {
            if !ann.discontinuity_pids.contains(pid) {
                breaks(pid, d).unlisted(&mut out, *pid);
            } else if within.is_some() {
                breaks(pid, d).record(&mut out, ann, within);
            } else {
                let max = ann
                    .max_pts_backward_breaks
                    .map_or("none".to_string(), |m| m.to_string());
                push(
                    &mut out,
                    "pts-order",
                    Some(*pid),
                    format!(
                        "{} backward PTS breaks ({total} over the listed PIDs) exceeds the reviewed bound {max}",
                        d.pts_backward_breaks
                    ),
                );
            }
        }
    } else {
        for (pid, d) in broken {
            breaks(pid, d).record(&mut out, ann, None);
        }
    }
    out
}
