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
//! - **Typed ST 0601 layer** (`st0601`) — the curated working subset of
//!   ST 0601 tags as a flat `UasDatalinkLs` struct with eager `decode` /
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

/// Shared body of the wire-code enums' `variant_inventory` tests (Arc 2 R2).
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
