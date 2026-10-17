//! ST 0903.6 VTargetPack model: `VTargetPackError`, `VTargetPack`,
//! and the pack-tag spec table (`PACK_TAGS`, `PackEncoding`, `PackTagSpec`,
//! `pack_lookup`).

use alloc::vec::Vec;
use thiserror::Error;

/// Per-pack-tag wire encoding. Mirrors `tags::Encoding` in shape but
/// scoped to the pack's tag set: notably, `Utf8` is absent (no string
/// fields in Table 10), and the U24 RGB special form is added for
/// Tag 8 (`targetColor`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum PackEncoding {
    /// Raw 1-byte unsigned (Tags 4, 5, 7, 23 per §10.2.2.5/.6/.8/.24).
    U8,
    /// Variable-length truncated big-endian unsigned, value bytes
    /// 1..=`max_bytes`. Wire form is the value's raw bytes (no length
    /// byte; the LS-encoded BER length supplies it). Mirrors the
    /// top-level `Encoding::VarUint` from `tags.rs`. Used for V2/V3/
    /// V4/V6 fields per Table 10.
    VarUint { max_bytes: u8 },
    /// Raw 3-byte RGB (Tag 8 `targetColor`, fixed length 3 per
    /// §10.2.2.9 — first byte = R, second = G, third = B).
    U24Rgb,
    /// IMAPB-encoded floating-point with linear range. Wire form is
    /// the raw bytes mapped via `klv::imapb::decode`.
    ImapbF64 { min: f64, max: f64 },
    /// Raw bytes (variable length); pass-through. Used for nested
    /// LSes (Tags 101 / 104 / 105), Series payloads (Tags 18 / 106 /
    /// 107), and the Defined Length Truncation Pack at Tag 17
    /// (`targetLocation`). Typed inner layers are deferred.
    RawBytes,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PackTagSpec {
    pub id: u8,
    pub name: &'static str,
    pub encoding: PackEncoding,
}

