//! PCR reading shared by the inventory and the digest: per-PID PCR spans
//! over a capture that may not start on a packet boundary, and the byte
//! offset where a prefix of a given length ends.

use std::collections::BTreeMap;

use tst_core::mpegts::demux::{DemuxEvent, Demuxer, DemuxerConfig, ProgramMap};
use tst_test_helpers::rawts::{ParsedStream, PmtStream, classify_packet};

use super::FEED_CHUNK;

const PKT: usize = 188;
const SYNC: u8 = 0x47;
/// Bytes read from the start of a capture for the PMT shape. A PMT repeats
/// within a fraction of a second, so the prefix always holds one.
pub const PMT_PREFIX: usize = 4 * 1024 * 1024;
/// A 33-bit PCR base wraps after this many 90 kHz ticks.
pub const PCR_BASE_WRAP: u64 = 1 << 33;
/// PCR bases count at this rate.
pub const PCR_BASE_HZ: u64 = 90_000;

/// Whether a packet starts at `k`: a sync byte, confirmed by sync bytes at
/// +188 and +376 (the demuxer's own rule needs several strides). With fewer
/// than three whole packets left in `buf`, the strides that exist must hold.
pub(crate) fn starts_packet(buf: &[u8], k: usize) -> bool {
    if buf.get(k) != Some(&SYNC) {
        return false;
    }
    [k + PKT, k + 2 * PKT]
        .iter()
        .all(|&next| next >= buf.len() || buf[next] == SYNC)
}

/// Offset of the first byte that starts a packet (see `starts_packet`). 0
/// for a capture that starts on a packet; the length of `bytes` when none is
/// found.
pub fn align_offset(bytes: &[u8]) -> usize {
    (0..)
        .take_while(|k| k + PKT <= bytes.len())
        .find(|&k| starts_packet(bytes, k))
        .unwrap_or(bytes.len())
}

/// The PMT facts of a capture, read by the demuxer from its first
/// [`PMT_PREFIX`] bytes after skipping any bytes before the first packet:
/// the first program map the demuxer emits gives the PMT PID, the PCR PID
/// and the streams. The demuxer reassembles a PMT that spans packets and
/// resyncs past damaged bytes, so neither keeps a capture from being read.
/// `buf` may be longer. No PMT in the window leaves `pmt_pid` unset.
pub fn parse_prefix(buf: &[u8]) -> ParsedStream {
    let buf = &buf[align_offset(buf)..];
    let buf = &buf[..buf.len().min(PMT_PREFIX)];
    let mut demuxer = Demuxer::with_config(DemuxerConfig::default());
    for piece in buf.chunks(FEED_CHUNK) {
        // As in the digest: a rejection keeps the rest of the input
        // buffered, and empty feeds resume it. Each consumes at least one
        // byte, so the bound is never what ends the loop on real input.
        let mut input = piece;
        let mut rejections = 0;
        while demuxer.feed(input).is_err() && rejections <= piece.len() {
            rejections += 1;
            input = &[];
        }
        while let Some(ev) = demuxer.next_event() {
            if let DemuxEvent::ProgramMap(pm) = ev {
                return program_facts(&pm);
            }
        }
    }
    ParsedStream::default()
}

/// A program map as the PMT facts the inventory and the pacer read.
fn program_facts(pm: &ProgramMap) -> ParsedStream {
    ParsedStream {
        pmt_pid: Some(pm.pmt_pid),
        pcr_pid: Some(pm.pcr_pid),
        streams: pm
            .streams
            .iter()
            .map(|s| PmtStream {
                pid: s.pid,
                stream_type: s.stream_type.as_byte(),
                // The last registration descriptor names the format.
                klva: s
                    .raw_descriptors
                    .iter()
                    .rfind(|d| d.tag == 0x05 && d.data.len() >= 4)
                    .is_some_and(|d| d.data[..4] == *b"KLVA"),
            })
            .collect(),
        ..ParsedStream::default()
    }
}

/// The largest forward step between two PCRs on one PID that counts as
/// elapsed time: one second, the demuxer's own PCR anomaly bound.
pub const PCR_MAX_STEP_TICKS: u64 = PCR_BASE_HZ;

/// How far one PID's clock has advanced, in 90 kHz ticks.
///
/// A backwards step of at least 2^32 base ticks is a 33-bit wrap and the
/// span keeps counting through it. A smaller backwards step, or a forward
/// step (wrap included) of more than [`PCR_MAX_STEP_TICKS`], is a PCR
/// discontinuity: it adds nothing, it is counted, and the span continues
/// from the new value. The inventory's duration, the prefix cut and the
/// pacer all read it this way, so a capture with a PCR reset or a jump
/// reports the time of its continuous parts, and the pacer never sleeps
/// across a jump.
#[derive(Default)]
pub(crate) struct PcrSpan {
    pub(crate) last: u64,
    pub(crate) ticks: u64,
    pub(crate) discontinuities: u64,
}

