//! MISB ST 1010.3 SDCC-FLP (Standard Deviation and Correlation Coefficient
//! pack, Floating-Point variant) — general-purpose parser/encoder.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! SDCC-FLP is its own MISB construct (a self-describing five-element pack,
//! ST 1010.3 §7 Table 4), usable by any Parent Document — not only ST 0601.
//! This module knows nothing about ST 0601 Tag 102 or the "Refined Source
//! List" row→item mapping; that caller-domain concern belongs to whichever
//! typed set embeds an SDCC-FLP occurrence (ST 0601 Tag 102 is
//! MULTI-INSTANCE with positional row→item semantics and is out of scope
//! for this module).
//!
//! ## Wire shape (§6-§7)
//!
//! Five elements, in order:
//! 1. **Matrix Size** `N` — BER-OID (§6.3.3).
//! 2. **Parse Control** — 1 byte (Mode 1, `bit7==0`) or 2 bytes (Mode 2,
//!    `bit7==1`) (§6.3.2).
//! 3. **Bit Vector** — present iff `CS==1` (sparse), `ceil(N(N-1)/16)`
//!    bytes, MSB-first, one bit per correlation slot in wire order
//!    (§6.3.4).
//! 4. **Standard Deviation values** — `N × Slen` bytes, present iff
//!    `Slen>0` (§6.3.2.3).
//! 5. **Correlation Coefficient values** — `count × Clen` bytes, present
//!    iff `Clen>0`, where `count` is the popcount of the Bit Vector
//!    (sparse) or `N(N-1)/2` (full) (§6.3.2.3).
//!
//! The matrix is `N×N` symmetric: the diagonal holds `N` standard
//! deviations, the upper-triangle off-diagonal holds `N(N-1)/2` correlation
//! coefficients in row-major order (`i<j`), and the lower triangle is the
//! transpose (§6.2, §6.3.1).
//!
//! ### Parse Control
//!
//! **Mode 1** (one byte, ST 1010.1 back-compat):
//! `[F:7][Slen:6-4][CS:3][Clen:2-0]`. Correlation format is *always*
//! ST 1201 per spec. Standard-deviation format is Parent-Document-defined;
//! this general-purpose decoder **assumes IEEE** (documented limitation —
//! ST 0601 never emits Mode 1 anyway, `ST 0601.10-22` mandates Mode 2).
//!
//! **Mode 2** (two bytes, adds runtime format selection):
//! byte 1 `[F1:7][R:6][CS:5][Cf:4][Clen:3-0]`,
//! byte 2 `[F2:7][R:6][R:5][Sf:4][Slen:3-0]`.
//! `Sf`/`Cf`: `0` = IEEE float (binary32 for `len==4`, binary64 for
//! `len==8`; other lengths are undefined and rejected), `1` = ST 1201
//! IMAPB. The correlation IMAPB range is always fixed at `[-1.0, 1.0]`
//! (§6.3.2.3); the standard-deviation IMAPB range is Parent-Document-
//! defined and therefore **unknown to this decoder** — an `Sf=1` std-dev
//! is rejected (ST 0601 restricts std devs to IEEE per `ST 0601.10-22`;
//! foreign Mode-2 IMAP std devs are out of scope, matching the analogous
//! Mode-1 policy above).
//!
//! ## Spec coverage
//!
//! **Standard:** MISB ST 1010.3 §5-§9, Appendix A (sparse cost model).
//! Both Mode 1 and Mode 2 decode; encode is Mode 2 only (the only mode
//! ST 0601 permits, `ST 0601.10-22`). The Bit Vector sparse choice on
//! encode follows Appendix A's Eq 6-7 cost model.

use alloc::vec::Vec;

use crate::error::{KlvDecodeError, KlvEncodeError, KlvFieldError};
use crate::klv::imapb::{ImapbParams, decode_imapb, encode_imapb};
use crate::klv::length::{read_ber_oid, write_ber_oid};

/// One parsed SDCC-FLP (ST 1010.3 §6-§7). N×N symmetric: N std devs
/// (diagonal) + N(N-1)/2 correlations (upper triangle, row-major).
#[derive(Debug, Clone, PartialEq)]
pub struct SdccFlp {
    /// `u64` (not `usize`) per the workspace's public-surface FFI-portability
    /// convention (`scripts/check/rust/no-public-usize.sh`) — a size/count
    /// field on a public struct must not vary shape across 32-/64-bit
    /// targets, even though typed KLV never crosses the C ABI today.
    pub matrix_size: u64,
    /// Diagonal σ values, length == matrix_size (empty when Slen == 0).
    pub std_devs: Vec<f64>,
    /// Upper-triangle correlations, row-major (i<j), ABSENT slots as 0.0
    /// (sparse-mode zeros are reconstituted). Unlike `std_devs`, this is
    /// ALWAYS the full `matrix_size*(matrix_size-1)/2` length regardless of
    /// `Clen` — a `Clen==0` pack (no correlation data at all) still gets a
    /// zero-filled vector of that length, not an empty one. Only empty
    /// when `matrix_size <= 1` (no possible correlation slots).
    pub correlations: Vec<f64>,
    /// True where the wire actually carried the slot (all-true in full mode).
    pub correlation_present: Vec<bool>,
}

