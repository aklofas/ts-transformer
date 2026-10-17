"""tstrans.klv — KLV typed sets (ST 0601, ST 0102, ST 0605, ST 0805, ST 0806,
ST 0903, ST 1010, ST 1204).

Decode surface:

- `TimeStatus` — ST 0603 §7.4 time-status byte wrapper
- `PrecisionTimeStampPack` (alias `Klv0605`) — ST 0605 §7 pack
- `SecurityClassification`, `ClassifyingCountryCodingMethod`,
  `ObjectCountryCodingMethod` — ST 0102 §6.1 enums
- `SecurityLs` (alias `Klv0102`) — ST 0102 Security Metadata LS
- `VTargetPack` — ST 0903 §10.2 per-target pack
- `VmtiLs` (alias `Klv0903`) — ST 0903 VMTI LS
- `GeoPoint`, `Attitude`, `FieldOfView`, `Corners` — ST 0601 composites
- `UasDatalinkLs` (alias `Klv0601`) — ST 0601 UAS Datalink LS
- `IdType` — ST 1204.3 sensor/platform UUID source type enum
- `CoreId` — ST 1204.3 MIIS Core Identifier
- `ImageHorizonPixels`, `ControlCommand`, `SensorFrameRate`,
  `MetadataSubstreamId`, `CountryCodes`, `WavelengthRecord`, `Location`,
  `AirbaseLocations`, `PayloadType`, `PayloadRecord`, `PayloadList`,
  `WeaponsStore`, `Waypoint`, `ViewDomainPair`, `ViewDomain`,
  `SdccFlpField` — ST 0601 §8 pack & list items (carried on
  `UasDatalinkLs`)
- `SdccFlp` — MISB ST 1010.3 SDCC-FLP pack (general-purpose; decode with
  `decode_sdcc_flp`, encode Mode 2 with `encode_sdcc_flp_mode2`)
- `MismmsViolation` — ST 0902.8 Minimum Metadata Set violation
- `IcingDetected`, `SensorFovName`, `OperationalMode` — ST 0601 §8.34/
  §8.63/§8.77 coded-value enums
- `st0601_sentinel_meaning` — ST 0601.19 INT_MIN sentinel meaning lookup
  (Out of Range / Reserved / N/A) for a given tag
- `KlvFieldError`, `KlvFieldErrorKind` — non-fatal per-field errors
- `decode_uas_datalink`, `decode_security`, `decode_precision_timestamp`,
  `decode_vmti`, `decode_core_id` — per-set entry points
- `encode_core_id`, `core_id_text` — ST 1204 encode + textual format
- `validate_mismms` — ST 0902.8 record-level MISMMS validator
- `parse_klv_universal` — UL-dispatching universal entry point
- `ST_0601_UL`, `SECURITY_LS_UL`, `PRECISION_TIMESTAMP_PACK_UL`,
  `VMTI_LS_UL` — well-known 16-byte UL constants
- `is_st0601_family` — predicate for the ST 0601 UL family
  (tolerates legacy non-zero byte 13 + byte 14)

Symmetric `encode_*` entry points live alongside the decoders.
"""

import enum
from dataclasses import dataclass, replace


class KlvFieldErrorKind(enum.Enum):
    """Mirrors Rust's `tst_core::error::KlvFieldError` variants.

    Field-level decode failures are recoverable — they do NOT raise;
    they accumulate on the typed-set object's `.field_errors` list
    ("best-effort parse" semantics for ST 0601 in the
    field). Marked `#[non_exhaustive]` on the Rust side; Python matchers
    should include a default arm."""

    OUT_OF_RANGE = "out_of_range"
    INVALID_UTF8 = "invalid_utf8"
    INVALID_UTF16 = "invalid_utf16"
    INVALID_LENGTH = "invalid_length"
    INVALID_CODEPOINT = "invalid_codepoint"
    TRUNCATED_FIELD = "truncated_field"
    UNSUPPORTED_IMAPB_LENGTH = "unsupported_imapb_length"
    INVALID_IMAPB_PARAMS = "invalid_imapb_params"


@dataclass(frozen=True, slots=True)
class KlvFieldError:
    """Non-fatal per-field decode failure. Each decoded typed-set
    object carries a `.field_errors: tuple[KlvFieldError, ...]`
    collecting these. `tag` is the BER-OID-decoded tag (u32 to cover
    forward-compat multi-byte BER-OID); `message` is the
    `Display` string from the Rust variant."""

    kind: KlvFieldErrorKind
    tag: int
    message: str


# ---------------------------------------------------------------------------
# Well-known SMPTE / MISB Universal Labels (16 bytes each)
# ---------------------------------------------------------------------------

# Per MISB ST 0601.19 §6.2 (PDF p.4) — bytes 13/14/15 all 0x00.
# UL CRC 56773.
ST_0601_UL: bytes = bytes.fromhex("060e2b34020b01010e01030101000000")

# Per MISB ST 0102.12 §6.7 — Security Metadata LS UL. CRC 40980.
SECURITY_LS_UL: bytes = bytes.fromhex("060e2b34020301010e01030302000000")

# Per MISB ST 0807.27 row 1061 — Precision Time Stamp Pack UL. CRC 23259.
PRECISION_TIMESTAMP_PACK_UL: bytes = bytes.fromhex("060e2b34020501010e01010311000000")

# Per MISB ST 0903.6 §10.1 — VMTI LS UL.
VMTI_LS_UL: bytes = bytes.fromhex("060e2b34020b01010e01030306000000")


def is_st0601_family(buf: bytes) -> bool:
    """Mirror of Rust `UniversalLabel::is_st0601_family`. Returns True
    iff `buf` is at least 16 bytes AND bytes 0..=12 match the canonical
    ST 0601 family prefix AND byte 15 is 0x00. Bytes 13 (document
    version byte) and 14 are tolerated for legacy capture interop
    per ST 0601.8-19's transitional rule."""

    if len(buf) < 16:
        return False
    canonical = bytes.fromhex("060e2b34020b01010e01030101")
    if buf[:13] != canonical:
        return False
    return buf[15] == 0x00


# ---------------------------------------------------------------------------
# ST 0605 §7 Precision Time Stamp Pack
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class TimeStatus:
    """Time Status byte per MISB ST 0603 §7.4 Table 3.

    - bit 7: 0 = Locked, 1 = Lock Unknown
    - bit 6: 0 = Normal, 1 = Discontinuity
    - bit 5: 0 = Forward, 1 = Reverse (only meaningful when bit 6=1)
    - bits 4-0: reserved, must be 0b11111"""

    raw: int

    def __post_init__(self) -> None:
        # The wire field is a single byte (0..=255); reject
        # out-of-range values early so callers see the construction site.
        if not 0 <= self.raw <= 0xFF:
            raise ValueError(f"TimeStatus.raw must be 0..=255; got {self.raw}")

    @property
    def is_locked(self) -> bool:
        """True if bit 7 = 0 (clock locked to absolute time reference)."""
        return (self.raw & 0x80) == 0

    @property
    def has_discontinuity(self) -> bool:
        """True if bit 6 = 1 (time has not incremented linearly forward)."""
        return (self.raw & 0x40) != 0

    @property
    def is_reverse_jump(self) -> bool:
        """True if bit 5 = 1 (only meaningful when `has_discontinuity` —
        indicates a backward time jump rather than forward)."""
        return (self.raw & 0x20) != 0

    @property
    def reserved_bits_valid(self) -> bool:
        """True if reserved bits 4-0 are the spec-required 0b11111."""
        return (self.raw & 0x1F) == 0x1F


# BEGIN GENERATED by print-records (PrecisionTimeStampPack); do not edit
@dataclass(frozen=True, slots=True)
class PrecisionTimeStampPack:
    """MISB ST 0605 §7 Precision Time Stamp Pack typed view.

    Wire form is 26 bytes: the 16-byte universal label, a 1-byte BER length,
    the 1-byte time status and the 8-byte big-endian microsecond timestamp.

    - ``timestamp_us`` — Microseconds since 1970-01-01T00:00:00Z (POSIX
      epoch), big-endian."""

    time_status: TimeStatus
    timestamp_us: int
# END GENERATED by print-records (PrecisionTimeStampPack)

    def with_(self, **changes: object) -> "PrecisionTimeStampPack":
        """Return a copy with the named fields replaced. The typed sets
        are frozen dataclasses (attribute assignment raises
        `FrozenInstanceError`) — this is the ergonomic update path.
        Thin wrapper over `dataclasses.replace`: unknown field names
        raise `TypeError`, and construction-time validation re-runs on
        the copy."""
        return replace(self, **changes)


# Short alias for `PrecisionTimeStampPack`.
Klv0605 = PrecisionTimeStampPack


# Re-export the Rust-side decode entry points. The Rust impls live in
# bindings/python/src/klv.rs and are exposed via `_native.decode_*`.
from tstrans import _native as _native_mod

decode_precision_timestamp = _native_mod.decode_precision_timestamp
encode_precision_timestamp = _native_mod.encode_precision_timestamp


# ---------------------------------------------------------------------------
# ST 0102.12 §6.1 enums
# ---------------------------------------------------------------------------


class SecurityClassification(enum.Enum):
    """ST 0102.12 §6.1.1 Tag 1 Security Classification.

    Rust adds an `Unknown(u8)` arm for forward-compat; on the Python
    side, unknown codepoints surface as the raw `int` on the
    `SecurityLs.security_classification` field rather than an enum
    instance (kept simple — the typed enum is the high-fidelity 90%
    path)."""

    UNCLASSIFIED = 0x01
    RESTRICTED = 0x02
    CONFIDENTIAL = 0x03
    SECRET = 0x04
    TOP_SECRET = 0x05


class ClassifyingCountryCodingMethod(enum.Enum):
    """ST 0102.12 §6.1.2 Tag 2 Classifying Country / Releasing
    Instructions Country Coding Method.

    Tag 2 and Tag 12 use DIFFERENT codepoints for the same logical
    coding method — see `ObjectCountryCodingMethod` for Tag 12 values.
    `OmittedValueXX` slots are spec-reserved; strict-mode decode
    rejects them."""

    ISO_3166_TWO_LETTER = 0x01
    ISO_3166_THREE_LETTER = 0x02
    FIPS_104_TWO_LETTER = 0x03
    FIPS_104_FOUR_LETTER = 0x04
    ISO_3166_NUMERIC = 0x05
    STANAG_1059_TWO_LETTER = 0x06
    STANAG_1059_THREE_LETTER = 0x07
    OMITTED_VALUE_08 = 0x08
    OMITTED_VALUE_09 = 0x09
    FIPS_104_MIXED = 0x0A
    ISO_3166_MIXED = 0x0B
    STANAG_1059_MIXED = 0x0C
    GENC_TWO_LETTER = 0x0D
    GENC_THREE_LETTER = 0x0E
    GENC_NUMERIC = 0x0F
    GENC_MIXED = 0x10


class ObjectCountryCodingMethod(enum.Enum):
    """ST 0102.12 §6.1.12 Tag 12 Object Country Coding Method.

    Codepoints differ from Tag 2 — the spec is non-contiguous and
    jumps to 0x40 for `GencAdminSub`."""

    ISO_3166_TWO_LETTER = 0x01
    ISO_3166_THREE_LETTER = 0x02
    ISO_3166_NUMERIC = 0x03  # vs Tag 2's 0x05
    FIPS_104_TWO_LETTER = 0x04  # vs Tag 2's 0x03
    FIPS_104_FOUR_LETTER = 0x05  # vs Tag 2's 0x04
    STANAG_1059_TWO_LETTER = 0x06
    STANAG_1059_THREE_LETTER = 0x07
    OMITTED_VALUE_08 = 0x08
    OMITTED_VALUE_09 = 0x09
    OMITTED_VALUE_0A = 0x0A
    OMITTED_VALUE_0B = 0x0B
    OMITTED_VALUE_0C = 0x0C
    GENC_TWO_LETTER = 0x0D
    GENC_THREE_LETTER = 0x0E
    GENC_NUMERIC = 0x0F
    GENC_ADMIN_SUB = 0x40


# ---------------------------------------------------------------------------
# ST 0102.12 Security Metadata LS
# ---------------------------------------------------------------------------


# BEGIN GENERATED by print-records (SecurityLs); do not edit
@dataclass(frozen=True, slots=True)
class SecurityLs:
    """MISB ST 0102.12 Security Metadata Local Set typed view.

    Required tags per ST 0102.12 §6.7 Table 1: 1, 2, 3, 12, 13 and 22.
    Lenient decode tolerates missing required tags, unknown enum codepoints
    and a malformed Tag 13 UTF-16 value (surfaced in ``field_errors``);
    strict decode rejects all of them.

    The three enum-typed fields (``security_classification``,
    ``classifying_country_coding_method`` and
    ``object_country_coding_method``) cross as their code point, so a
    forward-compat code point round-trips; a host with an enum type for the
    known code points may surface the member.

    - ``unknown`` — Forward-compat: tags outside the LS table preserved
      verbatim. Both the lenient and the strict decode populate this per ST
      0107.5 §6. A malformed Tag 13 value is not kept here (re-emitting
      malformed UTF-16 would help no consumer); it is recorded in
      ``field_errors``.
    - ``field_errors`` — Lenient-mode diagnosis: known tags whose value
      validation failed (e.g. Tag 13 UTF-16 decode failure, Tag 22 wrong
      length). Strict mode raises these as an error instead of populating
      this field. Encode does not consume this field."""

    security_classification: SecurityClassification | int | None = None
    classifying_country_coding_method: ClassifyingCountryCodingMethod | int | None = None
    classifying_country: str | None = None
    object_country_coding_method: ObjectCountryCodingMethod | int | None = None
    object_country_codes: str | None = None
    version: int | None = None
    sci_shi_info: str | None = None
    caveats: str | None = None
    releasing_instructions: str | None = None
    classified_by: str | None = None
    derived_from: str | None = None
    classification_reason: str | None = None
    declassification_date: str | None = None
    classification_marking_system: str | None = None
    classification_comments: str | None = None
    classifying_country_coding_method_version_date: str | None = None
    object_country_coding_method_version_date: str | None = None
    unknown: tuple[tuple[int, bytes], ...] = ()
    field_errors: tuple[KlvFieldError, ...] = ()
# END GENERATED by print-records (SecurityLs)

    def with_(self, **changes: object) -> "SecurityLs":
        """Return a copy with the named fields replaced. The typed sets
        are frozen dataclasses (attribute assignment raises
        `FrozenInstanceError`) — this is the ergonomic update path.
        Thin wrapper over `dataclasses.replace`: unknown field names
        raise `TypeError`, and construction-time validation re-runs on
        the copy."""
        return replace(self, **changes)


# Spec-compat alias.
Klv0102 = SecurityLs


decode_security = _native_mod.decode_security
encode_security = _native_mod.encode_security
encode_security_strict_compliance = _native_mod.encode_security_strict_compliance


# ---------------------------------------------------------------------------
# ST 0903.6 §10.2 VTargetPack (carried inside VmtiLs.targets)
# ---------------------------------------------------------------------------


