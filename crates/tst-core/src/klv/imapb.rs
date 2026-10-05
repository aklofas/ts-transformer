//! ST 1201.5 §7 IMAPB — bit-packed mapping between unsigned integers and a
//! defined floating-point range.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Given parameters `(min, max, length)` with `min < max` and `length ∈ 1..=8`:
//! - The integer occupies `length` bytes, big-endian, **unsigned** (ST 1201.5
//!   §7.2.3 Table 1 reserves the top of the unsigned range for special-value
//!   indicators — the top two bits both set marks the special-value space).
//! - Per ST 1201.5 §8.9 Summary:
//!   - `bPow = ceil(log2(max − min))`
//!   - `dPow = 8L − 1`
//!   - `sF = 2^(dPow − bPow)`  *(forward scale)*
//!   - `Zoffset = sF·min − floor(sF·min)` when `min<0 and max>0`; else 0
//! - Encode (§7.2.1): `y = truncate(sF·(value − min) + Zoffset)`, L-byte unsigned BE.
//! - Decode: `value = (y − Zoffset)·sR + min`, where `sR = 1/sF`.
//!
//! ## Special values
//!
//! ST 1201.5 §7.1.3 reserves the top of the unsigned integer range for
//! out-of-band signaling values. §7.2.2 step 1 mandates the reverse-mapping
//! algorithm test `bit(msb) & bit(msb-1)` first; if both are set, the
//! integer encodes a special value rather than a normal-range float.
//! §7.2.3 Table 2 enumerates the patterns the decoder recognizes.
//!
//! `decode_imapb` returns a [`DecodedImapb`] enum that carries the special
//! value (or out-of-range diagnostic) inline alongside the `Value(f64)`
//! normal-case variant. Use [`DecodedImapb::value`] for legacy `Option<f64>`
//! ergonomics when callers don't care about which special value was signaled.
//!
//! ST 0601 fixed-range mappings (which use a different convention with
//! INT_MIN as INVALID sentinel) live in `klv::st0601::mapping`.

use crate::error::{KlvEncodeError, KlvFieldError};
#[cfg(not(feature = "std"))]
use crate::float_ext::FloatExt;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImapbParams {
    pub min: f64,
    pub max: f64,
    /// Encoded width in bytes. Must be in `1..=8` (ST 1201.5 §6 allows any L;
    /// internal math uses `u64` which holds 8 bytes). L > 8 needs `u128`
    /// and is not currently supported.
    pub length: usize,
}

impl ImapbParams {
    /// Forward scale factor `sF = 2^(dPow − bPow)` per ST 1201.5 §8.9.
    /// Returns `f64::INFINITY` for the degenerate `max == min` case (caller
    /// pre-checks parameters; this function is internal).
    ///
    /// Preconditions (`min < max` and `length ∈ [1, 8]`) are now enforced
    /// at every public call site — `encode_imapb` and `decode_imapb`
    /// pre-screen the params and surface `KlvEncodeError::InvalidImapbParams`
    /// / `KlvFieldError::InvalidImapbParams` before invoking `sf()`.
    fn sf(&self) -> f64 {
        let span = self.max - self.min;
        let b_pow = span.log2().ceil();
        let d_pow = (8 * self.length as i32 - 1) as f64;
        2f64.powf(d_pow - b_pow)
    }

    /// Zero-Point offset per ST 1201.5 §7.1.2 step 6.
    /// Only nonzero when the range straddles zero (`min < 0 < max`).
    fn z_offset(&self) -> f64 {
        if self.min < 0.0 && self.max > 0.0 {
            let scaled = self.sf() * self.min;
            scaled - scaled.floor()
        } else {
            0.0
        }
    }
}

/// Result of decoding an IMAPB wire integer per ST 1201.5 §7.2.2 + §7.2.3.
///
/// The wire format reserves the top of the unsigned integer range for
/// out-of-band signaling (§7.1.3). The reverse algorithm in §7.2.2 step 1
/// tests `bit(msb) & bit(msb-1)`: when both are set, the integer is a
/// special value rather than a normal-range float.
///
/// Normal-range decodes also get a bounds check against the configured
/// `[min, max]` — wire integers in the inter-band reserved space (above
/// `floor(sF·(max-min))` but with top-2-bits != `0b11`, per ST 1201.5 §8.6
/// Eq.12) arithmetic-decode past `max` and surface as
/// [`DecodedImapb::OutOfRange`] carrying the raw arithmetic decode.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum DecodedImapb {
    /// Normal-range decode succeeded and the result falls within `[min, max]`.
    Value(f64),
    /// An ST 1201.5 §7.2.3 special value (infinity / NaN family / MISB
    /// overflow signal / user-defined). See [`ImapbSpecial`].
    Special(ImapbSpecial),
    /// Top-two-bits `0b11` set but the remaining bits do not match a
    /// pattern this decoder recognizes (reserved / non-zero-filled /
    /// future MISB-defined per ST 1201.5 §7.2.3 Table 2). `raw` is the
    /// L-byte integer the producer emitted; callers may map specific bit
    /// patterns themselves if they have out-of-band knowledge.
    ReservedSpecial { raw: u64 },
    /// Normal-range decode succeeded but the result falls outside
    /// `[min, max]`. This is the diagnostic for the inter-band reserved
    /// integer space (per ST 1201.5 §8.6 Eq.12) — bit patterns that aren't
    /// in the §7.2.3 special-value space but that arithmetic-decode past
    /// the declared range. `decoded` is the raw arithmetic result so
    /// callers can inspect how far out of band the producer was.
    OutOfRange { decoded: f64 },
}

impl DecodedImapb {
    /// Convenience accessor: returns `Some(f64)` only for the [`Value`]
    /// variant. [`Special`], [`ReservedSpecial`], and [`OutOfRange`] all
    /// return `None`.
    ///
    /// Use this at call sites that want the legacy "decode to f64"
    /// ergonomics. Sites that need to distinguish `+∞` / NaN families /
    /// `BelowMin` etc. should pattern-match [`Special`] directly.
    ///
    /// [`Value`]: DecodedImapb::Value
    /// [`Special`]: DecodedImapb::Special
    /// [`ReservedSpecial`]: DecodedImapb::ReservedSpecial
    /// [`OutOfRange`]: DecodedImapb::OutOfRange
    #[must_use]
    pub fn value(self) -> Option<f64> {
        match self {
            DecodedImapb::Value(v) => Some(v),
            _ => None,
        }
    }
}