impl SdccFlp {
    /// ρ(i,j) with symmetry; σ via `std_devs[i]`.
    ///
    /// Panics if `i` or `j` is `>= matrix_size`. Additionally, for a
    /// diagonal query (`i==j`) only: panics if this pack has no
    /// standard-deviation data at all (`Slen==0` — spec-legal, see the
    /// `std_devs` field doc; a manually-constructed `SdccFlp` can also
    /// hit this if `std_devs.len() < matrix_size`). Off-diagonal queries
    /// never hit that second case — `correlations` is always sized to the
    /// full triangle regardless of `Clen` (see its field doc).
    pub fn correlation(&self, i: usize, j: usize) -> f64 {
        let n = self.matrix_size as usize;
        assert!(
            i < n && j < n,
            "SdccFlp::correlation index out of bounds: ({i}, {j}) for matrix_size {n}"
        );
        if i == j {
            assert!(
                i < self.std_devs.len(),
                "SdccFlp::correlation({i}, {i}): no standard-deviation value \
                 available (Slen==0 / an empty std_devs for this pack)"
            );
            return self.std_devs[i];
        }
        let (lo, hi) = if i < j { (i, j) } else { (j, i) };
        self.correlations[triangle_slot(lo, hi, n)]
    }
}

/// Decode a wire-format SDCC-FLP (ST 1010.3 §6.3, both Mode 1 and Mode 2).
///
/// Expects exactly the pack bytes (Element 1 Matrix Size onward) — no
/// outer TLV framing and no leading Universal Label (the standalone
/// universal-keyed triplet form carries a 16-byte UL that callers must
/// strip before calling this function; an ST 0601 Tag 102 Local-Set value
/// never has one — see the module doc).
///
/// # Errors
///
/// [`KlvFieldError::TruncatedField`] on a short buffer, or
/// [`KlvFieldError::InvalidLength`] on trailing bytes after a fully
/// decoded pack (or, per the module doc's Mode-2 IMAP-std-dev policy, when
/// `Sf=1` — the Parent-Document-defined range is unknowable here).
/// IMAPB-substrate errors from correlation decode ([`decode_imapb`])
/// propagate as-is.
pub fn decode_sdcc_flp(bytes: &[u8]) -> Result<SdccFlp, KlvFieldError> {
    // Element 1: Matrix Size (BER-OID; §6.3.3).
    let (n_raw, mut rest) = read_ber_oid(bytes).map_err(substrate_err)?;
    let n = n_raw as usize;

    // Element 2: Parse Control (§6.3.2). Mode is signaled by bit7 of the
    // first PC byte: 0 => Mode 1 (one byte), 1 => Mode 2 (two bytes).
    let (pc1_buf, r) = take(rest, 1)?;
    rest = r;
    let pc1 = pc1_buf[0];
    let (cs, cf_imap, clen, sf_imap, slen) = if pc1 & 0x80 == 0 {
        let slen = ((pc1 >> 4) & 0x07) as usize;
        let cs = (pc1 >> 3) & 0x01 != 0;
        let clen = (pc1 & 0x07) as usize;
        // Mode 1: correlation format always ST 1201; std-dev format
        // assumed IEEE (see module doc).
        (cs, true, clen, false, slen)
    } else {
        let (pc2_buf, r) = take(rest, 1)?;
        rest = r;
        parse_mode2(pc1, pc2_buf[0])
    };

    // Bound N *before* any allocation sized by it — see
    // `check_matrix_size_fits`. Must run ahead of every `corr_slots(n)`
    // usize computation below (32-bit targets overflow it for large N).
    check_matrix_size_fits(n_raw, rest.len(), slen, clen, cs)?;

    // Element 3: Bit Vector (iff CS==1; §6.3.4). Read unconditionally on
    // `cs` per the Table-4 element order — independent of Clen, which is
    // handled below (a CS==1 + Clen==0 combination is spec-malformed, but
    // this defensive parser still consumes the Bit Vector for framing).
    let m = corr_slots(n);
    let v_bytes = m.div_ceil(8);
    let bitvec = if cs {
        let (b, r) = take(rest, v_bytes)?;
        rest = r;
        b
    } else {
        &[][..]
    };

    // Element 4: Standard Deviation values (§6.3.2.3) — N × Slen bytes,
    // present iff Slen>0.
    let mut std_devs = Vec::with_capacity(if slen > 0 { n } else { 0 });
    if slen > 0 {
        for _ in 0..n {
            let (raw, r) = take(rest, slen)?;
            rest = r;
            std_devs.push(decode_std_dev(raw, sf_imap)?);
        }
    }

    // Element 5: Correlation Coefficient values (§6.3.2.3). Per Table 3,
    // Clen==0 means no correlation values at all, regardless of CS — this
    // overrides the "CS==0 => all slots present" default below.
    //
    // Sparse-mode preflight (Copilot review): `check_matrix_size_fits`
    // already bounds `m` to at most `remaining*8` at Parse-Control time,
    // so the `present`/`correlations` allocations below can never
    // themselves abort the process — but per this project's precedent
    // (the H.264 `max_au_bytes` arc), invalid input should never pay even
    // a bounded allocation before failing. A truncated payload can still
    // declare, via the Bit Vector, more present slots than the buffer has
    // correlation bytes for. Count the *meaningful* popcount (bits past
    // slot `m-1` are wire padding, not slots) without allocating, and
    // reject before the m-sized vecs below if the buffer can't possibly
    // hold `popcount * clen` correlation bytes. (Full mode needs no
    // analogous check: `check_matrix_size_fits` already bounds `m` itself
    // against `clen` before `m` is even computed at line ~164, i.e.
    // before any allocation — verified, not just assumed.)
    if cs && clen > 0 {
        let popcount = (0..m)
            .filter(|&k| (bitvec[k / 8] >> (7 - (k % 8))) & 1 != 0)
            .count() as u64;
        if popcount.saturating_mul(clen as u64) > rest.len() as u64 {
            return Err(KlvFieldError::TruncatedField { tag: 0 });
        }
    }
    let present = if clen == 0 {
        alloc::vec![false; m]
    } else if cs {
        bit_vector_slots(bitvec, n)
    } else {
        alloc::vec![true; m]
    };
    let mut correlations = alloc::vec![0.0f64; m];
    for (slot, &is_present) in present.iter().enumerate() {
        if !is_present {
            continue;
        }
        let (raw, r) = take(rest, clen)?;
        rest = r;
        correlations[slot] = decode_correlation(raw, cf_imap, clen)?;
    }

    // Trailing garbage after a fully decoded pack is rejected.
    if !rest.is_empty() {
        return Err(KlvFieldError::InvalidLength {
            tag: 0,
            expected: bytes.len() - rest.len(),
            got: bytes.len(),
        });
    }

    Ok(SdccFlp {
        matrix_size: n as u64,
        std_devs,
        correlations,
        correlation_present: present,
    })
}