# BEGIN GENERATED by print-records (VTargetPack); do not edit
@dataclass(frozen=True, slots=True)
class VTargetPack:
    """MISB ST 0903.6 §10.2 Table 10 — typed VTargetPack.

    The wire form is a leading BER-OID-encoded ``targetId`` (no Tag — per
    §10.2.2.1 "has no Tag") followed by a Local Set–encoded body using
    BER-OID tag + BER short/long length + value tuples.

    ``target_id`` is an unsigned 64-bit integer decoded from the BER-OID
    Target ID (≈ 63 bits per ST 0903.6 §10.2.2.1; spec allows up to 9 wire
    bytes, V9 in §10.2 Table 10). The pixel-number fields are unsigned
    64-bit integers too: the V6 fields (``centroid_pixel``,
    ``bbox_top_left_pixel``, ``bbox_bottom_right_pixel``) carry up to 6 wire
    bytes; the V4 ``centroid_pix_row`` / ``centroid_pix_col`` are 4 wire
    bytes (see their per-field notes for the 32-bit ceiling).

    ``detection_status`` is the raw §10.2.2.24 / §7.2 Table 5 codepoint:
    0=Inactive, 1=Active-Moving, 2=Dropped, 3=Active-Stopped,
    4=Active-Coasting. Typed enum deferred — stays as a raw byte.

    7 nested/sibling Local Sets (``target_location``,
    ``geospatial_contour_series``, ``vmask``, ``vtracker``, ``vchip``,
    ``vchip_series``, ``vobject_series``) stay as optional pass-through
    bytes — typed inner layers deferred.

    - ``target_id`` — BER-OID ``targetId`` per §10.2.2.1. Up to 9 BER-OID
      wire bytes (≈ 63 bits) per ST 0903.6 §10.2.2.1; widened to 64 bits.
    - ``centroid_pixel`` — Tag 1 ``targetCentroid`` per §10.2.2.2 — pixel
      number, V6 truncated big-endian (up to 6 wire bytes, max ``(1u64 <<
      48) - 1``). A value above ``(1u64 << 48) - 1`` would encode to 7+
      bytes and the decoder rejects — callers must keep this within the V6
      range.
    - ``bbox_top_left_pixel`` — Tag 2 ``boundingBoxTopLeft`` per §10.2.2.3 —
      pixel number, V6 (up to 6 wire bytes, max ``(1u64 << 48) - 1``). Same
      V6 cap as Tag 1.
    - ``bbox_bottom_right_pixel`` — Tag 3 ``boundingBoxBottomRight`` per
      §10.2.2.4 — pixel number, V6 (up to 6 wire bytes, max ``(1u64 << 48) -
      1``). Same V6 cap as Tag 1.
    - ``priority`` — Tag 4 ``targetPriority`` per §10.2.2.5 — fixed-length
      1, valid 1..=255.
    - ``confidence_level`` — Tag 5 ``targetConfidenceLevel`` per §10.2.2.6 —
      fixed-length 1, valid 0..=100 (percent).
    - ``history`` — Tag 6 ``targetHistory`` per §10.2.2.7 — V2 (frame count,
      0..=65535).
    - ``percentage_of_target_pixels`` — Tag 7 ``percentageOfTargetPixels``
      per §10.2.2.8 — fixed-length 1, valid 1..=100 (percent).
    - ``target_color`` — Tag 8 ``targetColor`` per §10.2.2.9 — fixed 3 bytes
      [R, G, B].
    - ``target_intensity`` — Tag 9 ``targetIntensity`` per §10.2.2.10 — V3
      (24-bit dynamic range, max ``(1u32 << 24) - 1`` = 16777215). A value
      above ``(1u32 << 24) - 1`` would encode to 4 bytes but the decoder
      rejects (V3 max 3 bytes) — callers must keep this within the V3 range.
    - ``centroid_lat_offset`` — Tag 10 ``targetLocationOffsetLat`` per
      §10.2.2.11 — IMAPB(-19.2°, 19.2°, 3 bytes).
    - ``centroid_lon_offset`` — Tag 11 ``targetLocationOffsetLon`` per
      §10.2.2.12 — IMAPB(-19.2°, 19.2°, 3).
    - ``centroid_hae`` — Tag 12 ``targetHae`` per §10.2.2.13 — IMAPB(-900 m,
      19000 m, 2).
    - ``bbox_top_left_lat_offset`` — Tag 13 ``boundingBoxTopLeftLatOffset``
      per §10.2.2.14 — IMAPB(-19.2°, 19.2°, 3).
    - ``bbox_top_left_lon_offset`` — Tag 14 ``boundingBoxTopLeftLonOffset``
      per §10.2.2.15 — IMAPB(-19.2°, 19.2°, 3).
    - ``bbox_bottom_right_lat_offset`` — Tag 15
      ``boundingBoxBottomRightLatOffset`` per §10.2.2.16 — IMAPB(-19.2°,
      19.2°, 3).
    - ``bbox_bottom_right_lon_offset`` — Tag 16
      ``boundingBoxBottomRightLonOffset`` per §10.2.2.17 — IMAPB(-19.2°,
      19.2°, 3).
    - ``target_location`` — Tag 17 ``targetLocation`` per §10.2.2.18 —
      Defined Length Truncation Pack pass-through bytes (typed inner
      deferred).
    - ``geospatial_contour_series`` — Tag 18 ``geospatialContourSeries`` per
      §10.2.2.19 — Series of Location pass-through bytes (typed inner
      deferred).
    - ``centroid_pix_row`` — Tag 19 ``centroidPixRow`` per §10.2.2.20 — V4
      (1..=2^32-1); widened to 64 bits for consistency with other pixel
      fields. Note: the V4 wire format is 4 bytes (max 4294967295); a value
      above that would encode but fail to decode — callers must keep this
      within the V4 range.
    - ``centroid_pix_col`` — Tag 20 ``centroidPixCol`` per §10.2.2.21 — V4
      (1..=2^32-1); widened to 64 bits for consistency with other pixel
      fields. Note: the V4 wire format is 4 bytes (max 4294967295); a value
      above that would encode but fail to decode — callers must keep this
      within the V4 range.
    - ``algorithm_id`` — Tag 22 ``algorithmId`` per §10.2.2.23 — V3
      reference into the parent VMTI LS Algorithm Series (max ``(1u32 << 24)
      - 1``). Same V3 cap as Tag 9 — values above 16777215 encode to 4 bytes
      and the decoder rejects.
    - ``detection_status`` — Tag 23 ``detectionStatus`` per §10.2.2.24 —
      fixed-length 1. See struct-level doc for the 5 spec codepoints.
    - ``vmask`` — Tag 101 ``vMask`` per §10.2.2.25 — VMask LS pass-through
      bytes (typed inner deferred).
    - ``vtracker`` — Tag 104 ``vTracker`` per §10.2.2.28 — VTracker LS
      pass-through bytes (typed inner deferred).
    - ``vchip`` — Tag 105 ``vChip`` per §10.2.2.29 — VChip LS pass-through
      bytes (typed inner deferred).
    - ``vchip_series`` — Tag 106 ``vChipSeries`` per §10.2.2.30 — Series of
      VChip LS pass-through bytes (typed inner deferred).
    - ``vobject_series`` — Tag 107 ``vObjectSeries`` per §10.2.2.31 — Series
      of VObject LS pass-through bytes (typed inner deferred).
    - ``unknown`` — Tags not in the pack tag table (deprecated 21/102/103,
      future additions). Preserved per ST 0107.5 §6 future-proof skip rule.
    - ``field_errors`` — Per-field validation errors (e.g. malformed IMAPB)
      accumulated during lenient decode. The other typed sets collect the
      same diagnostic in their own ``field_errors``."""

    target_id: int
    centroid_pixel: int | None = None
    bbox_top_left_pixel: int | None = None
    bbox_bottom_right_pixel: int | None = None
    priority: int | None = None
    confidence_level: int | None = None
    history: int | None = None
    percentage_of_target_pixels: int | None = None
    target_color: tuple[int, int, int] | None = None
    target_intensity: int | None = None
    centroid_lat_offset: float | None = None
    centroid_lon_offset: float | None = None
    centroid_hae: float | None = None
    bbox_top_left_lat_offset: float | None = None
    bbox_top_left_lon_offset: float | None = None
    bbox_bottom_right_lat_offset: float | None = None
    bbox_bottom_right_lon_offset: float | None = None
    target_location: bytes | None = None
    geospatial_contour_series: bytes | None = None
    centroid_pix_row: int | None = None
    centroid_pix_col: int | None = None
    algorithm_id: int | None = None
    detection_status: int | None = None
    vmask: bytes | None = None
    vtracker: bytes | None = None
    vchip: bytes | None = None
    vchip_series: bytes | None = None
    vobject_series: bytes | None = None
    unknown: tuple[tuple[int, bytes], ...] = ()
    field_errors: tuple[KlvFieldError, ...] = ()
# END GENERATED by print-records (VTargetPack)

    def __post_init__(self) -> None:
        # target_color encodes as a 3-byte RGB value on the
        # wire (ST 0903.6 §10.2.2.8 Tag 8, 3 bytes); validate at
        # construction so callers see the error at their code site rather
        # than at the encode boundary.
        if self.target_color is not None:
            tc = self.target_color
            if len(tc) != 3:
                raise ValueError(
                    f"VTargetPack.target_color must be a 3-tuple (R, G, B); "
                    f"got length {len(tc)}"
                )
            if not all(0 <= ch <= 255 for ch in tc):
                raise ValueError(
                    f"VTargetPack.target_color channels must be 0..=255; "
                    f"got {tc!r}"
                )


# ---------------------------------------------------------------------------
# ST 0903.6 VMTI Local Set
# ---------------------------------------------------------------------------


# BEGIN GENERATED by print-records (VmtiLs); do not edit
@dataclass(frozen=True, slots=True)
class VmtiLs:
    """MISB ST 0903.6 VMTI (Video Moving Target Indicator) Local Set typed
    view.

    Required tags per ST 0903.6 §6 Table 1: ``precision_time_stamp``,
    ``vmti_system_name`` (when applicable), ``version_number``,
    ``frame_width``, ``frame_height``. Lenient decode tolerates missing
    required tags and surfaces per-field decode failures in
    ``field_errors``; strict decode rejects missing required tags.

    ``algorithm_series`` and ``ontology_series`` are top-level nested LS
    pass-through bytes (typed inner layers deferred); ``miis_id`` is the
    MISB ST 1204 Minor Item Identification System Core Identifier, also
    pass-through bytes (typed layer deferred).

    - ``checksum`` — Tag 1 (checkSum) per ST 0903.6 §10.1.1. Populated on
      decode for observability on standalone-VMTI captures. Encode-side
      semantics are asymmetric and load-bearing: the embedded-VMTI body
      encoders always drop this field per ST 0903.6-120, while the
      standalone-VMTI encoders ignore it, compute the running 16-bit
      checksum from the UL + body framing and emit Tag 1 last per ST
      0903.4-17 / ST 0903.6-119. A captured standalone-VMTI that is decoded
      and re-encoded therefore gets a fresh computed checksum, not a
      passthrough of the original.
    - ``version_number`` — Tag 4 ``vmtiLsVersionNum`` per ST 0903.6 §10.1.4
      — V2 (1..=2 wire bytes, value range 1..=65535) packed BE with leading
      zeros stripped. Tracks the spec revision the encoder followed (e.g. 6
      for ST 0903.6)."""

    checksum: int | None = None
    precision_time_stamp: int | None = None
    vmti_system_name: str | None = None
    version_number: int | None = None
    total_targets_in_frame: int | None = None
    num_targets_reported: int | None = None
    frame_width: int | None = None
    frame_height: int | None = None
    source_sensor: str | None = None
    horizontal_fov: float | None = None
    vertical_fov: float | None = None
    miis_id: bytes | None = None
    targets: tuple[VTargetPack, ...] = ()
    algorithm_series: bytes | None = None
    ontology_series: bytes | None = None
    unknown: tuple[tuple[int, bytes], ...] = ()
    field_errors: tuple[KlvFieldError, ...] = ()
# END GENERATED by print-records (VmtiLs)

    def with_(self, **changes: object) -> "VmtiLs":
        """Return a copy with the named fields replaced. The typed sets
        are frozen dataclasses (attribute assignment raises
        `FrozenInstanceError`) — this is the ergonomic update path.
        Thin wrapper over `dataclasses.replace`: unknown field names
        raise `TypeError`, and construction-time validation re-runs on
        the copy."""
        return replace(self, **changes)


# Spec-compat alias.
Klv0903 = VmtiLs


decode_vmti = _native_mod.decode_vmti
encode_vmti = _native_mod.encode_vmti
encode_vmti_standalone = _native_mod.encode_vmti_standalone
encode_vmti_strict_compliance = _native_mod.encode_vmti_strict_compliance
encode_vmti_standalone_strict_compliance = _native_mod.encode_vmti_standalone_strict_compliance


# ---------------------------------------------------------------------------
# ST 0601 composite read-only views
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class GeoPoint:
    """Lat / lon / alt triple. ST 0601 surfaces multiple GeoPoints
    (sensor position, frame center). Altitude is meters AMSL unless
    the source field specifies WGS84 ellipsoid height."""

    lat_deg: float
    lon_deg: float
    alt_m: float


@dataclass(frozen=True, slots=True)
class Attitude:
    """3-axis attitude in degrees. ST 0601 surfaces multiple
    Attitudes (sensor relative az/el/roll, platform heading/pitch/roll)."""

    heading_deg: float
    pitch_deg: float
    roll_deg: float


@dataclass(frozen=True, slots=True)
class FieldOfView:
    """Horizontal + vertical sensor field-of-view in degrees."""

    horizontal_deg: float
    vertical_deg: float


@dataclass(frozen=True, slots=True)
class Corners:
    """Four corner lat/lon points (upper-left looking forward). Each
    corner is a `(lat_deg, lon_deg)` tuple."""

    p1: tuple[float, float]
    p2: tuple[float, float]
    p3: tuple[float, float]
    p4: tuple[float, float]


# ---------------------------------------------------------------------------
# ST 0601.19 coded enums (Items 34, 63, 77)
# ---------------------------------------------------------------------------


class IcingDetected(enum.Enum):
    """ST 0601.19 §8.34 Item 34 — icing-detector state (vibrating-probe
    ice detector at the aircraft location).

    Rust adds an `Other(u8)` catch-all for forward-compat; on the Python
    side, unknown codepoints surface as the raw `int` on
    `UasDatalinkLs.icing_detected` rather than an enum instance (same
    asymmetric pattern as `SecurityClassification`)."""

    DETECTOR_OFF = 0
    NO_ICING_DETECTED = 1
    ICING_DETECTED = 2


class SensorFovName(enum.Enum):
    """ST 0601.19 §8.63 Item 63 — named Motion Imagery sensor FOV preset.

    Spec discrepancy: the item's own definition table caps the KLV range
    at `[0, 7]`, but the §8.63.1 Table 4 worked example lists a 9th
    codepoint, `8` = Continuous Zoom. Modeled per Table 4 since real-world
    encoders emit it (see the Rust `SensorFovName` rustdoc for detail)."""

    ULTRANARROW = 0
    NARROW = 1
    MEDIUM = 2
    WIDE = 3
    ULTRAWIDE = 4
    NARROW_MEDIUM = 5
    TWO_X_ULTRANARROW = 6
    FOUR_X_ULTRANARROW = 7
    CONTINUOUS_ZOOM = 8