impl PcrSpan {
    /// A span starting at `base`.
    pub(crate) fn at(base: u64) -> Self {
        Self {
            last: base,
            ..Self::default()
        }
    }

    pub(crate) fn advance(&mut self, base: u64) {
        let step = if base >= self.last {
            Some(base - self.last)
        } else if self.last - base >= PCR_BASE_WRAP / 2 {
            Some(base + PCR_BASE_WRAP - self.last)
        } else {
            None
        };
        match step {
            Some(step) if step <= PCR_MAX_STEP_TICKS => self.ticks += step,
            _ => self.discontinuities += 1,
        }
        self.last = base;
    }
}

/// Where a prefix ends: the PCR PID (`None` = the first PID seen carrying a
/// PCR) and the span, in 90 kHz ticks, that the prefix may cover.
struct Cut {
    pid: Option<u16>,
    limit_ticks: u64,
}

/// Per-PID PCR spans read packet by packet, carrying a partial packet
/// across chunks and hunting past bytes that are not a packet. PIDs are
/// never combined. A packet whose header or adaptation field is malformed
/// is skipped, not an error: the demuxer judges it.
#[derive(Default)]
pub struct PcrScan {
    carry: Vec<u8>,
    /// Absolute offset of `carry[0]` in everything fed so far.
    carry_at: u64,
    spans: BTreeMap<u16, PcrSpan>,
    first_pid: Option<u16>,
    cut: Option<Cut>,
    cut_at: Option<u64>,
}

impl PcrScan {
    /// A scan that also records the offset of the first packet whose PCR on
    /// `pid` lies more than `limit_ticks` after that PID's first PCR.
    pub fn with_cut(pid: Option<u16>, limit_ticks: u64) -> Self {
        Self {
            cut: Some(Cut { pid, limit_ticks }),
            ..Self::default()
        }
    }

    /// Absolute offset of the packet that crossed the cut, once seen.
    pub fn cut_at(&self) -> Option<u64> {
        self.cut_at
    }

    /// Absolute offset before which every byte fed has been read: whole
    /// packets and the bytes skipped between them. The cut can never lie
    /// before it.
    pub fn settled(&self) -> u64 {
        self.carry_at
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.carry.extend_from_slice(chunk);
        let mut off = 0;
        while off + PKT <= self.carry.len() {
            if !starts_packet(&self.carry, off) {
                off += 1;
                continue;
            }
            let pkt: &[u8; PKT] = self.carry[off..off + PKT]
                .try_into()
                .expect("chunk is one packet");
            let at = self.carry_at + off as u64;
            off += PKT;
            let Ok(info) = classify_packet(pkt) else {
                continue;
            };
            let Some(base) = info.pcr_base else { continue };
            self.first_pid.get_or_insert(info.pid);
            let span = self
                .spans
                .entry(info.pid)
                .or_insert_with(|| PcrSpan::at(base));
            span.advance(base);
            if self.cut_at.is_none()
                && let Some(cut) = &self.cut
                && cut.pid.or(self.first_pid) == Some(info.pid)
                && span.ticks > cut.limit_ticks
            {
                self.cut_at = Some(at);
            }
        }
        self.carry.drain(..off);
        self.carry_at += off as u64;
    }

    /// Seconds spanned by the declared PCR PID, else the first PID seen
    /// carrying a PCR; 0.0 with fewer than two PCRs.
    pub fn duration_s(&self, declared: Option<u16>) -> f64 {
        self.span(declared)
            .map_or(0.0, |s| s.ticks as f64 / PCR_BASE_HZ as f64)
    }

    /// PCR discontinuities (see `PcrSpan`) on the same PID as
    /// [`Self::duration_s`].
    pub fn discontinuities(&self, declared: Option<u16>) -> u64 {
        self.span(declared).map_or(0, |s| s.discontinuities)
    }

    fn span(&self, declared: Option<u16>) -> Option<&PcrSpan> {
        declared
            .and_then(|pid| self.spans.get(&pid))
            .or_else(|| self.first_pid.and_then(|pid| self.spans.get(&pid)))
    }
}

/// Absolute offset in `bytes` where a prefix of `seconds` ends: the start of
/// the first packet whose PCR, on the PMT's PCR PID (else the first PID seen
/// carrying a PCR), takes that PID's span past `seconds` x 90 000 ticks
/// (strictly greater; see `PcrSpan` for wraps and discontinuities).
/// `bytes[..offset]` excludes that packet. `bytes.len()` when the capture
/// is shorter. The digest and the replay pacer cut at this same point.
pub fn prefix_end(bytes: &[u8], seconds: u32) -> usize {
    let mut r = bytes;
    prefix_end_read(&mut r, seconds).expect("reading a slice cannot fail") as usize
}

