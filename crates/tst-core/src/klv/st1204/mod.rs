//! MISB ST 1204.3 MIIS Core Identifier — binary decode/encode, textual format,
//! and Appendix B check value.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! A MIIS Core Identifier is a compact binary record that uniquely identifies
//! a motion imagery source. It consists of a version byte, a usage byte, and
//! up to four 16-byte UUIDs (sensor, platform, window, minor), all defined by
//! the MISB ST 1204.3 standard.
//!
//! ## Carriage sites
//!
//! - **ST 0601 Tag 94** (`UasDatalinkLs::miis_core_id`) — the primary
//!   carriage path for UAS datalink (MISB ST 0601.19 §8.94).
//! - **ST 0903 VTarget Tag 13** (`VTargetPack::miis_id`) — per-target
//!   MIIS Core Identifier in a VMTI Local Set (MISB ST 0903.6 §10.2.2.14).
//!
//! Both fields carry raw bytes; use [`decode`] to parse them into a typed
//! [`CoreId`] and [`encode_to_vec`] to round-trip back to bytes.
//!
//! ## Textual format
//!
//! [`CoreId`] implements [`core::fmt::Display`] per ST 1204.3 §7.4.2:
//! `VVUU:XXXX-XXXX-…/XXXX-XXXX-…:CC` where `VV` and `UU` are the version
//! and usage bytes as uppercase hex, each UUID is 8 dash-separated 4-hex-char
//! groups, multiple UUID components are '/'-separated, and `CC` is the
//! Appendix B check value as 2-digit uppercase hex.
//!
//! ## Spec coverage
//!
//! **Standard:** MISB ST 1204.3 §7.3 MIIS Core Identifier Binary Format.
//!
//! **Version:** only version 1 (`0x01`) is accepted; any other byte value —
//! including multi-byte BER-OID forms (high-bit set) — returns
//! [`St1204Error::UnsupportedVersion`].
//!
//! **Usage byte** (ST 1204.3 §7.3.1 Table 3):
//! - b7 = 0 reserved (must be 0)
//! - b6–b5 = sensor type (00 None / 01 Managed / 10 Virtual / 11 Physical)
//! - b4–b3 = platform type (same encoding)
//! - b2 = window UUID included
//! - b1 = minor UUID included
//! - b0 = 0 reserved (must be 0)
//!
//! **EBNF rule:** a Minor Core Id (`minor` present) must have sensor,
//! platform, and window all absent. Usage byte `0x00` (all-None) is
//! invalid per ST 1204.3 §7.3.1. Reserved bits b7 or b0 set → error.

use alloc::vec::Vec;
use core::fmt;
use thiserror::Error;

// ── public types ────────────────────────────────────────────────────────────

/// Source type for a sensor or platform UUID within a [`CoreId`].
///
/// Maps to the two-bit field in the ST 1204.3 §7.3.1 Table 3 usage byte:
/// `11` → Physical, `10` → Virtual, `01` → Managed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdType {
    /// Identifies a physical (hardware) sensor or platform.
    Physical,
    /// Identifies a virtual (software-defined) sensor or platform.
    Virtual,
    /// Identifies a managed (assigned/registered) sensor or platform.
    Managed,
}

crate::binding_record! {
    /// A decoded MIIS Core Identifier (ST 1204.3 §7.3).
    ///
    /// Contains the raw version byte plus up to four optional UUID components.
    /// The EBNF constraint — `minor` XOR any-of-(sensor/platform/window) — is
    /// enforced by [`decode`] and should be maintained by callers that
    /// encode the record.
    ///
    /// Carriage sites: ST 0601 Tag 94 is the primary path for UAS datalink,
    /// and ST 0903 VTarget Tag 13 carries the identifier per target.
    #[non_exhaustive]
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CoreId {
        /// Wire version byte. Always `1` for decoded values; set to `1` when
        /// constructing a value to encode.
        pub version: u8,
        /// Sensor UUID and its type, if present.
        @[custom]
        pub sensor: Option<(IdType, [u8; 16])>,
        /// Platform UUID and its type, if present.
        @[custom]
        pub platform: Option<(IdType, [u8; 16])>,
        /// Window UUID, if present. Window has no type bits — its presence is
        /// indicated by usage-byte bit b2.
        @[custom]
        pub window: Option<[u8; 16]>,
        /// Minor Core Identifier UUID. When present, sensor/platform/window
        /// must all be `None` (ST 1204.3 §7.3.1 EBNF rule).
        @[custom]
        pub minor: Option<[u8; 16]>,
    }
}

