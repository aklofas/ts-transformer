//! A PCR-anchored paced sender over the bytes of one capture, with the
//! clock and the sleep injected so tests never wait.
//!
//! The pacing rule is the one `examples/sending/srt_serve_ts_file.rs`
//! uses: the first PCR is anchored to wall-clock zero, the sender sleeps
//! until a chunk's PCR offset has elapsed, and the bytes between two PCRs
//! go out at line rate in one write.
//!
//! A step between two PCRs is paced only up to [`MAX_PCR_GAP_S`]: a reset,
//! or a forward jump past it, is paced as no time at all, so a capture
//! with a broken PCR timeline is sent faster than its clock and never
//! waits longer than that between two chunks. [`PcrPacer::clamped_gaps`]
//! counts those steps.

use std::time::Duration;

use tst_test_helpers::rawts::classify_packet;

use super::pcr::{
    PCR_BASE_HZ, PCR_MAX_STEP_TICKS, PcrSpan, align_offset, parse_prefix, prefix_end, starts_packet,
};

const PKT: usize = 188;

/// The longest wait between two chunks: the largest PCR step counted as
/// elapsed time, the same bound the prefix cut and the inventory's
/// duration use, so all three agree on a capture's span.
pub const MAX_PCR_GAP_S: f64 = 1.0;

// The bound is the span rule's own; one second in 90 kHz ticks.
const _: () = assert!(PCR_MAX_STEP_TICKS == PCR_BASE_HZ);

/// The bytes from one PCR-bearing packet up to (excluding) the next one on
/// the pacing PID, and when they are due.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PacedChunk<'a> {
    /// Seconds after the first PCR (the PID's span, so a 33-bit wrap or a
    /// PCR discontinuity reads as in [`super::pcr`]).
    pub offset_s: f64,
    pub bytes: &'a [u8],
}

/// Chunks of one capture, cut at [`prefix_end`] when a prefix is asked for.
pub struct PcrPacer<'a> {
    bytes: &'a [u8],
    /// Packet-start offset and due time of each PCR on the pacing PID.
    marks: Vec<(usize, f64)>,
    /// The first confirmed packet start; bytes before it are never sent.
    start: usize,
    end: usize,
    clamped_gaps: u64,
}

impl<'a> PcrPacer<'a> {
    /// The pacing PID is the PMT's declared PCR PID, else the first PID
    /// seen carrying a PCR. Fails ("unpaceable") when fewer than two PCRs
    /// lie inside the (cut) stream.
    pub fn new(bytes: &'a [u8], prefix_seconds: Option<u32>) -> Result<Self, String> {
        let end = prefix_seconds.map_or(bytes.len(), |s| prefix_end(bytes, s));
        let start = align_offset(bytes);
        let declared = parse_prefix(bytes).pcr_pid;
        let mut pid = declared;
        let mut span: Option<PcrSpan> = None;
        let mut marks = Vec::new();
        let mut at = start;
        while at + PKT <= end {
            // Whole-slice check: more lenient near the end than the chunk-carry scan in `pcr`, which sees fewer bytes at a time.
            if !starts_packet(bytes, at) {
                at += 1;
                continue;
            }
            let pkt: &[u8; PKT] = bytes[at..at + PKT].try_into().expect("one packet");
            if let Ok(info) = classify_packet(pkt)
                && let Some(base) = info.pcr_base
                && *pid.get_or_insert(info.pid) == info.pid
            {
                let s = span.get_or_insert_with(|| PcrSpan::at(base));
                s.advance(base);
                marks.push((at, s.ticks as f64 / PCR_BASE_HZ as f64));
            }
            at += PKT;
        }
        if marks.len() < 2 {
            return Err(format!(
                "unpaceable: {} PCR(s) on the pacing PID, at least 2 are needed",
                marks.len()
            ));
        }
        Ok(Self {
            bytes,
            marks,
            start,
            end,
            clamped_gaps: span.map_or(0, |s| s.discontinuities),
        })
    }

    /// Chunks in order. The replay starts at the first confirmed full
    /// packet (leading partial-packet bytes are never sent); the bytes
    /// before the first PCR packet go out with the first chunk at offset 0,
    /// so the chunks concatenate to the stream from that packet (to the
    /// cut, with a prefix) exactly once.
    pub fn chunks(&self) -> impl Iterator<Item = PacedChunk<'_>> + '_ {
        (0..self.marks.len()).map(move |i| {
            let from = if i == 0 { self.start } else { self.marks[i].0 };
            let to = self.marks.get(i + 1).map_or(self.end, |m| m.0);
            PacedChunk {
                offset_s: self.marks[i].1,
                bytes: &self.bytes[from..to],
            }
        })
    }

    /// The due time of the last chunk.
    pub fn total_seconds(&self) -> f64 {
        self.marks.last().map_or(0.0, |m| m.1)
    }

    /// PCR steps paced as no time: resets and jumps past [`MAX_PCR_GAP_S`].
    pub fn clamped_gaps(&self) -> u64 {
        self.clamped_gaps
    }
}

/// Send every chunk through `write`, one call per chunk. The first chunk is
/// anchored to `clock()` at the start; before each chunk the sender sleeps
/// for whatever is left of its offset, never a negative time. A `write`
/// error is returned at once.
pub fn send_paced<F: FnMut(&[u8]) -> Result<(), String>>(
    pacer: &PcrPacer,
    clock: &mut dyn FnMut() -> Duration,
    sleep: &mut dyn FnMut(Duration),
    mut write: F,
) -> Result<(), String> {
    let t0 = clock();
    for c in pacer.chunks() {
        let due = t0 + Duration::from_secs_f64(c.offset_s);
        let now = clock();
        if due > now {
            sleep(due - now);
        }
        write(c.bytes)?;
    }
    Ok(())
}
