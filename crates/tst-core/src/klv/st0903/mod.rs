//! MISB ST 0903.6 VMTI (Video Moving Target Indicator) Local Set typed layer.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Sibling typed parser to [`crate::klv::st0601`]. Consumers who decode
//! a `UasDatalinkLs` and want typed access to the inner VMTI LS call
//! [`decode`] (or [`decode_strict`]) on `record.vmti.as_deref()?`.
//!
//! Two decode entry points:
//! - [`decode`] — lenient: tolerates missing tags, unknown tags
//!   (preserved in `unknown`), malformed sub-records (preserved in
//!   `field_errors`).
//! - [`decode_strict`] — strict: rejects missing required tags
//!   (per ST 0903.6 §6 Table 1), duplicate tags, malformed UTF-8,
//!   pack-level malformations. Unknown tags are still preserved per
//!   ST 0107.5 §6 future-proof skip rule.
//!
//! Encode is symmetric — decode + encode bit-identical round-trips for
//! all spec-conformant input.
//!
//! 7 nested/sibling Local Sets (VMask, VObject, VFeature, VTracker,
//! VChip on each `VTargetPack`; Algorithm Series and Ontology Series at
//! the VMTI top level) stay as `Option<Vec<u8>>` pass-through bytes —
//! typed layers deferred (see `docs/project/deferred-features.md`).
//!
//! Universal Set form of ST 0903 is out of scope (LS-only on
//! MPEG-TS+KLV streams).
//!
//! # Carriage paths
//!
//! VMTI rides two ways in the wild:
//!
//! 1. **Nested inside ST 0601 as Tag 74** — most common; encoder bundles
//!    VMTI alongside platform telemetry. Consumer pattern:
//!    ```ignore
//!    let uas = klv::st0601::decode(bytes)?;
//!    if let Some(vmti_bytes) = uas.vmti.as_deref() {
//!        let vmti = klv::st0903::decode(vmti_bytes)?;
//!        // ...
//!    }
//!    ```
//! 2. **Standalone on its own KLV PID** — the AU-cell payload is a VMTI
//!    LS with [`VMTI_LS_UL`] as the 16-byte UL prefix. Consumer pattern:
//!    ```ignore
//!    if data.starts_with(&klv::st0903::VMTI_LS_UL) {
//!        // Checks the label and the checksum, and reads only the bytes
//!        // the outer length declares.
//!        let vmti = klv::st0903::decode_standalone(data)?;
//!        // ...
//!    }
//!    ```
//!    The demuxer remains UL-agnostic; consumer-side dispatch keeps
//!    new typed-set additions from creating a coupling load on the
//!    demuxer.
//!
//! ## Spec coverage
//!
//! **Standard:** MISB ST 0903.6 VMTI (Video Moving Target Indicator)
//! Local Set + per-target packs (vTargetSeries, Tag 101).
//!
//! **Top-level tags parsed** (typed-modeled per ST 0903.6 §6
//! Table 1): checksum (1), precision timestamp (2), VMTI LS version
//! (3), system name + version + UID + source sensor (4–7), frame
//! width + height (8–9), image source sensor focal length (10–11),
//! LDS version (12), system source identifier (13), vTargetSeries
//! (101, per-target packs decoded via [`VTargetPack`]), plus nested
//! Local Set tags 102–109 (algorithm series, ontology series — bytes
//! preserved as `Option<Vec<u8>>` pass-through; typed layer deferred).
//!
//! **[`VTargetPack`] tags parsed** (typed-modeled per ST 0903.6
//! §10.2 Table 5): targetId/BER-OID (1), centroid pixel number (2),
//! boundary corners (3–4), priority + confidence + history (5–7),
//! color + intensity (8–9), lat/lon/HAE + offsets + bbox geo corners
//! (10–16), plus pack-form nested LSes (VMask, VObject, VFeature,
//! VTracker, VChip — bytes preserved as `Option<Vec<u8>>`; typed
//! layers deferred).
//!
//! **Tags preserved as `unknown`:** any tag not in the typed-modeled
//! set — per ST 0107.5 §6.
//!
//! **Decode modes:**
//! - [`decode`] — lenient: tolerates missing required tags, unknown
//!   tags (preserved in `unknown`), malformed sub-records (preserved
//!   in `field_errors`).
//! - [`decode_strict`] — strict: rejects missing required tags
//!   (per ST 0903.6 §6 Table 1), duplicate tags, malformed UTF-8,
//!   pack-level malformations.
//!
//! **Encode modes:**
//! - [`encode`] / [`encode_to_vec`] — VMTI LS body bytes (no UL
//!   prefix, no outer BER length); for nesting inside an ST 0601
//!   Tag 74.
//! - [`encode_standalone`] / [`encode_to_vec_standalone`] /
//!   [`encoded_len_standalone`] — VMTI LS bytes prepended with
//!   [`VMTI_LS_UL`] + outer BER length wrapper; for standalone
//!   carriage on a dedicated KLV PID.
//!
//! **Deferred per `docs/project/deferred-features.md`:** typed nested-LS
//! layers (VMask, VObject, VFeature, VTracker, VChip on each
//! [`VTargetPack`]; Algorithm Series and Ontology Series at the VMTI
//! top level); Universal Set form of ST 0903 (LS-only on
//! MPEG-TS+KLV streams).

pub(crate) mod decode;
pub(crate) mod emit;
pub(crate) mod encode;
pub(crate) mod enums;
pub(crate) mod model;
pub(crate) mod tags;
pub(crate) mod vtarget_pack;

#[cfg(test)]
mod tests;

mod rules;

pub use decode::{decode, decode_standalone, decode_standalone_strict, decode_strict};
pub use encode::{
    encode, encode_standalone, encode_standalone_strict_compliance, encode_strict_compliance,
    encode_to_vec, encode_to_vec_standalone, encoded_len, encoded_len_standalone,
};
pub use model::VmtiLs;
pub use vtarget_pack::{VTargetPack, VTargetPackError};

/// 16-byte Universal Label for the ST 0903 VMTI Local Set, per MISB ST 0903.6.
/// Used by consumers carrying VMTI as its own KLV stream (separate
/// MPEG-TS PID, not nested in an ST 0601 record).
pub const VMTI_LS_UL: [u8; 16] = [
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x03, 0x06, 0x00, 0x00, 0x00,
];
