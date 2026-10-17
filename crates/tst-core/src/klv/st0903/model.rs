//! ST 0903 typed model — `VmtiLs` flat struct and its manual `PartialEq`.

use crate::error::KlvFieldError;
use crate::klv::pack::OwnedRawField;
use crate::klv::st0903::vtarget_pack::VTargetPack;
use alloc::string::String;
use alloc::vec::Vec;

crate::binding_record! {
    /// MISB ST 0903.6 VMTI (Video Moving Target Indicator) Local Set typed view.
    ///
    /// Required tags per ST 0903.6 §6 Table 1: `precision_time_stamp`,
    /// `vmti_system_name` (when applicable), `version_number`, `frame_width`,
    /// `frame_height`. Lenient decode tolerates missing required tags and
    /// surfaces per-field decode failures in `field_errors`; strict decode
    /// rejects missing required tags.
    ///
    /// `algorithm_series` and `ontology_series` are top-level nested LS
    /// pass-through bytes (typed inner layers deferred); `miis_id` is the MISB
    /// ST 1204 Minor Item Identification System Core Identifier, also
    /// pass-through bytes (typed layer deferred).
    #[must_use]
    #[derive(Debug, Clone, Default)]
    pub struct VmtiLs {
        /// Tag 1 (checkSum) per ST 0903.6 §10.1.1. Populated by [`decode`]
        /// for observability on standalone-VMTI captures.
        ///
        /// **Encode-side semantics are asymmetric and load-bearing:**
        /// - [`encode`] / [`encode_to_vec`] (embedded-VMTI body) — this
        ///   field is **always dropped** per ST 0903.6-120.
        /// - [`encode_standalone`] / [`encode_to_vec_standalone`]
        ///   (standalone-VMTI) — this field is **ignored**; the encoder
        ///   computes the running 16-bit checksum from the UL + body
        ///   framing and emits Tag 1 last per ST 0903.4-17 / ST 0903.6-119.
        ///
        /// Callers who decode + re-encode a captured standalone-VMTI get
        /// a fresh substrate-computed checksum, not a passthrough of the
        /// original.
        ///
        /// [`decode`]: crate::klv::st0903::decode
        /// [`encode`]: crate::klv::st0903::encode
        /// [`encode_to_vec`]: crate::klv::st0903::encode_to_vec
        /// [`encode_standalone`]: crate::klv::st0903::encode_standalone
        /// [`encode_to_vec_standalone`]: crate::klv::st0903::encode_to_vec_standalone
        pub checksum: Option<u16>,
        pub precision_time_stamp: Option<u64>,
        pub vmti_system_name: Option<String>,
        /// Tag 4 `vmtiLsVersionNum` per ST 0903.6 §10.1.4 — V2 (1..=2 wire
        /// bytes, value range 1..=65535) packed BE with leading zeros
        /// stripped. Tracks the spec revision the encoder followed (e.g.
        /// 6 for ST 0903.6).
        pub version_number: Option<u16>,
        pub total_targets_in_frame: Option<u32>,
        pub num_targets_reported: Option<u32>,
        // Tag 7 (motionImageryFrameNumber) is deprecated in ST 0903.6 — no
        // typed field. Wire occurrences land in `unknown` per ST 0107.5 §6.
        pub frame_width: Option<u32>,
        pub frame_height: Option<u32>,
        pub source_sensor: Option<String>,
        pub horizontal_fov: Option<f64>,
        pub vertical_fov: Option<f64>,
        pub miis_id: Option<Vec<u8>>,
        pub targets: Vec<VTargetPack>,
        pub algorithm_series: Option<Vec<u8>>,
        pub ontology_series: Option<Vec<u8>>,
        @[custom] pub unknown: Vec<OwnedRawField>,
        @[custom] pub field_errors: Vec<KlvFieldError>,
    }
}

/// Manual `PartialEq` excluding [`VmtiLs::field_errors`]. The
/// `field_errors` vec is a decode-side diagnostic — two LSes that
/// produced identical field values are semantically equal regardless
/// of which fields failed strict-decode validation along the way.
/// Used by round-trip fuzz (decode → encode → decode → assert_eq);
/// `field_errors` is empty on the second decode since encode never
/// emits malformed bytes.
impl PartialEq for VmtiLs {
    fn eq(&self, other: &Self) -> bool {
        self.checksum == other.checksum
            && self.precision_time_stamp == other.precision_time_stamp
            && self.vmti_system_name == other.vmti_system_name
            && self.version_number == other.version_number
            && self.total_targets_in_frame == other.total_targets_in_frame
            && self.num_targets_reported == other.num_targets_reported
            && self.frame_width == other.frame_width
            && self.frame_height == other.frame_height
            && self.source_sensor == other.source_sensor
            && self.horizontal_fov == other.horizontal_fov
            && self.vertical_fov == other.vertical_fov
            && self.miis_id == other.miis_id
            && self.targets == other.targets
            && self.algorithm_series == other.algorithm_series
            && self.ontology_series == other.ontology_series
            && self.unknown == other.unknown
    }
}