/// An empty identifier: version `1` and no UUID.
///
/// `CoreId` is `#[non_exhaustive]`, so a crate that cannot name its fields
/// in a struct literal starts from this value and assigns each field.
impl Default for CoreId {
    fn default() -> Self {
        Self {
            version: 1,
            sensor: None,
            platform: None,
            window: None,
            minor: None,
        }
    }
}

/// Errors returned by [`decode`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum St1204Error {
    /// Input is too short to contain a complete Core Identifier.
    #[error("buffer truncated: not enough bytes for a complete Core Identifier")]
    Truncated,

    /// Version byte is not `1`. Carries the raw byte that was found.
    /// Multi-byte BER-OID forms (high bit set) also produce this variant.
    #[error("unsupported MIIS Core Identifier version: {0:#04x}")]
    UnsupportedVersion(u8),

    /// Usage byte has a reserved bit (b7 or b0) set, which is forbidden by
    /// ST 1204.3 §7.3.1 Table 3.
    #[error("reserved bits set in MIIS Core Identifier usage byte")]
    ReservedBitsSet,

    /// Usage byte is semantically invalid: either all component bits are
    /// zero (usage `0x00`), or `minor` is requested alongside at least one
    /// of sensor/platform/window (EBNF violation).
    #[error("invalid MIIS Core Identifier usage byte: all-None or minor+FCID combination")]
    InvalidUsage,

    /// Input has bytes remaining after a fully decoded Core Identifier.
    #[error("trailing bytes after MIIS Core Identifier")]
    TrailingBytes,
}

// ── decode ───────────────────────────────────────────────────────────────────

/// Decode a MIIS Core Identifier from its binary wire form.
///
/// Expects exactly the bytes of one Core Identifier — no framing, no BER
/// length wrapper. Returns [`St1204Error::TrailingBytes`] if any bytes
/// remain after the identifier is fully consumed.
///
/// # Errors
///
/// See [`St1204Error`] for the full list of failure modes.
pub fn decode(buf: &[u8]) -> Result<CoreId, St1204Error> {
    let mut pos = 0usize;

    // ── version byte ──────────────────────────────────────────────────────
    if buf.is_empty() {
        return Err(St1204Error::Truncated);
    }
    let version = buf[pos];
    pos += 1;
    // Accept only version 1. High bit set means a multi-byte BER-OID form,
    // which we also reject (UnsupportedVersion carries the raw byte).
    if version != 1 {
        return Err(St1204Error::UnsupportedVersion(version));
    }

    // ── usage byte ────────────────────────────────────────────────────────
    if pos >= buf.len() {
        return Err(St1204Error::Truncated);
    }
    let usage = buf[pos];
    pos += 1;

    // Reserved bits b7 and b0 must be 0.
    if usage & 0b1000_0001 != 0 {
        return Err(St1204Error::ReservedBitsSet);
    }

    // Decompose usage byte.
    let sensor_bits = (usage >> 5) & 0b11; // b6-b5
    let platform_bits = (usage >> 3) & 0b11; // b4-b3
    let window_present = (usage >> 2) & 0b1 != 0; // b2
    let minor_present = (usage >> 1) & 0b1 != 0; // b1

    let has_sensor = sensor_bits != 0;
    let has_platform = platform_bits != 0;
    let has_fcid_component = has_sensor || has_platform || window_present;

    // All-None usage is invalid.
    if !has_fcid_component && !minor_present {
        return Err(St1204Error::InvalidUsage);
    }

    // Minor is mutually exclusive with any FCID component (sensor/platform/window).
    if minor_present && has_fcid_component {
        return Err(St1204Error::InvalidUsage);
    }

    // ── read UUIDs in EBNF order ──────────────────────────────────────────
    // Order: sensor, platform, window (if any), or minor.

    let sensor = if has_sensor {
        Some((id_type_from_bits(sensor_bits), read_uuid(buf, &mut pos)?))
    } else {
        None
    };

    let platform = if has_platform {
        Some((id_type_from_bits(platform_bits), read_uuid(buf, &mut pos)?))
    } else {
        None
    };

    let window = if window_present {
        Some(read_uuid(buf, &mut pos)?)
    } else {
        None
    };

    let minor = if minor_present {
        Some(read_uuid(buf, &mut pos)?)
    } else {
        None
    };

    // ── trailing bytes check ──────────────────────────────────────────────
    if pos != buf.len() {
        return Err(St1204Error::TrailingBytes);
    }

    Ok(CoreId {
        version,
        sensor,
        platform,
        window,
        minor,
    })
}

