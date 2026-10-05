//! Fixed-range linear int↔float helpers used by ST 0601 typed tags, plus the
//! `St0601SentinelMeaning` enum and `st0601_sentinel_meaning` lookup.
//!
//! Two flavors per `LinearRange`:
//! - Signed: integer in `[INT_MIN+1, INT_MAX]`; `INT_MIN` is a spec-defined
//!   sentinel whose meaning varies by tag (see [`st0601_sentinel_meaning`]).
//! - Unsigned: integer in `[0, UINT_MAX]`, no sentinel.

use crate::error::{KlvEncodeError, KlvFieldError};
#[cfg(not(feature = "std"))]
use crate::float_ext::FloatExt;
use crate::klv::st0601::tags::LinearRange;

use super::model::OutOfRangePolicy;

/// Spec-defined meaning of the INT_MIN sentinel wire value for a given
/// ST 0601 signed-mapping tag. Derived from ST 0601.19 per-tag "Special
/// Values" table entries; not all signed tags reserve INT_MIN — only the
/// tags listed in [`st0601_sentinel_meaning`] do.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum St0601SentinelMeaning {
    /// INT_MIN signals that the value exceeded the mapped range.
    ///
    /// Tags: 6 (Platform Pitch — ST 0601.19 §8.6),
    /// 7 (Platform Roll — §8.7),
    /// 50 (Platform Angle of Attack — §8.50),
    /// 51 (Platform Vertical Speed — §8.51),
    /// 52 (Platform Sideslip Angle — §8.52),
    /// 79 (Sensor North Velocity — §8.79),
    /// 80 (Sensor East Velocity — §8.80),
    /// 90 (Platform Pitch Full — §8.90),
    /// 91 (Platform Roll Full — §8.91),
    /// 92 (Platform Angle of Attack Full — §8.92),
    /// 93 (Platform Sideslip Angle Full — §8.93).
    OutOfRange,

    /// INT_MIN is explicitly reserved with no assigned meaning.
    ///
    /// Tags: 13 (Sensor Latitude — ST 0601.19 §8.13),
    /// 14 (Sensor Longitude — §8.14),
    /// 19 (Sensor Relative Elevation — §8.19),
    /// 67 (Alternate Platform Latitude — §8.67),
    /// 68 (Alternate Platform Longitude — §8.68).
    Reserved,

    /// INT_MIN signals that the value is not available, typically because
    /// the sensor is pointing off-Earth and no ground intersection exists.
    ///
    /// Tags: 23 (Frame Center Latitude — ST 0601.19 §8.23),
    /// 24 (Frame Center Longitude — §8.24),
    /// 26–33 (Offset Corner Lat/Lon — §8.26–8.33),
    /// 40 (Target Location Latitude — §8.40),
    /// 41 (Target Location Longitude — §8.41),
    /// 82–89 (Full Corner Lat/Lon — §8.82–8.89).
    NotAvailable,
}

/// Return the spec-defined meaning of the INT_MIN sentinel for `tag`, or
/// `None` if the spec defines no INT_MIN special value for that tag.
///
/// This is the **complete** ST 0601.19 INT_MIN special-value assignment
/// table, derived from the "Special Values" column in every per-tag
/// summary table in the document. `None` means the spec does not assign
/// any meaning to INT_MIN for that tag; it does **not** mean the tag is
/// unsigned or that INT_MIN is valid for that tag.
///
/// Note: `decode_fixed_range` treats INT_MIN as a sentinel for every
/// signed range tag regardless of this table — it returns `Ok(None)` and
/// the caller records the tag in `sentinel_tags`. This function is a
/// pure reference lookup; the decoder never calls it during decode.
///
/// # Spec quotes (ST 0601.19, 02 March 2023)
///
/// | Tag | Special Values column |
/// |-----|----------------------|
/// | 6   | `0x8000 = "Out of Range" indicator` |
/// | 7   | `0x8000 = "Out of Range" indicator` |
/// | 13  | `0x80000000 = "Reserved"` |
/// | 14  | `0x80000000 = "Reserved"` |
/// | 19  | `0x80000000 = "Reserved"` |
/// | 23  | `0x80000000 = "N/A (Off-Earth)" indicator` |
/// | 24  | `0x80000000 = "N/A (Off-Earth)" indicator` |
/// | 26–33 | `0x8000 = "N/A (Off-Earth)" indicator` |
/// | 40  | `0x80000000 = "N/A (Off-Earth)" indicator` |
/// | 41  | `0x80000000 = "N/A (Off-Earth)" indicator` |
/// | 50  | `0x8000 = "Out of Range" indicator` |
/// | 51  | `0x8000 = "Out of Range" indicator` |
/// | 52  | `0x8000 = "Out of Range" indicator` |
/// | 67  | `0x80000000 = "Reserved"` |
/// | 68  | `0x80000000 = "Reserved"` |
/// | 79  | `0x8000 = "Out of Range" indicator` |
/// | 80  | `0x8000 = "Out of Range" indicator` |
/// | 82–89 | `0x80000000 = "N/A (Off-Earth)" indicator` |
/// | 90  | `0x80000000 = "Out of Range" indicator` |
/// | 91  | `0x80000000 = "Out of Range" indicator` |
/// | 92  | `0x80000000 = "Out of Range" indicator` |
/// | 93  | `0x80000000 = "Out of Range" indicator` |
#[must_use]
pub fn st0601_sentinel_meaning(tag: u32) -> Option<St0601SentinelMeaning> {
    match tag {
        6 | 7 | 50 | 51 | 52 | 79 | 80 | 90 | 91 | 92 | 93 => {
            Some(St0601SentinelMeaning::OutOfRange)
        }
        13 | 14 | 19 | 67 | 68 => Some(St0601SentinelMeaning::Reserved),
        23 | 24 | 26..=33 | 40 | 41 | 82..=89 => Some(St0601SentinelMeaning::NotAvailable),
        _ => None,
    }
}

