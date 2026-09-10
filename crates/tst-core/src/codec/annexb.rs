//! Shared Annex-B start-code scanner.
//!
//! H.264/H.265/H.266 elementary streams delimit each NAL unit with a
//! start code — either 3 bytes (`00 00 01`) or 4 bytes (`00 00 00 01`).
//! Two consumers in this crate need the same scan: the demuxer's NAL
//! splitter (`mpegts::demux::payload::split_nals`) and the Annex-B ↔
//! length-prefixed converter (`codec::nal_framing`). Both previously
//! carried byte-identical private copies; this module is the single
//! definition they share.
//!
//! Deliberately NOT shared with `codec::util::count_annex_b_nals` or
//! `mpegts::mux::state::validate_annex_b`: those two have different
//! acceptance rules (counting and validation semantics respectively),
//! not just a different presentation of the same scan.

use alloc::vec::Vec;

/// Offsets of one Annex-B start-code occurrence: where the prefix starts
/// (the run of `00`s plus the trailing `01`) and where the NAL data begins
/// (immediately after).
#[derive(Debug, Clone, Copy)]
pub(crate) struct StartCode {
    pub(crate) prefix_start: usize,
    pub(crate) data_start: usize,
}

/// Locate every Annex-B start code (`00 00 01` or `00 00 00 01`) in `buf`.
///
/// The scan is greedy left-to-right and non-overlapping: each match
/// consumes its own prefix bytes before the walk resumes. A run of more
/// than three leading zeros (e.g. `00 00 00 00 01`) therefore yields a
/// single code whose `prefix_start` skips the surplus zeros rather than
/// attributing them to the prefix: neither width matches at the first
/// zero of the run (the 3-byte form wants `01` where a `00` sits, and
/// the 4-byte form wants it one byte further on), so the walk advances
/// a byte and the 4-byte form matches there, leaving the surplus zero
/// outside the prefix.
pub(crate) fn start_codes(buf: &[u8]) -> Vec<StartCode> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if buf[i + 2] == 1 {
                out.push(StartCode {
                    prefix_start: i,
                    data_start: i + 3,
                });
                i += 3;
                continue;
            }
            if i + 4 <= buf.len() && buf[i + 2] == 0 && buf[i + 3] == 1 {
                out.push(StartCode {
                    prefix_start: i,
                    data_start: i + 4,
                });
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collapse the scanner output to `(prefix_start, data_start)` pairs so
    /// the expectations below read as the recognition table they are.
    fn scan(buf: &[u8]) -> Vec<(usize, usize)> {
        start_codes(buf)
            .iter()
            .map(|s| (s.prefix_start, s.data_start))
            .collect()
    }

    #[test]
    fn three_byte_start_code() {
        assert_eq!(scan(&[0x00, 0x00, 0x01, 0x65, 0xAA]), [(0, 3)]);
    }

    #[test]
    fn four_byte_start_code() {
        assert_eq!(scan(&[0x00, 0x00, 0x00, 0x01, 0x65, 0xAA]), [(0, 4)]);
    }

    /// Five bytes `00 00 00 00 01` are ONE start code, not two: the walk
    /// finds no match at i=0 (neither `00 00 01` nor `00 00 00 01` fits
    /// there), steps to i=1, and matches the 4-byte form. The leading
    /// zero is left outside the prefix.
    #[test]
    fn surplus_leading_zero_is_not_attributed_to_the_prefix() {
        assert_eq!(scan(&[0x00, 0x00, 0x00, 0x00, 0x01, 0x65]), [(1, 5)]);
    }

    #[test]
    fn back_to_back_three_byte_codes() {
        assert_eq!(
            scan(&[0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x41]),
            [(0, 3), (3, 6)]
        );
    }

    #[test]
    fn four_byte_then_three_byte_code() {
        assert_eq!(
            scan(&[0x00, 0x00, 0x00, 0x01, 0x09, 0x00, 0x00, 0x01, 0x41]),
            [(0, 4), (5, 8)]
        );
    }

    /// A trailing `00 00` is not a start code — the scan needs a `01` and
    /// runs out of buffer first.
    #[test]
    fn trailing_zero_run_is_not_a_start_code() {
        assert_eq!(scan(&[0x00, 0x00, 0x01, 0x41, 0x00, 0x00]), [(0, 3)]);
    }

    #[test]
    fn empty_buffer_yields_no_codes() {
        assert_eq!(scan(&[]), []);
    }

    #[test]
    fn buffer_without_any_start_code_yields_none() {
        assert_eq!(scan(&[0x01, 0x02, 0x03, 0x04]), []);
    }
}