class OperationalMode(enum.Enum):
    """ST 0601.19 §8.77 Item 77 — operating mode of the event portrayed
    in the Motion Imagery, per the §8.77.1 Table 5 enumeration.

    Spec code `0` is named "Other" in Table 5; this binding names it
    `OTHER_MODE` to avoid colliding with the raw-int catch-all used for
    wire-unknown codepoints (mirrors the Rust `OtherMode` variant name)."""

    OTHER_MODE = 0
    OPERATIONAL = 1
    TRAINING = 2
    EXERCISE = 3
    MAINTENANCE = 4
    TEST = 5


# ---------------------------------------------------------------------------
# ST 0601.19 coded enums (Items 125, 126)
# ---------------------------------------------------------------------------


class PlatformStatus(enum.Enum):
    """ST 0601.19 §8.125 Item 125 — operational status of the platform."""

    ACTIVE = 0
    PRE_FLIGHT = 1
    PRE_FLIGHT_TAXIING = 2
    RUN_UP = 3
    TAKE_OFF = 4
    INGRESS = 5
    MANUAL_OPERATION = 6
    AUTOMATED_ORBIT = 7
    TRANSITIONING = 8
    EGRESS = 9
    LANDING = 10
    LANDED_TAXIING = 11
    LANDED_PARKED = 12


class SensorControlMode(enum.Enum):
    """ST 0601.19 §8.126 Item 126 — what is currently controlling the sensor."""

    OFF = 0
    HOME_POSITION = 1
    UNCONTROLLED = 2
    MANUAL_CONTROL = 3
    CALIBRATING = 4
    AUTO_HOLDING_POSITION = 5
    AUTO_TRACKING = 6


# ---------------------------------------------------------------------------
# ST 0601.19 pack & list items, carried on UasDatalinkLs.
# Follows the VTargetPack nested-struct-list pattern.
# ---------------------------------------------------------------------------


# BEGIN GENERATED by print-records (ImageHorizonPixels); do not edit
@dataclass(frozen=True, slots=True)
class ImageHorizonPixels:
    """Item 81: Image Horizon Pixels (ST 0601.19 §8.81) — screen-space
    horizon line endpoints as percentages of image width/height, plus an
    optional geodetic pair for each endpoint.

    Wire shape (DLP, ST 0107 truncatable-trailing convention): 4 mandatory
    percentage bytes, then 0-4 further int32 linear-mapped fields in strict
    order (``start_lat``, ``start_lon``, ``end_lat``, ``end_lon``) — the
    pack may end after any of them. A wire int32 exactly at the most
    negative int32 value (``0x80000000``) on any of the four trailing fields
    is the item's spec-defined "error" indicator (§8.81): that field decodes
    to ``None`` just like a truncated-away field, but the byte position IS
    consumed (parsing continues to the next field).

    **Encode semantics:** a ``None`` field that lies BEFORE the last
    ``Some`` field (in wire order) is re-encoded as the ``0x80000000``
    sentinel — the spec's own error indicator, which decode maps straight
    back to ``None`` — so a later ``Some`` field is never silently dropped
    just because an earlier optional field is absent. Only a ``None`` AFTER
    the last ``Some`` (or all four ``None``) truncates the pack there,
    matching decode's own truncation semantics: there is no wire difference
    between "never sent" and "sentinel would be redundant here" once the
    pack has ended."""

    x0_pct: int
    y0_pct: int
    x1_pct: int
    y1_pct: int
    start_lat_deg: float | None = None
    start_lon_deg: float | None = None
    end_lat_deg: float | None = None
    end_lon_deg: float | None = None
# END GENERATED by print-records (ImageHorizonPixels)


# BEGIN GENERATED by print-records (ControlCommand); do not edit
@dataclass(frozen=True, slots=True)
class ControlCommand:
    """Item 115: Control Command (ST 0601.19 §8.115) — one command sent to
    the platform. MULTI-INSTANCE per ST 0601.19 Table 1 ("Multiples Allowed"
    = Yes): every wire occurrence of Tag 115 appends one ``ControlCommand``
    to ``control_commands`` rather than overwriting a single field.

    - ``id`` — BER-OID command id.
    - ``command`` — Command text, UTF-8, at most 127 bytes.
    - ``time_us`` — Time the command was issued/executed, microseconds —
      MISB var-length truncatable trailing field (present iff the wire pack
      has trailing bytes after ``command``)."""

    id: int
    command: str
    time_us: int | None = None
# END GENERATED by print-records (ControlCommand)


# BEGIN GENERATED by print-records (SensorFrameRate); do not edit
@dataclass(frozen=True, slots=True)
class SensorFrameRate:
    """Item 127: Sensor Frame Rate Pack (ST 0601.19 §8.127) — frame rate as
    a numerator/denominator ratio; ``denominator`` defaults to 1 when the
    wire pack ends after the numerator.

    - ``numerator`` — BER-OID numerator.
    - ``denominator`` — BER-OID denominator; wire-absent means 1
      (whole-number fps)."""

    numerator: int
    denominator: int
# END GENERATED by print-records (SensorFrameRate)

    @property
    def fps(self) -> float:
        """Frames per second as ``numerator / denominator``."""
        return self.numerator / self.denominator


# BEGIN GENERATED by print-records (MetadataSubstreamId); do not edit
@dataclass(frozen=True, slots=True)
class MetadataSubstreamId:
    """Item 143: Metadata Substream Id (ST 0601.19 §8.143) — identifies
    which metadata substream this Local Set belongs to. Per §8.143, ``uuid``
    is REQUIRED when ``local_id == 0`` and OMITTED when ``local_id > 0``;
    this decoder is lenient and stores whatever combination the wire
    carries.

    - ``local_id`` — BER-OID local substream id.
    - ``uuid`` — RFC 4122 UUID bytes, present per the §8.143 rule above."""

    local_id: int
    uuid: bytes | None = None
# END GENERATED by print-records (MetadataSubstreamId)

    def __post_init__(self) -> None:
        if self.uuid is not None and len(self.uuid) != 16:
            raise ValueError(
                f"MetadataSubstreamId.uuid must be 16 bytes; got {len(self.uuid)}"
            )


# BEGIN GENERATED by print-records (CountryCodes); do not edit
@dataclass(frozen=True, slots=True)
class CountryCodes:
    """Item 122: Country Codes (ST 0601.19 §8.122) — country-code metadata
    about the platform's operation and manufacture. VLP of four ``[BER
    length][value]`` fields in strict order: ``coding_method`` (mandatory
    uint — an enumeration from MISB ST 0102 Table 2 Item 12), ``overflight``
    (mandatory utf8 — though its own length may be 0, meaning "unknown"),
    ``operator`` (optional utf8), ``manufacture`` (optional utf8).

    Per §8.122.1: "if one of the country values is unknown, set the length
    for the country code to zero (0) and do not include the country code
    string" — length-0 always means "unknown", never a distinct empty
    string. Truncation removes a length-value pair ENTIRELY from the end
    ("When truncating a value, the length-value pair are both removed") —
    distinct from the length-0 marker, which still writes a (zero) length
    byte. Re-encode canonicalizes a length-0 ``manufacture`` immediately
    followed by nothing else down to plain truncation (no
    ``operator``/``manufacture`` bytes at all) when both are absent; an
    absent ``operator`` with a present ``manufacture`` still gets its own
    length-0 marker so ``manufacture`` is never silently swallowed by
    truncation."""

    coding_method: int
    overflight: str | None = None
    operator: str | None = None
    manufacture: str | None = None
# END GENERATED by print-records (CountryCodes)


# BEGIN GENERATED by print-records (WavelengthRecord); do not edit
@dataclass(frozen=True, slots=True)
class WavelengthRecord:
    """One record of Item 128, Wavelengths List (ST 0601.19 §8.128) — a
    sensor wavelength band definition. ``min_nm``/``max_nm`` are ST 1201.5
    IMAPB(0,1e9,4)-encoded on the wire, giving ~½ nm precision across the
    full X-ray-to-VHF spectrum span the spec cites.

    - ``id`` — BER-OID wavelength band id.
    - ``min_nm`` — Band minimum wavelength, nanometres.
    - ``max_nm`` — Band maximum wavelength, nanometres.
    - ``name`` — Band name."""

    id: int
    min_nm: float
    max_nm: float
    name: str
# END GENERATED by print-records (WavelengthRecord)


# BEGIN GENERATED by print-records (Location); do not edit
@dataclass(frozen=True, slots=True)
class Location:
    """A WGS84 geodetic point: latitude, longitude, and Height Above
    Ellipsoid (HAE), each ST 1201.5 IMAPB-encoded. Shared substrate for Item
    130 (Airbase Locations, ST 0601.19 §8.130) and the per-waypoint location
    in Item 141 (Waypoint List, §8.141).

    Wire shape (truncatable DLP, per §8.130.1 bullet 4): ``lat``
    IMAPB(-90,90,4) + ``lon`` IMAPB(-180,180,4) + ``hae`` IMAPB(-900,9000,3)
    (NB 9000 max, not the 40000 used by the ST 0601 items with their own tag
    — this is the ST 0601.19 Table 16 range, distinct from e.g. Item 113's
    Altitude AGL). ``lat``/``lon`` are mandatory once any bytes are present;
    only the trailing ``hae`` is truncatable.

    **Encode semantics for an interior-absent field** (e.g. ``lat: None``
    while ``hae: Some(..)``): IMAPB has no INT_MIN-style sentinel of its
    own, so this repurposes the ST 1201.5 special-value space — specifically
    a user-defined special with signal 0 — as an "absent" filler at that
    field's wire position, matching decode, which maps ANY
    special/reserved/out-of-range pattern back to ``None``. This is
    spec-legal (§7.2.3's special-value space is defined at the IMAPB layer,
    independent of the ST 0601 item using it) even though it is not itself a
    case ST 0601.19 §8.130 describes — the spec only ever truncates ``hae``
    from the end, never leaves ``lat``/``lon`` absent with ``hae`` present.
    Only a trailing ``None`` (or all three ``None``) truncates the pack."""

    lat_deg: float | None = None
    lon_deg: float | None = None
    hae_m: float | None = None
# END GENERATED by print-records (Location)


# BEGIN GENERATED by print-records (AirbaseLocations); do not edit
@dataclass(frozen=True, slots=True)
class AirbaseLocations:
    """Item 130: Airbase Locations (ST 0601.19 §8.130) — the take-off and
    recovery site locations. VLP: ``[BER length][Location]`` × 2 (take-off,
    then recovery).

    Per §8.130.1's bandwidth optimizations: a ``Location`` slot whose own
    length is 0 decodes to ``None`` ("unknown"); recovery ABSENT from the
    wire ENTIRELY (no second length-value pair at all, not even a length-0
    one) decodes to ``Some(take_off)`` — "if the Recovery Location is absent
    then the Recovery Location is set equal to the Take-Off location".
    Encode mirrors this: when ``recovery == take_off`` the second pair is
    omitted entirely (canonicalizing bullet-1's optimization); a
    ``recovery`` that differs from ``take_off`` — including ``None`` while
    ``take_off`` is ``Some`` (deliberately unknown, not "same-as-take-off")
    — always gets its own explicit pair, so the two cases stay
    distinguishable through a round trip."""

    take_off: Location | None = None
    recovery: Location | None = None
# END GENERATED by print-records (AirbaseLocations)


class PayloadType(enum.Enum):
    """ST 0601.19 §8.138 Table 17 Payload Type enumeration.

    Rust adds an ``Other(u64)`` catch-all for forward-compat; on the
    Python side, unknown codepoints surface as the raw ``int`` on
    ``PayloadRecord.payload_type`` rather than an enum instance (same
    asymmetric pattern as ``IcingDetected``)."""

    ELECTRO_OPTICAL = 0
    LIDAR = 1
    RADAR = 2
    SIGINT = 3
    SAR = 4


# BEGIN GENERATED by print-records (PayloadRecord); do not edit
@dataclass(frozen=True, slots=True)
class PayloadRecord:
    """One record of Item 138, Payload List (ST 0601.19 §8.138)."""

    id: int
    payload_type: PayloadType | int
    name: str
# END GENERATED by print-records (PayloadRecord)


# BEGIN GENERATED by print-records (PayloadList); do not edit
@dataclass(frozen=True, slots=True)
class PayloadList:
    """Item 138 value (ST 0601.19 §8.138): ``count`` (BER-OID Payload Count)
    followed by a VLP of ``[BER length][PayloadRecord]`` entries."""

    count: int
    records: tuple[PayloadRecord, ...]
# END GENERATED by print-records (PayloadList)


# BEGIN GENERATED by print-records (WeaponsStore); do not edit
@dataclass(frozen=True, slots=True)
class WeaponsStore:
    """One record of Item 140, Weapons Stores (ST 0601.19 §8.140) — a single
    weapon's physical address, status, and type. ``status_raw`` packs the
    spec's 14-bit Status BER-OID value verbatim: the low 8 bits are the
    §Table 21 General Status enumeration, the next 4 bits are the §Table 22
    Engagement Status flags, and any remaining high bits are spec-reserved
    (preserved verbatim, not masked away, in case a future revision widens
    the field)."""

    station_id: int
    hardpoint_id: int
    carriage_id: int
    store_id: int
    status_raw: int
    weapon_type: str
# END GENERATED by print-records (WeaponsStore)

    @property
    def general_status(self) -> int:
        """§Table 21 General Status code (low 8 bits of ``status_raw``)."""
        return self.status_raw & 0xFF

    @property
    def fuze_enabled(self) -> bool:
        """§Table 22 bit position 1."""
        return (self.status_raw & 0x100) != 0

    @property
    def laser_enabled(self) -> bool:
        """§Table 22 bit position 2."""
        return (self.status_raw & 0x200) != 0

    @property
    def target_enabled(self) -> bool:
        """§Table 22 bit position 3."""
        return (self.status_raw & 0x400) != 0

    @property
    def weapon_armed(self) -> bool:
        """§Table 22 bit position 4."""
        return (self.status_raw & 0x800) != 0


# BEGIN GENERATED by print-records (Waypoint); do not edit
@dataclass(frozen=True, slots=True)
class Waypoint:
    """One record of Item 141, Waypoint List (ST 0601.19 §8.141).

    ``info`` (the Mode/Source bitfield) and ``location`` are both optional
    trailing fields, but only ``location`` is self-delimiting by its own
    wire length the way ``Location``'s internal truncation is — ``info`` is
    a self-delimiting BER-OID (so its presence never needs an external
    marker), and this decoder distinguishes "info present" from "info
    absent, straight to location" by checking whether the remaining byte
    count already matches a valid ``Location`` length (``0``, ``8``, or
    ``11``, per ``Location``'s truncation rule — the pair of
    Latitude/Longitude is mandatory both-or-neither, only HAE truncates
    independently) BEFORE attempting to consume an ``info`` BER-OID. This is
    unambiguous only because no two members of ``{0, 8, 11}`` differ by
    exactly the 1 byte a conformant ``info`` value occupies (values 0-3 per
    §8.141's 2-bit Mode/Source field) — a future revision that widened
    ``info`` enough to need e.g. 8 BER-OID bytes could collide with a
    location-only remainder and misparse; not a concern for the field as
    currently defined."""

    id: int
    prosecution_order: int
    info: int | None = None
    location: Location | None = None
# END GENERATED by print-records (Waypoint)