/// Encode a Mode-2 SDCC-FLP: std devs IEEE binary32 (`Sf=0`, `Slen=4` — the
/// ST 0601 §8.102.1 recommendation), correlations ST 1201 IMAPB(-1,1,clen)
/// (`Cf=1`). Sparse mode + Bit Vector are chosen automatically when
/// zero-correlations make it pay (Appendix A Eq 6-7: `(Z/M)·8·clen > 1`).
///
/// `correlations.len()` must equal `std_devs.len() * (std_devs.len()-1) / 2`
/// (the upper-triangle slot count for a matrix of size `std_devs.len()`),
/// in row-major (i<j) order.
///
/// # Errors
///
/// [`KlvEncodeError::OutOfRange`] if `correlations.len()` doesn't match the
/// matrix size implied by `std_devs`. [`KlvEncodeError::UnsupportedImapbLength`]
/// if `clen` is outside `1..=8` (the IMAPB substrate cap — Mode 2's `Clen`
/// field allows up to 15, but no in-tree caller needs more than 8).
/// [`KlvEncodeError::OutOfRange`] also propagates from [`encode_imapb`] if
/// any correlation is outside `[-1.0, 1.0]`.
pub fn encode_sdcc_flp_mode2(
    std_devs: &[f64],
    correlations: &[f64],
    clen: usize,
) -> Result<Vec<u8>, KlvEncodeError> {
    let n = std_devs.len();
    let m = corr_slots(n);
    if correlations.len() != m {
        return Err(KlvEncodeError::OutOfRange {
            tag: 0,
            value: correlations.len() as f64,
            min: m as f64,
            max: m as f64,
            hint: Some("correlations.len() must equal matrix_size*(matrix_size-1)/2"),
        });
    }
    if clen == 0 || clen > 8 {
        return Err(KlvEncodeError::UnsupportedImapbLength { length: clen });
    }

    // Appendix A Eq 6-7: sparse pays when (Z/M)*8*clen > 1, i.e.
    // Z*8*clen > M (cross-multiplied to avoid a float division).
    let zeros = correlations.iter().filter(|&&c| c == 0.0).count();
    let sparse = m > 0 && zeros * 8 * clen > m;

    let mut out = Vec::new();

    // Element 1: Matrix Size (BER-OID).
    let mut n_buf = [0u8; 5];
    let n_len = write_ber_oid(n as u32, &mut n_buf)?;
    out.extend_from_slice(&n_buf[..n_len]);

    // Element 2: Parse Control, Mode 2. Std devs fixed IEEE binary32
    // (Sf=0, Slen=4); correlations fixed ST 1201 (Cf=1) at the
    // caller-chosen `clen`.
    out.push(0x80 | if sparse { 0x20 } else { 0x00 } | 0x10 | clen as u8);
    out.push(0x04); // Sf=0 (IEEE), Slen=4

    // Element 3: Bit Vector (iff sparse).
    if sparse {
        out.extend(encode_bit_vector(correlations));
    }

    // Element 4: Standard Deviation values, IEEE binary32.
    for &sigma in std_devs {
        out.extend_from_slice(&(sigma as f32).to_be_bytes());
    }

    // Element 5: Correlation Coefficient values, ST 1201 IMAPB(-1,1,clen).
    // In sparse mode, zero-valued correlations are omitted (the Bit
    // Vector marks their absence; decode reconstitutes them as 0.0).
    let params = ImapbParams {
        min: -1.0,
        max: 1.0,
        length: clen,
    };
    let mut buf = alloc::vec![0u8; clen];
    for &rho in correlations {
        if sparse && rho == 0.0 {
            continue;
        }
        encode_imapb(&params, rho, &mut buf)?;
        out.extend_from_slice(&buf);
    }

    Ok(out)
}

