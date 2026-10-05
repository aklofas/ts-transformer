// crates/tst-core/src/mpegts/demux/psi_assembler.rs
//! PSI section reassembly across TS packet boundaries.
//!
//! Mirrors ffmpeg's `MpegTSSectionFilter` in `libavformat/mpegts.c` — per-PID
//! buffer with a 4 KiB cap (`MAX_SECTION_SIZE`), reset on PUSI, overflow
//! surfaced as a non-conformance issue rather than allowed to OOM.
//!
//! ## Why this exists
//!
//! Before this module was extracted, the demuxer kept a `HashMap<PID, Vec<u8>>`
//! and unconditionally `extend_from_slice`'d continuation payloads. A malicious
//! PMT PUSI claiming `section_length=0xFFF` that never closed could grow the
//! buffer unboundedly. ffmpeg caps via `MAX_SECTION_SIZE=4096`; tsduck at 1024
//! (PSI) / 4096 (private). We use 4096 — same as ffmpeg, generous enough for
//! all in-spec PMTs.

/// 4 KiB cap matches ffmpeg's `MAX_SECTION_SIZE`. Per ISO/IEC 13818-1 §2.4.4.6
/// short-form sections (PAT, PMT) cap at 1021 bytes; long-form private sections
/// cap at 4093. 4096 covers both with a small slop.
use alloc::vec::Vec;
pub(crate) const MAX_SECTION_SIZE: usize = 4096;

/// Per-PID PSI section assembler. The demuxer drives state transitions:
/// `start_new_section` on PUSI, `append_continuation` on subsequent packets
/// without PUSI.
#[derive(Debug, Default)]
pub(crate) struct PsiSectionAssembler {
    buf: Vec<u8>,
    /// Total declared section length (`section_length` + 3 fixed bytes), set
    /// once we've accumulated at least 3 bytes. `None` until then.
    declared_total: Option<usize>,
}