# BEGIN GENERATED by print-records (ViewDomainPair); do not edit
@dataclass(frozen=True, slots=True)
class ViewDomainPair:
    """One ``(start, range)`` pair of Item 142, View Domain (ST 0601.19
    §8.142). ``start`` uses the axis-specific IMAPB range (see
    ``ViewDomain``'s azimuth/elevation/roll fields); ``range`` always uses
    IMAPB(0,360) — "the angular range specifies the limit from the starting
    point to the sensor's maximum value; numerically the angular range is
    always a positive value". Both fields of a pair always share the same
    IMAPB byte length (whatever the producer chose; this crate encodes at 3
    bytes)."""

    start_deg: float
    range_deg: float
# END GENERATED by print-records (ViewDomainPair)


# BEGIN GENERATED by print-records (ViewDomain); do not edit
@dataclass(frozen=True, slots=True)
class ViewDomain:
    """Item 142 value (ST 0601.19 §8.142): up to three ``ViewDomainPair``s —
    azimuth, elevation, roll, in that fixed order — each preceded by a BER
    pair-length. A pair-length of 0 means "unknown" (the pair is absent, but
    a byte was still spent saying so); the pack is also a truncation pack,
    so trailing pairs may be dropped from the wire entirely (no pair-length
    byte at all)."""

    azimuth: ViewDomainPair | None = None
    elevation: ViewDomainPair | None = None
    roll: ViewDomainPair | None = None
# END GENERATED by print-records (ViewDomain)


# BEGIN GENERATED by print-records (SdccFlpField); do not edit
@dataclass(frozen=True, slots=True)
class SdccFlpField:
    """One captured ST 0601 Item 102 (SDCC-FLP) occurrence, with its
    "Refined Source List" positional context (ST 0601.19 §8.102).

    Tag 102 is MULTI-INSTANCE (ST 0601.19 Table 1 "Multiples Allowed" =
    Yes): each occurrence's SDCC-FLP pack (MISB ST 1010.3, parsed by the ST
    1010 SDCC-FLP decoder) refines the accuracy of the Local Set items that
    immediately precede it on the wire. This struct keeps the raw pack bytes
    rather than a parsed ``SdccFlp`` so a malformed or foreign-encoder pack
    still round-trips byte-exact even when this crate cannot parse its
    interior — provided its declared matrix size fits the pack's own bytes
    (Parse Control plus the ST 1010 size bound); otherwise the occurrence is
    recorded as a ``TruncatedField`` and not captured. The encoder still
    emits caller-supplied ``sdcc_flps`` bytes verbatim, so a caller who
    supplies a pack whose matrix cannot fit will see it rejected on decode.

    **Capture semantics (decode):** ``preceding_tags`` is the ``N``
    wire-order item tags — known or unknown, but never another Tag 102 —
    immediately before this occurrence, where ``N`` is this pack's own
    parsed matrix size (fewer if the Local Set had fewer prior items).

    **Ascending-order emission caveat (encode):** every ``sdcc_flps`` entry
    is re-emitted verbatim from one place in tag-ascending order, grouping
    all occurrences together. This means the original interleaving with
    ``preceding_tags`` (e.g. tags 13/14/15 followed by one Tag 102
    occurrence, then tags 5/6/7 followed by another) is generally NOT
    reproduced on re-encode — the re-encoded wire carries both occurrences
    back to back instead. ``preceding_tags`` therefore records the original
    wire order, not a guarantee about the re-encoded one; the pack ``bytes``
    themselves are still byte-exact.

    - ``preceding_tags`` — Wire-order item tags immediately preceding this
      occurrence (see struct doc for the capture rule). Empty if this
      occurrence had no preceding items (matrix size 0, or fewer prior items
      than the matrix size — spec-legal-but-degenerate, not an error).
    - ``bytes`` — Raw SDCC-FLP pack bytes, exactly as they appeared on the
      wire — decode with the ST 1010 SDCC-FLP decoder."""

    preceding_tags: tuple[int, ...]
    bytes: bytes
# END GENERATED by print-records (SdccFlpField)


# ---------------------------------------------------------------------------
# ST 0601 UAS Datalink Local Set
# ---------------------------------------------------------------------------


