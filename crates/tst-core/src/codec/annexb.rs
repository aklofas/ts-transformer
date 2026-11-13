//! The Annex-B byte-stream scanner (H.264/H.265/H.266 Annex B): where each
//! start code (`00 00 01` or `00 00 00 01`) is, and the NAL units between
//! them. `no_std`, allocation-free.
//!
//! The one scan every Annex-B consumer in the crate runs: the demuxer's NAL
//! splitter, the length-prefixed converter, the muxer's input validation,
//! NAL counting and the MISP SEI splice. Each layers its own acceptance
//! rule on top (the muxer rejects empty NAL units and a trailing partial
//! start code; the counter calls a buffer with no start code one NAL); the
//! scan itself is defined once, here.

use alloc::vec::Vec;

/// Offsets of one Annex-B start-code occurrence: where the prefix starts
/// (the run of `00`s plus the trailing `01`) and where the NAL data begins
/// (immediately after).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartCode {
    /// Offset of the first zero of the prefix.
    pub prefix_start: usize,
    /// Offset of the first byte after the prefix (the NAL header).
    pub data_start: usize,
}

/// Every start code in `buf`, collected — for the one caller that needs random
/// access (`mpegts::demux::payload::split_nals`).
///
/// Same walk as [`start_codes`].
pub(crate) fn start_code_vec(buf: &[u8]) -> Vec<StartCode> {
    StartCodes::new(buf).collect()
}

/// Every Annex-B start code in `buf`, in order, as it is found.
///
/// The scan is greedy left-to-right and non-overlapping: each match
/// consumes its own prefix bytes before the walk resumes. A run of more
/// than three zeros before `01` yields one 4-byte code whose
/// `prefix_start` skips the surplus zeros.
pub fn start_codes(buf: &[u8]) -> StartCodes<'_> {
    StartCodes::new(buf)
}

/// Iterator returned by [`start_codes`].
pub struct StartCodes<'a> {
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

/// Iterate the NAL units of an Annex-B buffer: each item is one NAL unit
/// with its header and without its start code, the byte slice between one
/// start code's `data_start` and the next start code's `prefix_start` (or the
/// end of `buf` for the last NAL).
///
/// Allocation-free — it holds one [`StartCodes`] walk and one lookahead
/// match, so callers that only need the NAL bytes (not their offsets)
/// can size or convert a buffer without materializing a match list.
/// Bytes before the first start code are not part of any NAL and are
/// skipped; a `buf` with no start code at all yields nothing.
pub fn nals(buf: &[u8]) -> Nals<'_> {
    let mut codes = StartCodes::new(buf);
    let current = codes.next();
    Nals {
        buf,
        codes,
        current,
    }
}

