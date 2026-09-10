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
    StartCodes::new(buf).collect()
}

/// Lazy form of [`start_codes`]: the same walk, yielding each match as it
/// is found instead of collecting. This is the actual scanner — the
/// `Vec`-returning [`start_codes`] is a `collect()` over it, kept for the
/// one consumer that needs random access to the match list
/// (`mpegts::demux::payload::split_nals`, which pairs each match with the
/// next to slice offsets out of a shared buffer).
struct StartCodes<'a> {
    buf: &'a [u8],
    /// Index of the next byte the walk will examine.
    pos: usize,
}

impl<'a> StartCodes<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
}

impl Iterator for StartCodes<'_> {
    type Item = StartCode;

    fn next(&mut self) -> Option<StartCode> {
        let buf = self.buf;
        while self.pos + 3 <= buf.len() {
            let i = self.pos;
            if buf[i] == 0 && buf[i + 1] == 0 {
                if buf[i + 2] == 1 {
                    self.pos = i + 3;
                    return Some(StartCode {
                        prefix_start: i,
                        data_start: i + 3,
                    });
                }
                if i + 4 <= buf.len() && buf[i + 2] == 0 && buf[i + 3] == 1 {
                    self.pos = i + 4;
                    return Some(StartCode {
                        prefix_start: i,
                        data_start: i + 4,
                    });
                }
            }
            self.pos = i + 1;
        }
        None
    }
}

/// Iterate the NAL bodies of an Annex-B buffer: each item is the byte
/// slice between one start code's `data_start` and the next start code's
/// `prefix_start` (or the end of `buf` for the last NAL).
///
/// Allocation-free — it holds one [`StartCodes`] walk and one lookahead
/// match, so callers that only need the NAL bytes (not their offsets)
/// can size or convert a buffer without materializing a match list.
/// Bytes before the first start code are not part of any NAL and are
/// skipped; a `buf` with no start code at all yields nothing.
pub(crate) fn nals(buf: &[u8]) -> Nals<'_> {
    let mut codes = StartCodes::new(buf);
    let current = codes.next();
    Nals {
        buf,
        codes,
        current,
    }
}

/// Iterator returned by [`nals`].
pub(crate) struct Nals<'a> {
    buf: &'a [u8],
    codes: StartCodes<'a>,
    /// The start code whose NAL body the next `next()` call will yield;
    /// `None` once the walk is exhausted.
    current: Option<StartCode>,
}

impl<'a> Iterator for Nals<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let current = self.current?;
        // This NAL ends where the following start code's prefix begins;
        // the last NAL runs to the end of the buffer.
        let end = match self.codes.next() {
            Some(next) => {
                let end = next.prefix_start;
                self.current = Some(next);
                end
            }
            None => {
                self.current = None;
                self.buf.len()
            }
        };
        Some(&self.buf[current.data_start..end])
    }
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

    /// `nals` must slice exactly what pairing consecutive `start_codes`
    /// entries would: body runs from `data_start` to the NEXT match's
    /// `prefix_start`, so no inter-NAL prefix bytes bleed into a body.
    fn nal_bodies(buf: &[u8]) -> Vec<&[u8]> {
        nals(buf).collect()
    }

    #[test]
    fn nals_yields_each_body_without_start_code_bytes() {
        let buf = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, // NAL 1, 4-byte prefix
            0x00, 0x00, 0x01, 0x65, 0xBB, 0xCC, // NAL 2, 3-byte prefix
        ];
        assert_eq!(
            nal_bodies(&buf),
            vec![&[0x67, 0xAA][..], &[0x65, 0xBB, 0xCC][..]]
        );
    }

    #[test]
    fn nals_drops_bytes_before_the_first_start_code() {
        let buf = [0xDE, 0xAD, 0x00, 0x00, 0x01, 0x41];
        assert_eq!(nal_bodies(&buf), vec![&[0x41][..]]);
    }

    #[test]
    fn nals_yields_an_empty_body_for_back_to_back_start_codes() {
        let buf = [0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x41];
        assert_eq!(nal_bodies(&buf), vec![&[][..], &[0x41][..]]);
    }

    #[test]
    fn nals_yields_nothing_without_a_start_code() {
        assert_eq!(nal_bodies(&[]), Vec::<&[u8]>::new());
        assert_eq!(nal_bodies(&[0x01, 0x02, 0x03]), Vec::<&[u8]>::new());
    }

    /// The lazy walk and the collected one are the same walk: pairing
    /// `start_codes` by hand must reproduce `nals` byte for byte.
    #[test]
    fn nals_matches_pairing_start_codes_by_hand() {
        let buf = [
            0xFF, 0x00, 0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0x00, 0x00, 0x01, 0x65, 0x00, 0x00,
            0x00, 0x01, 0x68, 0x00, 0x00,
        ];
        let codes = start_codes(&buf);
        let mut expected: Vec<&[u8]> = codes
            .windows(2)
            .map(|w| &buf[w[0].data_start..w[1].prefix_start])
            .collect();
        if let Some(&last) = codes.last() {
            expected.push(&buf[last.data_start..]);
        }
        assert_eq!(nal_bodies(&buf), expected);
    }
}