# BEGIN GENERATED by print-records (UasDatalinkLs); do not edit
@dataclass(frozen=True, slots=True)
class UasDatalinkLs:
    """One ST 0601 UAS Datalink Local Set, one field per tag.

    A field whose doc states a range is checked when the record is encoded:
    under the default out-of-range policy a value outside it is an
    ``OutOfRange`` encode error; see ``OutOfRangePolicy`` for the tags that
    can carry the spec's Out-of-Range indicator instead.

    The composite views (sensor position, platform and sensor attitude,
    field of view, frame center and corners) are accessors that return
    nothing when any of the fields they combine is absent.

    - ``universal_label`` — The 16-byte SMPTE universal label that opens the
      local set.
    - ``declared_version`` — The version byte the label declares.
    - ``platform_heading_deg`` — Item 5: Platform Heading Angle — encode
      range [0, 360] deg (uint16).
    - ``platform_pitch_deg`` — Item 6: Platform Pitch Angle — encode range
      [-20, 20] deg (int16). For the full ±90° range use
      ``platform_pitch_full_deg`` (Item 90).
    - ``platform_roll_deg`` — Item 7: Platform Roll Angle — encode range
      [-50, 50] deg (int16). For the full ±90° range use
      ``platform_roll_full_deg`` (Item 91).
    - ``platform_true_airspeed`` — Item 8: Platform True Airspeed — encode
      range [0, 255] m/s (uint8).
    - ``platform_indicated_airspeed`` — Item 9: Platform Indicated Airspeed
      — encode range [0, 255] m/s (uint8).
    - ``platform_pitch_full_deg`` — Item 90: Platform Pitch Angle (Full) —
      encode range [-90, 90] deg (int32). Full-range twin of
      ``platform_pitch_deg`` (Item 6, ±20°).
    - ``platform_roll_full_deg`` — Item 91: Platform Roll Angle (Full) —
      encode range [-90, 90] deg (int32). Full-range twin of
      ``platform_roll_deg`` (Item 7, ±50°).
    - ``miis_core_id`` — Item 94: MIIS Core Identifier (MISB ST 1204 binary
      format). Raw bytes — decode/inspect via ``st1204``.
    - ``platform_angle_of_attack_deg`` — Item 50: Platform Angle of Attack —
      encode range [-20, 20] deg (int16).
    - ``sensor_lat_deg`` — Item 13: Sensor Latitude — encode range [-90, 90]
      deg (int32).
    - ``sensor_lon_deg`` — Item 14: Sensor Longitude — encode range [-180,
      180] deg (int32).
    - ``sensor_alt_m`` — Item 15: Sensor True Altitude — encode range [-900,
      19000] m (uint16).
    - ``sensor_ellipsoid_height_m`` — Item 75: Sensor Ellipsoid Height
      (WGS84) — encode range [-900, 19000] m (uint16). For the extended
      [-900, 40000] m range use ``sensor_ellipsoid_height_extended_m`` (Item
      104, IMAPB). Per ST 0601.19 requirements 0601.9-20/-21, a decoder that
      understands Item 104 shall use it and ignore this restricted twin when
      both are present; this struct populates both fields on decode — prefer
      ``sensor_ellipsoid_height_extended_m`` when producing new records.
    - ``sensor_hfov_deg`` — Item 16: Sensor Horizontal FOV — encode range
      [0, 180] deg (uint16).
    - ``sensor_vfov_deg`` — Item 17: Sensor Vertical FOV — encode range [0,
      180] deg (uint16).
    - ``sensor_rel_az_deg`` — Item 18: Sensor Relative Azimuth — encode
      range [0, 360] deg (uint32).
    - ``sensor_rel_el_deg`` — Item 19: Sensor Relative Elevation — encode
      range [-180, 180] deg (int32).
    - ``sensor_rel_roll_deg`` — Item 20: Sensor Relative Roll — encode range
      [0, 360] deg (uint32).
    - ``slant_range_m`` — Item 21: Slant Range — encode range [0, 5000000] m
      (uint32).
    - ``target_width_m`` — Item 22: Target Width — encode range [0, 10000] m
      (uint16). For the extended [0, 1500000] m range use
      ``target_width_extended_m`` (Item 96, IMAPB). Per ST 0601.19
      requirements 0601.9-20/-21, a decoder that understands Item 96 shall
      use it and ignore this restricted twin when both are present; this
      struct populates both fields on decode — prefer
      ``target_width_extended_m`` when producing new records.
    - ``frame_center_lat_deg`` — Item 23: Frame Center Latitude — encode
      range [-90, 90] deg (int32).
    - ``frame_center_lon_deg`` — Item 24: Frame Center Longitude — encode
      range [-180, 180] deg (int32).
    - ``frame_center_elev_m`` — Item 25: Frame Center Elevation — encode
      range [-900, 19000] m (uint16).
    - ``frame_center_ellipsoid_height_m`` — Item 78: Frame Center Height
      Above Ellipsoid (WGS84) — encode range [-900, 19000] m (uint16).
    - ``corner_lat_offset_p1_deg`` — Item 26: Offset Corner Latitude Point 1
      — encode range [-0.075, 0.075] deg (int16). For the full latitude
      range use ``corner_lat_p1_deg`` (Item 82, ±90°).
    - ``corner_lon_offset_p1_deg`` — Item 27: Offset Corner Longitude Point
      1 — encode range [-0.075, 0.075] deg (int16). For the full longitude
      range use ``corner_lon_p1_deg`` (Item 83, ±180°).
    - ``corner_lat_offset_p2_deg`` — Item 28: Offset Corner Latitude Point 2
      — encode range [-0.075, 0.075] deg (int16). For the full latitude
      range use ``corner_lat_p2_deg`` (Item 84, ±90°).
    - ``corner_lon_offset_p2_deg`` — Item 29: Offset Corner Longitude Point
      2 — encode range [-0.075, 0.075] deg (int16). For the full longitude
      range use ``corner_lon_p2_deg`` (Item 85, ±180°).
    - ``corner_lat_offset_p3_deg`` — Item 30: Offset Corner Latitude Point 3
      — encode range [-0.075, 0.075] deg (int16). For the full latitude
      range use ``corner_lat_p3_deg`` (Item 86, ±90°).
    - ``corner_lon_offset_p3_deg`` — Item 31: Offset Corner Longitude Point
      3 — encode range [-0.075, 0.075] deg (int16). For the full longitude
      range use ``corner_lon_p3_deg`` (Item 87, ±180°).
    - ``corner_lat_offset_p4_deg`` — Item 32: Offset Corner Latitude Point 4
      — encode range [-0.075, 0.075] deg (int16). For the full latitude
      range use ``corner_lat_p4_deg`` (Item 88, ±90°).
    - ``corner_lon_offset_p4_deg`` — Item 33: Offset Corner Longitude Point
      4 — encode range [-0.075, 0.075] deg (int16). For the full longitude
      range use ``corner_lon_p4_deg`` (Item 89, ±180°).
    - ``corner_lat_p1_deg`` — Item 82: Corner Latitude Point 1 (Full) —
      encode range [-90, 90] deg (int32). Full-range twin of
      ``corner_lat_offset_p1_deg`` (Item 26, ±0.075°).
    - ``corner_lon_p1_deg`` — Item 83: Corner Longitude Point 1 (Full) —
      encode range [-180, 180] deg (int32). Full-range twin of
      ``corner_lon_offset_p1_deg`` (Item 27, ±0.075°).
    - ``corner_lat_p2_deg`` — Item 84: Corner Latitude Point 2 (Full) —
      encode range [-90, 90] deg (int32). Full-range twin of
      ``corner_lat_offset_p2_deg`` (Item 28, ±0.075°).
    - ``corner_lon_p2_deg`` — Item 85: Corner Longitude Point 2 (Full) —
      encode range [-180, 180] deg (int32). Full-range twin of
      ``corner_lon_offset_p2_deg`` (Item 29, ±0.075°).
    - ``corner_lat_p3_deg`` — Item 86: Corner Latitude Point 3 (Full) —
      encode range [-90, 90] deg (int32). Full-range twin of
      ``corner_lat_offset_p3_deg`` (Item 30, ±0.075°).
    - ``corner_lon_p3_deg`` — Item 87: Corner Longitude Point 3 (Full) —
      encode range [-180, 180] deg (int32). Full-range twin of
      ``corner_lon_offset_p3_deg`` (Item 31, ±0.075°).
    - ``corner_lat_p4_deg`` — Item 88: Corner Latitude Point 4 (Full) —
      encode range [-90, 90] deg (int32). Full-range twin of
      ``corner_lat_offset_p4_deg`` (Item 32, ±0.075°).
    - ``corner_lon_p4_deg`` — Item 89: Corner Longitude Point 4 (Full) —
      encode range [-180, 180] deg (int32). Full-range twin of
      ``corner_lon_offset_p4_deg`` (Item 33, ±0.075°).
    - ``target_location_lat_deg`` — Item 40: Target Location Latitude —
      encode range [-90, 90] deg (int32).
    - ``target_location_lon_deg`` — Item 41: Target Location Longitude —
      encode range [-180, 180] deg (int32).
    - ``target_location_elev_m`` — Item 42: Target Location Elevation —
      encode range [-900, 19000] m (uint16).
    - ``target_track_gate_width_px`` — Item 43: Target Track Gate Width —
      encode range [0, 510] px (uint8).
    - ``target_track_gate_height_px`` — Item 44: Target Track Gate Height —
      encode range [0, 510] px (uint8).
    - ``target_error_ce90_m`` — Item 45: Target Error Estimate - CE90 —
      encode range [0, 4095] m (uint16).
    - ``target_error_le90_m`` — Item 46: Target Error Estimate - LE90 —
      encode range [0, 4095] m (uint16).
    - ``wind_direction_deg`` — Item 35: Wind Direction — encode range [0,
      360] deg (uint16).
    - ``wind_speed`` — Item 36: Wind Speed — encode range [0, 100] m/s
      (uint8).
    - ``static_pressure_mbar`` — Item 37: Static Pressure — encode range [0,
      5000] mbar (uint16).
    - ``density_altitude_m`` — Item 38: Density Altitude — encode range
      [-900, 19000] m (uint16). For the extended [-900, 40000] m range use
      ``density_altitude_extended_m`` (Item 103, IMAPB). Per ST 0601.19
      requirements 0601.9-20/-21, a decoder that understands Item 103 shall
      use it and ignore this restricted twin when both are present; this
      struct populates both fields on decode — prefer
      ``density_altitude_extended_m`` when producing new records.
    - ``differential_pressure_mbar`` — Item 49: Differential Pressure —
      encode range [0, 5000] mbar (uint16).
    - ``airfield_barometric_pressure_mbar`` — Item 53: Airfield Barometric
      Pressure — encode range [0, 5000] mbar (uint16).
    - ``airfield_elevation_m`` — Item 54: Airfield Elevation — encode range
      [-900, 19000] m (uint16).
    - ``relative_humidity_pct`` — Item 55: Relative Humidity — encode range
      [0, 100] % (uint8).
    - ``platform_vertical_speed`` — Item 51: Platform Vertical Speed —
      encode range [-180, 180] m/s (int16).
    - ``platform_sideslip_deg`` — Item 52: Platform Sideslip Angle — encode
      range [-20, 20] deg (int16). For the full ±180° range use
      ``platform_sideslip_full_deg`` (Item 93).
    - ``platform_ground_speed`` — Item 56: Platform Ground Speed — encode
      range [0, 255] m/s (uint8).
    - ``ground_range_m`` — Item 57: Ground Range — encode range [0, 5000000]
      m (uint32).
    - ``platform_fuel_remaining_kg`` — Item 58: Platform Fuel Remaining —
      encode range [0, 10000] kg (uint16).
    - ``platform_magnetic_heading_deg`` — Item 64: Platform Magnetic Heading
      — encode range [0, 360] deg (uint16).
    - ``platform_angle_of_attack_full_deg`` — Item 92: Platform Angle of
      Attack (Full) — encode range [-90, 90] deg (int32).
    - ``platform_sideslip_full_deg`` — Item 93: Platform Sideslip Angle
      (Full) — encode range [-180, 180] deg (int32). Full-range twin of
      ``platform_sideslip_deg`` (Item 52, ±20°).
    - ``alternate_platform_lat_deg`` — Item 67: Alternate Platform Latitude
      — encode range [-90, 90] deg (int32).
    - ``alternate_platform_lon_deg`` — Item 68: Alternate Platform Longitude
      — encode range [-180, 180] deg (int32).
    - ``alternate_platform_alt_m`` — Item 69: Alternate Platform Altitude —
      encode range [-900, 19000] m (uint16). Not to be confused with
      ``alternate_platform_ellipsoid_height_extended_m`` (Item 105) — Item
      105 extends Item 76 (Alternate Platform Ellipsoid Height, a WGS84
      ellipsoid-height item), not this plain-altitude item; Item 69 has no
      IMAPB extended-range twin of its own. Per ST 0601.19 §8.105, legacy
      systems preferring one representation should favor Item 105 over Item
      76 over this item — consistent with the requirements 0601.9-20/-21
      extended-representation precedence rule on the 76/105 pair (see
      ``alternate_platform_ellipsoid_height_m``).
    - ``alternate_platform_heading_deg`` — Item 71: Alternate Platform
      Heading — encode range [0, 360] deg (uint16).
    - ``alternate_platform_ellipsoid_height_m`` — Item 76: Alternate
      Platform Ellipsoid Height (WGS84) — encode range [-900, 19000] m
      (uint16). For the extended [-900, 40000] m range use
      ``alternate_platform_ellipsoid_height_extended_m`` (Item 105, IMAPB).
      Per ST 0601.19 requirements 0601.9-20/-21, a decoder that understands
      Item 105 shall use it and ignore this restricted twin when both are
      present; this struct populates both fields on decode — prefer
      ``alternate_platform_ellipsoid_height_extended_m`` when producing new
      records.
    - ``sensor_north_velocity`` — Item 79: Sensor North Velocity — encode
      range [-327, 327] m/s (int16).
    - ``sensor_east_velocity`` — Item 80: Sensor East Velocity — encode
      range [-327, 327] m/s (int16).
    - ``target_width_extended_m`` — Item 96: Target Width Extended — ST
      1201.5 IMAPB range [0, 1500000] m, wire length 1..=8 bytes (encode
      emits 3 bytes). Extended-range twin of ``target_width_m`` (Item 22,
      uint16, [0, 10000] m) — see that field's docs for the ST 0601.19
      extended-representation precedence rule (prefer this field).
    - ``density_altitude_extended_m`` — Item 103: Density Altitude Extended
      — ST 1201.5 IMAPB range [-900, 40000] m, wire length 1..=8 bytes
      (encode emits 3 bytes). Extended-range twin of ``density_altitude_m``
      (Item 38, uint16, [-900, 19000] m) — see that field's docs for the ST
      0601.19 extended-representation precedence rule (prefer this field).
    - ``sensor_ellipsoid_height_extended_m`` — Item 104: Sensor Ellipsoid
      Height Extended — ST 1201.5 IMAPB range [-900, 40000] m, wire length
      1..=8 bytes (encode emits 3 bytes). Extended-range twin of
      ``sensor_ellipsoid_height_m`` (Item 75, uint16, [-900, 19000] m) — see
      that field's docs for the ST 0601.19 extended-representation
      precedence rule (prefer this field).
    - ``alternate_platform_ellipsoid_height_extended_m`` — Item 105:
      Alternate Platform Ellipsoid Height Extended — ST 1201.5 IMAPB range
      [-900, 40000] m, wire length 1..=8 bytes (encode emits 3 bytes).
      Extended-range twin of ``alternate_platform_ellipsoid_height_m`` (Item
      76, uint16, [-900, 19000] m) — see that field's docs for the ST
      0601.19 extended-representation precedence rule (prefer this field);
      see also ``alternate_platform_alt_m`` (Item 69) for the disambiguation
      from plain (non-ellipsoid) altitude.
    - ``range_to_recovery_km`` — Item 109: Range To Recovery Location —
      distance from current position to airframe recovery position. ST
      1201.5 IMAPB range [0, 21000] km, wire length 1..=4 bytes (encode
      emits 3 bytes).
    - ``platform_course_angle_deg`` — Item 112: Platform Course Angle —
      direction the aircraft is moving relative to True North. ST 1201.5
      IMAPB range [0, 360] deg, wire length 1..=8 bytes (encode emits 2
      bytes).
    - ``altitude_agl_m`` — Item 113: Altitude AGL — Above Ground Level
      height above the ground/water. ST 1201.5 IMAPB range [-900, 40000] m,
      wire length 1..=4 bytes (encode emits 3 bytes).
    - ``radar_altimeter_m`` — Item 114: Radar Altimeter — height above the
      ground/water as reported by a RADAR altimeter (AGL, see
      ``altitude_agl_m``). ST 1201.5 IMAPB range [-900, 40000] m, wire
      length 1..=4 bytes (encode emits 3 bytes).
    - ``sensor_azimuth_rate_dps`` — Item 117: Sensor Azimuth Rate — rate the
      sensor's azimuth angle is changing. ST 1201.5 IMAPB range [-1000,
      1000] deg/s, wire length 1..=4 bytes (encode emits 2 bytes).
    - ``sensor_elevation_rate_dps`` — Item 118: Sensor Elevation Rate — rate
      the sensor's elevation angle is changing. ST 1201.5 IMAPB range
      [-1000, 1000] deg/s, wire length 1..=4 bytes (encode emits 3 bytes).
    - ``sensor_roll_rate_dps`` — Item 119: Sensor Roll Rate — rate the
      sensor's roll angle is changing. ST 1201.5 IMAPB range [-1000, 1000]
      deg/s, wire length 1..=4 bytes (encode emits 2 bytes).
    - ``mi_storage_percent_full`` — Item 120: On-board MI Storage Percent
      Full — amount of on-board Motion Imagery storage used, as a percentage
      of total storage. ST 1201.5 IMAPB range [0, 100] %, wire length 1..=3
      bytes (encode emits 2 bytes).
    - ``transmission_frequency_mhz`` — Item 132: Transmission Frequency —
      radio frequency used to transmit the Motion Imagery. ST 1201.5 IMAPB
      range [1, 99999] MHz, wire length 1..=4 bytes (encode emits 3 bytes).
      First IMAPB item whose own tag number is 2-byte BER-OID encoded (like
      Items 129/135 in the raw/UTF-8 set).
    - ``zoom_percentage`` — Item 134: Zoom Percentage — for a variable-zoom
      system, the percentage of zoom. ST 1201.5 IMAPB range [0, 100] %, wire
      length 1..=4 bytes (encode emits 2 bytes).
    - ``time_airborne_s`` — Item 110: Time Airborne — cumulative time the
      platform has been airborne. MISB var-length uint, wire length 1..=4
      bytes (seconds).
    - ``propulsion_unit_speed_rpm`` — Item 111: Propulsion Unit Speed —
      rotational speed of the platform's propulsion unit. MISB var-length
      uint, wire length 1..=4 bytes (RPM).
    - ``navsats_in_view`` — Item 123: Number of NAVSATs in View — count of
      navigation satellites contributing to the platform's position
      solution. MISB var-length uint, wire length 1 byte.
    - ``positioning_method_source`` — Item 124: Positioning Method Source —
      bitfield naming which satellite positioning system(s) contributed to
      the platform's position solution. MISB var-length uint, wire length 1
      byte. | Bit | System | |---:|---| | 0 | INS | | 1 | GPS | | 2 |
      Galileo | | 3 | QZSS | | 4 | NAVIC | | 5 | GLONASS | | 6 | BeiDou-1 |
      | 7 | BeiDou-2 | Per ST 0601.19 §8.124 a conformant producer sets at
      least one bit (an all-zero value is out of the item's declared
      ``1..255`` range); decode stays lenient and does not reject ``0x00``.
    - ``platform_status`` — Item 125: Platform Status — operational status
      of the platform. MISB var-length uint, wire length 1 byte. See
      ``PlatformStatus``.
    - ``sensor_control_mode`` — Item 126: Sensor Control Mode — what is
      currently controlling the sensor. MISB var-length uint, wire length 1
      byte. See ``SensorControlMode``.
    - ``take_off_time_us`` — Item 131: Take-off Time — MISB ST 0603
      timestamp of the platform's take-off. MISB var-length uint, wire
      length 1..=8 bytes (microseconds).
    - ``mi_storage_capacity_gb`` — Item 133: On-board MI Storage Capacity —
      total on-board Motion Imagery storage capacity (compare
      ``mi_storage_percent_full``, Item 120, the used percentage of that
      capacity). MISB var-length uint, wire length 1..=4 bytes (gigabytes).
    - ``leap_seconds`` — Item 136: Leap Seconds — the TAI-UTC leap-second
      offset in effect for ``timestamp_us``. MISB var-length int, wire
      length 1..=4 bytes (seconds).
    - ``correction_offset_us`` — Item 137: Correction Offset — signed
      correction applied to a platform time source (e.g. GPS-disciplined
      clock drift). MISB var-length int, wire length 1..=8 bytes
      (microseconds). **Spec typo:** ST 0601.19 §8.137 prints this item's
      decode type as "KLVuint" (unsigned), but its own worked example only
      decodes correctly as SIGNED (MISB var-length int) — this
      implementation follows the worked example, not the printed type name.
    - ``active_payloads`` — Item 139: Active Payloads — bitmask of which
      Payload IDs (Item 138, Payload List — ``payload_list``) are currently
      active. Raw bytes, LSB-first within each byte: bit ``i`` of byte 0 is
      Payload ID ``i`` (0-7), byte 1 covers IDs 8-15, and so on (multi-byte
      extends upward).
    - ``generic_flag_data`` — Item 47: Generic Flag Data — bitfield per ST
      0601.19 Table 3: bit 0 laser range on, bit 1 auto-track on, bit 2 IR
      polarity (set = black hot), bit 3 icing detected, bit 4 slant range
      measured (vs. calculated), bit 5 image invalid, bits 6-7 reserved.
    - ``security_local_set`` — Tag 48: the ST 0102 Security Local Set body
      bytes (no universal-label prefix). Pass-through bytes; consumers
      needing typed access decode them with the ST 0102 decoder.
    - ``rvt`` — Item 73: MISB ST 0806 Remote Video Terminal LS bytes.
      Pass-through bytes; consumers needing typed access decode them with
      the ST 0806 decoder (see ``st0806`` for the typed layer).
    - ``vmti`` — Tag 74 — VMTI Local Set (MISB ST 0903). Pass-through bytes;
      consumers needing typed access decode them with the ST 0903 decoder
      (see ``st0903`` for the typed layer). Sibling-layer pattern matches
      ``security_local_set`` (Tag 48, ST 0102).
    - ``sar_mi_local_set`` — Item 95: MISB ST 1206 SAR Motion Imagery Local
      Set bytes. Pass-through bytes; interior typing deferred.
    - ``range_image_local_set`` — Item 97: MISB ST 1002 Range Image Local
      Set bytes. Pass-through bytes; interior typing deferred.
    - ``geo_registration_local_set`` — Item 98: MISB ST 1601
      Geo-Registration Local Set bytes. Pass-through bytes; interior typing
      deferred.
    - ``composite_imaging_local_set`` — Item 99: MISB ST 1602 Composite
      Imaging Local Set bytes. Pass-through bytes; interior typing deferred.
    - ``segment_local_set`` — Item 100: MISB ST 1607 Segment Local Set
      bytes. Pass-through bytes; interior typing deferred.
    - ``amend_local_set`` — Item 101: MISB ST 1607 Amend Local Set bytes.
      Pass-through bytes; interior typing deferred.
    - ``weapon_load`` — Item 60: Weapon Load — bit-packed nibbles
      (high→low): store station, store substation, weapon type, weapon
      variant. Kept as a raw opaque 16-bit integer, not a sub-struct —
      callers needing individual nibbles shift and mask this value
      themselves.
    - ``icing_detected`` — Item 34: Icing Detected — icing-detector state.
      See ``IcingDetected``.
    - ``sensor_fov_name`` — Item 63: Sensor Field of View Name — named FOV
      preset. See ``SensorFovName``.
    - ``operational_mode`` — Item 77: Operational Mode — operating mode of
      the portrayed event. See ``OperationalMode``.
    - ``image_horizon`` — Item 81: Image Horizon Pixels. See
      ``ImageHorizonPixels``.
    - ``control_commands`` — Item 115: Control Command — MULTI-INSTANCE (ST
      0601.19 Table 1 "Multiples Allowed" = Yes); every wire occurrence
      appends one ``ControlCommand``. Empty means none were present.
    - ``control_command_verification`` — Item 116: Control Command
      Verification List — BER-OID command ids the platform has
      acknowledged/verified.
    - ``active_wavelengths`` — Item 121: Active Wavelength List — BER-OID
      wavelength ids currently active (``0`` = none active, exclusive per ST
      0601.19 §8.121).
    - ``sensor_frame_rate`` — Item 127: Sensor Frame Rate Pack. See
      ``SensorFrameRate``.
    - ``metadata_substream_id`` — Item 143: Metadata Substream Id. See
      ``MetadataSubstreamId``.
    - ``country_codes`` — Item 122: Country Codes. See ``CountryCodes``.
    - ``wavelengths_list`` — Item 128: Wavelengths List. See
      ``WavelengthRecord``.
    - ``airbase_locations`` — Item 130: Airbase Locations. See
      ``AirbaseLocations``.
    - ``payload_list`` — Item 138: Payload List. See ``PayloadList``.
    - ``weapons_stores`` — Item 140: Weapons Stores. See ``WeaponsStore``.
    - ``waypoint_list`` — Item 141: Waypoint List. See ``Waypoint``.
    - ``view_domain`` — Item 142: View Domain. See ``ViewDomain``.
    - ``sdcc_flps`` — Item 102: SDCC-FLP (Standard Deviation and Correlation
      Coefficient, Floating-Point). MULTI-INSTANCE per ST 0601.19 Table 1
      ("Multiples Allowed" = Yes) — the "Refined Source List" binding: every
      wire occurrence appends one ``SdccFlpField``, capturing which
      preceding items it refines. See ``SdccFlpField`` for the capture rule
      and the encode-side adjacency caveat.
    - ``unknown`` — Every tag the typed set does not model, preserved as raw
      bytes per the ST 0107.5 §6 future-proof skip rule. An entry whose tag
      a typed field owns is dropped on encode: the typed field wins.
    - ``field_errors`` — Per-field decode failures collected by a lenient
      decode; a strict decode returns an error instead. A parser diagnostic:
      it is not re-encoded.
    - ``sentinel_tags`` — Tags whose wire value was the INT_MIN sentinel for
      their signed linear mapping. INT_MIN is a spec-defined signal (not an
      error), so the corresponding typed field is left as ``None`` and the
      tag is recorded here instead of in ``field_errors``. Use
      ``st0601_sentinel_meaning`` to look up the spec-defined meaning for
      each tag (Out of Range / Reserved / N/A). **Encode semantics:**
      encoding a record that carries sentinel tags re-emits the INT_MIN
      bytes for each sentinel tag whose typed field is ``None``. If the
      typed field is ``Some(v)``, the value ``v`` is encoded instead — a
      non-None field always wins over a ``sentinel_tags`` entry. The encoder
      also auto-injects Tag 65 (version) and the trailing checksum, so the
      output byte sequence is not guaranteed to match the original wire
      input byte-for-byte; the sentinel value round-trips, but the emission
      order is: typed fields in ascending tag order, then ``sentinel_tags``
      entries, then ``unknown`` fields in caller order.
    - ``imapb_specials`` — Tags whose ST 1201.5 IMAPB wire value decoded to
      a spec-defined special value (§7.2.3 — ``+∞``/``−∞``, NaN families,
      the MISB-defined ``BelowMin``/``AboveMax`` overflow signals, or a
      user-defined signal) rather than a normal-range float. The
      corresponding typed field is left absent and the tag+special pair is
      recorded here instead. Use ``ImapbSpecial`` to inspect which special
      was signaled. **Encode semantics:** mirrors ``sentinel_tags`` —
      encoding a record re-emits the special's bytes (at the tag's
      ``default_len``) for each ``(tag, special)`` entry whose typed field
      is currently ``None``. If the typed field is ``Some(v)``, the value
      wins and the special entry is not re-emitted. Entries whose tag is not
      an IMAPB-encoded item are silently skipped on encode. **Decode
      semantics for the two IMAPB outcomes NOT carried here:** a wire
      integer in ST 1201.5's reserved-but-unrecognized special-value space
      (``ReservedSpecial``) or one that arithmetic-decodes outside the tag's
      ``[min, max]`` (``OutOfRange``) are both treated as producer
      non-conformance from this typed consumer's view: they are NOT pushed
      here, and the raw wire bytes are NOT preserved. Instead they are
      recorded in ``field_errors`` as ``OutOfRange`` — with a NaN value for
      ``ReservedSpecial`` (no arithmetic decode exists for a non-conformant
      bit pattern), or the raw arithmetic decode for ``OutOfRange``. This
      differs from ``unknown``/``patch()``, which preserve raw bytes
      verbatim for genuinely untyped tags — these tags ARE typed, so a
      malformed value is a decode error, not a pass-through."""

    universal_label: bytes = b"\x00" * 16
    declared_version: int = 0
    mission_id: str | None = None
    platform_tail_number: str | None = None
    platform_designation: str | None = None
    image_source_sensor: str | None = None
    image_coordinate_system: str | None = None
    platform_call_sign: str | None = None
    uas_ls_version: int | None = None
    timestamp_us: int | None = None
    platform_heading_deg: float | None = None
    platform_pitch_deg: float | None = None
    platform_roll_deg: float | None = None
    platform_true_airspeed: float | None = None
    platform_indicated_airspeed: float | None = None
    platform_pitch_full_deg: float | None = None
    platform_roll_full_deg: float | None = None
    miis_core_id: bytes | None = None
    platform_angle_of_attack_deg: float | None = None
    sensor_lat_deg: float | None = None
    sensor_lon_deg: float | None = None
    sensor_alt_m: float | None = None
    sensor_ellipsoid_height_m: float | None = None
    sensor_hfov_deg: float | None = None
    sensor_vfov_deg: float | None = None
    sensor_rel_az_deg: float | None = None
    sensor_rel_el_deg: float | None = None
    sensor_rel_roll_deg: float | None = None
    slant_range_m: float | None = None
    target_width_m: float | None = None
    frame_center_lat_deg: float | None = None
    frame_center_lon_deg: float | None = None
    frame_center_elev_m: float | None = None
    frame_center_ellipsoid_height_m: float | None = None
    corner_lat_offset_p1_deg: float | None = None
    corner_lon_offset_p1_deg: float | None = None
    corner_lat_offset_p2_deg: float | None = None
    corner_lon_offset_p2_deg: float | None = None
    corner_lat_offset_p3_deg: float | None = None
    corner_lon_offset_p3_deg: float | None = None
    corner_lat_offset_p4_deg: float | None = None
    corner_lon_offset_p4_deg: float | None = None
    corner_lat_p1_deg: float | None = None
    corner_lon_p1_deg: float | None = None
    corner_lat_p2_deg: float | None = None
    corner_lon_p2_deg: float | None = None
    corner_lat_p3_deg: float | None = None
    corner_lon_p3_deg: float | None = None
    corner_lat_p4_deg: float | None = None
    corner_lon_p4_deg: float | None = None
    target_location_lat_deg: float | None = None
    target_location_lon_deg: float | None = None
    target_location_elev_m: float | None = None
    target_track_gate_width_px: float | None = None
    target_track_gate_height_px: float | None = None
    target_error_ce90_m: float | None = None
    target_error_le90_m: float | None = None
    wind_direction_deg: float | None = None
    wind_speed: float | None = None
    static_pressure_mbar: float | None = None
    density_altitude_m: float | None = None
    differential_pressure_mbar: float | None = None
    airfield_barometric_pressure_mbar: float | None = None
    airfield_elevation_m: float | None = None
    relative_humidity_pct: float | None = None
    platform_vertical_speed: float | None = None
    platform_sideslip_deg: float | None = None
    platform_ground_speed: float | None = None
    ground_range_m: float | None = None
    platform_fuel_remaining_kg: float | None = None
    platform_magnetic_heading_deg: float | None = None
    platform_angle_of_attack_full_deg: float | None = None
    platform_sideslip_full_deg: float | None = None
    alternate_platform_lat_deg: float | None = None
    alternate_platform_lon_deg: float | None = None
    alternate_platform_alt_m: float | None = None
    alternate_platform_heading_deg: float | None = None
    alternate_platform_ellipsoid_height_m: float | None = None
    sensor_north_velocity: float | None = None
    sensor_east_velocity: float | None = None
    target_width_extended_m: float | None = None
    density_altitude_extended_m: float | None = None
    sensor_ellipsoid_height_extended_m: float | None = None
    alternate_platform_ellipsoid_height_extended_m: float | None = None
    range_to_recovery_km: float | None = None
    platform_course_angle_deg: float | None = None
    altitude_agl_m: float | None = None
    radar_altimeter_m: float | None = None
    sensor_azimuth_rate_dps: float | None = None
    sensor_elevation_rate_dps: float | None = None
    sensor_roll_rate_dps: float | None = None
    mi_storage_percent_full: float | None = None
    transmission_frequency_mhz: float | None = None
    zoom_percentage: float | None = None
    time_airborne_s: int | None = None
    propulsion_unit_speed_rpm: int | None = None
    navsats_in_view: int | None = None
    positioning_method_source: int | None = None
    platform_status: PlatformStatus | int | None = None
    sensor_control_mode: SensorControlMode | int | None = None
    take_off_time_us: int | None = None
    mi_storage_capacity_gb: int | None = None
    leap_seconds: int | None = None
    correction_offset_us: int | None = None
    active_payloads: bytes | None = None
    generic_flag_data: int | None = None
    security_local_set: bytes | None = None
    rvt: bytes | None = None
    vmti: bytes | None = None
    sar_mi_local_set: bytes | None = None
    range_image_local_set: bytes | None = None
    geo_registration_local_set: bytes | None = None
    composite_imaging_local_set: bytes | None = None
    segment_local_set: bytes | None = None
    amend_local_set: bytes | None = None
    outside_air_temp_c: int | None = None
    weapon_load: int | None = None
    weapon_fired: int | None = None
    laser_prf_code: int | None = None
    alternate_platform_name: str | None = None
    event_start_time_us: int | None = None
    stream_designator: str | None = None
    operational_base: str | None = None
    broadcast_source: str | None = None
    target_id: str | None = None
    communications_method: str | None = None
    icing_detected: IcingDetected | int | None = None
    sensor_fov_name: SensorFovName | int | None = None
    operational_mode: OperationalMode | int | None = None
    image_horizon: ImageHorizonPixels | None = None
    control_commands: tuple[ControlCommand, ...] = ()
    control_command_verification: tuple[int, ...] | None = None
    active_wavelengths: tuple[int, ...] | None = None
    sensor_frame_rate: SensorFrameRate | None = None
    metadata_substream_id: MetadataSubstreamId | None = None
    country_codes: CountryCodes | None = None
    wavelengths_list: tuple[WavelengthRecord, ...] | None = None
    airbase_locations: AirbaseLocations | None = None
    payload_list: PayloadList | None = None
    weapons_stores: tuple[WeaponsStore, ...] | None = None
    waypoint_list: tuple[Waypoint, ...] | None = None
    view_domain: ViewDomain | None = None
    sdcc_flps: tuple[SdccFlpField, ...] = ()
    unknown: tuple[tuple[int, bytes], ...] = ()
    field_errors: tuple[KlvFieldError, ...] = ()
    sentinel_tags: tuple[int, ...] = ()
    imapb_specials: tuple[tuple[int, str, int], ...] = ()