impl PsiSectionAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// PUSI on this PID — discard whatever was buffered (per
    /// ISO/IEC 13818-1 §2.4.4.4 a section starts at PUSI; prior partial state
    /// is invalid) and start fresh from `payload`.
    pub fn start_new_section(&mut self, payload: &[u8]) -> Result<Option<Vec<u8>>, AssemblerError> {
        self.reset();
        self.append(payload)
    }

    /// Continuation packet on this PID (no PUSI) — append to the existing
    /// buffer. If we have no prior PUSI state, drop the bytes silently
    /// (continuation without preceding PUSI is invalid per §2.4.4.4).
    pub fn append_continuation(
        &mut self,
        payload: &[u8],
    ) -> Result<Option<Vec<u8>>, AssemblerError> {
        if self.buf.is_empty() {
            return Ok(None);
        }
        self.append(payload)
    }

    fn append(&mut self, payload: &[u8]) -> Result<Option<Vec<u8>>, AssemblerError> {
        let new_len = self.buf.len() + payload.len();
        if new_len > MAX_SECTION_SIZE {
            self.reset();
            return Err(AssemblerError::Overflow {
                observed_len: new_len,
            });
        }
        self.buf.extend_from_slice(payload);

        if self.declared_total.is_none() && self.buf.len() >= 3 {
            // section_length is the low 12 bits across bytes 1..=2 of the
            // section header (after table_id at byte 0). Bits 7..4 of byte 1
            // are section_syntax_indicator + 0 + reserved.
            let section_length = (((self.buf[1] & 0x0F) as usize) << 8) | (self.buf[2] as usize);
            // Total section size = 3 fixed bytes + section_length payload.
            let total = 3 + section_length;
            if total > MAX_SECTION_SIZE {
                self.reset();
                return Err(AssemblerError::DeclaredTooLong {
                    declared_len: total,
                });
            }
            self.declared_total = Some(total);
        }

        if let Some(total) = self.declared_total {
            if self.buf.len() >= total {
                // Split off [0..total] as the completed section. Any
                // trailing bytes ([total..]) are bytes of the NEXT
                // section that arrived in the same payload chunk (per
                // H.222.0 §2.4.4.1 section-mapped layout); they remain
                // buffered for the next `try_complete_section` call.
                let leftover = self.buf.split_off(total);
                let complete = core::mem::take(&mut self.buf);
                self.buf = leftover;
                self.declared_total = None;
                return Ok(Some(complete));
            }
        }
        Ok(None)
    }

    /// Attempt to extract another complete section from already-buffered
    /// bytes (no new payload). Used by callers that may have multiple
    /// sections per payload — after a PUSI append completes one section,
    /// call this in a loop to drain any subsequent sections that arrived
    /// in the same payload window.
    pub fn try_complete_section(&mut self) -> Result<Option<Vec<u8>>, AssemblerError> {
        // ISO/IEC 13818-1 §2.4.4.5: stuffing within a section-mapped
        // payload is signaled by 0xFF table_id. If the leftover bytes
        // start with 0xFF, the rest of the payload is stuffing — drop
        // and stop. This matches ffmpeg `mpegts.c:3168-3170`.
        if self.buf.first() == Some(&0xFF) {
            self.reset();
            return Ok(None);
        }
        if self.buf.is_empty() {
            return Ok(None);
        }
        // Call append with no new bytes so the parse-section-length
        // + completion check runs against current buffer state.
        self.append(&[])
    }

    /// Reset internal state. Used on overflow + after returning a complete
    /// section. The assembler is reusable for the next section on the same PID.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.declared_total = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssemblerError {
    /// Bytes accumulated past the cap before any declared-length was seen.
    Overflow { observed_len: usize },
    /// `section_length` declared in the section header exceeds the cap.
    DeclaredTooLong { declared_len: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_complete_section_returns_in_one_call() {
        let mut a = PsiSectionAssembler::new();
        // table_id=0x02, syntax=1 + section_length=5 + 5 payload bytes.
        let buf = [0x02, 0xB0, 0x05, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        let r = a.start_new_section(&buf).expect("ok");
        assert_eq!(r.as_deref(), Some(&buf[..]));
    }

    #[test]
    fn split_section_reassembles_across_calls() {
        let mut a = PsiSectionAssembler::new();
        // table_id=0x02 + section_length=5 + 5 payload bytes, split 3/5.
        let part1 = [0x02, 0xB0, 0x05];
        let part2 = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        assert_eq!(a.start_new_section(&part1).unwrap(), None);
        let r = a.append_continuation(&part2).unwrap();
        assert!(r.is_some());
        assert_eq!(r.unwrap().len(), 8);
    }

    #[test]
    fn declared_overlong_returns_error_and_resets() {
        let mut a = PsiSectionAssembler::new();
        // section_length = 0xFFF = 4095 → total = 4098 > 4096 cap.
        let buf = [0x02, 0xBF, 0xFF];
        let err = a.start_new_section(&buf).unwrap_err();
        assert!(matches!(
            err,
            AssemblerError::DeclaredTooLong { declared_len: 4098 }
        ));
        // Assembler is reset and reusable.
        let r = a
            .start_new_section(&[0x02, 0xB0, 0x05, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE])
            .unwrap();
        assert!(r.is_some());
    }

    #[test]
    fn continuation_without_pusi_drops_silently() {
        let mut a = PsiSectionAssembler::new();
        let r = a.append_continuation(&[0x02, 0xB0]).unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn append_overflow_returns_error_and_resets() {
        let mut a = PsiSectionAssembler::new();
        // PUSI with 1 byte (no section_length yet).
        a.start_new_section(&[0x02]).unwrap();
        // Continue with > 4096 bytes.
        let oversized = vec![0xFFu8; MAX_SECTION_SIZE + 1];
        let err = a.append_continuation(&oversized).unwrap_err();
        assert!(matches!(err, AssemblerError::Overflow { .. }));
    }

    /// Leftover bytes after a completed section
    /// must be retained in `self.buf` (via `split_off`) so the next
    /// `try_complete_section` call can drain them. Tests the section-mapped
    /// layout per H.222.0 §2.4.4.1: a single payload window carrying the
    /// tail of section A followed by section B's prefix.
    #[test]
    fn split_off_preserves_leftover_for_next_section() {
        let mut a = PsiSectionAssembler::new();
        // Section A: table_id=0x02 + section_length=5 + 5 payload bytes = 8 bytes total.
        let section_a = [0x02, 0xB0, 0x05, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        // Section B prefix: just the first 3 bytes of an incomplete section.
        let section_b_prefix = [0x02, 0xB0, 0x05]; // declares section_length=5 → total 8, only have 3
        let mut combined = Vec::new();
        combined.extend_from_slice(&section_a);
        combined.extend_from_slice(&section_b_prefix);

        // Feed combined as one start_new_section call.
        let r = a.start_new_section(&combined).expect("ok");
        assert_eq!(r.as_deref(), Some(&section_a[..]));
        // Section A is dispatched; B's 3-byte prefix is buffered.
        // Calling try_complete_section returns None (B not yet complete)
        // but should NOT discard the leftover.
        let r2 = a.try_complete_section().expect("ok");
        assert_eq!(r2, None);
        // Now feed the remaining 5 bytes of section B as a continuation.
        let r3 = a
            .append_continuation(&[0xAA, 0xBB, 0xCC, 0xDD, 0xEE])
            .expect("ok");
        assert_eq!(
            r3.as_deref(),
            Some(&[0x02, 0xB0, 0x05, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE][..])
        );
    }

    /// 0xFF as the first byte of leftover means
    /// stuffing per §2.4.4.5; `try_complete_section` must reset and stop.
    #[test]
    fn try_complete_section_resets_on_0xff_stuffing() {
        let mut a = PsiSectionAssembler::new();
        // Section A + 0xFF stuffing prefix.
        let mut combined: Vec<u8> = vec![0x02, 0xB0, 0x05, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        combined.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        let r = a.start_new_section(&combined).expect("ok");
        assert!(r.is_some());
        // Section A delivered. Leftover = [0xFF, 0xFF, 0xFF, 0xFF]. Next
        // try_complete_section sees 0xFF first → reset + stop.
        let r2 = a.try_complete_section().expect("ok");
        assert_eq!(r2, None);
        // Buffer is now empty (reset). A follow-up start_new_section
        // works fine on a fresh section.
        let r3 = a
            .start_new_section(&[0x02, 0xB0, 0x05, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE])
            .expect("ok");
        assert!(r3.is_some());
    }

    /// Empty-leftover case: section A completes
    /// exactly at the end of the payload window, no bytes spill into the
    /// next section. `try_complete_section` returns None on the next call.
    #[test]
    fn empty_leftover_returns_none_on_followup() {
        let mut a = PsiSectionAssembler::new();
        let section_a = [0x02, 0xB0, 0x05, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        let r = a.start_new_section(&section_a).expect("ok");
        assert_eq!(r.as_deref(), Some(&section_a[..]));
        // No leftover → try_complete_section returns None and buf stays empty.
        let r2 = a.try_complete_section().expect("ok");
        assert_eq!(r2, None);
        assert!(a.buf.is_empty());
    }

    /// `try_complete_section` with leftover that
    /// already constitutes a full section returns it on the first call.
    /// Models the section-A-completes-then-section-B-fully-present case.
    #[test]
    fn try_complete_section_drains_second_full_section() {
        let mut a = PsiSectionAssembler::new();
        let section_a = [0x02, 0xB0, 0x05, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        let section_b = [0x02, 0xB0, 0x05, 0x11, 0x22, 0x33, 0x44, 0x55];
        let mut combined = Vec::new();
        combined.extend_from_slice(&section_a);
        combined.extend_from_slice(&section_b);
        let r = a.start_new_section(&combined).expect("ok");
        assert_eq!(r.as_deref(), Some(&section_a[..]));
        let r2 = a.try_complete_section().expect("ok");
        assert_eq!(r2.as_deref(), Some(&section_b[..]));
        // After draining both, buf is empty.
        let r3 = a.try_complete_section().expect("ok");
        assert_eq!(r3, None);
    }
}