/// Decode a two-bit type field into an [`IdType`].
/// Callers only invoke this when `bits != 0`; `00` (None) is handled upstream.
fn id_type_from_bits(bits: u8) -> IdType {
    match bits {
        0b11 => IdType::Physical,
        0b10 => IdType::Virtual,
        0b01 => IdType::Managed,
        // 0b00 (None) should never reach here — callers check has_sensor/platform first.
        _ => unreachable!("id_type_from_bits called with unvalidated bits"),
    }
}

/// Read exactly 16 bytes from `buf` at `*pos`, advancing `*pos` by 16.
fn read_uuid(buf: &[u8], pos: &mut usize) -> Result<[u8; 16], St1204Error> {
    if *pos + 16 > buf.len() {
        return Err(St1204Error::Truncated);
    }
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&buf[*pos..*pos + 16]);
    *pos += 16;
    Ok(uuid)
}

// ── encode ────────────────────────────────────────────────────────────────────

/// Encode a [`CoreId`] into its binary wire form.
///
/// The caller is responsible for upholding the ST 1204.3 EBNF constraint:
/// [`CoreId::minor`] must be `None` when any of sensor/platform/window is
/// present, and vice-versa. Passing a [`CoreId`] that violates this
/// constraint produces bytes that [`decode`] will reject with
/// [`St1204Error::InvalidUsage`].
///
/// Returns the two-byte header (version + usage) followed by the UUIDs in
/// EBNF order: sensor, platform, window, minor.
pub fn encode_to_vec(id: &CoreId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(2 + 16 * 4);

    // ── version ───────────────────────────────────────────────────────────
    buf.push(id.version);

    // ── usage byte ────────────────────────────────────────────────────────
    let mut usage: u8 = 0;
    if let Some((ref ty, _)) = id.sensor {
        usage |= id_type_to_bits(ty) << 5;
    }
    if let Some((ref ty, _)) = id.platform {
        usage |= id_type_to_bits(ty) << 3;
    }
    if id.window.is_some() {
        usage |= 1 << 2;
    }
    if id.minor.is_some() {
        usage |= 1 << 1;
    }
    buf.push(usage);

    // ── UUIDs in EBNF order ───────────────────────────────────────────────
    if let Some((_, ref uuid)) = id.sensor {
        buf.extend_from_slice(uuid);
    }
    if let Some((_, ref uuid)) = id.platform {
        buf.extend_from_slice(uuid);
    }
    if let Some(ref uuid) = id.window {
        buf.extend_from_slice(uuid);
    }
    if let Some(ref uuid) = id.minor {
        buf.extend_from_slice(uuid);
    }

    buf
}