pub fn encode_imapb(p: &ImapbParams, value: f64, out: &mut [u8]) -> Result<(), KlvEncodeError> {
    // ST 1201.5 §6 allows any L; internal math uses u64 (max 8 bytes).
    if !(1..=8).contains(&p.length) {
        return Err(KlvEncodeError::UnsupportedImapbLength { length: p.length });
    }
    // ST 1201.5 §6 `min < max` precondition. The §8.9
    // `bPow = ceil(log2(max − min))` derivation is undefined when
    // `max <= min` (log2(0) = -∞, log2(neg) = NaN). NaN-tolerant form:
    // `partial_cmp` returns `None` for NaN inputs, which we treat as a
    // violation (clippy::neg_cmp_op_on_partial_ord forbids `!(min<max)`).
    if !matches!(p.min.partial_cmp(&p.max), Some(core::cmp::Ordering::Less)) {
        return Err(KlvEncodeError::InvalidImapbParams {
            min: p.min,
            max: p.max,
            length: p.length as u8,
        });
    }
    if out.len() < p.length {
        return Err(KlvEncodeError::BufferTooSmall {
            needed: p.length,
            got: out.len(),
        });
    }
    if !value.is_finite() || value < p.min || value > p.max {
        return Err(KlvEncodeError::OutOfRange {
            tag: 0,
            value,
            min: p.min,
            max: p.max,
            hint: None,
        });
    }
    // ST 1201.5 §7.2.1 step 4a: y = truncate(sF*(x - min) + Zoffset).
    // `truncate` = `.floor()` for x >= min (which the bounds check above guarantees).
    let y_f = p.sf() * (value - p.min) + p.z_offset();
    let y = y_f.floor() as u64;
    let mask = if p.length == 8 {
        u64::MAX
    } else {
        (1u64 << (8 * p.length as u32)) - 1
    };
    let y_clamped = y & mask;
    for (i, slot) in out.iter_mut().enumerate().take(p.length) {
        *slot = ((y_clamped >> (8 * (p.length - 1 - i))) & 0xFF) as u8;
    }
    Ok(())
}

/// Decode an IMAPB wire integer per ST 1201.5 §7.2.2 + §7.2.3.
///
/// # Behavior
///
/// Per §7.2.2 step 1, this function first inspects `bit(msb) & bit(msb-1)`
/// of the L-byte unsigned big-endian integer (the top two bits of byte 0).
/// When both bits are set, the integer encodes a special value per §7.2.3
/// Table 2 and is returned as [`DecodedImapb::Special`] (with the
/// [`ImapbSpecial`] variant) or [`DecodedImapb::ReservedSpecial`] for
/// patterns that don't match a recognized family or that violate the
/// §7.2.3 zero-fill rule.
///
/// Otherwise the normal-range reverse map
/// `value = sR · (y − Zoffset) + min` is applied, and the result is
/// bounds-checked against `[min, max]`. Values that fall outside the
/// configured range (the inter-band reserved integer space per §8.6
/// Eq.12) are returned as [`OutOfRange`] carrying the raw arithmetic
/// decode.
///
/// # Errors
///
/// - [`KlvFieldError::UnsupportedImapbLength`] when `length` is outside
///   `1..=8` (substrate caps at L=8 because internal arithmetic uses `u64`).
/// - [`KlvFieldError::InvalidImapbParams`] when `min >= max` (ST 1201.5 §6
///   precondition; the §8.9 scale-factor derivation is undefined otherwise).
/// - [`KlvFieldError::InvalidLength`] when `bytes.len() != length`.
///
/// Note that special values and out-of-range bit patterns are **not**
/// errors — they are spec-defined wire signaling from a conformant peer.
/// Callers that want to treat them as failures can pattern-match the
/// returned [`DecodedImapb`] or chain [`DecodedImapb::value`] +
/// `.ok_or(...)`.
///
/// [`ReservedSpecial`]: DecodedImapb::ReservedSpecial
/// [`OutOfRange`]: DecodedImapb::OutOfRange
pub fn decode_imapb(p: &ImapbParams, bytes: &[u8]) -> Result<DecodedImapb, KlvFieldError> {
    if !(1..=8).contains(&p.length) {
        return Err(KlvFieldError::UnsupportedImapbLength { length: p.length });
    }
    // ST 1201.5 §6 `min < max` precondition — checked AFTER the L gate
    // so pure-length failures keep their narrow `UnsupportedImapbLength`
    // diagnostic, then BEFORE the `bytes.len()` and arithmetic steps so
    // a malformed `(min, max)` never reaches `sf()`. NaN-tolerant via
    // `partial_cmp` (clippy::neg_cmp_op_on_partial_ord forbids
    // `!(min<max)`).
    if !matches!(p.min.partial_cmp(&p.max), Some(core::cmp::Ordering::Less)) {
        return Err(KlvFieldError::InvalidImapbParams {
            min: p.min,
            max: p.max,
            length: p.length as u8,
        });
    }
    if bytes.len() != p.length {
        return Err(KlvFieldError::InvalidLength {
            tag: 0,
            expected: p.length,
            got: bytes.len(),
        });
    }
    // Read L bytes as unsigned BE.
    let mut y: u64 = 0;
    for &b in bytes {
        y = (y << 8) | b as u64;
    }

    // ST 1201.5 §7.2.2 step 1: special-value detection. The spec defines
    // `special_value = Bit(msb, y) & Bit(msb-1, y)` over the L-byte
    // integer — i.e., the top two bits of byte 0. When both are set, the
    // integer is decoded via the §7.2.3 Table 2/3 pattern map, not the
    // normal reverse arithmetic.
    let top_byte = bytes[0];
    if (top_byte & 0b1100_0000) == 0b1100_0000 {
        return Ok(match classify_imapb_special(bytes, y) {
            Some(special) => DecodedImapb::Special(special),
            None => DecodedImapb::ReservedSpecial { raw: y },
        });
    }

    // Normal-range reverse map: x = sR * (y - Zoffset) + min.
    // `sf` is cached because it is used again for y_max below. `z_offset` is
    // called through its accessor (not derived from the cached `sf`) so its
    // value stays byte-identical to the encoder's own computation — single
    // source of truth, no formula duplication.
    let sf = p.sf();
    let z_offset = p.z_offset();
    let s_r = 1.0 / sf;
    let value = s_r * (y as f64 - z_offset) + p.min;

    // Guard: degenerate ranges (e.g. min ≈ -f64::MAX, max = tiny subnormal)
    // can produce an enormous sR = 1/sF, making `value` non-finite even when
    // `y` is a valid normal-range wire integer. A non-finite decode is never a
    // usable measurement; OutOfRange is the correct classification (fuzz-found).
    if !value.is_finite() {
        return Ok(DecodedImapb::OutOfRange { decoded: value });
    }

    // ST 1201.5 §8.6 Eq.12 / §7.2.3 Table 1: upper-bound reserved-space
    // detection in the integer domain. The exact max wire integer is
    //
    //   y_max = floor(sF·(b−a) + Zoffset)
    //
    // which is identical to the wire integer the encoder produces for
    // value = max (see `encode_imapb`). Any normal-pattern integer y > y_max
    // is in the inter-band reserved space and MUST decode as OutOfRange —
    // using the float-epsilon upper bound here admitted y_max+1 as Value at
    // coarse grids (L=1) because the tolerance was exactly one quantization
    // step and the comparison was not strictly greater.
    let y_max = (sf * (p.max - p.min) + z_offset).floor() as u64;
    if y > y_max {
        return Ok(DecodedImapb::OutOfRange { decoded: value });
    }

    // Lower-bound check: Zoffset rounding may push decode of y=0 slightly
    // below min by up to one quantization step. Keep a float-epsilon
    // tolerance here (the integer-domain fix above covers the upper side).
    //
    // `s_r = 1 / sF = 2^(bPow−dPow)` (ST 1201.5 §8.9), which equals the
    // quantization step. f64 ULP propagation through sR*(y−Zoffset)+min
    // is covered by the fp_eps term.
    let scale = s_r;
    let fp_eps = (p.max - p.min).abs() * f64::EPSILON * 8.0;
    let epsilon = scale.max(fp_eps);
    if value < p.min - epsilon {
        return Ok(DecodedImapb::OutOfRange { decoded: value });
    }

    Ok(DecodedImapb::Value(value))
}