/// ST 0903.6 §10.2 Table 10 — VTargetPack body items in numeric tag
/// order. Deprecated tags (21, 102, 103) are intentionally omitted;
/// the lenient decoder treats them as unknown tags (preserved per
/// ST 0107.5 §6 future-proof skip rule). The leading BER-OID
/// `targetId` precedes the body and is NOT in this table — it has
/// no Tag (per §10.2.2.1: "has no Tag").
pub(crate) const PACK_TAGS: &[PackTagSpec] = &[
    PackTagSpec {
        id: 1,
        name: "targetCentroid",
        // §10.2.2.2 — uint V6 (range 1..=2^48-1, pixel number).
        encoding: PackEncoding::VarUint { max_bytes: 6 },
    },
    PackTagSpec {
        id: 2,
        name: "boundingBoxTopLeft",
        // §10.2.2.3 — uint V6 (pixel number).
        encoding: PackEncoding::VarUint { max_bytes: 6 },
    },
    PackTagSpec {
        id: 3,
        name: "boundingBoxBottomRight",
        // §10.2.2.4 — uint V6 (pixel number).
        encoding: PackEncoding::VarUint { max_bytes: 6 },
    },
    PackTagSpec {
        id: 4,
        name: "targetPriority",
        // §10.2.2.5 — uint, fixed length 1, valid 1..=255.
        encoding: PackEncoding::U8,
    },
    PackTagSpec {
        id: 5,
        name: "targetConfidenceLevel",
        // §10.2.2.6 — uint, fixed length 1, valid 0..=100 (percent).
        encoding: PackEncoding::U8,
    },
    PackTagSpec {
        id: 6,
        name: "targetHistory",
        // §10.2.2.7 — uint V2 (0..=65535 frames).
        encoding: PackEncoding::VarUint { max_bytes: 2 },
    },
    PackTagSpec {
        id: 7,
        name: "percentageOfTargetPixels",
        // §10.2.2.8 — uint, fixed length 1, valid 1..=100 (percent).
        encoding: PackEncoding::U8,
    },
    PackTagSpec {
        id: 8,
        name: "targetColor",
        // §10.2.2.9 — fixed length 3, R/G/B raw bytes.
        encoding: PackEncoding::U24Rgb,
    },
    PackTagSpec {
        id: 9,
        name: "targetIntensity",
        // §10.2.2.10 — uint V3 (24-bit dynamic range).
        encoding: PackEncoding::VarUint { max_bytes: 3 },
    },
    PackTagSpec {
        id: 10,
        name: "targetLocationOffsetLat",
        // §10.2.2.11 — IMAPB(-19.2, 19.2, 3), units °.
        encoding: PackEncoding::ImapbF64 {
            min: -19.2,
            max: 19.2,
        },
    },
    PackTagSpec {
        id: 11,
        name: "targetLocationOffsetLon",
        // §10.2.2.12 — IMAPB(-19.2, 19.2, 3), units °.
        encoding: PackEncoding::ImapbF64 {
            min: -19.2,
            max: 19.2,
        },
    },
    PackTagSpec {
        id: 12,
        name: "targetHae",
        // §10.2.2.13 — IMAPB(-900, 19000, 2), units m above WGS84.
        encoding: PackEncoding::ImapbF64 {
            min: -900.0,
            max: 19000.0,
        },
    },
    PackTagSpec {
        id: 13,
        name: "boundingBoxTopLeftLatOffset",
        // §10.2.2.14 — IMAPB(-19.2, 19.2, 3), units °.
        encoding: PackEncoding::ImapbF64 {
            min: -19.2,
            max: 19.2,
        },
    },
    PackTagSpec {
        id: 14,
        name: "boundingBoxTopLeftLonOffset",
        // §10.2.2.15 — IMAPB(-19.2, 19.2, 3), units °.
        encoding: PackEncoding::ImapbF64 {
            min: -19.2,
            max: 19.2,
        },
    },
    PackTagSpec {
        id: 15,
        name: "boundingBoxBottomRightLatOffset",
        // §10.2.2.16 — IMAPB(-19.2, 19.2, 3), units °.
        encoding: PackEncoding::ImapbF64 {
            min: -19.2,
            max: 19.2,
        },
    },
    PackTagSpec {
        id: 16,
        name: "boundingBoxBottomRightLonOffset",
        // §10.2.2.17 — IMAPB(-19.2, 19.2, 3), units °.
        encoding: PackEncoding::ImapbF64 {
            min: -19.2,
            max: 19.2,
        },
    },
    PackTagSpec {
        id: 17,
        name: "targetLocation",
        // §10.2.2.18 — Location, Defined Length Truncation Pack (V).
        // Typed inner layer deferred; pass-through bytes for now.
        encoding: PackEncoding::RawBytes,
    },
    PackTagSpec {
        id: 18,
        name: "geospatialContourSeries",
        // §10.2.2.19 — BoundarySeries (Series of Location). Typed
        // inner layer deferred.
        encoding: PackEncoding::RawBytes,
    },
    PackTagSpec {
        id: 19,
        name: "centroidPixRow",
        // §10.2.2.20 — uint V4 (1..=2^32-1).
        encoding: PackEncoding::VarUint { max_bytes: 4 },
    },
    PackTagSpec {
        id: 20,
        name: "centroidPixCol",
        // §10.2.2.21 — uint V4 (1..=2^32-1).
        encoding: PackEncoding::VarUint { max_bytes: 4 },
    },
    // Tag 21 is DEPRECATED in ST 0903.6 (§10.2.2.22). Decoders treat
    // any wire occurrence as an unknown tag (preserved in `unknown`
    // per ST 0107.5 §6); encoders do not emit it.
    PackTagSpec {
        id: 22,
        name: "algorithmId",
        // §10.2.2.23 — uint V3, references an Id from Algorithm Series.
        encoding: PackEncoding::VarUint { max_bytes: 3 },
    },
    PackTagSpec {
        id: 23,
        name: "detectionStatus",
        // §10.2.2.24 — uint, fixed length 1 (5 enumerated states +
        // Inactive). Length confirmed by §7.2 lifecycle table.
        encoding: PackEncoding::U8,
    },
    PackTagSpec {
        id: 101,
        name: "vMask",
        // §10.2.2.25 — VMask LS (V). Typed inner layer deferred.
        encoding: PackEncoding::RawBytes,
    },
    // Tags 102 and 103 are DEPRECATED in ST 0903.6 (§10.2.2.26 /
    // §10.2.2.27). Decoders treat any wire occurrence as an unknown
    // tag; encoders do not emit them.
    PackTagSpec {
        id: 104,
        name: "vTracker",
        // §10.2.2.28 — VTracker LS (V). Typed inner layer deferred.
        encoding: PackEncoding::RawBytes,
    },
    PackTagSpec {
        id: 105,
        name: "vChip",
        // §10.2.2.29 — VChip LS (V). Typed inner layer deferred.
        encoding: PackEncoding::RawBytes,
    },
    PackTagSpec {
        id: 106,
        name: "vChipSeries",
        // §10.2.2.30 — Series of VChip LS (V). Typed inner layer
        // deferred.
        encoding: PackEncoding::RawBytes,
    },
    PackTagSpec {
        id: 107,
        name: "vObjectSeries",
        // §10.2.2.31 — Series of VObject LS (V). Typed inner layer
        // deferred.
        encoding: PackEncoding::RawBytes,
    },
];