# END GENERATED by print-records (UasDatalinkLs)

    def __post_init__(self) -> None:
        # universal_label is the SMPTE UL identifying this LS;
        # it is always exactly 16 bytes on the wire. Reject other lengths
        # at construction rather than at the encoder/PyO3 boundary.
        if len(self.universal_label) != 16:
            raise ValueError(
                f"UasDatalinkLs.universal_label must be exactly 16 bytes; "
                f"got {len(self.universal_label)}"
            )

    def with_(self, **changes: object) -> "UasDatalinkLs":
        """Return a copy with the named fields replaced. The typed sets
        are frozen dataclasses (attribute assignment raises
        `FrozenInstanceError`) — this is the ergonomic update path.
        Thin wrapper over `dataclasses.replace`: unknown field names
        raise `TypeError`, and construction-time validation re-runs on
        the copy."""
        return replace(self, **changes)

    def sensor_position(self) -> GeoPoint | None:
        if (
            self.sensor_lat_deg is not None
            and self.sensor_lon_deg is not None
            and self.sensor_alt_m is not None
        ):
            return GeoPoint(
                lat_deg=self.sensor_lat_deg,
                lon_deg=self.sensor_lon_deg,
                alt_m=self.sensor_alt_m,
            )
        return None

    def sensor_attitude(self) -> Attitude | None:
        if (
            self.sensor_rel_az_deg is not None
            and self.sensor_rel_el_deg is not None
            and self.sensor_rel_roll_deg is not None
        ):
            return Attitude(
                heading_deg=self.sensor_rel_az_deg,
                pitch_deg=self.sensor_rel_el_deg,
                roll_deg=self.sensor_rel_roll_deg,
            )
        return None

    def sensor_fov(self) -> FieldOfView | None:
        if self.sensor_hfov_deg is not None and self.sensor_vfov_deg is not None:
            return FieldOfView(
                horizontal_deg=self.sensor_hfov_deg,
                vertical_deg=self.sensor_vfov_deg,
            )
        return None

    def platform_attitude(self) -> Attitude | None:
        if (
            self.platform_heading_deg is not None
            and self.platform_pitch_deg is not None
            and self.platform_roll_deg is not None
        ):
            return Attitude(
                heading_deg=self.platform_heading_deg,
                pitch_deg=self.platform_pitch_deg,
                roll_deg=self.platform_roll_deg,
            )
        return None

    def frame_center(self) -> GeoPoint | None:
        if (
            self.frame_center_lat_deg is not None
            and self.frame_center_lon_deg is not None
            and self.frame_center_elev_m is not None
        ):
            return GeoPoint(
                lat_deg=self.frame_center_lat_deg,
                lon_deg=self.frame_center_lon_deg,
                alt_m=self.frame_center_elev_m,
            )
        return None

    def corners(self) -> Corners | None:
        # Prefer absolute (tags 82-89) when fully populated.
        absolute = (
            self.corner_lat_p1_deg,
            self.corner_lon_p1_deg,
            self.corner_lat_p2_deg,
            self.corner_lon_p2_deg,
            self.corner_lat_p3_deg,
            self.corner_lon_p3_deg,
            self.corner_lat_p4_deg,
            self.corner_lon_p4_deg,
        )
        if all(v is not None for v in absolute):
            return Corners(
                p1=(absolute[0], absolute[1]),  # type: ignore[arg-type]
                p2=(absolute[2], absolute[3]),  # type: ignore[arg-type]
                p3=(absolute[4], absolute[5]),  # type: ignore[arg-type]
                p4=(absolute[6], absolute[7]),  # type: ignore[arg-type]
            )
        # Fall back to offsets + frame center.
        if self.frame_center_lat_deg is None or self.frame_center_lon_deg is None:
            return None
        offsets = (
            self.corner_lat_offset_p1_deg,
            self.corner_lon_offset_p1_deg,
            self.corner_lat_offset_p2_deg,
            self.corner_lon_offset_p2_deg,
            self.corner_lat_offset_p3_deg,
            self.corner_lon_offset_p3_deg,
            self.corner_lat_offset_p4_deg,
            self.corner_lon_offset_p4_deg,
        )
        if not all(v is not None for v in offsets):
            return None
        lat0 = self.frame_center_lat_deg
        lon0 = self.frame_center_lon_deg
        return Corners(
            p1=(lat0 + offsets[0], lon0 + offsets[1]),  # type: ignore[operator]
            p2=(lat0 + offsets[2], lon0 + offsets[3]),  # type: ignore[operator]
            p3=(lat0 + offsets[4], lon0 + offsets[5]),  # type: ignore[operator]
            p4=(lat0 + offsets[6], lon0 + offsets[7]),  # type: ignore[operator]
        )


# Spec-compat alias.
Klv0601 = UasDatalinkLs


OutOfRangePolicy = _native_mod.OutOfRangePolicy

decode_uas_datalink = _native_mod.decode_uas_datalink
encode_uas_datalink = _native_mod.encode_uas_datalink
encode_uas_datalink_strict_compliance = _native_mod.encode_uas_datalink_strict_compliance
st0601_sentinel_meaning = _native_mod.st0601_sentinel_meaning


# ---------------------------------------------------------------------------
# ST 1204.3 MIIS Core Identifier
# ---------------------------------------------------------------------------


class IdType(enum.Enum):
    """Source type for a sensor or platform UUID within a CoreId.

    Maps to the two-bit field in the ST 1204.3 §7.3.1 Table 3 usage byte:
    ``11`` → Physical, ``10`` → Virtual, ``01`` → Managed."""

    PHYSICAL = "physical"
    VIRTUAL = "virtual"
    MANAGED = "managed"