/// Iterator returned by [`nals`].
pub struct Nals<'a> {
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

    /// The byte-at-a-time walk that defines the scan, kept as the oracle for
    /// the search `StartCodes` runs.
    fn bytewise_start_codes(buf: &[u8]) -> Vec<StartCode> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos + 3 <= buf.len() {
            let i = pos;
            if buf[i] == 0 && buf[i + 1] == 0 {
                if buf[i + 2] == 1 {
                    pos = i + 3;
                    out.push(StartCode {
                        prefix_start: i,
                        data_start: i + 3,
                    });
                    continue;
                }
                if i + 4 <= buf.len() && buf[i + 2] == 0 && buf[i + 3] == 1 {
                    pos = i + 4;
                    out.push(StartCode {
                        prefix_start: i,
                        data_start: i + 4,
                    });
                    continue;
                }
            }
            pos = i + 1;
        }
        out
    }

    /// Every buffer of up to ten bytes over `00`, `01` and one other value,
    /// behind zero to eight filler bytes so a code meets every position of
    /// an eight-byte word and of the bytes left after the last whole word.
    #[test]
    fn scan_matches_the_bytewise_walk_on_every_short_buffer() {
        const ALPHABET: [u8; 3] = [0x00, 0x01, 0x65];
        for len in 0..=10u32 {
            for mut n in 0..3usize.pow(len) {
                let mut tail = Vec::with_capacity(len as usize);
                for _ in 0..len {
                    tail.push(ALPHABET[n % 3]);
                    n /= 3;
                }
                for lead in 0..=8 {
                    let mut buf = vec![0xFF; lead];
                    buf.extend_from_slice(&tail);
                    assert_eq!(
                        start_code_vec(&buf),
                        bytewise_start_codes(&buf),
                        "{buf:02X?}"
                    );
                }
            }
        }
    }

    /// The same, for the part of a buffer the scan takes 32 bytes at a
    /// time: every buffer of up to eight such bytes at every offset of a
    /// block's last word and across the block's end, with a second block
    /// after it.
    #[test]
    fn scan_matches_the_bytewise_walk_at_every_block_position() {
        const ALPHABET: [u8; 3] = [0x00, 0x01, 0x65];
        for len in 0..=8u32 {
            for mut n in 0..3usize.pow(len) {
                let mut middle = Vec::with_capacity(len as usize);
                for _ in 0..len {
                    middle.push(ALPHABET[n % 3]);
                    n /= 3;
                }
                for lead in 20..=34 {
                    let mut buf = vec![0xFF; lead];
                    buf.extend_from_slice(&middle);
                    buf.resize(72, 0xFF);
                    assert_eq!(
                        start_code_vec(&buf),
                        bytewise_start_codes(&buf),
                        "{buf:02X?}"
                    );
                }
            }
        }
    }

    /// One piece of a generated buffer: a start code of either length, a
    /// run of zeros, or bytes that hold no code.
    fn piece() -> impl proptest::strategy::Strategy<Value = Vec<u8>> {
        use proptest::prelude::*;
        prop_oneof![
            Just(vec![0x00, 0x00, 0x01]),
            Just(vec![0x00, 0x00, 0x00, 0x01]),
            (1usize..20).prop_map(|n| vec![0x00; n]),
            proptest::collection::vec(
                proptest::sample::select(vec![0x00u8, 0x01, 0x02, 0x03, 0x65, 0xFF]),
                1..24
            ),
            proptest::collection::vec(1u8..=255, 1..40),
        ]
    }

    proptest::proptest! {
        #[test]
        fn scan_matches_the_bytewise_walk(pieces in proptest::collection::vec(piece(), 0..24)) {
            let buf = pieces.concat();
            proptest::prop_assert_eq!(start_code_vec(&buf), bytewise_start_codes(&buf));
        }

        // The buffer ends in a start code, whole or cut short.
        #[test]
        fn scan_matches_the_bytewise_walk_with_a_code_at_the_end(
            pieces in proptest::collection::vec(piece(), 0..12),
            code in proptest::sample::select(vec![
                vec![0x00u8, 0x00, 0x01],
                vec![0x00, 0x00, 0x00, 0x01],
                vec![0x00, 0x00, 0x00, 0x00, 0x01],
                vec![0x00, 0x00],
                vec![0x00, 0x00, 0x00],
            ]),
        ) {
            let mut buf = pieces.concat();
            buf.extend_from_slice(&code);
            proptest::prop_assert_eq!(start_code_vec(&buf), bytewise_start_codes(&buf));
        }

        // The scan resumes after each code: a caller that stops early and
        // one that walks on see the same codes.
        #[test]
        fn nals_match_the_bytewise_walk(pieces in proptest::collection::vec(piece(), 0..24)) {
            let buf = pieces.concat();
            let codes = bytewise_start_codes(&buf);
            let mut expected: Vec<&[u8]> = codes
                .windows(2)
                .map(|w| &buf[w[0].data_start..w[1].prefix_start])
                .collect();
            if let Some(last) = codes.last() {
                expected.push(&buf[last.data_start..]);
            }
            proptest::prop_assert_eq!(nal_bodies(&buf), expected);
        }
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
        let codes = start_code_vec(&buf);
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