pub(crate) fn pack_lookup(tag: u8) -> Option<&'static PackTagSpec> {
    PACK_TAGS.iter().find(|t| t.id == tag)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum VTargetPackError {
    #[error("truncated BER-OID Target ID")]
    TruncatedTargetId,
    /// Field-level framing or value truncation. `tag` is BER-OID-
    /// decoded (`u32`) to cover both the §10.2 typed universe (1..=107)
    /// and forward-compat ST 0903.7+ multi-byte BER-OID tag IDs.
    #[error("VTargetPack tag {tag}: truncated value")]
    TruncatedField { tag: u32 },
    #[error("VTargetPack tag {tag}: declared length {declared} exceeds available {available}")]
    LengthOverrun {
        tag: u32,
        declared: usize,
        available: usize,
    },
    #[error("VTargetPack tag {tag}: malformed IMAPB value")]
    MalformedImapb { tag: u32 },
    #[error("VTargetPack tag {tag}: invalid value length {got} (expected {expected})")]
    InvalidLength {
        tag: u32,
        expected: usize,
        got: usize,
    },
}

crate::binding_record! {
    /// MISB ST 0903.6 §10.2 Table 10 — typed VTargetPack.
    ///
    /// The wire form is a leading BER-OID-encoded `targetId` (no Tag —
    /// per §10.2.2.1 "has no Tag") followed by a Local Set–encoded body
    /// using BER-OID tag + BER short/long length + value tuples.
    ///
    /// `target_id` is a `u64` decoded from the BER-OID Target ID (≈ 63 bits
    /// per ST 0903.6 §10.2.2.1; spec allows up to 9 wire bytes, V9 in
    /// §10.2 Table 10). The pixel-number fields are `u64`: the V6 fields
    /// (`centroid_pixel`, `bbox_top_left_pixel`, `bbox_bottom_right_pixel`)
    /// carry up to 6 wire bytes; the V4 `centroid_pix_row` /
    /// `centroid_pix_col` are 4 wire bytes (see their per-field notes for the
    /// 32-bit ceiling).
    ///
    /// `detection_status` is the raw §10.2.2.24 / §7.2 Table 5 codepoint:
    /// 0=Inactive, 1=Active-Moving, 2=Dropped, 3=Active-Stopped,
    /// 4=Active-Coasting. Typed enum deferred — stays as raw `u8`.
    ///
    /// 7 nested/sibling Local Sets (`target_location`,
    /// `geospatial_contour_series`, `vmask`, `vtracker`, `vchip`,
    /// `vchip_series`, `vobject_series`) stay as optional pass-through
    /// bytes — typed inner layers deferred.
    #[must_use]
    #[derive(Debug, Clone, Default)]
    pub struct VTargetPack {
        /// BER-OID `targetId` per §10.2.2.1. Up to 9 BER-OID wire bytes
        /// (≈ 63 bits) per ST 0903.6 §10.2.2.1; widened to `u64`.
        pub target_id: u64,
        /// Tag 1 `targetCentroid` per §10.2.2.2 — pixel number, V6
        /// truncated big-endian (up to 6 wire bytes, max `(1u64 << 48) - 1`).
        /// A value above `(1u64 << 48) - 1` would encode to 7+ bytes and the
        /// decoder rejects — callers must keep this within the V6 range.
        pub centroid_pixel: Option<u64>,
        /// Tag 2 `boundingBoxTopLeft` per §10.2.2.3 — pixel number, V6
        /// (up to 6 wire bytes, max `(1u64 << 48) - 1`). Same V6 cap as Tag 1.
        pub bbox_top_left_pixel: Option<u64>,
        /// Tag 3 `boundingBoxBottomRight` per §10.2.2.4 — pixel number, V6
        /// (up to 6 wire bytes, max `(1u64 << 48) - 1`). Same V6 cap as Tag 1.
        pub bbox_bottom_right_pixel: Option<u64>,
        /// Tag 4 `targetPriority` per §10.2.2.5 — fixed-length 1, valid 1..=255.
        pub priority: Option<u8>,
        /// Tag 5 `targetConfidenceLevel` per §10.2.2.6 — fixed-length 1,
        /// valid 0..=100 (percent).
        pub confidence_level: Option<u8>,
        /// Tag 6 `targetHistory` per §10.2.2.7 — V2 (frame count, 0..=65535).
        pub history: Option<u16>,
        /// Tag 7 `percentageOfTargetPixels` per §10.2.2.8 — fixed-length 1,
        /// valid 1..=100 (percent).
        pub percentage_of_target_pixels: Option<u8>,
        /// Tag 8 `targetColor` per §10.2.2.9 — fixed 3 bytes [R, G, B].
        @[custom]
        pub target_color: Option<[u8; 3]>,
        /// Tag 9 `targetIntensity` per §10.2.2.10 — V3 (24-bit dynamic range,
        /// max `(1u32 << 24) - 1` = 16777215). A value above `(1u32 << 24) - 1`
        /// would encode to 4 bytes but the decoder rejects (V3 max 3 bytes) —
        /// callers must keep this within the V3 range.
        pub target_intensity: Option<u32>,
        /// Tag 10 `targetLocationOffsetLat` per §10.2.2.11 —
        /// IMAPB(-19.2°, 19.2°, 3 bytes).
        pub centroid_lat_offset: Option<f64>,
        /// Tag 11 `targetLocationOffsetLon` per §10.2.2.12 — IMAPB(-19.2°, 19.2°, 3).
        pub centroid_lon_offset: Option<f64>,
        /// Tag 12 `targetHae` per §10.2.2.13 — IMAPB(-900 m, 19000 m, 2).
        pub centroid_hae: Option<f64>,
        /// Tag 13 `boundingBoxTopLeftLatOffset` per §10.2.2.14 —
        /// IMAPB(-19.2°, 19.2°, 3).
        pub bbox_top_left_lat_offset: Option<f64>,
        /// Tag 14 `boundingBoxTopLeftLonOffset` per §10.2.2.15 — IMAPB(-19.2°, 19.2°, 3).
        pub bbox_top_left_lon_offset: Option<f64>,
        /// Tag 15 `boundingBoxBottomRightLatOffset` per §10.2.2.16 —
        /// IMAPB(-19.2°, 19.2°, 3).
        pub bbox_bottom_right_lat_offset: Option<f64>,
        /// Tag 16 `boundingBoxBottomRightLonOffset` per §10.2.2.17 —
        /// IMAPB(-19.2°, 19.2°, 3).
        pub bbox_bottom_right_lon_offset: Option<f64>,
        /// Tag 17 `targetLocation` per §10.2.2.18 — Defined Length
        /// Truncation Pack pass-through bytes (typed inner deferred).
        pub target_location: Option<Vec<u8>>,
        /// Tag 18 `geospatialContourSeries` per §10.2.2.19 — Series of
        /// Location pass-through bytes (typed inner deferred).
        pub geospatial_contour_series: Option<Vec<u8>>,
        /// Tag 19 `centroidPixRow` per §10.2.2.20 — V4 (1..=2^32-1);
        /// widened to `u64` for consistency with other pixel fields. Note:
        /// the V4 wire format is 4 bytes (max 4294967295); a value above
        /// that would encode but fail to decode — callers must keep
        /// this within the V4 range.
        pub centroid_pix_row: Option<u64>,
        /// Tag 20 `centroidPixCol` per §10.2.2.21 — V4 (1..=2^32-1);
        /// widened to `u64` for consistency with other pixel fields. Note:
        /// the V4 wire format is 4 bytes (max 4294967295); a value above
        /// that would encode but fail to decode — callers must keep
        /// this within the V4 range.
        pub centroid_pix_col: Option<u64>,
        /// Tag 22 `algorithmId` per §10.2.2.23 — V3 reference into the
        /// parent VMTI LS Algorithm Series (max `(1u32 << 24) - 1`). Same V3
        /// cap as Tag 9 — values above 16777215 encode to 4 bytes and the
        /// decoder rejects.
        pub algorithm_id: Option<u32>,
        /// Tag 23 `detectionStatus` per §10.2.2.24 — fixed-length 1.
        /// See struct-level doc for the 5 spec codepoints.
        pub detection_status: Option<u8>,
        /// Tag 101 `vMask` per §10.2.2.25 — VMask LS pass-through bytes
        /// (typed inner deferred).
        pub vmask: Option<Vec<u8>>,
        /// Tag 104 `vTracker` per §10.2.2.28 — VTracker LS pass-through
        /// bytes (typed inner deferred).
        pub vtracker: Option<Vec<u8>>,
        /// Tag 105 `vChip` per §10.2.2.29 — VChip LS pass-through bytes
        /// (typed inner deferred).
        pub vchip: Option<Vec<u8>>,
        /// Tag 106 `vChipSeries` per §10.2.2.30 — Series of VChip LS
        /// pass-through bytes (typed inner deferred).
        pub vchip_series: Option<Vec<u8>>,
        /// Tag 107 `vObjectSeries` per §10.2.2.31 — Series of VObject
        /// LS pass-through bytes (typed inner deferred).
        pub vobject_series: Option<Vec<u8>>,
        /// Tags not in `PACK_TAGS` (deprecated 21/102/103, future
        /// additions). Preserved per ST 0107.5 §6 future-proof skip rule.
        @[custom]
        pub unknown: Vec<crate::klv::pack::OwnedRawField>,
        /// Per-field validation errors (e.g. malformed IMAPB) accumulated
        /// during lenient decode. The other typed sets collect the same
        /// diagnostic in their own `field_errors`.
        @[custom]
        pub field_errors: Vec<crate::error::KlvFieldError>,
    }
}

