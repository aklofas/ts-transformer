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


@dataclass(frozen=True, slots=True)
class PrecisionTimeStampPack:
    """MISB ST 0605 §7 Precision Time Stamp Pack typed view.

    Wire form is 26 bytes: 16-byte UL + 1-byte BER length (`0x09`) +
    1-byte `TimeStatus` + 8-byte big-endian microsecond timestamp."""

    time_status: TimeStatus
    timestamp_us: int

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


@dataclass(frozen=True, slots=True)
class SecurityLs:
    """MISB ST 0102.12 Security Metadata Local Set typed view.

    Required tags per spec (§6.7 Table 1): 1, 2, 3, 12, 13, 22.
    Lenient decode tolerates missing required tags + unknown enum
    codepoints (surfaced as raw int) + malformed Tag 13 UTF-16
    (recorded in `field_errors`; raw bytes are NOT preserved in
    `unknown` for Tag 13 per Rust's decode comment — re-emitting
    malformed UTF-16 wouldn't help any consumer). Strict decode
    (`decode_security(buf, strict=True)`) rejects all of the above.

    Unknown enum codepoints on Tags 1 / 2 / 12 surface as the raw
    `int` codepoint rather than an enum instance — callers checking
    `isinstance(field, SecurityClassification)` will be False for
    out-of-spec codepoints; check `isinstance(..., int)` if you want
    to handle forward-compat values.
    """

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


@dataclass(frozen=True, slots=True)
class VTargetPack:
    """MISB ST 0903.6 §10.2 Table 10 per-target pack.

    Wire form is a leading BER-OID-encoded `target_id` (no Tag — per
    §10.2.2.1) followed by a Local Set body with BER-OID tag + BER
    short/long length + value tuples.

    7 nested LSes (VMask, VObject, VFeature, VTracker, VChip, VChipSeries,
    VObjectSeries) stay as `bytes | None` pass-through bytes — typed
    inner layers are deferred at the Rust layer too (see
    `docs/deferred-features.md`).

    `detection_status` is the raw §10.2.2.24 / §7.2 Table 5 codepoint:
    0=Inactive, 1=Active-Moving, 2=Dropped, 3=Active-Stopped,
    4=Active-Coasting. Typed enum deferred — stays as raw `int`."""

    target_id: int  # BER-OID, capped at u32::MAX

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
    centroid_pixel: int | None = None
    bbox_top_left_pixel: int | None = None
    bbox_bottom_right_pixel: int | None = None
    priority: int | None = None
    confidence_level: int | None = None
    history: int | None = None
    percentage_of_target_pixels: int | None = None
    target_color: tuple[int, int, int] | None = None  # R, G, B
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


# ---------------------------------------------------------------------------
# ST 0903.6 VMTI Local Set
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class VmtiLs:
    """MISB ST 0903.6 VMTI (Video Moving Target Indicator) Local Set
    typed view.

    Required tags per ST 0903.6 §6 Table 1: precision_time_stamp,
    vmti_system_name (when applicable), version_number, frame_width,
    frame_height. Lenient decode tolerates missing required tags and
    surfaces per-field decode failures in `field_errors`. Strict
    decode rejects missing required tags.

    `algorithm_series` and `ontology_series` are top-level nested LS
    pass-through bytes (typed inner layers deferred at the Rust layer
    too — see `docs/deferred-features.md`).

    `miis_id` is the MISB ST 1204 Minor Item Identification System
    Core Identifier — pass-through bytes (typed layer deferred)."""

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


@dataclass(frozen=True, slots=True)
class ImageHorizonPixels:
    """ST 0601.19 §8.81 Item 81 — Image Horizon Pixels: screen-space
    horizon line endpoints as percentages of image width/height, plus an
    optional geodetic pair for each endpoint. The four ``*_deg`` fields
    are a truncatable trailing group — any of them may be absent."""

    x0_pct: int
    y0_pct: int
    x1_pct: int
    y1_pct: int
    start_lat_deg: float | None = None
    start_lon_deg: float | None = None
    end_lat_deg: float | None = None
    end_lon_deg: float | None = None


@dataclass(frozen=True, slots=True)
class ControlCommand:
    """ST 0601.19 §8.115 Item 115 — Control Command. MULTI-INSTANCE
    (ST 0601.19 Table 1 "Multiples Allowed" = Yes): every wire
    occurrence appends one ``ControlCommand`` to
    ``UasDatalinkLs.control_commands``."""

    id: int
    command: str
    time_us: int | None = None


