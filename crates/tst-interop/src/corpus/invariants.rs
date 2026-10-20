//! Invariants every capture must meet without any baseline: the demuxer saw
//! no feed error, KLV payloads decode, the demuxer's discontinuity count
//! agrees with an independent walk of the wire, and PTS never jumps back.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};
use tst_core::mpegts::demux::low_level::{parse_pat, parse_pmt};

use super::FEED_CHUNK;
use super::annotations::Annotations;
use super::digest::ConfigDigest;

/// One broken invariant. `rule` is `"feed-error"`, `"klv-unknown"`,
/// `"accounting"` or `"pts-order"`; it is a `String` so a report can be read
/// back from disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvariantFailure {
    pub rule: String,
    pub pid: Option<u16>,
    pub detail: String,
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
    streams: Vec<(u16, Vec<u8>)>,
}

/// The incremental form of [`walk`]. Feed it bytes in any chunking.
#[derive(Default)]
pub struct Walker {
    carry: Vec<u8>,
    state: HashMap<u16, PidCc>,
    assemblers: HashMap<u16, Assembler>,
    pat_version: Option<u8>,
    programs: BTreeMap<u16, Program>,
    declared: BTreeSet<u16>,
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

    pub fn feed(&mut self, bytes: &[u8]) {
        self.carry.extend_from_slice(bytes);
        let mut at = 0;
        // A byte is a packet start when 0x47 repeats at the next strides
        // that are already buffered.
        let locked = |c: &[u8], at: usize| {
            [at, at + 188, at + 376]
                .iter()
                .all(|&i| c.get(i).is_none_or(|&b| b == 0x47))
        };
        while self.carry.len() - at >= 188 {
            if !locked(&self.carry, at) {
                at += 1;
                continue;
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
        self.declared = self
            .programs
            .values()
            .flat_map(|p| p.streams.iter().map(|(pid, _)| *pid))
            .collect();
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
                let pids: Vec<u16> = p.streams.iter().map(|(pid, _)| *pid).collect();
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
        let mut streams: Vec<(u16, Vec<u8>)> = Vec::new();
        for s in &pmt.streams {
            let taken = self.programs.iter().any(|(p, other)| {
                *p != pmt_pid && other.streams.iter().any(|(e, _)| *e == s.elementary_pid)
            });
            if !taken {
                let mut sig = vec![s.stream_type];
                sig.extend(s.descriptors.iter().map(|d| d.tag));
                streams.push((s.elementary_pid, sig));
            }
        }
        // A PID the old version owned that is gone, or kept under another
        // kind, loses its continuity state.
        let mut reset = Vec::new();
        for (pid, sig) in &program.streams {
            match streams.iter().find(|(p, _)| p == pid) {
                None => reset.push(*pid),
                Some((_, new)) if new != sig => reset.push(*pid),
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
        if pid == 0 || self.programs.contains_key(&pid) {
            let payload = if payload_off < 188 {
                &p[payload_off..]
            } else {
                &[][..]
            };
            self.psi(pid, payload, p[1] & 0x40 != 0, jumped);
        }
    }
}

/// Walks `bytes` packet by packet, following the demuxer's own rules for
/// which PIDs count: PAT and PMT sections are reassembled across packets
/// and adopted only as the demuxer adopts them (checksum, current/next,
/// program number against the PAT, version change, a retired or re-typed
/// PID losing its counter state). The one thing the wire cannot show is the
/// demuxer's PES oversize discontinuity (4 MiB per PID by default); a
/// capture that trips it reports one more discontinuity than this walk.
/// Multi-section PATs are not followed.
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

/// The invariants broken by a capture whose `default`-configuration digest
/// and wire walk are given. `ann` excuses what a person has reviewed.
pub fn check(default: &ConfigDigest, walk: &WireWalk, ann: &Annotations) -> Vec<InvariantFailure> {
    let mut out = Vec::new();
    let mut fail = |rule: &str, pid: Option<u16>, detail: String| {
        out.push(InvariantFailure {
            rule: rule.to_string(),
            pid,
            detail,
        });
    };
    if default.feed_errors > 0 {
        fail(
            "feed-error",
            None,
            format!("{} feed errors", default.feed_errors),
        );
    }
    for (pid, d) in &default.pids {
        let klv = d.kind == "klv-sync" || d.kind == "klv-async";
        if !klv || ann.non_misb_private_pids.contains(pid) {
            continue;
        }
        if d.klv_unknown > 0 {
            fail(
                "klv-unknown",
                Some(*pid),
                d.klv_unknown_reason.clone().unwrap_or_default(),
            );
        } else if d.unknown_samples > 0 {
            fail(
                "klv-unknown",
                Some(*pid),
                format!("{} unknown sample(s)", d.unknown_samples),
            );
        }
    }
    if walk.packets == 0 {
        fail("accounting", None, "no packets".to_string());
    }
    if walk.cc_errors != default.stats.discontinuities {
        fail(
            "accounting",
            None,
            format!(
                "cc errors {} vs demuxer discontinuities {}",
                walk.cc_errors, default.stats.discontinuities
            ),
        );
    }
    if !ann.known_discontinuity {
        for (pid, d) in &default.pids {
            if d.pts_backward_breaks > 0 {
                fail(
                    "pts-order",
                    Some(*pid),
                    format!("{} backward PTS breaks", d.pts_backward_breaks),
                );
            }
        }
    }
    out
}