/// ST 0601 defines full-range/absolute twins for several narrow tags; when a
/// narrow encode rejects, point the caller at the twin (otherwise every
/// integrator discovers these one runtime crash at a time).
fn range_hint(tag: u8) -> Option<&'static str> {
    match tag {
        6 => Some("for extended range use platform_pitch_full_deg (Tag 90, +/-90 deg)"),
        7 => Some("for extended range use platform_roll_full_deg (Tag 91, +/-90 deg)"),
        22 => Some("for extended range use target_width_extended_m (Tag 96, 0-1500000 m)"),
        38 => Some("for extended range use density_altitude_extended_m (Tag 103, -900 to 40000 m)"),
        75 => Some(
            "for extended range use sensor_ellipsoid_height_extended_m (Tag 104, -900 to 40000 m)",
        ),
        76 => Some(
            "for extended range use alternate_platform_ellipsoid_height_extended_m \
             (Tag 105, -900 to 40000 m)",
        ),
        26..=33 => Some(
            "for out-of-range corner offsets use the absolute corner fields \
             corner_lat_p1_deg..corner_lon_p4_deg (Tags 82-89)",
        ),
        50 => Some("for extended range use platform_angle_of_attack_full_deg (Tag 92, +/-90 deg)"),
        52 => Some("for extended range use platform_sideslip_full_deg (Tag 93, +/-180 deg)"),
        _ => None,
    }
}

/// Encode a float value into `out` according to `range`.
/// `tag` is for error reporting only.
pub(crate) fn encode_fixed_range(
    range: &LinearRange,
    tag: u32,
    value: f64,
    out: &mut [u8],
    policy: OutOfRangePolicy,
) -> Result<(), KlvEncodeError> {
    if out.len() < range.byte_length {
        return Err(KlvEncodeError::BufferTooSmall {
            needed: range.byte_length,
            got: out.len(),
        });
    }
    if !value.is_finite() || value < range.min || value > range.max {
        // ST 0601.19 §7.5 / ST 0601.13-27: where the item defines an
        // "Out of Range" special value, an encoder shall use it. Only the
        // 11 tags whose INT_MIN sentinel *means* OutOfRange qualify —
        // emitting the same bit pattern on a Reserved/NotAvailable tag
        // would signal the wrong condition. Non-finite input is a caller
        // bug, not an out-of-range measurement: always an error.
        if policy == OutOfRangePolicy::Indicator
            && value.is_finite()
            && range.signed
            && st0601_sentinel_meaning(tag) == Some(St0601SentinelMeaning::OutOfRange)
        {
            let int_min_value: i64 = match range.byte_length {
                2 => i64::from(i16::MIN),
                4 => i64::from(i32::MIN),
                _ => unreachable!("OutOfRange sentinel tags are 2- or 4-byte"),
            };
            let all = int_min_value.to_be_bytes();
            out[..range.byte_length].copy_from_slice(&all[8 - range.byte_length..]);
            return Ok(());
        }
        return Err(KlvEncodeError::OutOfRange {
            tag,
            value,
            min: range.min,
            max: range.max,
            hint: range_hint(tag as u8),
        });
    }
    if range.signed {
        let int_max = signed_max(range.byte_length);
        let int_min_plus_one = -int_max;
        let span = range.max - range.min;
        let scale = span / (int_max as f64 - int_min_plus_one as f64);
        let midpoint = (range.min + range.max) / 2.0;
        let mut i = ((value - midpoint) / scale).round() as i64;
        if i > int_max {
            i = int_max;
        }
        if i < int_min_plus_one {
            i = int_min_plus_one;
        }
        // write_signed_be(i, buf) == write_unsigned_be(i as u64, buf): the
        // per-slot `& 0xFF` already extracts the correct two's-complement byte
        // regardless of any mask applied to the full value — see the test
        // `write_signed_be_equals_unsigned_cast` which proves byte-exactness.
        write_unsigned_be(i as u64, &mut out[..range.byte_length]);
    } else {
        let int_max = unsigned_max(range.byte_length);
        let span = range.max - range.min;
        let scale = span / int_max as f64;
        let mut i = ((value - range.min) / scale).round() as i64;
        if i > int_max {
            i = int_max;
        }
        if i < 0 {
            i = 0;
        }
        write_unsigned_be(i as u64, &mut out[..range.byte_length]);
    }
    Ok(())
}