@dataclass(frozen=True, slots=True)
class SensorFrameRate:
    """ST 0601.19 §8.127 Item 127 — Sensor Frame Rate Pack: frame rate as
    a numerator/denominator ratio."""

    numerator: int
    denominator: int

    @property
    def fps(self) -> float:
        """Frames per second as ``numerator / denominator``."""
        return self.numerator / self.denominator


@dataclass(frozen=True, slots=True)
class MetadataSubstreamId:
    """ST 0601.19 §8.143 Item 143 — Metadata Substream Id. Per §8.143,
    ``uuid`` is REQUIRED when ``local_id == 0`` and OMITTED when
    ``local_id > 0``; this binding is lenient and stores whatever
    combination the wire carries."""

    local_id: int
    uuid: bytes | None = None

    def __post_init__(self) -> None:
        if self.uuid is not None and len(self.uuid) != 16:
            raise ValueError(
                f"MetadataSubstreamId.uuid must be 16 bytes; got {len(self.uuid)}"
            )


@dataclass(frozen=True, slots=True)
class CountryCodes:
    """ST 0601.19 §8.122 Item 122 — Country Codes metadata about the
    platform's operation and manufacture. Per §8.122.1, a length-0 wire
    value means "unknown" (``None`` here), not a distinct empty string."""

    coding_method: int
    overflight: str | None = None
    operator: str | None = None
    manufacture: str | None = None


@dataclass(frozen=True, slots=True)
class WavelengthRecord:
    """One record of ST 0601.19 §8.128 Item 128, Wavelengths List — a
    sensor wavelength band definition (``min_nm``/``max_nm`` decoded via
    ST 1201.5 IMAPB(0, 1e9, 4))."""

    id: int
    min_nm: float
    max_nm: float
    name: str


@dataclass(frozen=True, slots=True)
class Location:
    """A WGS84 geodetic point: latitude, longitude, and Height Above
    Ellipsoid (HAE). Shared by ST 0601.19 §8.130 (Airbase Locations) and
    §8.141 (Waypoint List). ``lat_deg``/``lon_deg`` are a mandatory
    both-or-neither pair on the wire; only ``hae_m`` truncates
    independently — see the Rust ``Location`` rustdoc for the full
    wire-shape contract this binding's encoder enforces."""

    lat_deg: float | None = None
    lon_deg: float | None = None
    hae_m: float | None = None


@dataclass(frozen=True, slots=True)
class AirbaseLocations:
    """ST 0601.19 §8.130 Item 130 — the take-off and recovery site
    locations. Per §8.130.1, a ``recovery`` absent entirely from the
    wire means "same as take-off" — decode reflects that by setting
    ``recovery`` equal to ``take_off`` (only an explicit, *different*
    ``recovery`` — including ``None`` while ``take_off`` is set — is
    distinguishable from that default)."""

    take_off: Location | None = None
    recovery: Location | None = None


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


@dataclass(frozen=True, slots=True)
class PayloadRecord:
    """One record of ST 0601.19 §8.138 Item 138, Payload List."""

    id: int
    payload_type: PayloadType | int
    name: str


@dataclass(frozen=True, slots=True)
class PayloadList:
    """ST 0601.19 §8.138 Item 138 — Payload List: a declared ``count``
    plus the payload records themselves."""

    count: int
    records: tuple[PayloadRecord, ...] = ()


@dataclass(frozen=True, slots=True)
class WeaponsStore:
    """One record of ST 0601.19 §8.140 Item 140, Weapons Stores — a
    single weapon's physical address, status, and type. ``status_raw``
    packs the spec's 14-bit Status BER-OID value verbatim; the
    ``@property`` accessors below decode its sub-fields (§Table 21
    General Status in the low 8 bits, §Table 22 Engagement Status flags
    in the next 4)."""

    station_id: int
    hardpoint_id: int
    carriage_id: int
    store_id: int
    status_raw: int
    weapon_type: str

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


@dataclass(frozen=True, slots=True)
class Waypoint:
    """One record of ST 0601.19 §8.141 Item 141, Waypoint List.
    ``prosecution_order``: 0 = current, > 0 = planned, < 0 = historical,
    ``0x7FFF`` = cancelled. ``info`` is the Mode/Source bitfield
    (b0 = manual mode, b1 = adhoc source)."""

    id: int
    prosecution_order: int
    info: int | None = None
    location: Location | None = None


