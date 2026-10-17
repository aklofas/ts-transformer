//! The one sync-candidate check: a 188-byte TS alignment candidate is
//! accepted when enough of the bytes one packet apart carry the sync byte.
//!
//! The sender's framer and the receiver's syncer use the strict rule (every
//! stride must match); the demuxer's re-acquisition uses an N-of-M rule.

use super::common::{TS_PACKET_SIZE, TS_SYNC_BYTE};

/// Candidate verification for a 188-byte TS sync: `buf[at]`, `buf[at + 188]`,
/// and so on are checked for the sync byte. The candidate itself is the first
/// match; `confirmations` matches out of the next `window - 1` strides are
/// enough (`window == confirmations` is the strict rule).
#[derive(Clone, Copy, Debug)]
pub struct TsSync {
    confirmations: usize,
    window: usize,
}

impl TsSync {
    /// Every one of `confirmations` positions (the candidate included) must
    /// carry the sync byte.
    pub const fn strict(confirmations: usize) -> Self {
        Self::n_of_m(confirmations, confirmations)
    }

    /// At least `n` of the `m` positions (the candidate included) must carry
    /// the sync byte. Requires `1 <= n <= m`.
    pub const fn n_of_m(n: usize, m: usize) -> Self {
        assert!(n >= 1 && n <= m);
        Self {
            confirmations: n,
            window: m,
        }
    }

    /// Whether the candidate at `at` verifies.
    ///
    /// - `Some(true)`: enough strides carry the sync byte.
    /// - `Some(false)`: the candidate byte is not a sync byte, or too many
    ///   strides already differ for the rule to be met.
    /// - `None`: the bytes present neither settle it nor rule it out, so more
    ///   bytes are needed. This includes `at` itself being past the end of
    ///   `buf`; a stride whose index is past the end is not yet present.
    pub fn verify(&self, buf: &[u8], at: usize) -> Option<bool> {
        match buf.get(at) {
            None => return None,
            Some(&b) if b != TS_SYNC_BYTE => return Some(false),
            Some(_) => {}
        }
        let mut matches = 1;
        let mut present = 1;
        for k in 1..self.window {
            match buf.get(at + k * TS_PACKET_SIZE) {
                Some(&b) => {
                    present += 1;
                    if b == TS_SYNC_BYTE {
                        matches += 1;
                    }
                }
                None => break,
            }
        }
        if matches >= self.confirmations {
            Some(true)
        } else if matches + (self.window - present) < self.confirmations {
            Some(false)
        } else {
            None
        }
    }

    /// The first offset in `buf` whose candidate verifies.
    ///
    /// `None` is deliberately one answer for two situations: no candidate
    /// verifies, or a candidate is still undecided because `buf` ends before
    /// its strides do (the scan stops there, since a later candidate must not
    /// be taken ahead of an earlier undecided one). A caller that must tell
    /// them apart calls [`Self::verify`] itself; the demuxer does, so `find`
    /// has no production caller and is kept as the reference scan.
    pub fn find(&self, buf: &[u8]) -> Option<usize> {
        for at in 0..buf.len() {
            if buf[at] != TS_SYNC_BYTE {
                continue;
            }
            match self.verify(buf, at) {
                Some(true) => return Some(at),
                Some(false) => {}
                None => return None,
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::TsSync;
    use proptest::prelude::*;

    const STRIDE: usize = 188;

    /// A byte that is never the sync byte.
    fn non_sync() -> impl Strategy<Value = u8> {
        any::<u8>().prop_map(|b| if b == 0x47 { 0x00 } else { b })
    }

    /// Reference for `find`, written as a plain loop over candidates.
    fn reference_find(buf: &[u8], n: usize, m: usize) -> Option<usize> {
        for at in 0..buf.len() {
            if buf[at] != 0x47 {
                continue;
            }
            let mut matches = 1;
            let mut present = 1;
            for k in 1..m {
                if at + k * STRIDE < buf.len() {
                    present += 1;
                    if buf[at + k * STRIDE] == 0x47 {
                        matches += 1;
                    }
                }
            }
            if matches >= n {
                return Some(at);
            }
            if matches + (m - present) >= n {
                return None;
            }
        }
        None
    }

    proptest! {
        #[test]
        fn strict_finds_a_planted_run_iff_it_is_long_enough(
            prefix in proptest::collection::vec(non_sync(), 0..400),
            fill in proptest::collection::vec(non_sync(), 8 * STRIDE),
            k in 1usize..7,
            c in 2usize..6,
            tail in 1usize..STRIDE,
        ) {
            let o = prefix.len();
            let mut buf = prefix;
            buf.extend_from_slice(&fill[..k * STRIDE + tail]);
            for i in 0..k {
                buf[o + i * STRIDE] = 0x47;
            }
            prop_assert_eq!(TsSync::strict(c).find(&buf) == Some(o), k >= c);
        }

        #[test]
        fn n_of_m_agrees_with_the_reference_loop(
            buf in proptest::collection::vec(
                prop_oneof![3 => Just(0x47u8), 1 => any::<u8>()],
                0..1600,
            ),
            m in 1usize..8,
            n_seed in 0usize..8,
        ) {
            let n = 1 + n_seed % m;
            prop_assert_eq!(TsSync::n_of_m(n, m).find(&buf), reference_find(&buf, n, m));
        }
    }
}