/// Decode bytes into a float value according to `range`, or `None` if the
/// bytes encode the INT_MIN sentinel on a signed range.
///
/// Returns `Ok(None)` when `range.signed` is true and the wire value is
/// INT_MIN (e.g. `0x8000` for 2-byte, `0x80000000` for 4-byte). The caller
/// is responsible for recording the sentinel tag and meaning via
/// [`st0601_sentinel_meaning`]; this function never returns an error for a
/// spec-defined sentinel — a sentinel is a valid signal, not a malformed
/// field.
///
/// `tag` is for error-reporting on `InvalidLength` only.
pub(crate) fn decode_fixed_range(
    range: &LinearRange,
    tag: u32,
    bytes: &[u8],
) -> Result<Option<f64>, KlvFieldError> {
    if bytes.len() != range.byte_length {
        return Err(KlvFieldError::InvalidLength {
            tag,
            expected: range.byte_length,
            got: bytes.len(),
        });
    }
    if range.signed {
        let i = read_signed_be(bytes);
        let int_max = signed_max(range.byte_length);
        let int_min = -int_max - 1;
        if i == int_min {
            // Spec-defined sentinel — not an error. Caller handles meaning.
            return Ok(None);
        }
        let int_min_plus_one = int_min + 1;
        let span = range.max - range.min;
        let scale = span / (int_max as f64 - int_min_plus_one as f64);
        let midpoint = (range.min + range.max) / 2.0;
        Ok(Some(i as f64 * scale + midpoint))
    } else {
        let i = read_unsigned_be(bytes);
        let int_max = unsigned_max(range.byte_length);
        let span = range.max - range.min;
        let scale = span / int_max as f64;
        Ok(Some(i as f64 * scale + range.min))
    }
}

fn signed_max(n: usize) -> i64 {
    match n {
        1 => i8::MAX as i64,
        2 => i16::MAX as i64,
        4 => i32::MAX as i64,
        _ => unreachable!("byte_length validated by tags.rs"),
    }
}

fn unsigned_max(n: usize) -> i64 {
    match n {
        1 => u8::MAX as i64,
        2 => u16::MAX as i64,
        4 => u32::MAX as i64,
        _ => unreachable!("byte_length validated by tags.rs"),
    }
}

fn write_unsigned_be(value: u64, out: &mut [u8]) {
    let n = out.len();
    for (i, slot) in out.iter_mut().enumerate().take(n) {
        *slot = ((value >> (8 * (n - 1 - i))) & 0xFF) as u8;
    }
}

fn read_signed_be(bytes: &[u8]) -> i64 {
    let n = bytes.len();
    let mut bits: u64 = 0;
    for &b in bytes {
        bits = (bits << 8) | b as u64;
    }
    let sign_bit = 1u64 << (n as u32 * 8 - 1);
    if bits & sign_bit != 0 {
        let extension = !((1u64 << (n as u32 * 8)) - 1);
        (bits | extension) as i64
    } else {
        bits as i64
    }
}