@dataclass(frozen=True, slots=True)
class ViewDomainPair:
    """One ``(start, range)`` pair of ST 0601.19 §8.142 Item 142, View
    Domain. ``range_deg`` is always non-negative — "the angular range
    specifies the limit from the starting point to the sensor's maximum
    value"."""

    start_deg: float
    range_deg: float


@dataclass(frozen=True, slots=True)
class ViewDomain:
    """ST 0601.19 §8.142 Item 142 — up to three ``ViewDomainPair``s
    (azimuth, elevation, roll, in that fixed order); a pair absent from
    the wire (either an explicit zero-length marker or trailing
    truncation) decodes to ``None``."""

    azimuth: ViewDomainPair | None = None
    elevation: ViewDomainPair | None = None
    roll: ViewDomainPair | None = None


@dataclass(frozen=True, slots=True)
class SdccFlpField:
    """One captured ST 0601.19 §8.102 Item 102 (SDCC-FLP) occurrence, on
    ``UasDatalinkLs.sdcc_flps``. MULTI-INSTANCE (Table 1 "Multiples
    Allowed" = Yes): each wire occurrence refines the accuracy of the
    Local Set items that immediately precede it. ``preceding_tags`` is
    the wire-order item tags (known or unknown, but never another Item
    102) immediately before this occurrence; ``bytes`` is the raw
    SDCC-FLP pack — decode it with ``decode_sdcc_flp``.

    ``bytes`` round-trips byte-exact even for a malformed or
    foreign-encoder pack this binding cannot parse; re-encoding a record
    re-emits every ``sdcc_flps`` entry verbatim, grouped together in
    tag-ascending position (the *original* interleaving recorded by
    ``preceding_tags`` is therefore not guaranteed to be reproduced on
    re-encode — see the Rust ``SdccFlpField`` rustdoc)."""

    preceding_tags: tuple[int, ...]
    bytes: bytes


