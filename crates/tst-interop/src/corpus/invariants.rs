//! Invariants every capture must meet without any baseline: the demuxer saw
//! no feed error, KLV payloads decode, the demuxer's discontinuity count
//! agrees with an independent walk of the wire, and PTS never jumps back.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};

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

/// The incremental form of [`walk`]. Feed it bytes in any chunking.
#[derive(Default)]
pub struct Walker {
    carry: Vec<u8>,
    state: HashMap<u16, PidCc>,
    pmt_pids: BTreeSet<u16>,
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
        while self.carry.len() - at >= 188 {
            if self.carry[at] != 0x47 {
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

    /// Records the programs of a PAT or the streams of a PMT carried whole
    /// in one packet.
    fn psi(&mut self, pid: u16, pkt: &[u8; 188], payload_off: usize) {
        if pkt[1] & 0x40 == 0 || payload_off >= 188 {
            return;
        }
        let start = payload_off + 1 + usize::from(pkt[payload_off]);
        if start + 3 > 188 {
            return;
        }
        let table = pkt[start];
        let len = (usize::from(pkt[start + 1] & 0x0F) << 8) | usize::from(pkt[start + 2]);
        let body = start + 3;
        let end = (body + len).min(188);
        if pid == 0 && table == 0 {
            // Programs run to the 4-byte CRC; the first 5 bytes are the header.
            let mut i = body + 5;
            while i + 4 <= end.saturating_sub(4) {
                let program = (u16::from(pkt[i]) << 8) | u16::from(pkt[i + 1]);
                let p = (u16::from(pkt[i + 2] & 0x1F) << 8) | u16::from(pkt[i + 3]);
                if program != 0 {
                    self.pmt_pids.insert(p);
                }
                i += 4;
            }
        } else if table == 2 && self.pmt_pids.contains(&pid) && body + 9 <= end {
            let info = (usize::from(pkt[body + 7] & 0x0F) << 8) | usize::from(pkt[body + 8]);
            let mut i = body + 9 + info;
            while i + 5 <= end.saturating_sub(4) {
                let es = (u16::from(pkt[i + 1] & 0x1F) << 8) | u16::from(pkt[i + 2]);
                let es_info = (usize::from(pkt[i + 3] & 0x0F) << 8) | usize::from(pkt[i + 4]);
                self.declared.insert(es);
                i += 5 + es_info;
            }
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
        let payload_off = if afc & 2 != 0 {
            5 + usize::from(p[4])
        } else {
            4
        };
        if afc & 1 != 0 && (pid == 0 || self.pmt_pids.contains(&pid)) {
            self.psi(pid, p, payload_off);
        }
        // Packets without payload neither advance nor judge the counter.
        if afc & 1 == 0 {
            return;
        }
        let cc = p[3] & 0x0F;
        let indicator = afc & 2 != 0 && p[4] >= 1 && p[5] & 0x80 != 0;
        let counted = self.declared.contains(&pid);
        let Some(prev) = self.state.get_mut(&pid) else {
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
            return;
        };
        if cc == prev.cc && !indicator && same_but_pcr(&prev.raw, p) {
            if prev.dup {
                // A third identical packet is a jump; the stored state stays.
                if counted {
                    self.out.cc_errors += 1;
                }
            } else {
                prev.dup = true;
            }
            return;
        }
        prev.dup = false;
        if cc != (prev.cc + 1) & 0x0F && !indicator && counted {
            self.out.cc_errors += 1;
        }
        if indicator && counted {
            self.out.cc_errors += 1;
        }
        prev.cc = cc;
        prev.raw = *p;
    }
}

/// Walks `bytes` packet by packet.
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
        if klv && d.klv_unknown > 0 && !ann.non_misb_private_pids.contains(pid) {
            fail(
                "klv-unknown",
                Some(*pid),
                d.klv_unknown_reason.clone().unwrap_or_default(),
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