/// Convert an [`IdType`] to its two-bit representation.
fn id_type_to_bits(ty: &IdType) -> u8 {
    match ty {
        IdType::Physical => 0b11,
        IdType::Virtual => 0b10,
        IdType::Managed => 0b01,
    }
}

// ── textual format (ST 1204.3 §7.4.2) ────────────────────────────────────────

/// Display a [`CoreId`] in the ST 1204.3 §7.4.2 textual format.
///
/// Format: `VVUU:XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX/…:CC`
///
/// - `VV` = version byte as 2 uppercase hex digits.
/// - `UU` = usage byte as 2 uppercase hex digits.
/// - Each present UUID is 8 groups of 4 hex chars, dash-separated.
/// - Multiple UUID components (sensor, platform, window, or minor) are
///   separated by `/`.
/// - `CC` = Appendix B check value over all hex digits emitted (excluding
///   separators), as 2 uppercase hex digits.
impl fmt::Display for CoreId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Collect all hex digits emitted (for check-value computation) while
        // also building the formatted string in a single pass.
        //
        // We write via a wrapper that accumulates hex digits as we go.
        let mut hex_digits: alloc::vec::Vec<char> = alloc::vec::Vec::with_capacity(68);

        // Helper: push 2-digit hex for one byte into both the display and the
        // digit accumulator.
        macro_rules! emit_byte {
            ($b:expr) => {{
                let b: u8 = $b;
                let hi = core::char::from_digit((b >> 4) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase();
                let lo = core::char::from_digit((b & 0xF) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase();
                hex_digits.push(hi);
                hex_digits.push(lo);
                write!(f, "{}{}", hi, lo)?;
            }};
        }

        // Version + usage.
        emit_byte!(self.version);
        emit_byte!(self.usage_byte());
        write!(f, ":")?;

        // Collect present UUIDs in EBNF order: sensor, platform, window, minor.
        let uuids: alloc::vec::Vec<&[u8; 16]> = [
            self.sensor.as_ref().map(|(_, u)| u),
            self.platform.as_ref().map(|(_, u)| u),
            self.window.as_ref(),
            self.minor.as_ref(),
        ]
        .iter()
        .filter_map(|opt| *opt)
        .collect();

        for (ui, uuid) in uuids.iter().enumerate() {
            if ui > 0 {
                write!(f, "/")?;
            }
            // Each UUID: 16 bytes → 8 groups of 2 bytes (4 hex chars), dash-separated.
            for group in 0..8usize {
                if group > 0 {
                    write!(f, "-")?;
                }
                emit_byte!(uuid[group * 2]);
                emit_byte!(uuid[group * 2 + 1]);
            }
        }

        // Check value over all hex digits emitted.
        let cv = check_value(hex_digits.iter().copied());
        // Emit CV as 2 uppercase hex chars — these are NOT included in the
        // check-value computation (Appendix B covers version+usage+UUIDs only).
        let cv_hi = core::char::from_digit((cv >> 4) as u32, 16)
            .unwrap()
            .to_ascii_uppercase();
        let cv_lo = core::char::from_digit((cv & 0xF) as u32, 16)
            .unwrap()
            .to_ascii_uppercase();
        write!(f, ":{}{}", cv_hi, cv_lo)
    }
}

impl CoreId {
    /// Construct a [`CoreId`] from its component fields.
    ///
    /// Provided for downstream crates that cannot use the struct literal
    /// form due to the `#[non_exhaustive]` attribute.
    pub fn new(
        version: u8,
        sensor: Option<(IdType, [u8; 16])>,
        platform: Option<(IdType, [u8; 16])>,
        window: Option<[u8; 16]>,
        minor: Option<[u8; 16]>,
    ) -> Self {
        Self {
            version,
            sensor,
            platform,
            window,
            minor,
        }
    }