// ── internal helpers ─────────────────────────────────────────────────────

/// Number of correlation slots (upper-triangle, off-diagonal) for an
/// `n × n` symmetric matrix: `n(n-1)/2`. Avoids unsigned underflow at
/// `n==0` (`saturating_sub`).
fn corr_slots(n: usize) -> usize {
    n.saturating_sub(1) * n / 2
}

/// Rejects a Matrix Size `N` whose implied Std-Dev / Correlation vectors
/// could not possibly be backed by `remaining` — the wire bytes still
/// unread after Elements 1-2 (Matrix Size + Parse Control).
///
/// `N` is attacker-controlled via the Element-1 BER-OID (`read_ber_oid`
/// accepts any value up to `u32::MAX`), and `corr_slots(N) = N(N-1)/2`
/// grows quadratically. Sizing a `Vec` by that count *before* reading any
/// correlation byte lets a ~7-byte input demand a multi-exabyte
/// allocation, which aborts the process rather than returning an `Err`.
/// This must run before any `usize`-typed use of `N`/`corr_slots(N)`: on a
/// 32-bit target (thumbv7em/riscv32) that multiplication overflows `usize`
/// for `N` anywhere near `u32::MAX`, so every size here is computed in
/// `u64` (always safe — `corr_slots` of `u32::MAX` is a few percent under
/// `u64::MAX`) and only narrowed to `usize` once a passing bound proves it
/// small.
fn check_matrix_size_fits(
    n_raw: u32,
    remaining: usize,
    slen: usize,
    clen: usize,
    cs: bool,
) -> Result<(), KlvFieldError> {
    let n64 = u64::from(n_raw);
    let remaining64 = remaining as u64;
    let slots64 = n64.saturating_sub(1).saturating_mul(n64) / 2;

    // Element 4: N * Slen std-dev bytes, present iff Slen>0 (Table 4).
    if slen > 0 && n64.saturating_mul(slen as u64) > remaining64 {
        return Err(KlvFieldError::TruncatedField { tag: 0 });
    }

    // Element 5, full mode: corr_slots(N) * Clen bytes, present iff
    // Clen>0 and not sparse — sparse mode transmits only the present
    // slots (popcount of the Bit Vector), not the full triangle, so this
    // bound would false-reject a legitimately-sparse large matrix.
    if clen > 0 && !cs && slots64.saturating_mul(clen as u64) > remaining64 {
        return Err(KlvFieldError::TruncatedField { tag: 0 });
    }

    // Element 3 (Bit Vector, read whenever CS==1 regardless of Clen —
    // see the Element-3 comment below) needs ceil(corr_slots(N)/8) bytes,
    // i.e. corr_slots(N) <= remaining*8. Applied UNCONDITIONALLY, not
    // only when CS==1: it is also the only wire-derived anchor for the
    // Slen==0 && Clen==0 && CS==0 combination — a spec-legal, data-free
    // pack (Table 4: both are "present iff >0") that transmits zero bytes
    // for either element, yet still has `present`/`correlations` sized to
    // `corr_slots(N)` below. Without this unconditional check that
    // combination would leave N completely unbounded.
    if slots64 > remaining64.saturating_mul(8) {
        return Err(KlvFieldError::TruncatedField { tag: 0 });
    }

    Ok(())
}

/// Row-major upper-triangular slot index for `(i, j)` with `i < j` in an
/// `n × n` symmetric matrix (ST 1010.3 §6.3.4 slot ordering: row by row,
/// left to right within a row).
fn triangle_slot(i: usize, j: usize, n: usize) -> usize {
    let offset = i * (2 * n - i - 1) / 2;
    offset + (j - i - 1)
}