/// [`prefix_end`] over a reader: the same cut, found without holding the
/// capture in memory. Returns the byte count read when the capture is
/// shorter than `seconds`. It drives the [`PrefixCutter`] the digest's
/// incremental form uses, over [`FEED_CHUNK`] blocks from offset 0, so the
/// sender's cut and the sweep's cut come from one scanner.
pub fn prefix_end_read<R: std::io::Read>(r: &mut R, seconds: u32) -> std::io::Result<u64> {
    let mut cutter = PrefixCutter::new(seconds);
    let mut released = 0u64;
    let mut buf = vec![0u8; FEED_CHUNK];
    while !cutter.is_done() {
        let mut filled = 0;
        while filled < buf.len() {
            let k = r.read(&mut buf[filled..])?;
            if k == 0 {
                break;
            }
            filled += k;
        }
        if filled == 0 {
            break;
        }
        cutter.push(&buf[..filled], &mut |p| released += p.len() as u64);
    }
    if !cutter.is_done() {
        cutter.finish(&mut |p| released += p.len() as u64);
    }
    Ok(released)
}

/// The streaming form of [`prefix_end_read`]: bytes go in as they arrive and
/// the bytes known to lie inside the prefix come out, in order, through a
/// callback. The first `PMT_PREFIX + 188` bytes are held back to read the
/// PCR PID; after that only the part of a block the scan has not settled
/// yet (a partial packet) is held. Scanning runs in [`FEED_CHUNK`] blocks,
/// so the cut does not depend on how small the arriving pieces are. The cut
/// lands on the same packet as [`prefix_end`] over the whole input.
pub struct PrefixCutter {
    seconds: u32,
    state: CutState,
}

enum CutState {
    /// Collecting the head the PCR PID is read from.
    Head(Vec<u8>),
    Scan {
        scan: PcrScan,
        /// Bytes received but not yet scanned.
        inbox: Vec<u8>,
        /// Scanned bytes not yet released; the first is at offset `fed`.
        held: Vec<u8>,
        fed: u64,
    },
    Done,
}

impl PrefixCutter {
    pub fn new(seconds: u32) -> Self {
        Self {
            seconds,
            state: CutState::Head(Vec::new()),
        }
    }

    /// Whether the cut has been found; nothing more is released.
    pub fn is_done(&self) -> bool {
        matches!(self.state, CutState::Done)
    }

    pub fn push(&mut self, chunk: &[u8], out: &mut dyn FnMut(&[u8])) {
        match &mut self.state {
            CutState::Done => {}
            CutState::Head(buf) => {
                buf.extend_from_slice(chunk);
                if buf.len() >= PMT_PREFIX + PKT {
                    self.start(out);
                }
            }
            CutState::Scan { inbox, .. } => {
                inbox.extend_from_slice(chunk);
                self.scan_blocks(false, out);
            }
        }
    }

    /// Releases everything still held; call once, after the last `push`.
    pub fn finish(&mut self, out: &mut dyn FnMut(&[u8])) {
        if matches!(self.state, CutState::Head(_)) {
            self.start(out);
        }
        self.scan_blocks(true, out);
        if let CutState::Scan { held, .. } = &mut self.state {
            out(held);
            held.clear();
        }
    }

    /// Reads the PCR PID from the head and moves to scanning.
    fn start(&mut self, out: &mut dyn FnMut(&[u8])) {
        let CutState::Head(head) = std::mem::replace(&mut self.state, CutState::Done) else {
            return;
        };
        let pid_window = &head[..head.len().min(PMT_PREFIX + PKT)];
        let pcr_pid = parse_prefix(&pid_window[align_offset(pid_window)..]).pcr_pid;
        self.state = CutState::Scan {
            scan: PcrScan::with_cut(pcr_pid, u64::from(self.seconds) * PCR_BASE_HZ),
            inbox: head,
            held: Vec::new(),
            fed: 0,
        };
        self.scan_blocks(false, out);
    }

    /// Scans whole blocks of the inbox (all of it when `flush`), releasing
    /// the bytes the scan has settled, or everything before the cut.
    fn scan_blocks(&mut self, flush: bool, out: &mut dyn FnMut(&[u8])) {
        let CutState::Scan {
            scan,
            inbox,
            held,
            fed,
        } = &mut self.state
        else {
            return;
        };
        while inbox.len() >= FEED_CHUNK || (flush && !inbox.is_empty()) {
            let block: Vec<u8> = inbox.drain(..inbox.len().min(FEED_CHUNK)).collect();
            scan.feed(&block);
            held.extend_from_slice(&block);
            let upto = scan.cut_at().unwrap_or_else(|| scan.settled());
            let n = (upto - *fed) as usize;
            out(&held[..n]);
            held.drain(..n);
            *fed = upto;
            if scan.cut_at().is_some() {
                self.state = CutState::Done;
                return;
            }
        }
    }
}
