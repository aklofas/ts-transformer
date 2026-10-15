//! KLV codec — generic substrate plus the typed ST 0601/0102/0806/0903 local
//! sets, the ST 0605 and ST 1010 packs, the ST 1204 Core Identifier, and the
//! ST 0805 KLV→CoT conversion layer.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Generic substrate plus typed layers:
//!
//! - **Generic substrate** (`universal_label`, `length`, `pack`, `imapb`,
//!   `checksum`) — handles raw KLV machinery: 16-byte SMPTE Universal Labels,
//!   BER short/long and BER-OID length encodings, IMAPB integer↔float
//!   mapping per ST 1201 §7, ST 0601 16-bit running-sum checksum, and
//!   generic local-set / universal-set pack-and-iterate.
//! - **Typed ST 0601 layer** (`st0601`) — 142 of the 143 ST 0601.19 items
//!   (all but the deprecated Tag 66) as a flat `UasDatalinkLs` struct with eager `decode` /
//!   free-function `encode`. Anything not typed-modeled passes through as
//!   `OwnedRawField` in `record.unknown`.
//! - **Typed ST 0102 layer** (`st0102`) — the Security Metadata Local Set
//!   as a flat `SecurityLs` struct with lenient/strict `decode` /
//!   free-function `encode`. Sibling typed parser to `st0601`; consumers
//!   typically reach this from `UasDatalinkLs::security_local_set`.
//!   Anything not typed-modeled passes through as `OwnedRawField` in
//!   `record.unknown`.
//! - **Further typed layers** (`st0605`, `st0805`, `st0806`, `st0903`,
//!   `st1010`, `st1204`) follow the same flat-struct decode/encode
//!   pattern as `st0601`/`st0102` — see each module's own docs for its
//!   MISB standard and scope (`st0805` is KLV→CoT conversion rather
//!   than a decode/encode pair).
//!
//! MPEG-TS sync-metadata AU cell carriage lives at
//! [`crate::mpegts::au_cell`] (per ITU-T H.222.0 V9 §2.12.4.2 — that's
//! a TS-systems-layer concern, not a KLV substrate concern; the muxer
//! auto-wraps for `KlvStreamType::SynchronousMetadata` streams).
//!
//! Top-level re-exports (substrate types likely useful to consumers) live in
//! the crate root via `crate::lib.rs`.

pub mod checksum;
pub mod imapb;
pub mod length;
pub mod pack;
pub mod st0102;
pub mod st0601;
pub mod st0605;
pub mod st0805;
pub mod st0806;
pub mod st0903;
pub mod st1010;
pub mod st1204;
pub mod universal_label;

pub use imapb::ImapbSpecial;
pub use pack::{OwnedRawField, RawField};
pub use st0102::{
    ClassifyingCountryCodingMethod, ObjectCountryCodingMethod, SECURITY_LS_UL,
    SecurityClassification, SecurityLs,
};
pub use st0605::{PrecisionTimeStampPack, TimeStatus};
pub use st0805::{
    CotConfig, platform_position_xml, platform_uid, sensor_point_of_interest_xml, spi_uid,
};
pub use st0806::{
    RVT_AOI_LS_UL, RVT_LS_UL, RVT_POI_LS_UL, RVT_USER_DEFINED_LS_UL, RvtAoi, RvtAoiType, RvtLs,
    RvtPoi, RvtPoiType, RvtUserData, RvtUserDataType,
};
pub use st0903::{VMTI_LS_UL, VTargetPack, VTargetPackError, VmtiLs};
pub use st1010::{SdccFlp, decode_sdcc_flp, encode_sdcc_flp_mode2};
pub use universal_label::UniversalLabel;