/// Decode Mode 2's two-byte Parse Control (§6.3.2.2 Fig 8 / Table 2).
/// Returns `(cs, cf_imap, clen, sf_imap, slen)`.
pub(crate) fn parse_mode2(b1: u8, b2: u8) -> (bool, bool, usize, bool, usize) {
    let cs = (b1 >> 5) & 0x01 != 0;
    let cf_imap = (b1 >> 4) & 0x01 != 0;
    let clen = (b1 & 0x0F) as usize;
    let sf_imap = (b2 >> 4) & 0x01 != 0;
    let slen = (b2 & 0x0F) as usize;
    (cs, cf_imap, clen, sf_imap, slen)
}

/// Extract the presence bits for `n(n-1)/2` correlation slots from a Bit
/// Vector, MSB-first (§6.3.4 — "the vector starts with the most
/// significant bit of the most significant byte"), in the same row-major
/// order as the correlation slots themselves.
pub(crate) fn bit_vector_slots(bitvec: &[u8], n: usize) -> Vec<bool> {
    (0..corr_slots(n))
        .map(|k| (bitvec[k / 8] >> (7 - (k % 8))) & 1 != 0)
        .collect()
}

/// Cheap peek at Element 1 (Matrix Size) for callers that need `N` before
/// deciding how many preceding Local Set items an SDCC-FLP occurrence
/// refines (`st0601::decode::apply_typed_tag`'s Tag 102 arm). `N` is
/// bounded by what the pack's own bytes can hold — Element 2's Parse
/// Control plus [`check_matrix_size_fits`], the rule `decode_sdcc_flp`
/// applies before it allocates — so a hostile `N` cannot make the caller
/// copy a tag history the pack could never describe (review 9, ext R9-02:
/// n occurrences after n items retained n² tags). Returns `None` on a
/// truncated/malformed BER-OID, a missing Parse Control, or a size the pack
/// cannot hold; Elements 3–5 are not validated here.
pub(crate) fn peek_matrix_size(bytes: &[u8]) -> Option<usize> {
    let (n_raw, rest) = read_ber_oid(bytes).ok()?;
    let (pc1, rest) = rest.split_first()?;
    let (cs, clen, slen, rest) = if pc1 & 0x80 == 0 {
        // Mode 1 (one byte): same field layout as `decode_sdcc_flp`.
        (
            (pc1 >> 3) & 0x01 != 0,
            (pc1 & 0x07) as usize,
            ((pc1 >> 4) & 0x07) as usize,
            rest,
        )
    } else {
        let (pc2, rest) = rest.split_first()?;
        let (cs, _cf_imap, clen, _sf_imap, slen) = parse_mode2(*pc1, *pc2);
        (cs, clen, slen, rest)
    };
    check_matrix_size_fits(n_raw, rest.len(), slen, clen, cs).ok()?;
    Some(n_raw as usize)
}

/// Row-major (i<j) presence bit vector, MSB-first, `ceil(len/8)` bytes —
/// the encode-side inverse of [`bit_vector_slots`]. Bit=1 marks a nonzero
/// (transmitted) correlation; bit=0 marks an omitted (reconstituted-as-
/// 0.0) one.
fn encode_bit_vector(correlations: &[f64]) -> Vec<u8> {
    let mut out = alloc::vec![0u8; correlations.len().div_ceil(8)];
    for (k, &rho) in correlations.iter().enumerate() {
        if rho != 0.0 {
            out[k / 8] |= 1 << (7 - (k % 8));
        }
    }
    out
}

/// Decode one standard-deviation value. IEEE binary32/binary64 by length;
/// ST 1201/IMAP-mapped std devs (`sf_imap`) are rejected — their min/max
/// are Parent-Document-defined and unknowable to this general-purpose
/// parser (module doc; ST 0601 never emits them per `ST 0601.10-22`).
fn decode_std_dev(raw: &[u8], sf_imap: bool) -> Result<f64, KlvFieldError> {
    if sf_imap {
        return Err(std_dev_unsupported(raw.len()));
    }
    decode_ieee_be(raw).ok_or_else(|| std_dev_unsupported(raw.len()))
}

/// `InvalidLength`-class error for a standard-deviation encoding this
/// module cannot decode: either explicitly ST 1201-mapped with unknown
/// range (`sf_imap`), or an IEEE length other than 4 (binary32) / 8
/// (binary64), which the spec's format notes leave undefined. `expected`
/// is `0` — there is no single "correct" length to report, only "none is
/// decodable for this format."
fn std_dev_unsupported(len: usize) -> KlvFieldError {
    KlvFieldError::InvalidLength {
        tag: 0,
        expected: 0,
        got: len,
    }
}

/// Decode one correlation-coefficient value. IEEE binary32/binary64 by
/// length, or ST 1201 IMAPB(-1.0, 1.0, `clen`) when `cf_imap`. Unlike std
/// devs, the correlation IMAPB range is spec-fixed (§6.3.2.3), so this
/// path never hits the "unknown range" problem.
///
/// A foreign producer's IMAPB special/out-of-range wire pattern decodes as
/// `0.0` — correlations already carry "0.0 means no correlation" as their
/// spec-defined absent-value semantic (§8.102.1), so collapsing an
/// unusual signal into that same value is a documented, deliberate
/// simplification rather than a silent data-corruption risk.
fn decode_correlation(raw: &[u8], cf_imap: bool, clen: usize) -> Result<f64, KlvFieldError> {
    if cf_imap {
        let params = ImapbParams {
            min: -1.0,
            max: 1.0,
            length: clen,
        };
        Ok(decode_imapb(&params, raw)?.value().unwrap_or(0.0))
    } else {
        decode_ieee_be(raw).ok_or_else(|| std_dev_unsupported(raw.len()))
    }
}

