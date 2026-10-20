//! A PCR-anchored paced sender over the bytes of one capture, with the
//! clock and the sleep injected so tests never wait.
//!
//! The pacing rule is the one `examples/sending/srt_serve_ts_file.rs`
//! uses: the first PCR is anchored to wall-clock zero, the sender sleeps
//! until a chunk's PCR offset has elapsed, and the bytes between two PCRs
//! go out at line rate in one write.

use std::time::Duration;

use tst_test_helpers::rawts::classify_packet;

use super::pcr::{PCR_BASE_HZ, PcrSpan, align_offset, parse_prefix, prefix_end, starts_packet};

const PKT: usize = 188;

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
    end: usize,
}

impl<'a> PcrPacer<'a> {
    /// The pacing PID is the PMT's declared PCR PID, else the first PID
    /// seen carrying a PCR. Fails ("unpaceable") when fewer than two PCRs
    /// lie inside the (cut) stream.
    pub fn new(bytes: &'a [u8], prefix_seconds: Option<u32>) -> Result<Self, String> {
        let end = prefix_seconds.map_or(bytes.len(), |s| prefix_end(bytes, s));
        let start = align_offset(bytes);
        let declared = parse_prefix(bytes).ok().and_then(|p| p.pcr_pid);
        let mut pid = declared;
        let mut span: Option<PcrSpan> = None;
        let mut marks = Vec::new();
        let mut at = start;
        while at + PKT <= end {
            if !starts_packet(bytes, at) {
                at += 1;
                continue;
            }
            let pkt: &[u8; PKT] = bytes[at..at + PKT].try_into().expect("one packet");
            if let Ok(info) = classify_packet(pkt)
                && let Some(base) = info.pcr_base
                && *pid.get_or_insert(info.pid) == info.pid
            {
                let s = span.get_or_insert(PcrSpan {
                    last: base,
                    ticks: 0,
                });
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
        Ok(Self { bytes, marks, end })
    }

    /// Chunks in order. The bytes before the first PCR packet go out with
    /// the first chunk at offset 0, so the chunks concatenate to the
    /// stream (to the cut, with a prefix) exactly once.
    pub fn chunks(&self) -> impl Iterator<Item = PacedChunk<'_>> + '_ {
        (0..self.marks.len()).map(move |i| {
            let from = if i == 0 { 0 } else { self.marks[i].0 };
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