/// Manual `PartialEq` excluding [`VTargetPack::field_errors`]. Same
/// rationale as [`super::super::VmtiLs`]'s manual impl — `field_errors` is a
/// decode-side diagnostic, not part of the pack's semantic value.
/// Required for the ST 0903 round-trip fuzz target since `VTargetPack`
/// participates in `VmtiLs::targets`.
impl PartialEq for VTargetPack {
    fn eq(&self, other: &Self) -> bool {
        self.target_id == other.target_id
            && self.centroid_pixel == other.centroid_pixel
            && self.bbox_top_left_pixel == other.bbox_top_left_pixel
            && self.bbox_bottom_right_pixel == other.bbox_bottom_right_pixel
            && self.priority == other.priority
            && self.confidence_level == other.confidence_level
            && self.history == other.history
            && self.percentage_of_target_pixels == other.percentage_of_target_pixels
            && self.target_color == other.target_color
            && self.target_intensity == other.target_intensity
            && self.centroid_lat_offset == other.centroid_lat_offset
            && self.centroid_lon_offset == other.centroid_lon_offset
            && self.centroid_hae == other.centroid_hae
            && self.bbox_top_left_lat_offset == other.bbox_top_left_lat_offset
            && self.bbox_top_left_lon_offset == other.bbox_top_left_lon_offset
            && self.bbox_bottom_right_lat_offset == other.bbox_bottom_right_lat_offset
            && self.bbox_bottom_right_lon_offset == other.bbox_bottom_right_lon_offset
            && self.target_location == other.target_location
            && self.geospatial_contour_series == other.geospatial_contour_series
            && self.centroid_pix_row == other.centroid_pix_row
            && self.centroid_pix_col == other.centroid_pix_col
            && self.algorithm_id == other.algorithm_id
            && self.detection_status == other.detection_status
            && self.vmask == other.vmask
            && self.vtracker == other.vtracker
            && self.vchip == other.vchip
            && self.vchip_series == other.vchip_series
            && self.vobject_series == other.vobject_series
            && self.unknown == other.unknown
    }
}