/// Classify a top-two-bits-set IMAPB byte sequence into an [`ImapbSpecial`]
/// per ST 1201.5 §7.2.3 Tables 2/3, enforcing the "zero filled" rule.
/// Returns `None` for reserved / non-zero-filled patterns (caller maps
/// those to [`DecodedImapb::ReservedSpecial`]). `y` is the L-byte big-endian
/// integer already accumulated by the caller.
///
/// **Forward-compat asymmetry:** NaN families
/// (`PositiveQuietNaN`, `NegativeQuietNaN`, `PositiveSignalingNaN`,
/// `NegativeSignalingNaN`) surface non-zero `nan_id` / `signal` payloads
/// verbatim. ST 1201.5 Table 2 defines only the zero NaN-Id, but its
/// developer note says to "handle unknown special cases … preventing a
/// software failure," so future NaN identifiers are preserved rather than
/// demoted to [`DecodedImapb::ReservedSpecial`]. By contrast, ±Infinity
/// with a non-zero low-bit payload is demoted to `ReservedSpecial` (the
/// spec provides no equivalent forward-compat note for infinity payloads).
fn classify_imapb_special(bytes: &[u8], y: u64) -> Option<ImapbSpecial> {
    let len = bytes.len();
    let top = bytes[0];
    // Table 3 (MISB Defined): full 8-bit discriminator, remaining bytes zero.
    if (top & 0b1111_1000) == 0b1110_0000 {
        let rest_zero = bytes[1..].iter().all(|&b| b == 0);
        return match (top, rest_zero) {
            (0xE0, true) => Some(ImapbSpecial::BelowMin),
            (0xE1, true) => Some(ImapbSpecial::AboveMax),
            _ => None, // 0xE2..=0xE7 reserved, or non-zero "Other bits"
        };
    }
    // Table 2: 5-bit family prefix (bn..bn-4) + (8L-5)-bit payload.
    let payload_bits = 8 * len - 5;
    let payload = if payload_bits >= 64 {
        y
    } else {
        y & ((1u64 << payload_bits) - 1)
    };
    match top >> 3 {
        0b11001 => (payload == 0).then_some(ImapbSpecial::PositiveInfinity),
        0b11101 => (payload == 0).then_some(ImapbSpecial::NegativeInfinity),
        0b11010 => Some(ImapbSpecial::PositiveQuietNaN { nan_id: payload }),
        0b11110 => Some(ImapbSpecial::NegativeQuietNaN { nan_id: payload }),
        0b11011 => Some(ImapbSpecial::PositiveSignalingNaN { signal: payload }),
        0b11111 => Some(ImapbSpecial::NegativeSignalingNaN { signal: payload }),
        0b11000 => Some(ImapbSpecial::UserDefined { signal: payload }),
        _ => None,
    }
}

/// MISB ST 1201.5 §7.2.3 Table 2 + §7.2.3.1 Table 3 special-value families.
///
/// IMAPB reserves the top two bits (`11`) of byte 0 to signal a special
/// value. Bit `bn-2` is the sign, `bn-3` selects NaN, `bn-4` selects
/// quiet/signaling, and the remaining `8L-5` low bits carry the NaN
/// identifier / signal payload (zero-filled for the standard families).
/// The MISB-defined overflow signals (`BelowMin`/`AboveMax`) use the full
/// 8-bit byte-0 discriminator of Table 3 with all subsequent bytes zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImapbSpecial {
    /// `+∞` — byte-0 `0xC8`.
    PositiveInfinity,
    /// `−∞` — byte-0 `0xE8`.
    NegativeInfinity,
    /// Positive quiet NaN — byte-0 `0xD0` (default `nan_id = 0`).
    PositiveQuietNaN { nan_id: u64 },
    /// Negative quiet NaN — byte-0 `0xF0`.
    NegativeQuietNaN { nan_id: u64 },
    /// Positive signaling NaN — byte-0 `0xD8` (`signal` in the low bits).
    PositiveSignalingNaN { signal: u64 },
    /// Negative signaling NaN — byte-0 `0xF8`.
    NegativeSignalingNaN { signal: u64 },
    /// `IMAP_BELOW_MINIMUM` overflow signal — byte-0 `0xE0` (Table 3).
    BelowMin,
    /// `IMAP_ABOVE_MAXIMUM` overflow signal — byte-0 `0xE1` (Table 3).
    AboveMax,
    /// User-defined special — byte-0 `0xC0` (`signal` in the low bits).
    UserDefined { signal: u64 },
}