# ---------------------------------------------------------------------------
# ST 0601 UAS Datalink Local Set
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class UasDatalinkLs:
    """MISB ST 0601 UAS Datalink Local Set typed view.

    Mirror of the 147-field Rust `tst_core::klv::st0601::UasDatalinkLs`
    flat struct. Composite views (sensor position, attitude, FOV,
    corners) are accessor methods that return `None` when any of the
    underlying primitive fields is absent.

    `security_local_set: bytes | None` carries the Tag 48 ST 0102 LS
    body bytes (no UL prefix); call `tstrans.klv.decode_security(...)`
    on it for typed access.

    `vmti: bytes | None` carries the Tag 74 ST 0903 LS body bytes (no
    UL prefix); call `tstrans.klv.decode_vmti(...)` on it for typed
    access.

    `unknown` preserves any tag not in the typed-modeled set per
    ST 0107.5 §6 future-proof skip rule.

    `field_errors` collects per-field decode failures from lenient
    mode; strict mode raises `KlvError` instead."""

    universal_label: bytes = b"\x00" * 16
    declared_version: int = 0

    def __post_init__(self) -> None:
        # universal_label is the SMPTE UL identifying this LS;
        # it is always exactly 16 bytes on the wire. Reject other lengths
        # at construction rather than at the encoder/PyO3 boundary.
        if len(self.universal_label) != 16:
            raise ValueError(
                f"UasDatalinkLs.universal_label must be exactly 16 bytes; "
                f"got {len(self.universal_label)}"
            )

    # Identity
    mission_id: str | None = None
    platform_tail_number: str | None = None
    platform_designation: str | None = None
    image_source_sensor: str | None = None
    image_coordinate_system: str | None = None
    platform_call_sign: str | None = None
    uas_ls_version: int | None = None
    # Time
    timestamp_us: int | None = None
    # Platform state
    platform_heading_deg: float | None = None
    platform_pitch_deg: float | None = None
    platform_roll_deg: float | None = None
    platform_true_airspeed: float | None = None
    platform_indicated_airspeed: float | None = None
    platform_pitch_full_deg: float | None = None
    platform_roll_full_deg: float | None = None
    platform_angle_of_attack_deg: float | None = None
    # Sensor pose & position
    sensor_lat_deg: float | None = None
    sensor_lon_deg: float | None = None
    sensor_alt_m: float | None = None
    sensor_ellipsoid_height_m: float | None = None
    sensor_hfov_deg: float | None = None
    sensor_vfov_deg: float | None = None
    sensor_rel_az_deg: float | None = None
    sensor_rel_el_deg: float | None = None
    sensor_rel_roll_deg: float | None = None
    # Ranging & frame center
    slant_range_m: float | None = None
    target_width_m: float | None = None
    frame_center_lat_deg: float | None = None
    frame_center_lon_deg: float | None = None
    frame_center_elev_m: float | None = None
    frame_center_ellipsoid_height_m: float | None = None
    # Image corner offsets (tags 26-33)
    corner_lat_offset_p1_deg: float | None = None
    corner_lon_offset_p1_deg: float | None = None
    corner_lat_offset_p2_deg: float | None = None
    corner_lon_offset_p2_deg: float | None = None
    corner_lat_offset_p3_deg: float | None = None
    corner_lon_offset_p3_deg: float | None = None
    corner_lat_offset_p4_deg: float | None = None
    corner_lon_offset_p4_deg: float | None = None
    # Image corner full lat/lon (tags 82-89)
    corner_lat_p1_deg: float | None = None
    corner_lon_p1_deg: float | None = None
    corner_lat_p2_deg: float | None = None
    corner_lon_p2_deg: float | None = None
    corner_lat_p3_deg: float | None = None
    corner_lon_p3_deg: float | None = None
    corner_lat_p4_deg: float | None = None
    corner_lon_p4_deg: float | None = None
    # Target location & tracking (tags 40-46)
    target_location_lat_deg: float | None = None
    target_location_lon_deg: float | None = None
    target_location_elev_m: float | None = None
    target_track_gate_width_px: float | None = None
    target_track_gate_height_px: float | None = None
    target_error_ce90_m: float | None = None
    target_error_le90_m: float | None = None
    # Weather / atmospheric (tags 35-38, 49, 53-55)
    wind_direction_deg: float | None = None
    wind_speed: float | None = None
    static_pressure_mbar: float | None = None
    density_altitude_m: float | None = None
    differential_pressure_mbar: float | None = None
    airfield_barometric_pressure_mbar: float | None = None
    airfield_elevation_m: float | None = None
    relative_humidity_pct: float | None = None
    # Extended platform state (tags 51, 52, 56-58, 64, 92, 93)
    platform_vertical_speed: float | None = None
    platform_sideslip_deg: float | None = None
    platform_ground_speed: float | None = None
    ground_range_m: float | None = None
    platform_fuel_remaining_kg: float | None = None
    platform_magnetic_heading_deg: float | None = None
    platform_angle_of_attack_full_deg: float | None = None
    platform_sideslip_full_deg: float | None = None
    # Alternate platform (tags 67-69, 71, 76)
    alternate_platform_lat_deg: float | None = None
    alternate_platform_lon_deg: float | None = None
    alternate_platform_alt_m: float | None = None
    alternate_platform_heading_deg: float | None = None
    alternate_platform_ellipsoid_height_m: float | None = None
    # Sensor velocity (tags 79-80)
    sensor_north_velocity: float | None = None
    sensor_east_velocity: float | None = None
    # IMAPB extended items (ST 1201.5, tags 96, 103-105, 109, 112-114,
    # 117-120, 132, 134)
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
    # Var-length int/enum items (ST 0601 tags 110-139)
    time_airborne_s: int | None = None
    propulsion_unit_speed_rpm: int | None = None
    navsats_in_view: int | None = None
    positioning_method_source: int | None = None  # bitfield: see Rust rustdoc for bit table
    platform_status: PlatformStatus | int | None = None
    sensor_control_mode: SensorControlMode | int | None = None
    take_off_time_us: int | None = None
    mi_storage_capacity_gb: int | None = None
    leap_seconds: int | None = None
    correction_offset_us: int | None = None
    active_payloads: bytes | None = None  # Tag 139 bitmask, LSB-first
    # Misc
    generic_flag_data: int | None = None
    security_local_set: bytes | None = None  # Tag 48 → ST 0102
    rvt: bytes | None = None  # Tag 73 → ST 0806 (module lands in a later work package)
    vmti: bytes | None = None  # Tag 74 → ST 0903
    miis_core_id: bytes | None = None  # Tag 94 → ST 1204
    sar_mi_local_set: bytes | None = None  # Tag 95 → ST 1206 (interior typing deferred)
    range_image_local_set: bytes | None = None  # Tag 97 → ST 1002 (deferred)
    geo_registration_local_set: bytes | None = None  # Tag 98 → ST 1601 (deferred)
    composite_imaging_local_set: bytes | None = None  # Tag 99 → ST 1602 (deferred)
    segment_local_set: bytes | None = None  # Tag 100 → ST 1607 (deferred)
    amend_local_set: bytes | None = None  # Tag 101 → ST 1607 (deferred)
    # Raw scalar & string items (tags 39, 60-62, 70, 72, 106-108, 129, 135)
    outside_air_temp_c: int | None = None
    weapon_load: int | None = None  # bit-packed nibbles: station/substation/type/variant
    weapon_fired: int | None = None
    laser_prf_code: int | None = None
    alternate_platform_name: str | None = None
    event_start_time_us: int | None = None
    stream_designator: str | None = None
    operational_base: str | None = None
    broadcast_source: str | None = None
    target_id: str | None = None
    communications_method: str | None = None
    # Coded enums (tags 34, 63, 77)
    icing_detected: IcingDetected | int | None = None
    sensor_fov_name: SensorFovName | int | None = None
    operational_mode: OperationalMode | int | None = None
    # Pack & list items (tags 81/102/115/116/121/122/127/
    # 128/130/138/140/141/142/143)
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
    # Pass-through
    unknown: tuple[tuple[int, bytes], ...] = ()
    field_errors: tuple[KlvFieldError, ...] = ()
    sentinel_tags: tuple[int, ...] = ()
    # Tags whose ST 1201.5 IMAPB wire value decoded to a spec-defined
    # special value rather than a normal-range float. Each entry is
    # ``(tag, code, payload)`` where ``code`` is one of ``"below_min"``,
    # ``"above_max"``, ``"pos_infinity"``, ``"neg_infinity"``,
    # ``"pos_quiet_nan"``, ``"neg_quiet_nan"``, ``"pos_signaling_nan"``,
    # ``"neg_signaling_nan"``, ``"user_defined"``, and ``payload`` is the
    # NaN-id/signal value (0 for the payload-less codes). Mirrors
    # ``sentinel_tags`` encode semantics: a non-None typed field wins over
    # a matching ``imapb_specials`` entry.
    imapb_specials: tuple[tuple[int, str, int], ...] = ()

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