fn read_unsigned_be(bytes: &[u8]) -> u64 {
    let mut bits: u64 = 0;
    for &b in bytes {
        bits = (bits << 8) | b as u64;
    }
    bits
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- KLV-3: write_signed_be ≡ write_unsigned_be(v as u64) ---
    //
    // Proves the per-slot `(value >> shift) & 0xFF` already extracts the
    // correct two's-complement byte, so the old `mask` computation was dead.
    // Reference: both functions must emit identical bytes for negative values.
    #[test]
    fn write_signed_be_equals_unsigned_cast() {
        // Test sentinel cases across all byte widths the signed path uses.
        let cases: &[(i64, usize)] = &[
            (-1, 1),
            (-128, 1),
            (i8::MAX as i64, 1),
            (-1, 2),
            (-32768, 2),
            (i16::MAX as i64, 2),
            (-1, 4),
            (i32::MIN as i64 + 1, 4), // INT32_MIN+1 = the spec VALID minimum
            (i32::MAX as i64, 4),
        ];
        for &(value, n) in cases {
            let mut old = vec![0u8; n];
            // Compute what the old write_signed_be would produce (inline the
            // removed function so the test proves the equivalence explicitly).
            let mask = if n == 8 {
                u64::MAX
            } else {
                (1u64 << (n as u32 * 8)) - 1
            };
            let bits = (value as u64) & mask;
            for (i, slot) in old.iter_mut().enumerate().take(n) {
                *slot = ((bits >> (8 * (n - 1 - i))) & 0xFF) as u8;
            }

            let mut new = vec![0u8; n];
            write_unsigned_be(value as u64, &mut new);

            assert_eq!(
                old, new,
                "byte mismatch for value={value} n={n}: old={old:?} new={new:?}"
            );
        }
    }

    #[test]
    fn signed_round_trip_lat() {
        let r = LinearRange {
            signed: true,
            byte_length: 4,
            min: -90.0,
            max: 90.0,
        };
        for v in [-89.999, -45.0, 0.0, 45.0, 89.999] {
            let mut buf = [0u8; 4];
            encode_fixed_range(&r, 13, v, &mut buf, OutOfRangePolicy::Error).unwrap();
            let back = decode_fixed_range(&r, 13, &buf)
                .unwrap()
                .expect("non-sentinel round-trip");
            assert!((back - v).abs() < 1e-6, "v={v} back={back}");
        }
    }

    #[test]
    fn sentinel_decodes_to_none() {
        // RED → GREEN: ST 0601.19 §8.6 says 0x8000 = "Out of Range" indicator
        // for Tag 6. decode_fixed_range must return Ok(None), not an error.
        let r = LinearRange {
            signed: true,
            byte_length: 2,
            min: -20.0,
            max: 20.0,
        };
        let buf = [0x80, 0x00]; // INT16_MIN
        let result = decode_fixed_range(&r, 6, &buf).expect("sentinel is not a decode error");
        assert!(
            result.is_none(),
            "INT16_MIN on a signed tag must decode as sentinel (None)"
        );
    }

    #[test]
    fn sentinel_meaning_table() {
        use super::St0601SentinelMeaning::*;
        // Tag 6 — "Out of Range" indicator (ST 0601.19 §8.6, p.41)
        assert_eq!(super::st0601_sentinel_meaning(6), Some(OutOfRange));
        // Tag 13 — "Reserved" (ST 0601.19 §8.13, p.50)
        assert_eq!(super::st0601_sentinel_meaning(13), Some(Reserved));
        // Tag 26 — "N/A (Off-Earth)" indicator (ST 0601.19 §8.26, p.65)
        assert_eq!(super::st0601_sentinel_meaning(26), Some(NotAvailable));
        // Tag 5 is unsigned — no sentinel
        assert_eq!(super::st0601_sentinel_meaning(5), None);
    }

    #[test]
    fn unsigned_round_trip_heading() {
        let r = LinearRange {
            signed: false,
            byte_length: 2,
            min: 0.0,
            max: 360.0,
        };
        for v in [0.0, 90.0, 180.0, 270.0, 359.99] {
            let mut buf = [0u8; 2];
            encode_fixed_range(&r, 5, v, &mut buf, OutOfRangePolicy::Error).unwrap();
            // Unsigned ranges never return None; the inner unwrap is infallible.
            let back = decode_fixed_range(&r, 5, &buf).unwrap().unwrap();
            assert!((back - v).abs() < 0.01, "v={v} back={back}");
        }
    }

    #[test]
    fn unsigned_round_trip_alt() {
        let r = LinearRange {
            signed: false,
            byte_length: 2,
            min: -900.0,
            max: 19000.0,
        };
        for v in [-900.0, -500.0, 0.0, 1000.0, 18000.0, 19000.0] {
            let mut buf = [0u8; 2];
            encode_fixed_range(&r, 15, v, &mut buf, OutOfRangePolicy::Error).unwrap();
            let back = decode_fixed_range(&r, 15, &buf).unwrap().unwrap();
            assert!((back - v).abs() < 1.0, "v={v} back={back}");
        }
    }

    #[test]
    fn out_of_range_rejected() {
        let r = LinearRange {
            signed: true,
            byte_length: 4,
            min: -90.0,
            max: 90.0,
        };
        let mut buf = [0u8; 4];
        let err = encode_fixed_range(&r, 13, 100.0, &mut buf, OutOfRangePolicy::Error).unwrap_err();
        matches!(err, KlvEncodeError::OutOfRange { .. });
    }

    #[test]
    fn corner_offset_round_trip() {
        let r = LinearRange {
            signed: true,
            byte_length: 2,
            min: -0.075,
            max: 0.075,
        };
        for v in [-0.075, -0.05, 0.0, 0.05, 0.075] {
            let mut buf = [0u8; 2];
            encode_fixed_range(&r, 26, v, &mut buf, OutOfRangePolicy::Error).unwrap();
            let back = decode_fixed_range(&r, 26, &buf)
                .unwrap()
                .expect("non-sentinel round-trip");
            assert!((back - v).abs() < 1e-5, "v={v} back={back}");
        }
    }

    #[test]
    fn indicator_policy_emits_int_min_for_out_of_range_tag() {
        // Tag 6 (Platform Pitch, ±20°, 2-byte signed): 25.0 is out of range and
        // Tag 6's sentinel meaning is OutOfRange → emit 0x8000, not an error.
        let r = LinearRange {
            signed: true,
            byte_length: 2,
            min: -20.0,
            max: 20.0,
        };
        let mut buf = [0u8; 2];
        encode_fixed_range(&r, 6, 25.0, &mut buf, OutOfRangePolicy::Indicator).unwrap();
        assert_eq!(buf, [0x80, 0x00]);
    }

    #[test]
    fn indicator_policy_ineligible_tag_still_errors() {
        // Tag 13 (Sensor Latitude): sentinel meaning is Reserved, not OutOfRange.
        let r = LinearRange {
            signed: true,
            byte_length: 4,
            min: -90.0,
            max: 90.0,
        };
        let mut buf = [0u8; 4];
        let err =
            encode_fixed_range(&r, 13, 95.0, &mut buf, OutOfRangePolicy::Indicator).unwrap_err();
        assert!(matches!(err, KlvEncodeError::OutOfRange { tag: 13, .. }));
    }

    #[test]
    fn indicator_policy_nonfinite_still_errors() {
        let r = LinearRange {
            signed: true,
            byte_length: 2,
            min: -20.0,
            max: 20.0,
        };
        let mut buf = [0u8; 2];
        assert!(
            encode_fixed_range(&r, 6, f64::NAN, &mut buf, OutOfRangePolicy::Indicator).is_err()
        );
    }

    #[test]
    fn out_of_range_hint_names_full_range_twin_for_tag_50() {
        // Tag 50 (Platform Angle of Attack, ±20°, 2-byte signed): 25.0 is out
        // of range and should name its full-range twin (Tag 92, ±90°) in the
        // hint. Default Error policy (not Indicator) so the range check
        // actually rejects instead of emitting the sentinel.
        let r = LinearRange {
            signed: true,
            byte_length: 2,
            min: -20.0,
            max: 20.0,
        };
        let mut buf = [0u8; 2];
        let err = encode_fixed_range(&r, 50, 25.0, &mut buf, OutOfRangePolicy::Error).unwrap_err();
        match err {
            KlvEncodeError::OutOfRange { hint, .. } => {
                assert_eq!(
                    hint,
                    Some(
                        "for extended range use platform_angle_of_attack_full_deg (Tag 92, +/-90 deg)"
                    )
                );
            }
            other => panic!("expected OutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn indicator_policy_in_range_value_encodes_normally() {
        let r = LinearRange {
            signed: true,
            byte_length: 2,
            min: -20.0,
            max: 20.0,
        };
        let (mut a, mut b) = ([0u8; 2], [0u8; 2]);
        encode_fixed_range(&r, 6, 10.0, &mut a, OutOfRangePolicy::Error).unwrap();
        encode_fixed_range(&r, 6, 10.0, &mut b, OutOfRangePolicy::Indicator).unwrap();
        assert_eq!(a, b);
    }
}