/// Encode an ST 1201.5 §7.2.3 special value into `length`-byte big-endian
/// wire form: the family pattern in the top bits, the NaN-Id / signal
/// payload in the low `8L-5` bits, all remaining bits zero-filled.
///
/// # Errors
/// - [`KlvEncodeError::UnsupportedImapbLength`] when `length ∉ 1..=8`.
/// - [`KlvEncodeError::BufferTooSmall`] when `out.len() < length`.
/// - [`KlvEncodeError::OutOfRange`] when a NaN-Id / signal payload does not
///   fit the `8L-5` available bits for the chosen `length`.
pub fn encode_imapb_special(
    special: ImapbSpecial,
    length: usize,
    out: &mut [u8],
) -> Result<(), KlvEncodeError> {
    if !(1..=8).contains(&length) {
        return Err(KlvEncodeError::UnsupportedImapbLength { length });
    }
    if out.len() < length {
        return Err(KlvEncodeError::BufferTooSmall {
            needed: length,
            got: out.len(),
        });
    }
    let buf = &mut out[..length];
    buf.fill(0);
    // Table 3 (MISB Defined): full byte-0 discriminator, remaining bytes zero.
    let (prefix5, payload): (u8, u64) = match special {
        ImapbSpecial::BelowMin => {
            buf[0] = 0xE0;
            return Ok(());
        }
        ImapbSpecial::AboveMax => {
            buf[0] = 0xE1;
            return Ok(());
        }
        // Table 2: 5-bit family prefix (bn..bn-4) + (8L-5)-bit payload.
        ImapbSpecial::PositiveInfinity => (0b11001, 0),
        ImapbSpecial::NegativeInfinity => (0b11101, 0),
        ImapbSpecial::PositiveQuietNaN { nan_id } => (0b11010, nan_id),
        ImapbSpecial::NegativeQuietNaN { nan_id } => (0b11110, nan_id),
        ImapbSpecial::PositiveSignalingNaN { signal } => (0b11011, signal),
        ImapbSpecial::NegativeSignalingNaN { signal } => (0b11111, signal),
        ImapbSpecial::UserDefined { signal } => (0b11000, signal),
    };
    let payload_bits = 8 * length - 5;
    if payload_bits < 64 && payload >= (1u64 << payload_bits) {
        return Err(KlvEncodeError::OutOfRange {
            tag: 0,
            value: payload as f64,
            min: 0.0,
            max: ((1u64 << payload_bits) - 1) as f64,
            hint: None,
        });
    }
    let word = ((prefix5 as u64) << payload_bits) | payload;
    for (i, slot) in buf.iter_mut().enumerate() {
        *slot = (word >> (8 * (length - 1 - i))) as u8;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_2byte_unit_range() {
        let p = ImapbParams {
            min: -1.0,
            max: 1.0,
            length: 2,
        };
        for value in [-1.0, -0.5, 0.0, 0.5, 1.0] {
            let mut buf = [0u8; 2];
            encode_imapb(&p, value, &mut buf).unwrap();
            let back = decode_imapb(&p, &buf).unwrap().value().unwrap();
            assert!(
                (back - value).abs() < 1e-4,
                "round trip failed: input={value}, output={back}"
            );
        }
    }

    #[test]
    fn round_trip_4byte_signed_range() {
        let p = ImapbParams {
            min: -180.0,
            max: 180.0,
            length: 4,
        };
        for value in [-180.0, -90.0, 0.0, 45.5, 179.999] {
            let mut buf = [0u8; 4];
            encode_imapb(&p, value, &mut buf).unwrap();
            let back = decode_imapb(&p, &buf).unwrap().value().unwrap();
            assert!((back - value).abs() < 1e-6, "v={value}, back={back}");
        }
    }

    #[test]
    fn out_of_range_rejected() {
        let p = ImapbParams {
            min: -1.0,
            max: 1.0,
            length: 2,
        };
        let mut buf = [0u8; 2];
        let err = encode_imapb(&p, 2.0, &mut buf).unwrap_err();
        matches!(err, KlvEncodeError::OutOfRange { .. });
    }

    #[test]
    fn nan_rejected() {
        let p = ImapbParams {
            min: -1.0,
            max: 1.0,
            length: 2,
        };
        let mut buf = [0u8; 2];
        let err = encode_imapb(&p, f64::NAN, &mut buf).unwrap_err();
        matches!(err, KlvEncodeError::OutOfRange { .. });
    }

    #[test]
    fn buffer_too_small_rejected() {
        let p = ImapbParams {
            min: -1.0,
            max: 1.0,
            length: 4,
        };
        let mut buf = [0u8; 2];
        let err = encode_imapb(&p, 0.0, &mut buf).unwrap_err();
        matches!(err, KlvEncodeError::BufferTooSmall { .. });
    }

    #[test]
    fn decode_wrong_length_rejected() {
        let p = ImapbParams {
            min: -1.0,
            max: 1.0,
            length: 2,
        };
        let buf = [0u8; 3];
        let err = decode_imapb(&p, &buf).unwrap_err();
        matches!(err, KlvFieldError::InvalidLength { .. });
    }

    // --- Spec-vector regression tests (ST 1201.5 §10 + ST 0903.6 §10.1) ---

    #[test]
    fn st_0903_section_10_1_11_fov_12_5_deg() {
        // ST 0903.6 §10.1.11 worked example: IMAPB(0, 180, 2) for 12.5° → 0x0640.
        let p = ImapbParams {
            min: 0.0,
            max: 180.0,
            length: 2,
        };
        let mut buf = [0u8; 2];
        encode_imapb(&p, 12.5, &mut buf).unwrap();
        assert_eq!(
            buf,
            [0x06, 0x40],
            "spec says 0x0640, got {:#04X}{:02X}",
            buf[0],
            buf[1]
        );
        let back = decode_imapb(&p, &buf).unwrap().value().unwrap();
        assert!((back - 12.5).abs() < 1e-2, "decoded {back}, expected 12.5");
    }

    #[test]
    fn st_0903_section_10_1_12_fov_10_0_deg() {
        // ST 0903.6 §10.1.12 worked example: IMAPB(0, 180, 2) for 10.0° → 0x0500.
        let p = ImapbParams {
            min: 0.0,
            max: 180.0,
            length: 2,
        };
        let mut buf = [0u8; 2];
        encode_imapb(&p, 10.0, &mut buf).unwrap();
        assert_eq!(buf, [0x05, 0x00]);
    }

    #[test]
    fn st_0903_section_10_1_11_fov_90_0_deg() {
        // Mid-range cross-check: IMAPB(0, 180, 2) for 90.0° → 0x2D00 (= 128 * 90 = 11520).
        // Pre-fix code emits 0xAD00 (MSB flipped by signed-midpoint shift).
        let p = ImapbParams {
            min: 0.0,
            max: 180.0,
            length: 2,
        };
        let mut buf = [0u8; 2];
        encode_imapb(&p, 90.0, &mut buf).unwrap();
        assert_eq!(buf, [0x2D, 0x00]);
    }

    #[test]
    fn st_1201_5_appendix_a_test_2_unsigned_be() {
        // ST 1201.5 Appendix A Test 2: IMAPB(0.0, 100.0, 3) value 100 → 0x640000.
        let p = ImapbParams {
            min: 0.0,
            max: 100.0,
            length: 3,
        };
        let mut buf = [0u8; 3];
        encode_imapb(&p, 100.0, &mut buf).unwrap();
        assert_eq!(
            buf,
            [0x64, 0x00, 0x00],
            "spec mandates unsigned BE; pre-fix code emits 0xE40000"
        );
    }

    #[test]
    fn st_1201_5_appendix_a_test_3_zero_mapping() {
        // ST 1201.5 Appendix A Test 3: IMAPB(-9.9, 110.0, 3) value 0.0 → 0x09E667
        // (the Zero mapping case — requires Zoffset = sF*a - floor(sF*a)).
        let p = ImapbParams {
            min: -9.9,
            max: 110.0,
            length: 3,
        };
        let mut buf = [0u8; 3];
        encode_imapb(&p, 0.0, &mut buf).unwrap();
        assert_eq!(
            buf,
            [0x09, 0xE6, 0x67],
            "Zoffset rule unimplemented; pre-fix code emits 0x89E666"
        );
        let back = decode_imapb(&p, &buf).unwrap().value().unwrap();
        assert!(
            back.abs() < 1e-4,
            "Zero mapping must round-trip to 0.0, got {back}"
        );
    }

    #[test]
    fn length_8_round_trip() {
        // ST 1201.5 allows any L; pre-fix code rejects L=8 due to i64 overflow.
        let p = ImapbParams {
            min: -1000.0,
            max: 1000.0,
            length: 8,
        };
        let mut buf = [0u8; 8];
        encode_imapb(&p, 123.456, &mut buf).unwrap();
        let back = decode_imapb(&p, &buf).unwrap().value().unwrap();
        assert!(
            (back - 123.456).abs() < 1e-9,
            "L=8 round-trip failed: {back}"
        );
    }

    // --- ST 1201.5 §7.2.2 step 1 special-value detection + bounds ---

    #[test]
    fn imapb_decode_positive_infinity_wire_pattern_returns_special_variant() {
        // ST 1201.5 §7.2.3 Table 2: byte 0 = 0xC8, zero-filled → +∞.
        let p = ImapbParams {
            min: 0.0,
            max: 100.0,
            length: 3,
        };
        let decoded = decode_imapb(&p, &[0xC8, 0x00, 0x00]).unwrap();
        assert_eq!(
            decoded,
            DecodedImapb::Special(ImapbSpecial::PositiveInfinity)
        );
        assert_eq!(decoded.value(), None);
    }

    #[test]
    fn imapb_decode_negative_infinity_wire_pattern_returns_special_variant() {
        // ST 1201.5 §7.2.3 Table 2: byte 0 = 0xE8, zero-filled → -∞.
        let p = ImapbParams {
            min: -100.0,
            max: 100.0,
            length: 3,
        };
        let decoded = decode_imapb(&p, &[0xE8, 0x00, 0x00]).unwrap();
        assert_eq!(
            decoded,
            DecodedImapb::Special(ImapbSpecial::NegativeInfinity)
        );
    }

    #[test]
    fn imapb_decode_nan_wire_pattern_returns_special_variant() {
        // ST 1201.5 §7.2.3 Table 2: byte 0 = 0xD0 → positive quiet NaN (id=0).
        let p = ImapbParams {
            min: 0.0,
            max: 100.0,
            length: 3,
        };
        let decoded = decode_imapb(&p, &[0xD0, 0x00, 0x00]).unwrap();
        assert_eq!(
            decoded,
            DecodedImapb::Special(ImapbSpecial::PositiveQuietNaN { nan_id: 0 })
        );
    }

    #[test]
    fn imapb_decode_below_min_wire_pattern_returns_special_variant() {
        // ST 1201.5 §7.2.3 Table 3: byte 0 = 0xE0 → IMAP_BELOW_MINIMUM.
        let p = ImapbParams {
            min: 0.0,
            max: 100.0,
            length: 3,
        };
        let decoded = decode_imapb(&p, &[0xE0, 0x00, 0x00]).unwrap();
        assert_eq!(decoded, DecodedImapb::Special(ImapbSpecial::BelowMin));
    }

    #[test]
    fn imapb_decode_above_max_wire_pattern_returns_special_variant() {
        // ST 1201.5 §7.2.3 Table 3: byte 0 = 0xE1 → IMAP_ABOVE_MAXIMUM.
        let p = ImapbParams {
            min: 0.0,
            max: 100.0,
            length: 3,
        };
        let decoded = decode_imapb(&p, &[0xE1, 0x00, 0x00]).unwrap();
        assert_eq!(decoded, DecodedImapb::Special(ImapbSpecial::AboveMax));
    }

    #[test]
    fn imapb_decode_unrecognized_special_pattern_returns_reserved_variant() {
        // Top 2 bits set (0b11xx_xxxx) but not one of the 5 named patterns
        // → ReservedSpecial carrying the raw integer. 0xCC = 0b1100_1100.
        let p = ImapbParams {
            min: 0.0,
            max: 100.0,
            length: 3,
        };
        let decoded = decode_imapb(&p, &[0xCC, 0x12, 0x34]).unwrap();
        assert!(
            matches!(decoded, DecodedImapb::ReservedSpecial { raw } if raw == 0x00CC_1234),
            "got {decoded:?}"
        );
    }

    #[test]
    fn imapb_decode_out_of_range_returns_typed_variant() {
        // IMAPB(0, 100, 3): top 2 bits NOT both set (0b10xx_xxxx is reserved
        // inter-band per §8.6 Eq.12), arithmetic decodes to ~128.0, past
        // max=100.0. Per H-02, surface as OutOfRange carrying the decode.
        let p = ImapbParams {
            min: 0.0,
            max: 100.0,
            length: 3,
        };
        let decoded = decode_imapb(&p, &[0x80, 0x00, 0x00]).unwrap();
        match decoded {
            DecodedImapb::OutOfRange { decoded } => {
                assert!(
                    (decoded - 128.0).abs() < 1e-6,
                    "expected ~128.0, got {decoded}"
                );
            }
            other => panic!("expected OutOfRange, got {other:?}"),
        }
        assert_eq!(decoded.value(), None);
    }

    #[test]
    fn imapb_decode_max_value_pattern_within_range() {
        // ST 1201.5 §7.2.3 Table 1 row 2: integer floor(sF·(b-a)) is the
        // legitimate max-value mapping even with MSB=1. For IMAPB(0,100,3)
        // that integer is 0x640000, which arithmetic-decodes to exactly
        // 100.0 — must NOT be flagged as OutOfRange.
        let p = ImapbParams {
            min: 0.0,
            max: 100.0,
            length: 3,
        };
        let decoded = decode_imapb(&p, &[0x64, 0x00, 0x00]).unwrap();
        match decoded {
            DecodedImapb::Value(v) => {
                assert!((v - 100.0).abs() < 1e-9, "expected 100.0, got {v}");
            }
            other => panic!("expected Value(100.0), got {other:?}"),
        }
    }

    #[test]
    fn imapb_decode_min_value_within_range() {
        // Integer 0 must decode to exactly `min` (the §7.1.2 Starting Point
        // B reference). For IMAPB(0, 100, 3), wire 0x000000 → 0.0.
        let p = ImapbParams {
            min: 0.0,
            max: 100.0,
            length: 3,
        };
        let decoded = decode_imapb(&p, &[0x00, 0x00, 0x00]).unwrap();
        match decoded {
            DecodedImapb::Value(v) => assert!(v.abs() < 1e-9, "expected 0.0, got {v}"),
            other => panic!("expected Value(0.0), got {other:?}"),
        }
    }

    // --- ST 1201.5 §6 IMAPB precondition guards ---
    //
    // These tests pin the new `min < max` precondition on both the encode
    // and decode entry points, plus the evaluation-order contract between
    // the existing `UnsupportedImapbLength` check and the new
    // `InvalidImapbParams` check.

    #[test]
    fn encode_rejects_min_eq_max() {
        // ST 1201.5 §6 requires min < max. Degenerate ranges blow up §8.9's
        // `bPow = ceil(log2(max − min))` derivation (log2(0) = −∞).
        let p = ImapbParams {
            min: 5.0,
            max: 5.0,
            length: 2,
        };
        let mut buf = [0u8; 2];
        let err = encode_imapb(&p, 5.0, &mut buf).unwrap_err();
        assert!(
            matches!(
                err,
                KlvEncodeError::InvalidImapbParams {
                    min: 5.0,
                    max: 5.0,
                    length: 2,
                }
            ),
            "expected InvalidImapbParams, got {err:?}"
        );
    }

    #[test]
    fn encode_rejects_min_gt_max() {
        let p = ImapbParams {
            min: 10.0,
            max: -10.0,
            length: 4,
        };
        let mut buf = [0u8; 4];
        let err = encode_imapb(&p, 0.0, &mut buf).unwrap_err();
        assert!(
            matches!(
                err,
                KlvEncodeError::InvalidImapbParams {
                    min: 10.0,
                    max: -10.0,
                    length: 4,
                }
            ),
            "expected InvalidImapbParams, got {err:?}"
        );
    }

    #[test]
    fn encode_rejects_length_out_of_range() {
        // L=0 and L=9 both surface as the narrower UnsupportedImapbLength,
        // NOT InvalidImapbParams — the length check fires first.
        let p0 = ImapbParams {
            min: 0.0,
            max: 1.0,
            length: 0,
        };
        let mut buf = [0u8; 4];
        let err = encode_imapb(&p0, 0.0, &mut buf).unwrap_err();
        assert!(
            matches!(err, KlvEncodeError::UnsupportedImapbLength { length: 0 }),
            "L=0: expected UnsupportedImapbLength, got {err:?}"
        );

        let p9 = ImapbParams {
            min: 0.0,
            max: 1.0,
            length: 9,
        };
        let mut buf = [0u8; 16];
        let err = encode_imapb(&p9, 0.0, &mut buf).unwrap_err();
        assert!(
            matches!(err, KlvEncodeError::UnsupportedImapbLength { length: 9 }),
            "L=9: expected UnsupportedImapbLength, got {err:?}"
        );
    }

    #[test]
    fn decode_rejects_min_eq_max() {
        let p = ImapbParams {
            min: -1.0,
            max: -1.0,
            length: 2,
        };
        let err = decode_imapb(&p, &[0x00, 0x00]).unwrap_err();
        assert!(
            matches!(
                err,
                KlvFieldError::InvalidImapbParams {
                    min: -1.0,
                    max: -1.0,
                    length: 2,
                }
            ),
            "expected InvalidImapbParams, got {err:?}"
        );
    }

    #[test]
    fn decode_rejects_min_gt_max() {
        let p = ImapbParams {
            min: 100.0,
            max: 0.0,
            length: 3,
        };
        let err = decode_imapb(&p, &[0x00, 0x00, 0x00]).unwrap_err();
        assert!(
            matches!(
                err,
                KlvFieldError::InvalidImapbParams {
                    min: 100.0,
                    max: 0.0,
                    length: 3,
                }
            ),
            "expected InvalidImapbParams, got {err:?}"
        );
    }

    #[test]
    fn decode_length_zero_or_nine_still_caught_by_existing_check() {
        // Evaluation-order contract: when BOTH the L gate AND the min<max
        // gate would trigger, the existing UnsupportedImapbLength diagnostic
        // wins. This keeps pre-existing diagnostics narrow and stable.
        let p0 = ImapbParams {
            min: 5.0,
            max: 5.0, // would also trigger InvalidImapbParams
            length: 0,
        };
        let err = decode_imapb(&p0, &[]).unwrap_err();
        assert!(
            matches!(err, KlvFieldError::UnsupportedImapbLength { length: 0 }),
            "L=0+badrange: expected UnsupportedImapbLength to fire first, got {err:?}"
        );

        let p9 = ImapbParams {
            min: 10.0,
            max: -10.0, // would also trigger InvalidImapbParams
            length: 9,
        };
        let err = decode_imapb(&p9, &[0u8; 9]).unwrap_err();
        assert!(
            matches!(err, KlvFieldError::UnsupportedImapbLength { length: 9 }),
            "L=9+badrange: expected UnsupportedImapbLength to fire first, got {err:?}"
        );
    }

    // --- encode_imapb_special tests ---

    #[test]
    fn encode_special_positive_infinity_zero_filled() {
        let mut out = [0xFFu8; 3];
        encode_imapb_special(ImapbSpecial::PositiveInfinity, 3, &mut out).unwrap();
        assert_eq!(out, [0xC8, 0x00, 0x00]);
    }

    #[test]
    fn encode_special_below_above_min_max() {
        let mut out = [0xFFu8; 2];
        encode_imapb_special(ImapbSpecial::BelowMin, 2, &mut out).unwrap();
        assert_eq!(out, [0xE0, 0x00]);
        encode_imapb_special(ImapbSpecial::AboveMax, 2, &mut out).unwrap();
        assert_eq!(out, [0xE1, 0x00]);
    }

    #[test]
    fn encode_special_negative_signaling_nan_carries_payload() {
        // L=2 → payload occupies 8*2-5 = 11 bits; prefix 0b11111 in the top 5.
        let mut out = [0u8; 2];
        encode_imapb_special(
            ImapbSpecial::NegativeSignalingNaN { signal: 0x2A },
            2,
            &mut out,
        )
        .unwrap();
        // word = (0b11111 << 11) | 0x2A = 0xF82A.
        assert_eq!(out, [0xF8, 0x2A]);
    }

    #[test]
    fn encode_special_payload_too_large_is_rejected() {
        // L=1 → only 3 payload bits; nan_id 0x10 doesn't fit.
        let mut out = [0u8; 1];
        let err =
            encode_imapb_special(ImapbSpecial::PositiveQuietNaN { nan_id: 0x10 }, 1, &mut out);
        assert!(
            matches!(err, Err(KlvEncodeError::OutOfRange { .. })),
            "got {err:?}"
        );
    }

    #[test]
    fn encode_special_bad_length_and_buffer() {
        let mut out = [0u8; 4];
        assert!(matches!(
            encode_imapb_special(ImapbSpecial::PositiveInfinity, 9, &mut out),
            Err(KlvEncodeError::UnsupportedImapbLength { length: 9 })
        ));
        let mut small = [0u8; 1];
        assert!(matches!(
            encode_imapb_special(ImapbSpecial::PositiveInfinity, 2, &mut small),
            Err(KlvEncodeError::BufferTooSmall { needed: 2, got: 1 })
        ));
    }

    // --- decode full IMAPB special-value families ---

    #[test]
    fn decode_recognizes_all_nan_families() {
        let p = ImapbParams {
            min: 0.0,
            max: 1.0,
            length: 3,
        };
        assert_eq!(
            decode_imapb(&p, &[0xD0, 0, 0]).unwrap(),
            DecodedImapb::Special(ImapbSpecial::PositiveQuietNaN { nan_id: 0 })
        );
        assert_eq!(
            decode_imapb(&p, &[0xF0, 0, 0]).unwrap(),
            DecodedImapb::Special(ImapbSpecial::NegativeQuietNaN { nan_id: 0 })
        );
        assert_eq!(
            decode_imapb(&p, &[0xD8, 0, 0]).unwrap(),
            DecodedImapb::Special(ImapbSpecial::PositiveSignalingNaN { signal: 0 })
        );
        assert_eq!(
            decode_imapb(&p, &[0xF8, 0, 0]).unwrap(),
            DecodedImapb::Special(ImapbSpecial::NegativeSignalingNaN { signal: 0 })
        );
    }

    #[test]
    fn decode_special_rejects_non_zero_fill() {
        // §7.2.3: +∞ must be zero-filled in the remaining bits.
        let p = ImapbParams {
            min: 0.0,
            max: 1.0,
            length: 3,
        };
        assert_eq!(
            decode_imapb(&p, &[0xC8, 0xFF, 0xFF]).unwrap(),
            DecodedImapb::ReservedSpecial { raw: 0x00C8_FFFF }
        );
    }

    // --- exact reserved-space detection ---

    /// IMAPB(0,180,1): sF=0.5, y_max=90. The integer y=91 (top bits `01`,
    /// NOT in the §7.2.3 special-value space) arithmetic-decodes to 182.0,
    /// which is past max. The old float-epsilon code admitted it as
    /// Value(182.0) because the tolerance was exactly scale=2.0 and
    /// `182.0 > 180.0 + 2.0` is FALSE (equal). After the fix, the
    /// integer-domain check `y > y_max` (91 > 90) correctly returns
    /// OutOfRange.
    ///
    /// This test is RED on the unmodified code (Value) and GREEN after
    /// the fix (OutOfRange).
    #[test]
    fn imapb_l1_reserved_integer_above_ymax_is_out_of_range() {
        let p = ImapbParams {
            min: 0.0,
            max: 180.0,
            length: 1,
        };
        // y_max = floor(sF*(max-min)+Zoffset) = floor(0.5*180+0) = 90 = 0x5A.
        // y = 91 = 0x5B: the first reserved integer above y_max.
        let decoded = decode_imapb(&p, &[0x5B]).unwrap();
        match decoded {
            DecodedImapb::OutOfRange { decoded } => {
                // arithmetic decode of 91 via sR=2.0: 2.0*91+0 = 182.0
                assert!(
                    (decoded - 182.0).abs() < 1e-9,
                    "expected 182.0 in OutOfRange, got {decoded}"
                );
            }
            other => panic!("expected OutOfRange for y=91 (above y_max=90), got {other:?}"),
        }
    }

    /// Guard against off-by-one in y_max: encode max=180.0 at L=1, decode
    /// it, and assert Value (not OutOfRange). y_max must equal the encoder's
    /// max wire integer exactly, or a legitimately-encoded max would
    /// misclassify.
    #[test]
    fn imapb_l1_max_value_encodes_to_ymax_and_decodes_as_value() {
        let p = ImapbParams {
            min: 0.0,
            max: 180.0,
            length: 1,
        };
        let mut buf = [0u8; 1];
        encode_imapb(&p, 180.0, &mut buf).unwrap();
        // Encoder: y = floor(0.5*(180-0)+0) = floor(90) = 90 = 0x5A.
        assert_eq!(buf, [0x5A], "encoder must produce y_max=90=0x5A for max");
        let decoded = decode_imapb(&p, &buf).unwrap();
        match decoded {
            DecodedImapb::Value(v) => {
                assert!(
                    (v - 180.0).abs() < 1e-6,
                    "max must decode to ~180.0, got {v}"
                );
            }
            other => panic!("max wire integer must decode as Value, got {other:?}"),
        }
    }

    /// Pinning test: IMAPB(0,180,2) — the ST 0903 FOV shape (L=2, 65536 integers).
    ///
    /// Exhaustively iterates ALL 65536 two-byte wire integers. For each
    /// normal-pattern integer (top two bits != `11`) that is above y_max,
    /// asserts it decodes as `OutOfRange` (not `Value`).
    ///
    /// Pre-fix counts (computed analytically, unmodified code):
    /// - y=23041 (= y_max+1) was admitted as `Value(180.0078125)` because the
    ///   float tolerance `epsilon = 1/128 = 0.0078125` made `180.0078125 >
    ///   180.0078125` evaluate to FALSE (not strictly greater). All 26110 other
    ///   reserved integers (23042..=49151) were already `OutOfRange`.
    ///
    /// Post-fix: all 26111 reserved integers are `OutOfRange` (0 `Value`).
    #[test]
    fn imapb_l2_fov_all_reserved_integers_are_out_of_range() {
        let p = ImapbParams {
            min: 0.0,
            max: 180.0,
            length: 2,
        };
        // y_max = floor(128.0 * 180.0 + 0.0) = 23040.
        let y_max: u64 = 23040;
        let mut value_count = 0u32;
        let mut out_of_range_count = 0u32;
        for y in 0u64..=0xFFFFu64 {
            // Skip special-value space (top two bits `11`).
            if (y >> 14) == 0b11 {
                continue;
            }
            if y <= y_max {
                continue; // valid range — not reserved
            }
            let bytes = [(y >> 8) as u8, (y & 0xFF) as u8];
            match decode_imapb(&p, &bytes).unwrap() {
                DecodedImapb::Value(_) => value_count += 1,
                DecodedImapb::OutOfRange { .. } => out_of_range_count += 1,
                other => panic!("unexpected variant for y={y}: {other:?}"),
            }
        }
        assert_eq!(
            value_count, 0,
            "expected 0 reserved integers as Value, got {value_count}"
        );
        // 49151 (= 0xBFFF, last normal-pattern 16-bit integer) - 23040 (y_max) = 26111.
        assert_eq!(
            out_of_range_count, 26111,
            "expected 26111 reserved integers as OutOfRange, got {out_of_range_count}"
        );
    }

    /// Pinning test: IMAPB(-900,19000,2) — the ST 0903 targetHae shape (L=2).
    ///
    /// Same structure as the FOV test. sF=1.0, y_max=19900.
    ///
    /// Pre-fix: y=19901 was `Value(19001.0)` (epsilon=1.0 so 19001.0 > 19001.0
    /// was FALSE); 29250 other reserved integers were `OutOfRange`.
    /// Post-fix: all 29251 are `OutOfRange`.
    #[test]
    fn imapb_l2_target_hae_all_reserved_integers_are_out_of_range() {
        let p = ImapbParams {
            min: -900.0,
            max: 19000.0,
            length: 2,
        };
        // sF=1.0, Zoffset=0 (min<0, max>0 but sF*min=-900 is an integer so frac=0).
        // y_max = floor(1.0 * 19900 + 0) = 19900.
        let y_max: u64 = 19900;
        let mut value_count = 0u32;
        let mut out_of_range_count = 0u32;
        for y in 0u64..=0xFFFFu64 {
            if (y >> 14) == 0b11 {
                continue;
            }
            if y <= y_max {
                continue;
            }
            let bytes = [(y >> 8) as u8, (y & 0xFF) as u8];
            match decode_imapb(&p, &bytes).unwrap() {
                DecodedImapb::Value(_) => value_count += 1,
                DecodedImapb::OutOfRange { .. } => out_of_range_count += 1,
                other => panic!("unexpected variant for y={y}: {other:?}"),
            }
        }
        assert_eq!(
            value_count, 0,
            "expected 0 reserved integers as Value, got {value_count}"
        );
        // 49151 - 19900 = 29251.
        assert_eq!(
            out_of_range_count, 29251,
            "expected 29251 reserved integers as OutOfRange, got {out_of_range_count}"
        );
    }

    /// Pinning test: IMAPB(-19.2,19.2,3) — the ST 0903 offsets shape (L=3).
    ///
    /// Exhaustive at L=3 is 2^24 ≈ 16.7M iterations (~4s per core). This
    /// test uses a boundary-band sample around y_max ± 4096 instead, which
    /// covers the critical transition and avoids long test runtimes.
    ///
    /// sF=131072, Zoffset=0.6, y_max=5033165.
    ///
    /// Pre-fix: y=5033166 was `Value` (decoded 19.2+4.58e-6, epsilon=7.63e-6
    /// so `19.2+4.58e-6 > 19.2+7.63e-6` was FALSE); 4095 band integers
    /// (5033167..5037261) were `OutOfRange`.
    /// Post-fix: all 4096 band reserved integers are `OutOfRange`.
    #[test]
    fn imapb_l3_offsets_boundary_band_reserved_integers_are_out_of_range() {
        let p = ImapbParams {
            min: -19.2,
            max: 19.2,
            length: 3,
        };
        // sF=131072, Zoffset=0.6, y_max=floor(131072*38.4+0.6)=floor(5033165.4)=5033165.
        let y_max: u64 = 5033165;
        let band_start = y_max.saturating_sub(4096);
        let band_end = y_max + 4096;
        let mut value_count = 0u32;
        let mut out_of_range_count = 0u32;
        for y in band_start..=band_end {
            // Top two bits of 24-bit integer: bits 23-22.
            if (y >> 22) == 0b11 {
                continue;
            }
            if y <= y_max {
                continue;
            }
            let bytes = [
                ((y >> 16) & 0xFF) as u8,
                ((y >> 8) & 0xFF) as u8,
                (y & 0xFF) as u8,
            ];
            match decode_imapb(&p, &bytes).unwrap() {
                DecodedImapb::Value(_) => value_count += 1,
                DecodedImapb::OutOfRange { .. } => out_of_range_count += 1,
                other => panic!("unexpected variant for y={y}: {other:?}"),
            }
        }
        assert_eq!(
            value_count, 0,
            "expected 0 reserved integers in band as Value, got {value_count}"
        );
        // band reserved integers: y_max+1 .. y_max+4096 = 4096 integers.
        assert_eq!(
            out_of_range_count, 4096,
            "expected 4096 reserved integers in band as OutOfRange, got {out_of_range_count}"
        );
    }

    /// In-range decode guard for the three shipped ST 0903 IMAPB shapes.
    ///
    /// Each pinning sweep skips `y <= y_max`. This test fills that gap: it
    /// encodes `max` (the boundary value mapping to `y_max`) and one
    /// mid-range value for each shape and asserts both decode as `Value`.
    #[test]
    fn imapb_shipped_shapes_in_range_values_decode_as_value() {
        // FOV: IMAPB(0, 180, 2) — sF=128, sR=1/128≈0.0078
        {
            let p = ImapbParams {
                min: 0.0,
                max: 180.0,
                length: 2,
            };
            let tol = 0.01;
            for &v in &[90.0_f64, 180.0] {
                let mut buf = [0u8; 2];
                encode_imapb(&p, v, &mut buf).unwrap();
                match decode_imapb(&p, &buf).unwrap() {
                    DecodedImapb::Value(d) => assert!(
                        (d - v).abs() <= tol,
                        "FOV: encode({v}) round-trip: expected ~{v}, got {d}"
                    ),
                    other => panic!("FOV: expected Value for {v}, got {other:?}"),
                }
            }
        }
        // Offsets: IMAPB(-19.2, 19.2, 3) — sF=131072, sR≈7.6e-6
        {
            let p = ImapbParams {
                min: -19.2,
                max: 19.2,
                length: 3,
            };
            let tol = 1e-4;
            for &v in &[0.0_f64, 19.2] {
                let mut buf = [0u8; 3];
                encode_imapb(&p, v, &mut buf).unwrap();
                match decode_imapb(&p, &buf).unwrap() {
                    DecodedImapb::Value(d) => assert!(
                        (d - v).abs() <= tol,
                        "offsets: encode({v}) round-trip: expected ~{v}, got {d}"
                    ),
                    other => panic!("offsets: expected Value for {v}, got {other:?}"),
                }
            }
        }
        // TargetHae: IMAPB(-900, 19000, 2) — sF=1, sR=1.0
        {
            let p = ImapbParams {
                min: -900.0,
                max: 19000.0,
                length: 2,
            };
            let tol = 1.1;
            for &v in &[9000.0_f64, 19000.0] {
                let mut buf = [0u8; 2];
                encode_imapb(&p, v, &mut buf).unwrap();
                match decode_imapb(&p, &buf).unwrap() {
                    DecodedImapb::Value(d) => assert!(
                        (d - v).abs() <= tol,
                        "targetHae: encode({v}) round-trip: expected ~{v}, got {d}"
                    ),
                    other => panic!("targetHae: expected Value for {v}, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn special_value_round_trips_through_encode_decode() {
        let p = ImapbParams {
            min: 0.0,
            max: 1.0,
            length: 4,
        };
        for s in [
            ImapbSpecial::PositiveInfinity,
            ImapbSpecial::NegativeInfinity,
            ImapbSpecial::BelowMin,
            ImapbSpecial::AboveMax,
            ImapbSpecial::PositiveQuietNaN { nan_id: 0x123 },
            ImapbSpecial::NegativeSignalingNaN { signal: 0x456 },
            ImapbSpecial::UserDefined { signal: 0x7 },
        ] {
            let mut buf = [0u8; 4];
            encode_imapb_special(s, 4, &mut buf).unwrap();
            assert_eq!(
                decode_imapb(&p, &buf).unwrap(),
                DecodedImapb::Special(s),
                "round-trip {s:?}"
            );
        }
    }

    /// Fuzz-found regression: degenerate range (min ≈ −f64::MAX,
    /// max = tiny subnormal, L=4) produced an enormous sR = 1/sF, causing
    /// `value = sR*(y−Zoffset)+min` to overflow to −∞ when y=0.
    ///
    /// Mechanism: Zoffset = frac(sF·min) ≈ 0.674. For y=0:
    ///   value = sR*(0 − 0.674) + min = −8.37e298·0.674 + (−1.798e308)
    ///         = −5.64e298 − 1.798e308 → overflows to −∞.
    /// The lower-bound epsilon guard `value < p.min − epsilon` fails because
    /// epsilon = sR = 8.37e298, making p.min − epsilon = −inf, and
    /// `−∞ < −∞` is FALSE. So the pre-fix code returns Value(−∞), a
    /// non-finite Value in violation of the decode contract.
    ///
    /// The fix: guard `!value.is_finite()` immediately after computing
    /// `value`, returning `OutOfRange { decoded: value }`.
    ///
    /// Invariant: `decode_imapb` MUST NOT return `Value(x)` for non-finite x.
    ///
    /// Byte layout of the minimized libFuzzer artifact:
    ///   data[0]=0x2B → length=(43%8)+1=4; data[1..9]=min; data[9..17]=max;
    ///   the crash path in the fuzz target encodes v_raw (payload[0..8]) and
    ///   decodes it: encode gives y=0, decode(y=0) → −∞.
    #[test]
    fn decode_imapb_never_returns_nonfinite_value() {
        // Exact parameters from the minimized fuzz artifact.
        let min = f64::from_bits(u64::from_le_bytes([
            0xf3, 0xde, 0xd4, 0xff, 0xff, 0xff, 0xef, 0xff,
        ]));
        let max = f64::from_bits(u64::from_le_bytes([
            0x7e, 0x12, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]));
        // Sanity: artifact params must satisfy the ST 1201.5 §6 precondition.
        assert!(min.is_finite() && max.is_finite() && min < max);

        let p = ImapbParams {
            min,
            max,
            length: 4,
        };

        // y=0 wire bytes: the crash path. sR*(0 − Zoffset) + min overflows
        // to −∞; with the fix, this must return OutOfRange, not Value.
        let result_y0 = decode_imapb(&p, &[0x00, 0x00, 0x00, 0x00])
            .expect("decode_imapb must not error for valid params");
        match result_y0 {
            DecodedImapb::Value(v) => panic!(
                "decode(y=0) returned Value({v}) but value must be non-finite \
                 for this degenerate range; pre-fix code returned Value(−inf), \
                 post-fix must return OutOfRange"
            ),
            DecodedImapb::OutOfRange { decoded } => {
                assert!(
                    !decoded.is_finite(),
                    "OutOfRange.decoded should be −∞ for this overflow case, got {decoded}"
                );
            }
            other => panic!("unexpected variant for y=0: {other:?}"),
        }

        // Belt-and-suspenders: no Value variant from any normal-pattern
        // 4-byte wire integer may carry a non-finite float with these params.
        // Normal-pattern means top two bits NOT both set (0b00..=0b10xxxxxx).
        // We sweep a broad sample (all combinations of hi ∈ 0x00..=0x3F and
        // a few lo values) to cover both the overflow zone (small y) and the
        // in-range zone.
        for hi in 0x00u8..=0x3Fu8 {
            for lo in [0x00u8, 0x80u8, 0xFFu8] {
                let w = [0x00u8, 0x00, hi, lo];
                if let DecodedImapb::Value(v) = decode_imapb(&p, &w).unwrap() {
                    assert!(
                        v.is_finite(),
                        "Value({v}) is non-finite for wire {w:?} with degenerate params \
                         min={min:.6e}, max={max:.6e}"
                    );
                }
            }
        }
    }
}