    /// Reconstruct the usage byte from the decoded fields.
    fn usage_byte(&self) -> u8 {
        let mut usage: u8 = 0;
        if let Some((ref ty, _)) = self.sensor {
            usage |= id_type_to_bits(ty) << 5;
        }
        if let Some((ref ty, _)) = self.platform {
            usage |= id_type_to_bits(ty) << 3;
        }
        if self.window.is_some() {
            usage |= 1 << 2;
        }
        if self.minor.is_some() {
            usage |= 1 << 1;
        }
        usage
    }
}

// ── Appendix B check value ─────────────────────────────────────────────────────

/// Apply the ST 1204.3 Appendix B.1 `pMap` permutation to a nibble.
///
/// Bits are labeled MSB-first: `nibble = (a<<3)|(b<<2)|(c<<1)|d`.
/// `pMap([a,b,c,d]) = [a^b, c, d, a]`.
fn p_map(nibble: u8) -> u8 {
    let a = (nibble >> 3) & 1;
    let b = (nibble >> 2) & 1;
    let c = (nibble >> 1) & 1;
    let d = nibble & 1;
    ((a ^ b) << 3) | (c << 2) | (d << 1) | a
}

/// Apply the ST 1204.3 Appendix B.1 `qMap` permutation to a nibble.
///
/// Bits are labeled MSB-first: `nibble = (a<<3)|(b<<2)|(c<<1)|d`.
/// `qMap([a,b,c,d]) = [d, a^d, b, c]`.
fn q_map(nibble: u8) -> u8 {
    let a = (nibble >> 3) & 1;
    let b = (nibble >> 2) & 1;
    let c = (nibble >> 1) & 1;
    let d = nibble & 1;
    (d << 3) | ((a ^ d) << 2) | (b << 1) | c
}

/// Build the 15×16 lookup tables P and Q for Appendix B check-value
/// computation (ST 1204.3 Appendix B.1).
///
/// `P[0][j] = Q[0][j] = j` (identity); each subsequent row applies the
/// corresponding permutation to the previous row.
fn build_pq_tables() -> ([[u8; 16]; 15], [[u8; 16]; 15]) {
    let mut p = [[0u8; 16]; 15];
    let mut q = [[0u8; 16]; 15];

    // Row 0: identity.
    for j in 0..16usize {
        p[0][j] = j as u8;
        q[0][j] = j as u8;
    }
    // Rows 1..14: apply pMap / qMap to the previous row.
    for i in 1..15usize {
        for j in 0..16usize {
            p[i][j] = p_map(p[i - 1][j]);
            q[i][j] = q_map(q[i - 1][j]);
        }
    }

    (p, q)
}

