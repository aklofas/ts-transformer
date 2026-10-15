//! ST 0903.6 §10.1 Table 9 — top-level VMTI Local Set tag schema.
//!
//! The decoder and encoder loop this table. Adding or modifying a tag
//! is a one-entry change here.
//!
//! Wire-format conventions per ST 0903.6 §9.1 / §10.1:
//! - `V<n>` (e.g. V2, V3, V32, V128) means a variable-length encoding
//!   of 1..=n bytes. For uints, the wire bytes are the natural
//!   big-endian encoding with leading zero bytes stripped — *not*
//!   BER-OID. For UTF-8, the byte-count cap is `n` *characters*
//!   (UTF-8 expansion may produce up to 4·n bytes; see §10.1.3 +
//!   §10.1.10).
//! - `IMAPB` is decoded via `klv::imapb` over the linear range stated
//!   in the per-item description (§10.1.11/§10.1.12 fix [0, 180]°).

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Encoding {
    /// Raw 2-byte big-endian unsigned (Tag 1 Checksum, fixed length 2
    /// per §10.1.1).
    U16Be,
    /// Raw 8-byte big-endian unsigned (Tag 2 Precision Time Stamp, µs
    /// since UNIX epoch, fixed length 8 per §10.1.2).
    U64Be,
    /// Variable-length truncated big-endian unsigned (`V<n>` per
    /// §9.1) — value's natural BE encoding with leading zero bytes
    /// stripped, length 1..=`max_bytes`. Used for Tag 4 (V2) and
    /// Tags 5/6/8/9 (V3).
    VarUint { max_bytes: u8 },
    /// UTF-8 string. `max_chars` is the spec's character cap (§10.1.3
    /// V32 = 32 chars, §10.1.10 V128 = 128 chars). Wire byte count may
    /// exceed `max_chars` because UTF-8 allows up to 4 bytes per code
    /// point (§10.1.3 notes 4× expansion).
    Utf8 { max_chars: usize },
    /// IMAPB-encoded floating-point with linear range. Wire form is
    /// `length` raw bytes mapped via `klv::imapb::decode`.
    ImapbF64 { min: f64, max: f64 },
    /// Raw bytes (variable length); pass-through. Used for nested LSes
    /// and Series payloads (Tags 13, 101, 102, 103). The vTargetSeries
    /// (Tag 101) inner is parsed in a second pass after the lenient
    /// decode walks the LS.
    RawBytes,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TagSpec {
    pub id: u8,
    pub name: &'static str,
    pub encoding: Encoding,
    /// True if ST 0903.6 §10.1 marks this tag as unconditionally
    /// required across every VMTI LS instance (standalone *and*
    /// embedded). Tags whose presence is conditional on
    /// standalone-vs-embedded or VMTI-MI vs user-MI relationships
    /// (Tags 1, 2, 11, 12, 13) are NOT marked required here — strict
    /// validation of those rules belongs to the decoder body, which
    /// has the surrounding context this static table lacks.
    pub required: bool,
}

/// ST 0903.6 §10.1 Table 9 (VMTI LS) — items in numeric tag order,
/// with deprecated Tag 7 omitted. Tag 13 (`miisId`) replaces the
/// design-doc placeholder Tag 104 (no Tag 104 exists in v6).
pub(crate) const TAGS: &[TagSpec] = &[
    TagSpec {
        id: 1,
        name: "Checksum",
        encoding: Encoding::U16Be,
        // Conditional — required only for standalone-VMTI
        // (ST 0903.6-119); prohibited for embedded-VMTI
        // (ST 0903.6-120). Decoder enforces both rules with the
        // standalone/embedded context.
        required: false,
    },
    TagSpec {
        id: 2,
        name: "Precision Time Stamp",
        encoding: Encoding::U64Be,
        // Conditional — required only for standalone-VMTI
        // (ST 0903.6-117); embedded-VMTI may omit when parent supplies
        // the timestamp (§10 prelude).
        required: false,
    },
    TagSpec {
        id: 3,
        name: "VMTI System Name",
        encoding: Encoding::Utf8 { max_chars: 32 },
        required: false,
    },
    TagSpec {
        id: 4,
        name: "VMTI LS Version Number",
        // V2 per §10.1.4 — values 1..=65535 packed BE with leading
        // zeros stripped (example: 6 → 0x06, length 1).
        encoding: Encoding::VarUint { max_bytes: 2 },
        // ST 0903.5-99 — unconditionally required in all VMTI LS
        // instances.
        required: true,
    },
    TagSpec {
        id: 5,
        name: "Total Number of Targets Detected",
        encoding: Encoding::VarUint { max_bytes: 3 },
        required: false,
    },
    TagSpec {
        id: 6,
        name: "Number of Targets Reported",
        encoding: Encoding::VarUint { max_bytes: 3 },
        // ST 0903.4-19 — "shall always be specified".
        required: true,
    },
    // Tag 7 (motionImageryFrameNumber) is DEPRECATED in ST 0903.6
    // (§10.1.7). Decoders treat any wire occurrence as an unknown
    // tag (preserved in `unknown` per ST 0107.5 §6); encoders do
    // not emit it.
    TagSpec {
        id: 8,
        name: "Frame Width",
        encoding: Encoding::VarUint { max_bytes: 3 },
        required: false,
    },
    TagSpec {
        id: 9,
        name: "Frame Height",
        encoding: Encoding::VarUint { max_bytes: 3 },
        required: false,
    },
    TagSpec {
        id: 10,
        name: "VMTI Source Sensor",
        encoding: Encoding::Utf8 { max_chars: 128 },
        required: false,
    },
    TagSpec {
        id: 11,
        name: "VMTI Horizontal FOV",
        // §10.1.11 — IMAPB(0, 180, 2), units °.
        encoding: Encoding::ImapbF64 {
            min: 0.0,
            max: 180.0,
        },
        // Conditional — required for standalone-VMTI or when the
        // VMTI-MI differs from the user-MI (ST 0903.6-122 / ST
        // 0903.4-26). Otherwise the parent LS supplies HFOV.
        required: false,
    },
    TagSpec {
        id: 12,
        name: "VMTI Vertical FOV",
        // §10.1.12 — IMAPB(0, 180, 2), units °.
        encoding: Encoding::ImapbF64 {
            min: 0.0,
            max: 180.0,
        },
        // Conditional — see Tag 11.
        required: false,
    },
    TagSpec {
        id: 13,
        name: "MIIS Core Identifier",
        // §10.1.13 — opaque MISB ST 1204 conformant identifier.
        encoding: Encoding::RawBytes,
        // Conditional — required for standalone-VMTI or when
        // VMTI-MI ≠ user-MI (ST 0903.6-124 / ST 0903.6-125).
        required: false,
    },
    TagSpec {
        id: 101,
        name: "VTarget Series",
        encoding: Encoding::RawBytes,
        required: false,
    },
    TagSpec {
        id: 102,
        name: "Algorithm Series",
        encoding: Encoding::RawBytes,
        required: false,
    },
    TagSpec {
        id: 103,
        name: "Ontology Series",
        encoding: Encoding::RawBytes,
        required: false,
    },
];

pub(crate) fn lookup(tag: u8) -> Option<&'static TagSpec> {
    TAGS.iter().find(|t| t.id == tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_prelude::*;

    #[test]
    fn tags_table_has_unique_ids() {
        let mut ids: Vec<u8> = TAGS.iter().map(|t| t.id).collect();
        ids.sort();
        let len_before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), len_before, "duplicate tag IDs in TAGS");
    }

    #[test]
    fn tags_table_lookup_round_trips() {
        for tag in TAGS {
            assert_eq!(lookup(tag.id), Some(tag));
        }
        assert_eq!(lookup(0), None);
        assert_eq!(lookup(255), None);
        // Deprecated Tag 7 is intentionally absent.
        assert_eq!(lookup(7), None);
        // Tag 104 from the provisional design doc does not exist in
        // ST 0903.6 — MIIS Core ID is Tag 13.
        assert_eq!(lookup(104), None);
    }

    #[test]
    fn required_tags_match_spec() {
        let required: Vec<u8> = TAGS.iter().filter(|t| t.required).map(|t| t.id).collect();
        // Per ST 0903.6: only Tag 4 (vmtiLsVersionNum, ST 0903.5-99)
        // and Tag 6 (numTargetsReported, ST 0903.4-19) are
        // unconditionally required across both standalone- and
        // embedded-VMTI carriage. Tags 1/2/11/12/13 are conditional
        // on standalone-vs-embedded context and are validated in the
        // decoder body, not via this static `required` flag.
        assert_eq!(required, vec![4, 6]);
    }
}