# BEGIN GENERATED by print-records (CoreId); do not edit
@dataclass(frozen=True, slots=True)
class CoreId:
    """A decoded MIIS Core Identifier (ST 1204.3 §7.3).

    Contains the raw version byte plus up to four optional UUID components.
    The EBNF constraint — ``minor`` XOR any-of-(sensor/platform/window) — is
    enforced by ``decode`` and should be maintained by callers that encode
    the record.

    Carriage sites: ST 0601 Tag 94 is the primary path for UAS datalink, and
    ST 0903 VTarget Tag 13 carries the identifier per target.

    - ``version`` — Wire version byte. Always ``1`` for decoded values; set
      to ``1`` when constructing a value to encode.
    - ``sensor`` — Sensor UUID and its type, if present.
    - ``platform`` — Platform UUID and its type, if present.
    - ``window`` — Window UUID, if present. Window has no type bits — its
      presence is indicated by usage-byte bit b2.
    - ``minor`` — Minor Core Identifier UUID. When present,
      sensor/platform/window must all be ``None`` (ST 1204.3 §7.3.1 EBNF
      rule)."""

    version: int
    sensor: tuple[IdType, bytes] | None = None
    platform: tuple[IdType, bytes] | None = None
    window: bytes | None = None
    minor: bytes | None = None
# END GENERATED by print-records (CoreId)

    def with_(self, **changes: object) -> "CoreId":
        """Return a copy with the named fields replaced."""
        return replace(self, **changes)


decode_core_id = _native_mod.decode_core_id
encode_core_id = _native_mod.encode_core_id
core_id_text = _native_mod.core_id_text


# ---------------------------------------------------------------------------
# ST 1010.3 SDCC-FLP (Standard Deviation and Correlation Coefficient
# pack, Floating-Point variant) — general-purpose, not ST 0601-specific.
# Carried inside ST 0601 Item 102 (see ``SdccFlpField``), but usable by
# any Parent Document.
# ---------------------------------------------------------------------------


# BEGIN GENERATED by print-records (SdccFlp); do not edit
@dataclass(frozen=True, slots=True)
class SdccFlp:
    """One parsed SDCC-FLP (ST 1010.3 §6-§7). N×N symmetric: N std devs
    (diagonal) + N(N-1)/2 correlations (upper triangle, row-major).

    - ``matrix_size`` — The matrix side length N. A fixed-width 64-bit
      count: a size on a public struct must not vary shape across 32- and
      64-bit targets.
    - ``std_devs`` — Diagonal σ values, length == matrix_size (empty when
      Slen == 0).
    - ``correlations`` — Upper-triangle correlations, row-major (i<j),
      ABSENT slots as 0.0 (sparse-mode zeros are reconstituted). Unlike
      ``std_devs``, this is ALWAYS the full
      ``matrix_size*(matrix_size-1)/2`` length regardless of ``Clen`` — a
      ``Clen==0`` pack (no correlation data at all) still gets a zero-filled
      vector of that length, not an empty one. Only empty when ``matrix_size
      <= 1`` (no possible correlation slots).
    - ``correlation_present`` — True where the wire actually carried the
      slot (all-true in full mode)."""

    matrix_size: int
    std_devs: tuple[float, ...] = ()
    correlations: tuple[float, ...] = ()
    correlation_present: tuple[bool, ...] = ()
# END GENERATED by print-records (SdccFlp)

    def correlation(self, i: int, j: int) -> float:
        """rho(i,j) with symmetry; sigma via std_devs[i] on the diagonal.
        Pure-Python port of the Rust `SdccFlp::correlation` (5-line
        row-major upper-triangle index) — no FFI crossing needed.

        Raises `IndexError` if `i` or `j` is `>= matrix_size` (the
        natural Python equivalent of the Rust panic), or on a diagonal
        query (`i == j`) if this pack has no standard-deviation data at
        all (`Slen == 0` — spec-legal, see the `std_devs` field doc).
        Off-diagonal queries never hit that second case — `correlations`
        is always sized to the full triangle regardless of `Clen`."""
        n = self.matrix_size
        if not (0 <= i < n and 0 <= j < n):
            raise IndexError(
                f"SdccFlp.correlation index out of bounds: ({i}, {j}) for matrix_size {n}"
            )
        if i == j:
            if i >= len(self.std_devs):
                raise IndexError(
                    f"SdccFlp.correlation({i}, {i}): no standard-deviation value "
                    "available (Slen==0 / an empty std_devs for this pack)"
                )
            return self.std_devs[i]
        lo, hi = (i, j) if i < j else (j, i)
        offset = lo * (2 * n - lo - 1) // 2
        return self.correlations[offset + (hi - lo - 1)]


decode_sdcc_flp = _native_mod.decode_sdcc_flp
encode_sdcc_flp_mode2 = _native_mod.encode_sdcc_flp_mode2


# ---------------------------------------------------------------------------
# ST 0902.8 Minimum Metadata Set (MISMMS) violation
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class MismmsViolation:
    """A violation of the ST 0902.8 Minimum Metadata Set requirements.

    ``kind`` is one of:

    - ``"missing"`` — a required MISMMS item is absent from the record;
      for Tag 48, also covers a ST 0102 decode failure.
    - ``"missing_security"`` — a required sub-item of the ST 0102 Security
      Local Set (Tag 48) is absent.
    - ``"zero_length"`` — the tag is present but has a zero-length wire
      value, which does NOT satisfy MISMMS presence (ST 0902.8-05).
    - ``"alternation_conflict"`` — Tags 75 and 104 are both present within
      the ``15|75|104`` group; they are mutually exclusive.

    ``tag`` is the primary (or only) tag involved. For
    ``"alternation_conflict"``, ``tag_b`` carries the second tag (104).
    ``name`` is the human-readable MISMMS item label when available."""

    kind: str  # "missing" | "missing_security" | "zero_length" | "alternation_conflict"
    tag: int
    name: str | None = None
    tag_b: int | None = None


validate_mismms = _native_mod.validate_mismms


def patch_uas_datalink(raw: bytes, edits: UasDatalinkLs | dict[str, object]) -> bytes:
    """Patch named tags in a raw ST 0601 local set; every other TLV is
    copied byte-for-byte in original order and the Tag 1 checksum is
    recomputed (only if the input carries one).

    ``edits`` is either a ``dict`` of ``UasDatalinkLs`` field names
    (e.g. ``{"corner_lat_p1_deg": 33.99}``) or a partial
    ``UasDatalinkLs`` itself — ``None`` fields leave the input
    untouched. Tags outside the typed model can be replaced through the
    ``unknown`` field: ``{"unknown": ((200, b"..."),)}``. Edited tags
    absent from the input are inserted before the trailing checksum.
    Bytes after the declared outer length are preserved verbatim.
    ``universal_label`` and ``declared_version`` are accepted but
    ignored — the input's 16-byte UL is always copied verbatim.
    Deleting a tag is not supported.

    Unlike ``decode_uas_datalink`` → ``encode_uas_datalink`` round
    trips, this never reorders TLVs and never re-encodes values you did
    not name — vendor tags, unmodeled tags, and non-canonical length
    encodings all survive verbatim. (Editing a tag canonicalizes that
    one TLV's encoding, even if the new value equals the old.)

    Raises ``KlvError`` for a malformed input local set and
    ``KlvEncodeError`` for an edit value that cannot be encoded
    (out-of-range, string too long, typed tag in ``unknown``).
    """
    if isinstance(edits, dict):
        edits = UasDatalinkLs(**edits)
    return _native_mod.patch_uas_datalink(raw, edits)


# ---------------------------------------------------------------------------
# Universal Label dispatcher
# ---------------------------------------------------------------------------


def _read_ber_length(buf: bytes, offset: int) -> tuple[int, int]:
    """Read a BER short/long-form length starting at `offset`. Returns
    `(value, bytes_consumed)`. Raises ValueError if malformed."""
    if offset >= len(buf):
        raise ValueError("truncated BER length")
    first = buf[offset]
    if first < 0x80:
        return (first, 1)
    nbytes = first & 0x7F
    if nbytes == 0:
        raise ValueError("indefinite-length BER not permitted in KLV")
    if offset + 1 + nbytes > len(buf):
        raise ValueError("truncated BER long-form length")
    value = int.from_bytes(buf[offset + 1 : offset + 1 + nbytes], "big")
    return (value, 1 + nbytes)


def parse_klv_universal(buf: bytes, *, strict: bool = False):
    """Inspect the first 16 bytes of `buf` (the SMPTE Universal Label)
    and route to the matching typed decoder. Returns one of:

    - `UasDatalinkLs` (alias `Klv0601`) when the UL is in the ST 0601 family
    - `SecurityLs` (alias `Klv0102`) for `SECURITY_LS_UL`
    - `PrecisionTimeStampPack` (alias `Klv0605`) for `PRECISION_TIMESTAMP_PACK_UL`
    - `VmtiLs` (alias `Klv0903`) for `VMTI_LS_UL`
    - `None` when the UL doesn't match any known set

    With `strict=True`, the per-set decoder's strict mode is used:
    ST 0601 additionally requires the ST 0601 family UL pattern
    (bytes 13/14 are tolerated for legacy interop — see
    `is_st0601_family`); ST 0102 / ST 0903 reject missing required
    tags. ST 0605 has a single always-validating decode (no strict
    knob). Default is lenient — per-field issues land on the typed
    set's `.field_errors`.

    Raises `tstrans.exceptions.KlvError(BAD_UNIVERSAL_LABEL)` when
    `buf` is too short to contain a 16-byte UL.

    For the body-only sets (ST 0102, ST 0903), `parse_klv_universal`
    peels the UL + outer BER length wrapper before invoking the
    per-set decoder. For the others (ST 0601, ST 0605), the
    per-set decoder consumes the full buffer including the UL."""

    # Local import dodges any module-init ordering ambiguity.
    from tstrans.exceptions import KlvError, KlvErrorKind

    if len(buf) < 16:
        raise KlvError(
            kind=KlvErrorKind.BAD_UNIVERSAL_LABEL,
            message=f"buffer too short for 16-byte UL: have {len(buf)} bytes",
        )

    ul = buf[:16]

    if is_st0601_family(ul):
        return decode_uas_datalink(buf, strict=strict)
    if ul == PRECISION_TIMESTAMP_PACK_UL:
        return decode_precision_timestamp(buf)
    if ul == SECURITY_LS_UL:
        try:
            value_len, ber_bytes = _read_ber_length(buf, 16)
        except ValueError as e:
            raise KlvError(
                kind=KlvErrorKind.TRUNCATED_SET,
                message=f"ST 0102 outer BER length unreadable: {e}",
            ) from e
        body_start = 16 + ber_bytes
        body_end = body_start + value_len
        if body_end > len(buf):
            raise KlvError(
                kind=KlvErrorKind.TRUNCATED_SET,
                message=(
                    f"ST 0102 declared body length {value_len} exceeds "
                    f"available {len(buf) - body_start}"
                ),
            )
        if body_end < len(buf):
            raise KlvError(
                kind=KlvErrorKind.MALFORMED_BYTES,
                message=(
                    f"ST 0102 universal record has {len(buf) - body_end} "
                    f"trailing bytes after declared body length {value_len}"
                ),
            )
        return decode_security(buf[body_start:body_end], strict=strict)
    if ul == VMTI_LS_UL:
        try:
            value_len, ber_bytes = _read_ber_length(buf, 16)
        except ValueError as e:
            raise KlvError(
                kind=KlvErrorKind.TRUNCATED_SET,
                message=f"ST 0903 outer BER length unreadable: {e}",
            ) from e
        body_start = 16 + ber_bytes
        body_end = body_start + value_len
        if body_end > len(buf):
            raise KlvError(
                kind=KlvErrorKind.TRUNCATED_SET,
                message=(
                    f"ST 0903 declared body length {value_len} exceeds "
                    f"available {len(buf) - body_start}"
                ),
            )
        if body_end < len(buf):
            raise KlvError(
                kind=KlvErrorKind.MALFORMED_BYTES,
                message=(
                    f"ST 0903 universal record has {len(buf) - body_end} "
                    f"trailing bytes after declared body length {value_len}"
                ),
            )
        return decode_vmti(buf[body_start:body_end], strict=strict)

    return None


# ---------------------------------------------------------------------------
# ST 0806.4 RVT (Remote Video Terminal) Local Set
# ---------------------------------------------------------------------------


class RvtPoiType(enum.Enum):
    """ST 0806.4 Table 8-2 Tag 5 — Point of Interest Type. Wire-unknown
    codepoints surface as the raw `int` on `RvtPoi.poi_type` rather than
    an enum instance (same asymmetric pattern as `IcingDetected`)."""

    FRIENDLY = 1
    HOSTILE = 2
    TARGET = 3
    UNKNOWN = 4


class RvtAoiType(enum.Enum):
    """ST 0806.4 Table 8-3 Tag 6 — Area of Interest Type. Shares codes
    1/2/4 with `RvtPoiType`; code 3 is "Reserved" here rather than
    "Target". Wire-unknown codepoints surface as the raw `int` on
    `RvtAoi.aoi_type`."""

    FRIENDLY = 1
    HOSTILE = 2
    RESERVED = 3
    UNKNOWN = 4


class RvtUserDataType(enum.Enum):
    """ST 0806.4 Table 8-4 Tag 1 top-2-bit data-type code, derived from
    `RvtUserData.numeric_id_raw` — see `RvtUserData.data_type`. Fully
    enumerated (a 2-bit field has exactly these 4 values); unlike
    `RvtPoiType`/`RvtAoiType` there is no wire-unknown catch-all."""

    STRINGS = 0
    INT = 1
    UINT = 2
    EXPERIMENTAL = 3


# BEGIN GENERATED by print-records (RvtUserData); do not edit
@dataclass(frozen=True, slots=True)
class RvtUserData:
    """User Defined Local Set (Table 8-4), carried in RVT Tag 11. Exactly
    two items, fixed order (-21..-24): the numeric-id byte, then the
    payload.

    - ``numeric_id_raw`` — Tag 1 raw byte: bits 8-7 = data type, bits 6-1 =
      numeric id 0-63.
    - ``data`` — Tag 2 payload; concrete width is conveyed by its length."""

    numeric_id_raw: int
    data: bytes = b""
# END GENERATED by print-records (RvtUserData)

    @property
    def data_type(self) -> RvtUserDataType:
        return RvtUserDataType(self.numeric_id_raw >> 6)

    @property
    def numeric_id(self) -> int:
        return self.numeric_id_raw & 0x3F