/// Compute the ST 1204.3 Appendix B check value over a sequence of uppercase
/// hex digit characters.
///
/// The caller must supply only valid hex digits (`'0'`–`'9'`, `'A'`–`'F'`;
/// lowercase `'a'`–`'f'` are also accepted). Non-hex characters must not be
/// fed — behaviour is unspecified (the digit value will be treated as 0).
///
/// The index into the P/Q tables is **1-based**: position 1, 2, 3, … (the
/// first digit is at index 1, not 0). The table wraps modulo 15.
///
/// Returns the packed check byte `(p << 4) | q`.
///
/// # Example
///
/// ```
/// use tst_core::klv::st1204::check_value;
/// // ST 1204.3 Appendix B.2 worked example.
/// assert_eq!(check_value("031FA3".chars()), 0x79);
/// ```
pub fn check_value(hex_digits: impl Iterator<Item = char>) -> u8 {
    let (p_table, q_table) = build_pq_tables();
    let mut p_acc: u8 = 0;
    let mut q_acc: u8 = 0;

    for (zero_idx, ch) in hex_digits.enumerate() {
        // 1-based index into the tables.
        let i = zero_idx + 1;
        let row = i % 15; // wraps to 0..14; row 0 is the identity row
        let digit = ch.to_digit(16).unwrap_or(0) as usize;
        p_acc ^= p_table[row][digit];
        q_acc ^= q_table[row][digit];
    }

    (p_acc << 4) | q_acc
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_prelude::*;

    /// ST 1204.3 Table 7 reference vector.
    /// version=0x01, usage=0x70 (b6-5=11 Physical sensor, b4-3=10 Virtual platform)
    const TABLE7: [u8; 34] = [
        0x01, 0x70, 0xF5, 0x92, 0xF0, 0x23, 0x73, 0x36, 0x4A, 0xF8, 0xAA, 0x91, 0x62, 0xC0, 0x0F,
        0x2E, 0xB2, 0xDA, 0x16, 0xB7, 0x43, 0x41, 0x00, 0x08, 0x41, 0xA0, 0xBE, 0x36, 0x5B, 0x5A,
        0xB9, 0x6A, 0x36, 0x45,
    ];

    #[test]
    fn decode_table7_example() {
        let id = decode(&TABLE7).unwrap();
        assert_eq!(id.version, 1);
        assert_eq!(id.sensor.unwrap().0, IdType::Physical); // usage 0x70: b6-5 = 11
        assert_eq!(id.platform.unwrap().0, IdType::Virtual); // b4-3 = 10
        assert!(id.window.is_none() && id.minor.is_none());
        assert_eq!(encode_to_vec(&id), TABLE7.to_vec());
    }

    #[test]
    fn usage_examples_from_spec() {
        // §7.3.1 Example 1: Physical/Physical/None/None = usage 0x78
        // b6-5=11 sensor, b4-3=11 platform, b2=0 window, b1=0 minor
        let uuid1 = [0xAAu8; 16];
        let uuid2 = [0xBBu8; 16];
        let mut buf_78 = vec![0x01u8, 0x78];
        buf_78.extend_from_slice(&uuid1);
        buf_78.extend_from_slice(&uuid2);
        let id_78 = decode(&buf_78).unwrap();
        assert_eq!(id_78.sensor.unwrap().0, IdType::Physical);
        assert_eq!(id_78.platform.unwrap().0, IdType::Physical);
        assert!(id_78.window.is_none() && id_78.minor.is_none());
        assert_eq!(encode_to_vec(&id_78), buf_78);

        // §7.3.1 Example 2: Physical/Virtual/Included/None = usage 0x74
        // b6-5=11 sensor, b4-3=10 platform, b2=1 window, b1=0 minor
        let uuid3 = [0xCCu8; 16];
        let mut buf_74 = vec![0x01u8, 0x74];
        buf_74.extend_from_slice(&uuid1);
        buf_74.extend_from_slice(&uuid2);
        buf_74.extend_from_slice(&uuid3);
        let id_74 = decode(&buf_74).unwrap();
        assert_eq!(id_74.sensor.unwrap().0, IdType::Physical);
        assert_eq!(id_74.platform.unwrap().0, IdType::Virtual);
        assert!(id_74.window.is_some() && id_74.minor.is_none());
        assert_eq!(encode_to_vec(&id_74), buf_74);
    }

    #[test]
    fn rejects_malformed() {
        assert!(matches!(decode(&[]), Err(St1204Error::Truncated)));
        assert!(matches!(
            decode(&[0x02, 0x70]),
            Err(St1204Error::UnsupportedVersion(2))
        ));
        assert!(matches!(
            decode(&[0x01, 0xF0]),
            Err(St1204Error::ReservedBitsSet)
        )); // b7 set
        assert!(matches!(
            decode(&[0x01, 0x00]),
            Err(St1204Error::InvalidUsage)
        )); // all-None
        assert!(matches!(
            decode(&[0x01, 0x70, 0xAA]),
            Err(St1204Error::Truncated)
        )); // short UUIDs
        let mut extra = TABLE7.to_vec();
        extra.push(0);
        assert!(matches!(decode(&extra), Err(St1204Error::TrailingBytes)));
        // minor + sensor simultaneously violates the EBNF:
        // 0x62 = 0b0110_0010: b6-5=11 (sensor Physical), b1=1 (minor)
        assert!(matches!(
            decode(&[0x01, 0x62]),
            Err(St1204Error::InvalidUsage)
        ));
    }

    #[test]
    fn rejects_multi_byte_ber_oid_version() {
        // 0x82 has high bit set — multi-byte BER-OID form, reject as UnsupportedVersion
        assert!(matches!(
            decode(&[0x82, 0x70]),
            Err(St1204Error::UnsupportedVersion(0x82))
        ));
    }

    #[test]
    fn minor_core_id_round_trip() {
        // usage 0x02: b1=1 minor, all others 0 → Minor Core Id
        let uuid = [0xDEu8; 16];
        let mut buf = vec![0x01u8, 0x02];
        buf.extend_from_slice(&uuid);
        let id = decode(&buf).unwrap();
        assert!(id.sensor.is_none());
        assert!(id.platform.is_none());
        assert!(id.window.is_none());
        assert_eq!(id.minor.unwrap(), uuid);
        assert_eq!(encode_to_vec(&id), buf);
    }

    #[test]
    fn managed_id_type_round_trip() {
        // usage 0x28: b6-5=01 (Managed sensor), b4-3=01 (Managed platform) = 0b0010_1000
        let uuid1 = [0x11u8; 16];
        let uuid2 = [0x22u8; 16];
        let mut buf = vec![0x01u8, 0x28];
        buf.extend_from_slice(&uuid1);
        buf.extend_from_slice(&uuid2);
        let id = decode(&buf).unwrap();
        assert_eq!(id.sensor.unwrap().0, IdType::Managed);
        assert_eq!(id.platform.unwrap().0, IdType::Managed);
        assert_eq!(encode_to_vec(&id), buf);
    }

    #[test]
    fn truncated_on_missing_usage_byte() {
        // version byte present but no usage byte
        assert!(matches!(decode(&[0x01]), Err(St1204Error::Truncated)));
    }

    #[test]
    fn reserved_bit_b0_rejected() {
        // usage 0x71 has b0=1 — reserved, must be rejected
        assert!(matches!(
            decode(&[0x01, 0x71]),
            Err(St1204Error::ReservedBitsSet)
        ));
    }

    // ── Appendix B check-value tests ─────────────────────────────────────────

    #[test]
    fn p_map_q_map_worked_examples() {
        // ST 1204.3 Appendix B.1 worked examples: pMap(13)=3, qMap(13)=10.
        assert_eq!(p_map(13), 3);
        assert_eq!(q_map(13), 10);
    }

    #[test]
    fn p_table_row1_matches_spec_table14() {
        // ST 1204.3 Table 14 row 1: {0,2,4,6,8,A,C,E,9,B,D,F,1,3,5,7}.
        let (p, _q) = build_pq_tables();
        let expected: [u8; 16] = [0, 2, 4, 6, 8, 0xA, 0xC, 0xE, 9, 0xB, 0xD, 0xF, 1, 3, 5, 7];
        assert_eq!(p[1], expected);
    }

    #[test]
    fn check_value_appendix_b_vector() {
        // Appendix B.2: "031FA3" -> 0x79.
        assert_eq!(check_value("031FA3".chars()), 0x79);
    }

    #[test]
    fn display_matches_spec_example() {
        // §7.4.2 example (same bytes as TABLE7), check value D3.
        let id = decode(&TABLE7).unwrap();
        assert_eq!(
            id.to_string(),
            "0170:F592-F023-7336-4AF8-AA91-62C0-0F2E-B2DA/16B7-4341-0008-41A0-BE36-5B5A-B96A-3645:D3"
        );
    }
}