@dataclass(frozen=True, slots=True)
class CoreId:
    """MISB ST 1204.3 MIIS Core Identifier typed view.

    A MIIS Core Identifier uniquely identifies a motion imagery source.
    It consists of a version byte and up to four optional 16-byte UUIDs
    (sensor, platform, window, minor). The ``minor`` field is mutually
    exclusive with sensor/platform/window (EBNF rule from ST 1204.3 §7.3.1).

    Carriage sites:
    - ST 0601 Tag 94 (``UasDatalinkLs.miis_core_id``) — primary path.
    - ST 0903 VTarget Tag 13 (``VTargetPack.miis_id``) — per-target.

    Use ``decode_core_id`` to parse raw bytes and ``encode_core_id`` to
    round-trip back to bytes. ``core_id_text`` returns the ST 1204.3
    §7.4.2 textual representation."""

    version: int
    sensor: tuple[IdType, bytes] | None = None
    platform: tuple[IdType, bytes] | None = None
    window: bytes | None = None
    minor: bytes | None = None

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


@dataclass(frozen=True, slots=True)
class SdccFlp:
    """One parsed SDCC-FLP (ST 1010.3 §6-§7): an N×N symmetric matrix —
    N standard deviations on the diagonal, N(N-1)/2 correlation
    coefficients in the upper triangle (row-major, i<j).

    ``correlations`` is always the full ``matrix_size * (matrix_size - 1)
    / 2``-length triangle regardless of how many the wire actually
    carried — absent (sparse-mode-omitted) slots are reconstituted as
    ``0.0``; ``correlation_present`` marks which slots the wire actually
    carried (all ``True`` in full/non-sparse mode). ``std_devs`` is empty
    when the wire carried no standard-deviation data at all (``Slen==0``
    — spec-legal)."""

    matrix_size: int
    std_devs: tuple[float, ...] = ()
    correlations: tuple[float, ...] = ()
    correlation_present: tuple[bool, ...] = ()

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