# BEGIN GENERATED by print-records (RvtPoi); do not edit
@dataclass(frozen=True, slots=True)
class RvtPoi:
    """Point of Interest Local Set (ST 0806.4 Table 8-2), carried in RVT Tag
    12 (repeatable, ST 0806.4-25).

    Number, latitude and longitude are mandatory on encode (ST
    0806.4-08..-10). A ``sentinel_tags`` entry for Tag 2 or 3 counts as
    present even when the paired float field is ``None``: the wire carried
    the spec's ``0x80000000`` "error" indicator instead of a value (the same
    convention as the sentinel tag list of the ST 0601 record).

    - ``number`` — Tag 1: POI Number — mandatory on encode; unique
      identifier (uint16).
    - ``lat_deg`` — Tag 2: POI Latitude — encode range [-90, 90] deg
      (int32); ``0x80000000`` is the "error" sentinel (see
      ``sentinel_tags``).
    - ``lon_deg`` — Tag 3: POI Longitude — encode range [-180, 180] deg
      (int32); same sentinel handling as ``lat_deg``.
    - ``alt_m`` — Tag 4: POI Altitude — encode range [-900, 19000] m MSL
      (uint16).
    - ``poi_type`` — Tag 5: POI Type (uint8) — see ``RvtPoiType``.
    - ``text`` — Tag 6: POI Text Description — ISO 7 string, max 2048 bytes.
    - ``source_icon`` — Tag 7: POI Source Icon — ISO 7 string, max 127 bytes
      (MIL-STD-2525B).
    - ``source_id`` — Tag 8: POI Source ID — ISO 7 string, max 255 bytes.
    - ``label`` — Tag 9: POI Label — ISO 7 string, max 16 bytes. The spec
      text for this item gives the bare number "16" rather than the "Max. N"
      phrasing used by sibling items; treated here as a ≤16-byte cap like
      the others, but a fixed-width-padded reading remains possible pending
      a real-capture check (spec ambiguity, not yet resolved).
    - ``operation_id`` — Tag 10: POI Operation ID — ISO 7 string, max 127
      bytes.
    - ``sentinel_tags`` — Tags whose lat/lon carried the ``0x80000000``
      "error" sentinel on decode (see ``lat_deg`` / ``lon_deg``).
    - ``unknown`` — Tags not modeled above, passed through byte-for-byte.
    - ``field_errors`` — Per-field decode errors collected instead of
      aborting the whole set."""

    number: int | None = None
    lat_deg: float | None = None
    lon_deg: float | None = None
    alt_m: float | None = None
    poi_type: RvtPoiType | int | None = None
    text: str | None = None
    source_icon: str | None = None
    source_id: str | None = None
    label: str | None = None
    operation_id: str | None = None
    sentinel_tags: tuple[int, ...] = ()
    unknown: tuple[tuple[int, bytes], ...] = ()
    field_errors: tuple[KlvFieldError, ...] = ()
# END GENERATED by print-records (RvtPoi)


# BEGIN GENERATED by print-records (RvtAoi); do not edit
@dataclass(frozen=True, slots=True)
class RvtAoi:
    """Area of Interest Local Set (Table 8-3), carried in RVT Tag 13
    (repeatable, ST 0806.4-25). Corners: Point 1 = NW (upper-left), Point 3
    = SE (lower-right).

    Number, both corner pairs and the type are mandatory on encode (ST
    0806.4-13..-18); a sentinel entry counts as present under the same rule
    as the Point of Interest record.

    - ``number`` — Tag 1: AOI Number — mandatory on encode; unique
      identifier (uint16).
    - ``corner_lat_p1_deg`` — Tag 2: AOI Point 1 (NW) Latitude — encode
      range [-90, 90] deg (int32).
    - ``corner_lon_p1_deg`` — Tag 3: AOI Point 1 (NW) Longitude — encode
      range [-180, 180] deg (int32).
    - ``corner_lat_p3_deg`` — Tag 4: AOI Point 3 (SE) Latitude — encode
      range [-90, 90] deg (int32).
    - ``corner_lon_p3_deg`` — Tag 5: AOI Point 3 (SE) Longitude — encode
      range [-180, 180] deg (int32).
    - ``aoi_type`` — Tag 6: AOI Type (uint8) — see ``RvtAoiType``.
    - ``text`` — Tag 7: AOI Text Description — ISO 7 string, max 2048 bytes.
    - ``source_id`` — Tag 8: AOI Source ID — ISO 7 string, max 255 bytes.
    - ``label`` — Tag 9: AOI Label — ISO 7 string, max 16 bytes. Same
      bare-"16" spec-ambiguity caveat as ``label`` applies here.
    - ``operation_id`` — Tag 10: AOI Operation ID — ISO 7 string, max 127
      bytes.
    - ``sentinel_tags`` — Tags whose lat/lon carried the ``0x80000000``
      "error" sentinel on decode (see the corner lat/lon fields above).
    - ``unknown`` — Tags not modeled above, passed through byte-for-byte.
    - ``field_errors`` — Per-field decode errors collected instead of
      aborting the whole set."""

    number: int | None = None
    corner_lat_p1_deg: float | None = None
    corner_lon_p1_deg: float | None = None
    corner_lat_p3_deg: float | None = None
    corner_lon_p3_deg: float | None = None
    aoi_type: RvtAoiType | int | None = None
    text: str | None = None
    source_id: str | None = None
    label: str | None = None
    operation_id: str | None = None
    sentinel_tags: tuple[int, ...] = ()
    unknown: tuple[tuple[int, bytes], ...] = ()
    field_errors: tuple[KlvFieldError, ...] = ()
# END GENERATED by print-records (RvtAoi)


def _rvt_mgrs_string(
    zone: int | None,
    band_grid: str | None,
    easting: int | None,
    northing: int | None,
) -> str | None:
    """Reconstruct a 15-char MGRS string (zone zero-padded to 2, 3-char
    band+grid, easting/northing zero-padded to 5) — pure-Python port of
    the Rust `RvtLs::aircraft_mgrs`/`frame_center_mgrs` helper. `None` if
    any of the four components is missing."""
    if zone is None or band_grid is None or easting is None or northing is None:
        return None
    return f"{zone:02d}{band_grid}{easting:05d}{northing:05d}"


# BEGIN GENERATED by print-records (RvtLs); do not edit
@dataclass(frozen=True, slots=True)
class RvtLs:
    """Remote Video Terminal Local Set (ST 0806.4 Table 8-1).

    Standalone-capable: it carries its own 16-byte Universal Label and, per
    ST 0806.4-02/-04, a timestamp-first Tag 2 plus a CRC-last Tag 1 when
    transmitted independently. It is also embeddable: ST 0601 Tag 73 carries
    the RVT LS body, with no Universal Label and no timestamp/CRC-position
    requirement.

    The checksum is CRC-32/MPEG-2 (ISO/IEC 13818-1), a real divergence from
    the ST 0601 16-bit running-sum checksum. It is captured on decode but
    only verified by the standalone decoder.

    - ``crc32`` — Tag 1: CRC-32/MPEG-2 checksum (uint32) — verified only by
      the standalone decoder (an embedded RVT need not carry it).
    - ``timestamp_us`` — Tag 2: User Defined Time Stamp — microseconds
      (uint64).
    - ``platform_true_airspeed`` — Tag 3: Platform True Airspeed — m/s, 1:1
      counts (uint16).
    - ``platform_indicated_airspeed`` — Tag 4: Platform Indicated Airspeed —
      m/s, 1:1 counts (uint16).
    - ``telemetry_accuracy_indicator`` — Tag 5: Telemetry Accuracy Indicator
      (uint8) — reserved by the spec.
    - ``frag_circle_radius_m`` — Tag 6: Frag Circle Radius — meters
      (uint16).
    - ``frame_code`` — Tag 7: Frame Code — 60 Hz counter (uint32).
    - ``rvt_ls_version`` — Tag 8: UAS LS Version Number (uint8) —
      informational; do NOT assert this equals 4 (the document edition), it
      tracks the wire schema.
    - ``video_data_rate`` — Tag 9: Video Data Rate — bps / Hz (uint32).
    - ``digital_video_file_format`` — Tag 10: Digital Video File Format —
      ISO 7 string, max 127 bytes.
    - ``user_defined`` — Tag 11: User Defined LS — repeatable (ST
      0806.4-25).
    - ``points_of_interest`` — Tag 12: Point of Interest LS — repeatable (ST
      0806.4-25).
    - ``areas_of_interest`` — Tag 13: Area of Interest LS — repeatable (ST
      0806.4-25).
    - ``aircraft_mgrs_zone`` — Tag 14: Aircraft MGRS Zone — UTM zone 1-60
      (uint8).
    - ``aircraft_mgrs_band_grid`` — Tag 15: Aircraft MGRS Latitude Band and
      Grid Square — 3-char ISO 7 string.
    - ``aircraft_mgrs_easting_m`` — Tag 16: Aircraft MGRS Easting — meters,
      0-99999 (uint24).
    - ``aircraft_mgrs_northing_m`` — Tag 17: Aircraft MGRS Northing —
      meters, 0-99999 (uint24).
    - ``frame_center_mgrs_zone`` — Tag 18: Frame Center MGRS Zone — UTM zone
      1-60 (uint8).
    - ``frame_center_mgrs_band_grid`` — Tag 19: Frame Center MGRS Latitude
      Band and Grid Square — 3-char ISO 7 string.
    - ``frame_center_mgrs_easting_m`` — Tag 20: Frame Center MGRS Easting —
      meters, 0-99999 (uint24).
    - ``frame_center_mgrs_northing_m`` — Tag 21: Frame Center MGRS Northing
      — meters, 0-99999 (uint24).
    - ``unknown`` — Tags not modeled above, passed through byte-for-byte.
    - ``field_errors`` — Per-field decode errors collected instead of
      aborting the whole set."""

    crc32: int | None = None
    timestamp_us: int | None = None
    platform_true_airspeed: int | None = None
    platform_indicated_airspeed: int | None = None
    telemetry_accuracy_indicator: int | None = None
    frag_circle_radius_m: int | None = None
    frame_code: int | None = None
    rvt_ls_version: int | None = None
    video_data_rate: int | None = None
    digital_video_file_format: str | None = None
    user_defined: tuple[RvtUserData, ...] = ()
    points_of_interest: tuple[RvtPoi, ...] = ()
    areas_of_interest: tuple[RvtAoi, ...] = ()
    aircraft_mgrs_zone: int | None = None
    aircraft_mgrs_band_grid: str | None = None
    aircraft_mgrs_easting_m: int | None = None
    aircraft_mgrs_northing_m: int | None = None
    frame_center_mgrs_zone: int | None = None
    frame_center_mgrs_band_grid: str | None = None
    frame_center_mgrs_easting_m: int | None = None
    frame_center_mgrs_northing_m: int | None = None
    unknown: tuple[tuple[int, bytes], ...] = ()
    field_errors: tuple[KlvFieldError, ...] = ()
# END GENERATED by print-records (RvtLs)

    def with_(self, **changes: object) -> "RvtLs":
        """Return a copy with the named fields replaced."""
        return replace(self, **changes)

    @property
    def aircraft_mgrs(self) -> str | None:
        """Reconstructed 15-char aircraft MGRS string (Tags 14-17), or
        `None` if any component is missing."""
        return _rvt_mgrs_string(
            self.aircraft_mgrs_zone,
            self.aircraft_mgrs_band_grid,
            self.aircraft_mgrs_easting_m,
            self.aircraft_mgrs_northing_m,
        )

    @property
    def frame_center_mgrs(self) -> str | None:
        """Reconstructed 15-char frame-center MGRS string (Tags 18-21),
        same layout as `aircraft_mgrs`."""
        return _rvt_mgrs_string(
            self.frame_center_mgrs_zone,
            self.frame_center_mgrs_band_grid,
            self.frame_center_mgrs_easting_m,
            self.frame_center_mgrs_northing_m,
        )


decode_rvt = _native_mod.decode_rvt
decode_rvt_standalone = _native_mod.decode_rvt_standalone
encode_rvt = _native_mod.encode_rvt
encode_rvt_standalone = _native_mod.encode_rvt_standalone


# ---------------------------------------------------------------------------
# ST 0805.1 KLV -> Cursor-on-Target (CoT) conversion
# ---------------------------------------------------------------------------


# BEGIN GENERATED by print-records (CotConfig); do not edit
@dataclass(frozen=True, slots=True)
class CotConfig:
    """Configuration for KLV→CoT conversion (ST 0805.1).

    - ``platform_type`` — CoT type for the Platform Position event. ST
      0805.1 §5 gives ``a-f-A-M-F`` as the fixed-wing example and explicitly
      requires it be overridable per platform (rotary-wing, manned pods,
      ...).
    - ``update_interval_us`` — The ``stale`` time is the event time plus
      this interval. ST 0805.1 defines ``stale`` as "time of next message"
      but gives no concrete interval — this default is an implementation
      choice, not a spec value.
    - ``producer`` — XML attribute name stamped in ``<detail><_flow-tags_
      .../>``. Written verbatim as an XML ``Name`` production (an attribute
      name, not a value) — it must be a syntactically valid XML Name. It is
      neither validated nor escaped; an invalid value produces malformed
      XML.
    - ``geoid_undulation_m`` — Geoid undulation (HAE − MSL) applied when
      only an MSL-referenced altitude tag is available. Absent emits the MSL
      value as-is.
    - ``how`` — CoT ``how`` attribute. ST 0805.1 §5 fixes this at ``m-p``
      (machine-passed) for both event types."""

    platform_type: str = "a-f-A-M-F"
    update_interval_us: int = 5_000_000
    producer: str = "ST0601CoT"
    geoid_undulation_m: float | None = None
    how: str = "m-p"
# END GENERATED by print-records (CotConfig)


platform_position_xml = _native_mod.platform_position_xml
sensor_point_of_interest_xml = _native_mod.sensor_point_of_interest_xml
platform_uid = _native_mod.platform_uid
spi_uid = _native_mod.spi_uid


__all__: list[str] = [
    "KlvFieldErrorKind",
    "KlvFieldError",
    "ST_0601_UL",
    "SECURITY_LS_UL",
    "PRECISION_TIMESTAMP_PACK_UL",
    "VMTI_LS_UL",
    "is_st0601_family",
    "TimeStatus",
    "PrecisionTimeStampPack",
    "Klv0605",
    "decode_precision_timestamp",
    "encode_precision_timestamp",
    "SecurityClassification",
    "ClassifyingCountryCodingMethod",
    "ObjectCountryCodingMethod",
    "SecurityLs",
    "Klv0102",
    "decode_security",
    "encode_security",
    "encode_security_strict_compliance",
    "VTargetPack",
    "VmtiLs",
    "Klv0903",
    "decode_vmti",
    "encode_vmti",
    "encode_vmti_standalone",
    "encode_vmti_strict_compliance",
    "encode_vmti_standalone_strict_compliance",
    "GeoPoint",
    "Attitude",
    "FieldOfView",
    "Corners",
    "OutOfRangePolicy",
    "IcingDetected",
    "SensorFovName",
    "OperationalMode",
    "PlatformStatus",
    "SensorControlMode",
    "ImageHorizonPixels",
    "ControlCommand",
    "SensorFrameRate",
    "MetadataSubstreamId",
    "CountryCodes",
    "WavelengthRecord",
    "Location",
    "AirbaseLocations",
    "PayloadType",
    "PayloadRecord",
    "PayloadList",
    "WeaponsStore",
    "Waypoint",
    "ViewDomainPair",
    "ViewDomain",
    "SdccFlpField",
    "UasDatalinkLs",
    "Klv0601",
    "decode_uas_datalink",
    "encode_uas_datalink",
    "encode_uas_datalink_strict_compliance",
    "patch_uas_datalink",
    "st0601_sentinel_meaning",
    "parse_klv_universal",
    "IdType",
    "CoreId",
    "decode_core_id",
    "encode_core_id",
    "core_id_text",
    "SdccFlp",
    "decode_sdcc_flp",
    "encode_sdcc_flp_mode2",
    "MismmsViolation",
    "validate_mismms",
    "RvtPoiType",
    "RvtAoiType",
    "RvtUserDataType",
    "RvtUserData",
    "RvtPoi",
    "RvtAoi",
    "RvtLs",
    "decode_rvt",
    "decode_rvt_standalone",
    "encode_rvt",
    "encode_rvt_standalone",
    "CotConfig",
    "platform_position_xml",
    "sensor_point_of_interest_xml",
    "platform_uid",
    "spi_uid",
]