/// IEEE-754 big-endian decode for the two lengths the spec's format notes
/// define: 4 bytes (binary32) or 8 bytes (binary64). Any other length is
/// undefined by ST 1010.3 and returns `None`.
fn decode_ieee_be(bytes: &[u8]) -> Option<f64> {
    match bytes.len() {
        4 => Some(f32::from_be_bytes(bytes.try_into().unwrap()) as f64),
        8 => Some(f64::from_be_bytes(bytes.try_into().unwrap())),
        _ => None,
    }
}

/// Take exactly `n` bytes off the front of `buf`, or a truncation error.
fn take(buf: &[u8], n: usize) -> Result<(&[u8], &[u8]), KlvFieldError> {
    if buf.len() < n {
        return Err(KlvFieldError::TruncatedField { tag: 0 });
    }
    Ok((&buf[..n], &buf[n..]))
}

/// Map a substrate [`KlvDecodeError`] (from `read_ber_oid`) to the
/// [`KlvFieldError`] this module's public functions return. All of
/// `read_ber_oid`'s failure modes (truncation, BER-OID overflow, a
/// too-long continuation run) mean the same thing here: the Matrix Size
/// element could not be parsed.
fn substrate_err(_: KlvDecodeError) -> KlvFieldError {
    KlvFieldError::TruncatedField { tag: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Test-local helper: strip whitespace and parse a hex string into bytes.
    fn hex(s: &str) -> Vec<u8> {
        let clean: alloc::string::String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..clean.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn sdcc_parse_control_spec_examples() {
        // Fig 7 (Mode 1): 0x4B -> Slen 4, sparse, Clen 3. Full end-to-end
        // Mode-1 decode is covered separately by
        // sdcc_mode1_full_3x3_golden (a different PC byte, 0x43, chosen so
        // the full pack has a clean non-sparse correlation set that's
        // hand-derivable without a bit vector).
        // Fig 9 (Mode 2): 0xB3 0x08 -> sparse, Cf=ST1201, Clen 3, Sf=IEEE, Slen 8.
        // Pinned indirectly by the goldens; direct unit tests target the (private) pc parser:
        assert_eq!(parse_mode2(0xB3, 0x08), (true, true, 3, false, 8)); // (cs, cf_imap, clen, sf_imap, slen)
    }

    #[test]
    fn sdcc_mode1_full_3x3_golden() {
        // Hand-derived Mode-1 golden — Mode 1 has no encode path in this
        // module, so this is NOT round-tripped through our own encoder;
        // every byte below is derived directly from the spec algorithm.
        //
        // PC = 0x43 = 0100_0011: F=0 (Mode 1, bit7==0), Slen=4 (bits 6-4 =
        // 0100), CS=0 (bit3, not sparse), Clen=3 (bits 2-0 = 011). Mode 1
        // correlations are ALWAYS ST 1201 (never IEEE, per spec); std-dev
        // format is assumed IEEE (module doc — Parent-Document-defined,
        // undocumented by the spec itself for Mode 1).
        //
        // Std devs: IEEE binary32 for 1.0/2.0/4.0 — standard, independently
        // verifiable bit patterns (sign=0, biased exponent 127/128/129,
        // zero mantissa): 0x3F800000 / 0x40000000 / 0x40800000.
        //
        // Correlations: IMAPB(-1.0, 1.0, Clen=3) per ST 1010.3 §6.3.2.3.
        // ST 1201.5 §8.9: dPow = 8*3-1 = 23; bPow = ceil(log2(max-min)) =
        // ceil(log2(2)) = 1; sF = 2^(dPow-bPow) = 2^22 = 4194304. Zoffset =
        // frac(sF*min) = frac(-4194304) = 0 (min<0<max, but sF*min is
        // already an integer). Encode: y = floor(sF*(x-min) + Zoffset) =
        // floor(2^22*(x+1)):
        //   x=0.5  -> y = floor(2^22 * 1.5) = 6291456 = 0x600000
        //   x=0.0  -> y = floor(2^22 * 1.0) = 4194304 = 0x400000
        //   x=-0.5 -> y = floor(2^22 * 0.5) = 2097152 = 0x200000
        let bytes = hex(concat!(
            "03 43",
            "3F800000 40000000 40800000",
            "600000 400000 200000",
        ));
        let m = decode_sdcc_flp(&bytes).unwrap();
        assert_eq!(m.matrix_size, 3);
        assert_eq!(m.std_devs, alloc::vec![1.0, 2.0, 4.0]);
        assert_eq!(m.correlations, alloc::vec![0.5, 0.0, -0.5]);
        assert_eq!(m.correlation_present, alloc::vec![true, true, true]);
    }

    #[test]
    fn sdcc_full_3x3_ieee_golden() {
        // Constructed golden (byte-verified 2026-07-16): N=3, Mode 2, all-IEEE binary32,
        // sigma=[1,2,4], rho=[0.5,0.0,-0.5]. PC = 0x84 0x04.
        let bytes = hex(concat!(
            "03 84 04",
            "3F800000 40000000 40800000",
            "3F000000 00000000 BF000000",
        ));
        let m = decode_sdcc_flp(&bytes).unwrap();
        assert_eq!(m.matrix_size, 3);
        assert_eq!(m.std_devs, alloc::vec![1.0, 2.0, 4.0]);
        assert_eq!(m.correlations, alloc::vec![0.5, 0.0, -0.5]);
        assert_eq!(m.correlation(2, 0), 0.0); // symmetry accessor
    }

    #[test]
    fn sdcc_sparse_3x3_bit_vector_golden() {
        // N=3 sparse, only rho13=0.25 present. PC = 0xA4 0x04, bit vector 0x40.
        let bytes = hex("03 A4 04 40 3F800000 40000000 40800000 3E800000");
        let m = decode_sdcc_flp(&bytes).unwrap();
        assert_eq!(m.correlations, alloc::vec![0.0, 0.25, 0.0]);
        assert_eq!(m.correlation_present, alloc::vec![false, true, false]);
    }

    #[test]
    fn sdcc_fig12_bit_vector_layout() {
        // ST 1010.3 Fig 12: N=5, bit string 1010 0110 1100 0000 = bytes A6 C0.
        // Slot order (row-major upper triangle, MSB-first):
        // (1,2)(1,3)(1,4)(1,5)(2,3)(2,4)(2,5)(3,4)(3,5)(4,5) -> set slots are
        // 0,2,5,6,8,9 = correlations (1,2)(1,4)(2,4)(2,5)(3,5)(4,5), 6 present.
        let present = bit_vector_slots(&[0xA6, 0xC0], 5);
        assert_eq!(
            present,
            alloc::vec![
                true, false, true, false, false, true, true, false, true, true
            ]
        );
        assert_eq!(present.iter().filter(|&&b| b).count(), 6);
    }

    #[test]
    fn sdcc_mode2_encode_round_trips() {
        let bytes = encode_sdcc_flp_mode2(&[1.0, 2.0, 4.0], &[0.5, 0.0, -0.5], 2).unwrap();
        let m = decode_sdcc_flp(&bytes).unwrap();
        assert_eq!(m.std_devs, alloc::vec![1.0, 2.0, 4.0]);
        assert!((m.correlations[0] - 0.5).abs() < 1e-3); // IMAPB(-1,1,2) quantization
    }

    #[test]
    fn sdcc_correlation_diagonal_returns_std_dev() {
        // Diagonal access (i==j) returns std_devs[i] — previously
        // uncovered. Reuses the full 3x3 IEEE golden's known sigmas.
        let bytes = hex(concat!(
            "03 84 04",
            "3F800000 40000000 40800000",
            "3F000000 00000000 BF000000",
        ));
        let m = decode_sdcc_flp(&bytes).unwrap();
        assert_eq!(m.correlation(0, 0), 1.0);
        assert_eq!(m.correlation(1, 1), 2.0);
        assert_eq!(m.correlation(2, 2), 4.0);
    }

    #[test]
    fn sdcc_slen_zero_offdiagonal_correlation_still_works() {
        // Mode 2, N=3, Slen=0 (no std-dev data at all — spec-legal, Table 4:
        // std devs "present iff Slen>0"), Clen=4 IEEE correlations.
        // PC = 0x84 0x00: byte1 F1=1,CS=0,Cf=0(IEEE),Clen=4 = 1000_0100;
        // byte2 Sf=0,Slen=0 = 0x00 (Sf is irrelevant when Slen==0 — no
        // std-dev bytes are ever read).
        let bytes = hex("03 84 00 3F000000 00000000 BF000000");
        let m = decode_sdcc_flp(&bytes).unwrap();
        assert!(m.std_devs.is_empty());
        // `correlations` is always full-triangle-sized regardless of
        // Slen, so off-diagonal access must succeed even with no std devs.
        assert_eq!(m.correlation(0, 1), 0.5);
        assert_eq!(m.correlation(2, 0), 0.0);
    }

    #[test]
    #[should_panic(expected = "no standard-deviation value")]
    fn sdcc_slen_zero_diagonal_correlation_panics() {
        let bytes = hex("03 84 00 3F000000 00000000 BF000000");
        let m = decode_sdcc_flp(&bytes).unwrap();
        // i==0 is well within matrix_size=3 — this must be the documented
        // "no std-dev data" panic, not an index-out-of-bounds one.
        let _ = m.correlation(0, 0);
    }

    #[test]
    fn sdcc_hostile_matrix_size_returns_err_not_abort() {
        // Reviewer-confirmed abort vector: BER-OID N ≈ u32::MAX
        // (8F FF FF FF 7F), Mode 2 PC (84 00: cs=false, clen=4, slen=0).
        // Pre-fix this aborted the process via a ~9.2 EiB `Vec<bool>`
        // allocation (`corr_slots(N)` sized before any correlation byte
        // was read); it must now return an Err.
        let bytes = hex("8F FF FF FF 7F 84 00");
        assert!(decode_sdcc_flp(&bytes).is_err());
    }

    #[test]
    fn sdcc_matrix_size_exact_fit_and_one_byte_short() {
        // N=4 (m=6 correlation slots), Mode 2, cs=false, cf_imap=true
        // (IMAPB, Clen=1), Slen=0. Exactly 6 correlation bytes fits the
        // new pre-allocation size guard; removing the last byte must
        // still Err — same truncation outcome as before the fix, just
        // caught one step earlier — proving the guard's boundary is
        // exact, not over-conservative.
        let fits = hex("04 91 00 00 00 00 00 00 00");
        let m = decode_sdcc_flp(&fits).unwrap();
        assert_eq!(m.matrix_size, 4);
        assert_eq!(m.correlations.len(), 6);

        let short = hex("04 91 00 00 00 00 00 00"); // one byte short
        assert!(decode_sdcc_flp(&short).is_err());
    }

    #[test]
    fn sdcc_correlation_special_folds_to_zero() {
        // N=2, Mode 2, cs=false, cf_imap=true (IMAPB), Clen=4, Slen=0.
        // Correlation bytes E0 00 00 00 = ImapbSpecial::BelowMin, which
        // `decode_correlation`'s documented fold collapses to 0.0 (the
        // "no correlation" absent-value semantic, §8.102.1) rather than
        // propagating an OutOfRange/Special distinction this pack format
        // has no slot for.
        let bytes = hex("02 94 00 E0000000");
        let m = decode_sdcc_flp(&bytes).unwrap();
        assert_eq!(m.correlations, alloc::vec![0.0]);
    }

    #[test]
    fn sdcc_sparse_correlation_bytes_truncated_preflights_before_alloc() {
        // N=3, sparse (cs=1), Clen=4 IEEE, Slen=0. PC = 0xA4 0x00. Bit
        // vector 0xC0 (1100_0000) declares slots 0 and 1 present (popcount
        // 2 of the 3 meaningful bits), but the buffer ends right after the
        // bit vector — zero correlation bytes follow, though 2*4=8 are
        // required. The sparse-mode preflight added above must reject this
        // BEFORE allocating the m-sized `present`/`correlations` vecs; by
        // construction, reaching the early `Err` return means those
        // `alloc::vec!` calls are never reached (no separate runtime probe
        // needed for the no-allocation property).
        let bytes = hex("03 A4 00 C0");
        assert!(decode_sdcc_flp(&bytes).is_err());
    }

    /// Review 9 (ext R9-02): the peek is bounded by what the pack's own
    /// bytes can hold — Parse Control + `check_matrix_size_fits` — so an ST
    /// 0601 Tag 102 occurrence can never make its caller copy a tag history
    /// the pack could not describe.
    #[test]
    fn peek_matrix_size_is_bounded_by_what_the_pack_can_hold() {
        // The C1 full golden: N = 3, Mode 2 (0x84 0x04 → Slen = 4, Clen = 4,
        // CS = 0), 3 std-devs + 3 correlations = 24 data bytes.
        let golden: [u8; 27] = [
            0x03, 0x84, 0x04, 0x3F, 0x80, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x40, 0x80, 0x00,
            0x00, 0x3F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xBF, 0x00, 0x00, 0x00,
        ];
        assert_eq!(peek_matrix_size(&golden), Some(3));
        // No Parse Control at all is not a pack.
        assert_eq!(peek_matrix_size(&[0x03]), None);
        assert_eq!(peek_matrix_size(&[]), None);
        // Mode 1, data-free (Slen = 0, Clen = 0, CS = 0): the unconditional
        // bit-vector rule bounds N(N-1)/2 <= 8 * remaining.
        assert_eq!(peek_matrix_size(&[0x01, 0x00]), Some(1));
        assert_eq!(peek_matrix_size(&[0x03, 0x00]), None); // 3 slots > 0
        assert_eq!(peek_matrix_size(&[0x03, 0x00, 0x00]), Some(3)); // 3 slots <= 8 (Review Focus 3)
        assert_eq!(peek_matrix_size(&[0x05, 0x00, 0x00]), None); // 10 slots > 8
        // Mode 1 with Slen = 1 (0x10): N std-dev bytes must fit.
        assert_eq!(peek_matrix_size(&[0x03, 0x10, 0xAA, 0xBB, 0xCC]), Some(3));
        assert_eq!(peek_matrix_size(&[0x03, 0x10, 0xAA, 0xBB]), None);
    }
}