@dataclass(frozen=True, slots=True)
class RvtUserData:
    """ST 0806.4 Table 8-4 User Defined Local Set, carried on RVT Tag 11
    (repeatable — ST 0806.4-25). `numeric_id_raw` packs the Tag 1 byte
    verbatim (bits 8-7 = data type, bits 6-1 = numeric id 0-63);
    `data_type`/`numeric_id` are `@property` accessors computed from it
    (mirrors the Rust `RvtUserData::data_type()`/`numeric_id()` methods
    and the pure-Python bitfield-accessor pattern used by
    `WeaponsStore`) — only the two wire fields cross the binding."""

    numeric_id_raw: int
    data: bytes = b""

    @property
    def data_type(self) -> RvtUserDataType:
        return RvtUserDataType(self.numeric_id_raw >> 6)

    @property
    def numeric_id(self) -> int:
        return self.numeric_id_raw & 0x3F


@dataclass(frozen=True, slots=True)
class RvtPoi:
    """ST 0806.4 Table 8-2 Point of Interest Local Set, carried on RVT
    Tag 12 (repeatable — ST 0806.4-25). Number/Latitude/Longitude are
    mandatory on encode (ST 0806.4-08..-10); a `sentinel_tags` entry for
    tag 2 or 3 counts as present even when the paired float field is
    `None` (the wire carried the spec's `0x80000000` "error" indicator
    instead of a value — see `UasDatalinkLs.sentinel_tags` for the same
    convention at the ST 0601 layer)."""

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


@dataclass(frozen=True, slots=True)
class RvtAoi:
    """ST 0806.4 Table 8-3 Area of Interest Local Set, carried on RVT
    Tag 13 (repeatable — ST 0806.4-25). Corner 1 = NW (upper-left),
    Corner 3 = SE (lower-right). Number/both corner pairs/Type are
    mandatory on encode (ST 0806.4-13..-18); same sentinel-counts-as-
    present rule as `RvtPoi`."""

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


@dataclass(frozen=True, slots=True)
class RvtLs:
    """MISB ST 0806.4 RVT (Remote Video Terminal) Local Set typed view.

    Standalone-capable: carries its own 16-byte Universal Label and, per
    ST 0806.4-02/-04, a timestamp-first Tag 2 plus a CRC-last Tag 1 when
    transmitted independently — see `decode_rvt_standalone` /
    `encode_rvt_standalone`. Also embeddable: ST 0601 Tag 73 carries the
    RVT LS *body* (no UL, no timestamp/CRC-position requirement) — see
    `decode_rvt` / `encode_rvt` and `UasDatalinkLs`.

    The checksum (`crc32`) is CRC-32/MPEG-2 (ISO/IEC 13818-1) — a real
    divergence from the ST 0601 16-bit running-sum checksum. It is
    captured on decode but only verified by `decode_rvt_standalone` (an
    embedded RVT LS is not required to carry one)."""

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


@dataclass(frozen=True, slots=True)
class CotConfig:
    """Configuration for ST 0805.1 KLV -> CoT conversion
    (`platform_position_xml` / `sensor_point_of_interest_xml`). Defaults
    match the Rust `CotConfig::default()`.

    - `platform_type` — CoT `type` for the Platform Position event. ST
      0805.1 §5 gives `a-f-A-M-F` as the fixed-wing example and requires
      it be overridable per platform (rotary-wing, manned pods, ...). The
      Sensor Point of Interest event's `type` is fixed at `b-m-p-s-p-i`
      by the spec and is not configurable.
    - `update_interval_us` — `stale = time + update_interval_us`. ST
      0805.1 defines `stale` as "time of next message" but gives no
      concrete interval — this default is an implementation choice, not
      a spec value.
    - `producer` — XML attribute *name* stamped verbatim into
      `<detail><_flow-tags_ .../>`. It is a Name production (an
      attribute name, not a value): neither validated nor escaped, so an
      invalid value produces malformed XML.
    - `geoid_undulation_m` — geoid undulation (HAE - MSL) applied when
      only an MSL-referenced altitude tag is available. `None` emits the
      MSL value as-is.
    - `how` — CoT `how` attribute. ST 0805.1 §5 fixes this at `m-p`
      (machine-passed) for both event types.
    """

    platform_type: str = "a-f-A-M-F"
    update_interval_us: int = 5_000_000
    producer: str = "ST0601CoT"
    geoid_undulation_m: float | None = None
    how: str = "m-p"


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