/// Shared body of the wire-code enums' `variant_inventory` tests.
///
/// Generates one `#[test]` that walks `<Enum>::ALL` and pins the inventory
/// TWICE:
///
/// 1. the `match` carries **no wildcard** — these tests live inside tst-core,
///    where `#[non_exhaustive]` does not force one, so a newly added variant
///    fails to compile until its pattern is listed here;
/// 2. every listed pattern records a hit by name and the test asserts each
///    name was hit **exactly once**, so `ALL` must contain every variant once
///    (a missing entry reads 0, a duplicate reads 2).
///
/// The compiler alone only gives (1): a developer who adds the arm but forgets
/// the `ALL` entry would otherwise pass. Binding crates cannot get (1) at all
/// (matching a foreign `#[non_exhaustive]` enum without a wildcard is E0004);
/// they iterate `ALL` instead.
#[cfg(test)]
macro_rules! inventory_test {
    ($name:ident, $ty:ident, [$($pat:pat),+ $(,)?]) => {
        #[test]
        fn $name() {
            let mut hit: alloc::vec::Vec<&'static str> = alloc::vec::Vec::new();
            for v in $ty::ALL {
                // wildcard-free: a new variant is a compile error here
                match v {
                    $( $pat => hit.push(stringify!($pat)), )+
                }
                assert_eq!($ty::from_wire(v.to_wire()), *v, "{v:?}");
            }
            let expected: &[&'static str] = &[$( stringify!($pat) ),+];
            for p in expected {
                let n = hit.iter().filter(|h| *h == p).count();
                assert_eq!(
                    n, 1,
                    "{}: `{p}` was hit {n} times by ALL (0 = missing from ALL, >1 = duplicate)",
                    stringify!($ty)
                );
            }
            assert_eq!(
                hit.len(),
                expected.len(),
                "{}: ALL has {} entries, {} patterns listed",
                stringify!($ty),
                hit.len(),
                expected.len()
            );
        }
    };
}

#[cfg(test)]
pub(crate) use inventory_test;

/// Emits a coded-value enum's two wire conversions from one table.
///
/// Invoke it inside the enum's inherent `impl`, so the type keeps a single
/// `impl` block and its public-API rendering does not change.
///
/// `$from(u8) -> Self` maps each listed code to its variant and every other
/// byte to `$escape(byte)`; `$to(self) -> u8` is its exact inverse, so a
/// wire-unknown code round-trips byte-exact. `$spec` names the table in the
/// generated docs. The `match` in `$to` is exhaustive over the enum, so a
/// variant missing from the table does not compile.
macro_rules! wire_codepoints {
    ($from:ident, $to:ident, $escape:ident, $spec:literal;
     $($code:literal => $variant:ident),+ $(,)?) => {
            #[doc = concat!($spec, " wire codepoint → variant (`", stringify!($escape), "(b)` for unknown codes).")]
            pub fn $from(b: u8) -> Self {
                match b {
                    $($code => Self::$variant,)+
                    other => Self::$escape(other),
                }
            }

            #[doc = concat!("Variant → ", $spec, " wire codepoint (inverse of [`Self::", stringify!($from), "`]).")]
            pub fn $to(self) -> u8 {
                match self {
                    $(Self::$variant => $code,)+
                    Self::$escape(b) => b,
                }
            }
    };
}

pub(crate) use wire_codepoints;

/// Shared body of the typed sets' BER-OID unknown-tag tests.
///
/// Generates four `#[test]`s, one per BER-OID width boundary of a tag in the
/// `unknown` bucket (ST 0107.5 §6.3.1): 127 (`7F`), 128 (`81 00`), 16383
/// (`FF 7F`) and 16384 (`81 80 00`). Each puts one `OwnedRawField` into
/// `$record`, encodes it with the calling module's `encode_to_vec`, checks
/// `encoded_len`, checks the tag's BER-OID bytes on the wire, and checks that
/// `decode` returns the field unchanged. The typed-set encoders write the
/// unknown bucket last, so the tag sits right before its one-byte BER length
/// and its value at the end of the output.
#[cfg(test)]
macro_rules! ber_oid_unknown_tag_tests {
    ($record:expr) => {
        fn ber_oid_unknown_tag_round_trip(tag: u32, value: &[u8], oid: &[u8]) {
            let mut record = $record;
            record.unknown = vec![OwnedRawField {
                tag,
                value: value.to_vec(),
            }];
            let bytes = encode_to_vec(&record).unwrap();
            assert_eq!(encoded_len(&record), bytes.len(), "tag {tag}: encoded_len");
            let at = bytes.len() - (value.len() + 1 + oid.len());
            assert_eq!(&bytes[at..at + oid.len()], oid, "tag {tag}: BER-OID bytes");
            let decoded = decode(&bytes).unwrap();
            assert_eq!(decoded.unknown.len(), 1, "tag {tag}: unknown count");
            assert_eq!(decoded.unknown[0].tag, tag);
            assert_eq!(decoded.unknown[0].value, value);
        }

        /// Tag 127, the last single-byte BER-OID value.
        #[test]
        fn unknown_tag_127_round_trips_single_byte_ber_oid() {
            ber_oid_unknown_tag_round_trip(127, b"max-single-byte", &[0x7F]);
        }

        /// Tag 128, the first multi-byte value: the continuation bit on the
        /// first byte says more bytes follow.
        #[test]
        fn unknown_tag_128_round_trips_multi_byte_ber_oid() {
            ber_oid_unknown_tag_round_trip(128, b"first-multi-byte", &[0x81, 0x00]);
        }

        /// Tag 16383 (2^14 − 1), the last two-byte value.
        #[test]
        fn unknown_tag_16383_round_trips_two_byte_ber_oid() {
            ber_oid_unknown_tag_round_trip(16383, b"max-two-byte", &[0xFF, 0x7F]);
        }

        /// Tag 16384 (2^14), the first three-byte value.
        #[test]
        fn unknown_tag_16384_round_trips_three_byte_ber_oid() {
            ber_oid_unknown_tag_round_trip(16384, b"first-three-byte", &[0x81, 0x80, 0x00]);
        }
    };
}

#[cfg(test)]
pub(crate) use ber_oid_unknown_tag_tests;

#[cfg(test)]
mod wire_codepoint_tests {
    //! Every byte of every coded-value enum, both directions. The expected
    //! strings list the named codepoints; every other byte must come back as
    //! the escape variant carrying the same byte.
    use super::st0601::{
        IcingDetected, OperationalMode, PlatformStatus, SensorControlMode, SensorFovName,
    };
    use super::{
        ClassifyingCountryCodingMethod, ObjectCountryCodingMethod, RvtAoiType, RvtPoiType,
        SecurityClassification,
    };
    use alloc::format;
    use alloc::string::String;
    use core::fmt::Write as _;

    fn table<T: core::fmt::Debug + Copy>(from: fn(u8) -> T, to: fn(T) -> u8) -> String {
        let mut named = String::new();
        for b in 0..=u8::MAX {
            let v = from(b);
            assert_eq!(to(v), b, "{v:?} does not map back to {b}");
            let d = format!("{v:?}");
            if d.starts_with("Other(") || d.starts_with("Unknown(") {
                assert!(d.ends_with(&format!("({b})")), "{d} should carry {b}");
            } else {
                let _ = write!(named, "{b}={d} ");
            }
        }
        named
    }

    #[test]
    fn wire_codepoint_tables_are_pinned() {
        assert_eq!(
            table(IcingDetected::from_wire, IcingDetected::to_wire),
            "0=DetectorOff 1=NoIcingDetected 2=IcingDetected "
        );
        assert_eq!(
            table(SensorFovName::from_wire, SensorFovName::to_wire),
            "0=Ultranarrow 1=Narrow 2=Medium 3=Wide 4=Ultrawide 5=NarrowMedium \
             6=TwoXUltranarrow 7=FourXUltranarrow 8=ContinuousZoom "
        );
        assert_eq!(
            table(OperationalMode::from_wire, OperationalMode::to_wire),
            "0=OtherMode 1=Operational 2=Training 3=Exercise 4=Maintenance 5=Test "
        );
        assert_eq!(
            table(PlatformStatus::from_wire, PlatformStatus::to_wire),
            "0=Active 1=PreFlight 2=PreFlightTaxiing 3=RunUp 4=TakeOff 5=Ingress \
             6=ManualOperation 7=AutomatedOrbit 8=Transitioning 9=Egress 10=Landing \
             11=LandedTaxiing 12=LandedParked "
        );
        assert_eq!(
            table(SensorControlMode::from_wire, SensorControlMode::to_wire),
            "0=Off 1=HomePosition 2=Uncontrolled 3=ManualControl 4=Calibrating \
             5=AutoHoldingPosition 6=AutoTracking "
        );
        assert_eq!(
            table(
                SecurityClassification::from_u8,
                SecurityClassification::to_u8
            ),
            "1=Unclassified 2=Restricted 3=Confidential 4=Secret 5=TopSecret "
        );
        assert_eq!(
            table(
                ClassifyingCountryCodingMethod::from_u8,
                ClassifyingCountryCodingMethod::to_u8
            ),
            "1=Iso3166TwoLetter 2=Iso3166ThreeLetter 3=Fips104TwoLetter 4=Fips104FourLetter \
             5=Iso3166Numeric 6=Stanag1059TwoLetter 7=Stanag1059ThreeLetter 8=OmittedValue08 \
             9=OmittedValue09 10=Fips104Mixed 11=Iso3166Mixed 12=Stanag1059Mixed \
             13=GencTwoLetter 14=GencThreeLetter 15=GencNumeric 16=GencMixed "
        );
        assert_eq!(
            table(
                ObjectCountryCodingMethod::from_u8,
                ObjectCountryCodingMethod::to_u8
            ),
            "1=Iso3166TwoLetter 2=Iso3166ThreeLetter 3=Iso3166Numeric 4=Fips104TwoLetter \
             5=Fips104FourLetter 6=Stanag1059TwoLetter 7=Stanag1059ThreeLetter \
             8=OmittedValue08 9=OmittedValue09 10=OmittedValue0A 11=OmittedValue0B \
             12=OmittedValue0C 13=GencTwoLetter 14=GencThreeLetter 15=GencNumeric \
             64=GencAdminSub "
        );
        assert_eq!(
            table(RvtPoiType::from_wire, RvtPoiType::to_wire),
            "1=Friendly 2=Hostile 3=Target 4=Unknown "
        );
        assert_eq!(
            table(RvtAoiType::from_wire, RvtAoiType::to_wire),
            "1=Friendly 2=Hostile 3=Reserved 4=Unknown "
        );
    }
}
