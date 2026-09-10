"""tstrans.mpegts — MPEG-TS packet, PES, PSI, muxer, demuxer surface.

Public types:

- `Pts90khz` — 90 kHz timestamp wrapper.
- `VideoCodec`, `AudioCodec`, `SubtitleCodec`, `StrictMode` enums.
- `StreamId`, `RawDescriptor`, `StreamInfo`, `KlvLink`, `ProgramMap` dataclasses.
- `DemuxEvent` base + subclasses: `ProgramMap`, `Video`, `Audio`,
  `Subtitle`, `Klv`, `UnknownSample`, `Discontinuity`, `NonConformant`,
  `ReconnectDiscontinuity`.
- `DemuxerConfig`, `Demuxer` — feed bytes, get events; supports
  `strict_mode`, `pes_cap_*`, `cfi_tolerance`.
- `MuxerConfig`, `MuxerProgramConfig`, `Muxer`, `MuxerFileSink` —
  build TS, drain to file (with optional atomic-rename mode).
- `MultiCellAuReason`, `CellFragmentIndication` — typed diagnostics
  on `NonConformant` events.
"""

import enum
import os
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any, ClassVar, Optional

# The compiled extension. Imported at module top (not just at the bottom
# re-export block) because `_VideoEvent` / `_AudioEvent` wrap their `.raw`
# in `_native.RawBytes` (WP-E PY-01). `_native` is self-contained and does
# not import `tstrans.mpegts`, so this is not a circular import.
from tstrans import _native


@dataclass(frozen=True, slots=True)
class Pts90khz:
    """A 90 kHz timestamp tick count, the MPEG-TS PTS unit.

    Wraps an `i64` to allow signed-diff arithmetic (per the Rust
    `tst_core::mpegts::common::Pts90khz`). Construct via
    `Pts90khz.from_raw(int)`, `Pts90khz.from_ms(int)`, or
    `Pts90khz.from_seconds(float)`.
    """

    raw: int

    def __post_init__(self) -> None:
        # Audit-2 #4 — fail-fast on out-of-i64 values; Rust extracts as
        # i64 and would raise OverflowError later anyway, but the early
        # ValueError points at the user's construction site.
        if not -(1 << 63) <= self.raw <= (1 << 63) - 1:
            raise ValueError(
                f"Pts90khz.raw must fit signed i64; got {self.raw}"
            )

    @classmethod
    def from_raw(cls, ticks: int) -> "Pts90khz":
        return cls(raw=int(ticks))

    @classmethod
    def from_ms(cls, ms: int) -> "Pts90khz":
        return cls(raw=int(ms) * 90)

    @classmethod
    def from_seconds(cls, seconds: float) -> "Pts90khz":
        return cls(raw=int(seconds * 90_000))

    @property
    def ms(self) -> int:
        # Sign-aware integer truncate toward zero — matches Rust's `i64 / 90`
        # semantics. Python's `//` floors toward -inf, which diverges on
        # negatives (e.g. `-1 // 90 == -1` vs Rust's `0`); a float-based
        # `int(raw / 90)` would lose precision past float64's 53-bit
        # mantissa, so do it in integers only.
        if self.raw < 0:
            return -((-self.raw) // 90)
        return self.raw // 90

    @property
    def seconds(self) -> float:
        return self.raw / 90_000.0

    def __repr__(self) -> str:
        return f"Pts90khz(raw={self.raw}, ms={self.ms})"


class VideoCodec(enum.Enum):
    """Mirrors Rust `tst_core::mpegts::demux::event::VideoCodec`."""
    H264 = "h264"
    H265 = "h265"
    H266 = "h266"
    AV1 = "av1"


class AudioCodec(enum.Enum):
    """Mirrors Rust `tst_core::mpegts::demux::event::AudioCodec`."""
    MP2 = "mp2"
    AAC = "aac"
    AAC_LATM = "aac_latm"
    AC3 = "ac3"


class SubtitleCodec(enum.Enum):
    """Mirrors Rust `tst_core::mpegts::demux::event::SubtitleCodec`."""
    DVB_SUBTITLING = "dvb_subtitling"
    DVB_TELETEXT = "dvb_teletext"
    CEA708_STANDALONE = "cea708_standalone"
    WEBVTT_IN_TS = "webvtt_in_ts"


class KlvStreamType(enum.Enum):
    """Mirrors Rust `tst_core::mpegts::mux::KlvStreamType`. Picks the
    PMT stream_type byte (and PES wrap shape) for KLV streams:
    `SYNCHRONOUS_METADATA` (0x15) is strict ST 1402 sync and triggers
    a 5-byte H.222.0 §2.12.4.2 Metadata_AU_cell wrapper (auto-prepended
    by the muxer — callers pass raw LS bytes); `PRIVATE_DATA` (0x06)
    is the broadly-recognized form (pass-through, no AU cell wrap).

    Whether the KLV PES carries a PTS is controlled separately via the
    `carries_pts` field on `KlvStreamSpec`.
    """

    PRIVATE_DATA = "private_data"
    SYNCHRONOUS_METADATA = "synchronous_metadata"


class Av1CarriageMode(enum.Enum):
    """Mirrors Rust `tst_core::mpegts::mux::Av1CarriageMode`. Default
    is `MPEG2_TS_BINDING` — AV1-in-MPEG-2-TS binding conformant
    carriage (PES `stream_id=0xBD`, OBUs wrapped in
    `ts_open_bitstream_unit()` framing).

    `INTEROP_RAW_OBU` is the interoperability mode for the
    ffmpeg / libaom / hls.js / mediamtx AV1-in-TS toolchain — PES
    `stream_id=0xE0` and raw OBU payload (no
    `ts_open_bitstream_unit` framing). Non-conformant per the binding
    spec, but matches the de facto carriage used by those tools today.

    Set the same value on both sides for a successful round-trip:

    - Sender side: pass to `MuxerConfigBuilder.av1_carriage(...)`.
    - Receiver side: pass to `DemuxerConfig(av1_carriage=...)`.

    A mismatch surfaces on the receiver as
    `NonConformantKind.AV1_WRONG_STREAM_ID` plus
    `NonConformantKind.AV1_MISSING_TS_OBU_FRAMING`; the Sample still
    arrives via the lenient raw-OBU fallback path.
    """

    MPEG2_TS_BINDING = "mpeg2_ts_binding"
    INTEROP_RAW_OBU = "interop_raw_obu"


# StreamSpec ABC + 5 concrete subclasses — match-statement-compat
# tagged union, same pattern as the DemuxEvent hierarchy.
# Mirrors Rust `tst_core::mpegts::mux::StreamSpec`'s variants.

@dataclass(frozen=True, slots=True)
class StreamSpec:
    """Abstract base for elementary-stream specs within a program.

    Concrete subclasses: `VideoStreamSpec`, `KlvStreamSpec`,
    `AudioStreamSpec`, `SubtitleStreamSpec`, `DataStreamSpec`. Frozen +
    slotted so instances are hashable, value-equal, and immutable.
    """

    pid: int


@dataclass(frozen=True, slots=True)
class VideoStreamSpec(StreamSpec):
    """Video elementary stream — `pid` + `codec`."""

    codec: VideoCodec


@dataclass(frozen=True, slots=True)
class KlvStreamSpec(StreamSpec):
    """KLV metadata elementary stream — `pid`, `stream_type`
    (`PRIVATE_DATA` 0x06 or `SYNCHRONOUS_METADATA` 0x15), and
    `carries_pts` (whether to emit a PTS in the PES header)."""

    stream_type: KlvStreamType
    carries_pts: bool


@dataclass(frozen=True, slots=True)
class AudioStreamSpec(StreamSpec):
    """Audio elementary stream — `pid`, `codec`, optional `language`
    (3-byte ISO 639-2 lowercase ASCII, e.g. `b"eng"`; None omits the
    ISO 639 language descriptor)."""

    codec: AudioCodec
    language: Optional[bytes] = None


@dataclass(frozen=True, slots=True)
class SubtitleStreamSpec(StreamSpec):
    """Subtitle / caption elementary stream — `pid` + `codec`. The
    codec value itself carries any per-variant parameters (language,
    DVB subtitling_type, etc.)."""

    codec: SubtitleCodec


@dataclass(frozen=True, slots=True)
class DataStreamSpec(StreamSpec):
    """Private/application data elementary stream — `pid`, raw PMT
    `stream_type` byte, and `carries_pts` (whether to emit a PTS in the
    PES header). The write-side twin of `StreamKindTag.UNKNOWN`
    streams: `MuxerConfig.from_program_map` reconstructs one per
    unknown-type stream. PMT descriptors are not carried here — they
    live on the muxer config's descriptor surface
    (`MuxerProgramConfig.stream_descriptors`), not the spec."""

    stream_type: int
    carries_pts: bool


# ---------------------------------------------------------------------------
# Mux-side subtitle codec config dataclasses
# ---------------------------------------------------------------------------
#
# Rust `tst_core::mpegts::mux::SubtitleCodec` is a struct-variant enum
# (DvbSubtitling + DvbTeletext carry per-stream params; Cea708Standalone
# + WebVttInTs are unit variants). The flat Python `SubtitleCodec` enum
# above is the demuxer-facing discriminator only — it doesn't carry the
# language / page-id payload the mux-side wants.
#
# These dataclasses model the full mux-side construction surface. Pass
# any of them to `MuxerProgramConfigBuilder.add_subtitle(codec_config)`
# and the PyO3 bridge translates to the Rust enum.
#
# Field naming matches the Rust struct fields exactly (snake_case);
# DVB language is the ISO 639-2/B 3-letter code as a 3-byte `bytes`
# value (lowercase ASCII), mirroring `[u8; 3]` on the Rust side.
# `__post_init__` enforces all wire-spec ranges so misuse fails at
# construction time, not later inside the muxer.


@dataclass(frozen=True, slots=True)
class DvbSubtitlingConfig:
    """DVB subtitling (bitmap-shaped). Per ETSI EN 300 468 §6.2.41 +
    ETSI EN 300 743.

    Fields mirror `tst_core::mpegts::mux::SubtitleCodec::DvbSubtitling`:

    - `language` — 3-byte ISO 639-2/B lowercase ASCII (e.g. `b"eng"`).
    - `subtitling_type` — ETSI EN 300 468 Table 26 codepoint
      (u8, 0..=255). Common values: 0x10 (DVB sub, no AR signalling),
      0x14 (DVB sub for 4:3 aspect-ratio).
    - `composition_page_id` — u16, 0..=0xFFFF.
    - `ancillary_page_id` — u16, 0..=0xFFFF.
    """

    language: bytes
    subtitling_type: int
    composition_page_id: int
    ancillary_page_id: int

    def __post_init__(self) -> None:
        # Reject bytearray: frozen+slots dataclass stores a reference, so a
        # mutable bytearray would weaken the immutability contract and break
        # hashing. bytes is the only accepted type.
        if not isinstance(self.language, bytes) or len(self.language) != 3:
            raise ValueError(
                f"language must be 3 bytes (ISO 639-2/B), "
                f"got {self.language!r}"
            )
        # `bool` is a subclass of `int` in Python — exclude it explicitly.
        if isinstance(self.subtitling_type, bool) or not isinstance(
            self.subtitling_type, int
        ):
            raise TypeError(
                f"subtitling_type must be int; "
                f"got {type(self.subtitling_type).__name__}"
            )
        if not 0 <= self.subtitling_type <= 0xFF:
            raise ValueError(
                f"subtitling_type must fit u8 (0..=255); got {self.subtitling_type}"
            )
        if isinstance(self.composition_page_id, bool) or not isinstance(
            self.composition_page_id, int
        ):
            raise TypeError(
                f"composition_page_id must be int; "
                f"got {type(self.composition_page_id).__name__}"
            )
        if not 0 <= self.composition_page_id <= 0xFFFF:
            raise ValueError(
                f"composition_page_id must fit u16 (0..=0xFFFF); "
                f"got {self.composition_page_id}"
            )
        if isinstance(self.ancillary_page_id, bool) or not isinstance(
            self.ancillary_page_id, int
        ):
            raise TypeError(
                f"ancillary_page_id must be int; "
                f"got {type(self.ancillary_page_id).__name__}"
            )
        if not 0 <= self.ancillary_page_id <= 0xFFFF:
            raise ValueError(
                f"ancillary_page_id must fit u16 (0..=0xFFFF); "
                f"got {self.ancillary_page_id}"
            )


@dataclass(frozen=True, slots=True)
class DvbTeletextConfig:
    """DVB teletext. Per ETSI EN 300 468 §6.2.43 + ETSI EN 300 706.

    Fields mirror `tst_core::mpegts::mux::SubtitleCodec::DvbTeletext`:

    - `language` — 3-byte ISO 639-2/B lowercase ASCII.
    - `teletext_type` — 5-bit teletext_type, 0..=31. Common values:
      0x01 (initial page), 0x02 (subtitle page), 0x05 (programme
      schedule).
    - `magazine_number` — 3-bit magazine number, 0..=7. Note: the
      conventional "magazine 8" wraps to 0 in the 3-bit field.
    - `page_number` — BCD-encoded page number, 0x00..=0x99. (Each
      nibble must be 0..=9.)
    """

    language: bytes
    teletext_type: int
    magazine_number: int
    page_number: int

    def __post_init__(self) -> None:
        # Reject bytearray: frozen+slots dataclass stores a reference, so a
        # mutable bytearray would weaken the immutability contract and break
        # hashing. bytes is the only accepted type.
        if not isinstance(self.language, bytes) or len(self.language) != 3:
            raise ValueError(
                f"language must be 3 bytes (ISO 639-2/B), "
                f"got {self.language!r}"
            )
        # `bool` is a subclass of `int` in Python — exclude it explicitly.
        if isinstance(self.teletext_type, bool) or not isinstance(
            self.teletext_type, int
        ):
            raise TypeError(
                f"teletext_type must be int; "
                f"got {type(self.teletext_type).__name__}"
            )
        if not 0 <= self.teletext_type <= 0x1F:
            raise ValueError(
                f"teletext_type must fit 5 bits (0..=31); got {self.teletext_type}"
            )
        if isinstance(self.magazine_number, bool) or not isinstance(
            self.magazine_number, int
        ):
            raise TypeError(
                f"magazine_number must be int; "
                f"got {type(self.magazine_number).__name__}"
            )
        if not 0 <= self.magazine_number <= 7:
            raise ValueError(
                f"magazine_number must fit 3 bits (0..=7); "
                f"got {self.magazine_number}"
            )
        if isinstance(self.page_number, bool) or not isinstance(
            self.page_number, int
        ):
            raise TypeError(
                f"page_number must be int; "
                f"got {type(self.page_number).__name__}"
            )
        if not 0 <= self.page_number <= 0x99:
            raise ValueError(
                f"page_number must be BCD-encoded (0x00..=0x99); "
                f"got {self.page_number:#04x}"
            )
        # BCD validity: each nibble must be 0..=9.
        if (self.page_number & 0x0F) > 9 or ((self.page_number >> 4) & 0x0F) > 9:
            raise ValueError(
                f"page_number nibbles must each be 0..=9 (BCD); "
                f"got {self.page_number:#04x}"
            )


@dataclass(frozen=True, slots=True)
class Cea708StandaloneConfig:
    """CEA-708 caption data carried as a separate elementary stream
    (rather than embedded in H.264 / H.265 SEI).

    Mirrors unit-variant `tst_core::mpegts::mux::SubtitleCodec::Cea708Standalone`
    — no fields. The muxer auto-emits a `registration_descriptor` with
    `format_identifier = "GA94"` (informal industry convention; see the
    Rust docstring for the spec caveat).

    **Library-internal round-trip only** — external-tool interop has
    not been empirically verified. See `docs/deferred-features.md`
    "CEA-708 interop" for status.
    """


@dataclass(frozen=True, slots=True)
class WebVttInTsConfig:
    """WebVTT cues carried inside MPEG-TS PES.

    Mirrors unit-variant `tst_core::mpegts::mux::SubtitleCodec::WebVttInTs`
    — no fields. The muxer auto-emits a `registration_descriptor` with
    `format_identifier = "VTTC"` (ffmpeg `mpegtsenc.c` convention,
    recognized by hls.js v1.7+ and mediamtx; not a normatively-defined
    codepoint).
    """


# Discriminated union accepted by `MuxerProgramConfigBuilder.add_subtitle`.
# PEP 604 union syntax — requires Python 3.10+ (the project floor).
SubtitleCodecConfig = (
    DvbSubtitlingConfig
    | DvbTeletextConfig
    | Cea708StandaloneConfig
    | WebVttInTsConfig
)


class StreamKindTag(enum.Enum):
    """Discriminator for `StreamKind`. The actual codec (when applicable)
    lives on the `codec` field of `StreamInfo` / `StreamId`."""
    VIDEO = "video"
    AUDIO = "audio"
    SUBTITLE = "subtitle"
    KLV_SYNC = "klv_sync"
    KLV_ASYNC = "klv_async"
    UNKNOWN = "unknown"


class MetadataKindTag(enum.Enum):
    """Discriminator for `DemuxEvent.Metadata.kind`. Mirrors Rust
    `MetadataKind` collapsed to its variant tag."""
    KLV_SYNC_AU_CELL = "klv_sync_au_cell"
    KLV_ASYNC = "klv_async"
    UNKNOWN = "unknown"


class DiscontinuityKindTag(enum.Enum):
    """Discriminator for `DemuxEvent.Discontinuity.kind`. Mirrors Rust
    `DiscontinuityKind` variants."""
    CONTINUITY_JUMP = "continuity_jump"
    PES_OVERSIZE = "pes_oversize"
    PES_TOTAL_OVERSIZE = "pes_total_oversize"
    ADAPTATION_FIELD_FLAG = "adaptation_field_flag"


class NonConformantKind(enum.Enum):
    """Discriminator for `DemuxEvent.NonConformant.kind`. Collapses
    Rust `NonConformantIssue`'s 30+ variants. The `issue` field on the
    event carries the human-readable detail string."""
    PCR_ANOMALY = "pcr_anomaly"
    PSI_CHECKSUM_MISMATCH = "psi_checksum_mismatch"
    MALFORMED_PES = "malformed_pes"
    PUSI_MID_PES = "pusi_mid_pes"
    TRANSPORT_ERROR_PACKET = "transport_error_packet"
    STREAM_TYPE_MISMATCH = "stream_type_mismatch"
    PID_REUSED_ACROSS_PROGRAMS = "pid_reused_across_programs"
    NAL_HEADER = "nal_header"
    AV1_OBU_HEADER = "av1_obu_header"
    AV1_REGISTRATION_MALFORMED = "av1_registration_malformed"
    PES_HEADER_MALFORMED = "pes_header_malformed"
    PTS_ANOMALY = "pts_anomaly"
    MISSING_REQUIRED_PTS = "missing_required_pts"
    PCR_MALFORMED = "pcr_malformed"
    SUBTITLE_MISSING_DESCRIPTOR = "subtitle_missing_descriptor"
    SUBTITLE_ALIGNMENT_MISSING = "subtitle_alignment_missing"
    MULTI_CELL_AU = "multi_cell_au"
    CFI_TOLERATED = "malformed_au_cell_cfi_tolerated"
    PSI_MULTI_SECTION_UNSUPPORTED = "psi_multi_section_unsupported"
    PSI_CC_DISCONTINUITY = "psi_cc_discontinuity"
    PSI_OVERLONG_SECTION = "psi_overlong_section"
    DVB_SUB_DATA_IDENTIFIER = "dvb_sub_data_identifier"
    AC3_SYNC_MISSING = "ac3_sync_missing"
    LATM_FRAMING = "latm_framing"
    AV1_WRONG_STREAM_ID = "av1_wrong_stream_id"
    AV1_MISSING_TS_OBU_FRAMING = "av1_missing_ts_obu_framing"
    AV1_OBU_MISSING_SIZE_FIELD = "av1_obu_missing_size_field"
    AV1_TILE_LIST_NOT_ALLOWED = "av1_tile_list_not_allowed"
    MISSING_METADATA_DESCRIPTOR = "missing_metadata_descriptor"
    SUBTITLE_DESCRIPTOR_AMBIGUOUS = "subtitle_descriptor_ambiguous"
    SUBTITLE_DESCRIPTOR_MALFORMED = "subtitle_descriptor_malformed"
    PMT_PROGRAM_NUMBER_MISMATCH = "pmt_program_number_mismatch"
    UNSUPPORTED_SCRAMBLING = "unsupported_scrambling"
    ADAPTATION_FIELD_MALFORMED = "adaptation_field_malformed"
    ZERO_LENGTH_PES_NON_VIDEO = "zero_length_pes_non_video"
    PSI_SYNTAX = "psi_syntax"
    OTHER = "other"


class StrictMode(enum.Enum):
    """Mirrors Rust `tst_core::mpegts::demux::StrictMode`. The strictness
    ladder the demuxer applies when it sees non-conformant streams:
    Off lets everything through as NonConformant events; Full converts
    them to DemuxError."""
    OFF = "off"
    TIMING_ONLY = "timing_only"
    PSI_ONLY = "psi_only"
    FULL = "full"


class LinkSource(enum.Enum):
    """Mirrors Rust `tst_core::mpegts::demux::event::LinkSource`. Tells
    where a KLV-to-video link came from."""
    DECLARED = "declared"
    INFERRED = "inferred"
    OVERRIDE = "override"


# Type alias for "any codec enum or None". Used on StreamId / StreamInfo
# where the kind determines which codec enum (if any) is meaningful.
# PEP 604 union syntax — requires Python 3.10+ (the project floor).
Codec = VideoCodec | AudioCodec | SubtitleCodec | None


@dataclass(frozen=True, slots=True)
class StreamId:
    """Identity of a single elementary stream in a TS. `kind` is the
    discriminator; `codec` carries the specific codec when applicable
    (None for KLV / Unknown). `program_number` resolves cross-program
    PID reuse (per the project's first-program-wins policy)."""

    pid: int
    kind: StreamKindTag
    codec: Codec
    program_number: int


@dataclass(frozen=True, slots=True)
class RawDescriptor:
    """One raw PMT per-stream descriptor TLV. `tag` is the
    descriptor_tag byte; `data` the payload bytes (length stripped)."""

    tag: int  # u8
    data: bytes


@dataclass(frozen=True, slots=True)
class StreamInfo:
    """PMT-derived per-stream metadata. `stream_type` is the raw PMT
    byte (kept for forward-compat with stream types not yet recognized
    by the demuxer). `raw_descriptors` holds the per-stream PMT descriptor
    loop verbatim, in loop order; empty tuple when the PMT carried none."""

    pid: int
    stream_type: int  # u8
    kind: StreamKindTag
    codec: Codec
    program_number: int
    raw_descriptors: tuple = ()


@dataclass(frozen=True, slots=True)
class KlvLink:
    """A declared, inferred, or overridden link between a KLV PID and
    the video PID it timestamps against. Lives on `ProgramMap`."""

    klv_pid: int
    video_pid: int
    source: LinkSource


@dataclass(frozen=True, slots=True)
class ProgramMap:
    """One program's PSI summary, emitted on PAT/PMT discovery and on
    each PSI version-bump. `streams` is the elementary-stream list;
    `klv_links` is the demuxer's view of which KLV PIDs belong to
    which video PIDs. `pmt_pid` is the PID carrying this program's PMT
    (from the PAT entry) — needed to reconstruct a `MuxerConfig` via
    `MuxerConfig.from_program_map`.

    `streams` and `klv_links` are tuples (not lists) so the dataclass
    remains hashable + value-equal."""

    program_number: int
    pcr_pid: int
    pmt_pid: int
    streams: tuple
    klv_links: tuple


class DemuxEvent:
    """Top-level event emitted by `Demuxer.next_event()`.

    `DemuxEvent` is a namespace base class — instantiate one of its
    subclasses (`DemuxEvent.Video(...)`, `DemuxEvent.Metadata(...)`, etc.)
    or let `Demuxer` construct them for you. Pattern-match with
    Python 3.10+:

    ```
    for ev in parse_file(path):
        match ev:
            case DemuxEvent.Video(stream=s, pts=p, raw=b):
                units = ev.parse()  # opt-in NAL/OBU split
            case DemuxEvent.Metadata(payload=b):
                ...
    ```

    Subclasses are defined immediately below as class-attribute
    dataclasses for clean `DemuxEvent.Video(...)` access syntax.
    """

    # ClassVar annotations so type-checkers (Pylance, mypy) see the
    # subclasses as proper types when users do
    # `DemuxEvent.Video(...)`. The actual assignments happen after
    # the subclass dataclass definitions below.
    ProgramMap: ClassVar[type["_ProgramMapEvent"]]
    Video: ClassVar[type["_VideoEvent"]]
    Audio: ClassVar[type["_AudioEvent"]]
    Subtitle: ClassVar[type["_SubtitleEvent"]]
    Metadata: ClassVar[type["_MetadataEvent"]]
    Klv: ClassVar[type["_MetadataEvent"]]  # deprecated alias for Metadata, remove at 1.0
    UnknownSample: ClassVar[type["_UnknownSampleEvent"]]
    Discontinuity: ClassVar[type["_DiscontinuityEvent"]]
    NonConformant: ClassVar[type["_NonConformantEvent"]]
    ReconnectDiscontinuity: ClassVar[type["_ReconnectDiscontinuityEvent"]]


# Subclasses use the `DemuxEvent.X` attribute pattern. Most are
# frozen dataclasses so they're hashable, equality-by-value, and read
# nicely with pattern matching. (`_VideoEvent` / `_AudioEvent` are the
# exception — hand-written frozen classes for lazy `.raw`; see below.)

@dataclass(frozen=True, slots=True)
class _ProgramMapEvent(DemuxEvent):
    programs: tuple[ProgramMap, ...]


# `_VideoEvent` / `_AudioEvent` are hand-written frozen classes rather than
# `@dataclass(frozen=True, slots=True)` because their `.raw` is materialized
# LAZILY (WP-E PY-01): the demuxer hands a native `_native.RawBytes` holder (a
# cheap Arc clone — no payload copy) and the Python `bytes` is only produced on
# first `.raw` access, then cached. A value cannot be both a dataclass field
# (`raw: bytes`, eagerly stored) AND a computed `@property` of the same name, so
# auto-`__init__` cannot be kept here. The class therefore reproduces the frozen
# dataclass's value semantics (slots, frozen `__setattr__`, value-eq/hash/repr,
# `__match_args__`) by hand, storing the holder in `_raw` and exposing `.raw`
# via a property.
#
# Retention tradeoff: holding the event keeps the underlying demux buffer (the
# Arc inside `RawBytes`) alive until the event is dropped, even if `.raw` is
# never materialized. The win is pay-per-access (lazy) materialization, not
# zero-copy at the boundary — abi3 copies the bytes into a fresh `bytes`
# regardless (see docs/specs/2026-06-08-raw-first-sample-model-design.md §4.1).

class _VideoEvent(DemuxEvent):
    __slots__ = (
        "stream",
        "pts",
        "dts",
        "codec",
        "_raw",  # native _native.RawBytes holder; `.raw` materializes lazily
        "random_access_indicator",
        "av1_carriage",
    )

    # Positional match binds `raw` (the property), not `_raw` (the holder),
    # so `case DemuxEvent.Video(s, p, d, c, raw, rai, av1)` sees `bytes`.
    __match_args__ = (
        "stream",
        "pts",
        "dts",
        "codec",
        "raw",
        "random_access_indicator",
        "av1_carriage",
    )

    def __init__(
        self,
        *,
        stream: StreamId,
        pts: Pts90khz,
        dts: Optional[Pts90khz],
        codec: VideoCodec,
        raw,
        random_access_indicator: bool,
        # AV1 carriage provenance from the demuxer. `Some(mode)` for AV1
        # samples; `None` for H.264/H.265/H.266 (carriage is an AV1-only
        # concept). Pass this to `split_units(carriage=...)` and to the
        # destination muxer's `push_video_wire_to` to ensure a faithful
        # round-trip. Default `None` for backward compatibility with call
        # sites that construct the event directly.
        av1_carriage: Optional["Av1CarriageMode"] = None,
    ) -> None:
        object.__setattr__(self, "stream", stream)
        object.__setattr__(self, "pts", pts)
        object.__setattr__(self, "dts", dts)
        object.__setattr__(self, "codec", codec)
        # Accept either a pre-built holder (the demuxer path) or raw bytes
        # (direct construction with `raw=b"..."`).
        object.__setattr__(
            self,
            "_raw",
            raw if isinstance(raw, _native.RawBytes) else _native.RawBytes(raw),
        )
        object.__setattr__(
            self, "random_access_indicator", random_access_indicator
        )
        object.__setattr__(self, "av1_carriage", av1_carriage)

    @property
    def raw(self) -> bytes:
        """The exact encoded access unit (Annex-B for H.26x; on-wire PES
        payload for AV1). Materialized on first access and cached."""
        return self._raw.value

    def __setattr__(self, name, value):
        raise AttributeError(f"cannot assign to field {name!r}")

    def __delattr__(self, name):
        raise AttributeError(f"cannot delete field {name!r}")

    def __eq__(self, other) -> bool:
        if other.__class__ is not self.__class__:
            return NotImplemented
        return (
            self.stream == other.stream
            and self.pts == other.pts
            and self.dts == other.dts
            and self.codec == other.codec
            and self._raw == other._raw  # content equality
            and self.random_access_indicator == other.random_access_indicator
            and self.av1_carriage == other.av1_carriage
        )

    def __hash__(self) -> int:
        return hash(
            (
                self.stream,
                self.pts,
                self.dts,
                self.codec,
                hash(self._raw),  # content hash
                self.random_access_indicator,
                self.av1_carriage,
            )
        )

    def __repr__(self) -> str:
        return (
            f"DemuxEvent.Video(stream={self.stream!r}, pts={self.pts!r}, "
            f"dts={self.dts!r}, codec={self.codec!r}, raw={self.raw!r}, "
            f"random_access_indicator={self.random_access_indicator!r}, "
            f"av1_carriage={self.av1_carriage!r})"
        )

    def parse(self, *, strict: bool = False):
        """Opt-in: split `raw` into typed NAL/OBU units. Lenient drops the issue
        list (use `tstrans.codec.split_units` if you want the issues).

        For AV1, the carriage provenance (`self.av1_carriage`) is forwarded to
        `split_units` automatically so the framing expectation matches the
        on-wire bytes demuxed from the source stream."""
        from tstrans import codec as _codec
        if strict:
            return _codec.split_units(self.raw, self.codec, strict=True,
                                      carriage=self.av1_carriage)
        units, _issues = _codec.split_units(self.raw, self.codec,
                                            carriage=self.av1_carriage)
        return units


class _AudioEvent(DemuxEvent):
    # Hand-written frozen class (see `_VideoEvent` above) so `.raw` can be a
    # lazily-materialized property rather than an eager dataclass field.
    __slots__ = ("stream", "pts", "dts", "codec", "_raw")

    __match_args__ = ("stream", "pts", "dts", "codec", "raw")

    def __init__(
        self,
        *,
        stream: StreamId,
        pts: Pts90khz,
        dts: Optional[Pts90khz],
        codec: AudioCodec,
        raw,
    ) -> None:
        object.__setattr__(self, "stream", stream)
        object.__setattr__(self, "pts", pts)
        object.__setattr__(self, "dts", dts)
        object.__setattr__(self, "codec", codec)
        object.__setattr__(
            self,
            "_raw",
            raw if isinstance(raw, _native.RawBytes) else _native.RawBytes(raw),
        )

    @property
    def raw(self) -> bytes:
        """Raw audio elementary-stream bytes. Materialized on first access and
        cached."""
        return self._raw.value

    def __setattr__(self, name, value):
        raise AttributeError(f"cannot assign to field {name!r}")

    def __delattr__(self, name):
        raise AttributeError(f"cannot delete field {name!r}")

    def __eq__(self, other) -> bool:
        if other.__class__ is not self.__class__:
            return NotImplemented
        return (
            self.stream == other.stream
            and self.pts == other.pts
            and self.dts == other.dts
            and self.codec == other.codec
            and self._raw == other._raw  # content equality
        )

    def __hash__(self) -> int:
        return hash(
            (self.stream, self.pts, self.dts, self.codec, hash(self._raw))
        )

    def __repr__(self) -> str:
        return (
            f"DemuxEvent.Audio(stream={self.stream!r}, pts={self.pts!r}, "
            f"dts={self.dts!r}, codec={self.codec!r}, raw={self.raw!r})"
        )

    def parse(self, *, strict: bool = False):
        """Opt-in: parse `raw` into typed audio frames (empty list for codecs with
        no typed parser — parse `raw` directly)."""
        from tstrans import codec as _codec
        return _codec.parse_audio(self.raw, self.codec, strict=strict)


class _SubtitleEvent(DemuxEvent):
    # Hand-written frozen class (see `_VideoEvent` above) so `.payload` can be a
    # lazily-materialized property rather than an eager dataclass field.
    # `SamplePayload::Subtitle.payload` is SharedBytes in tst-core, so the Rust
    # side passes a `RawBytes` holder (cheap Arc clone; no payload copy).
    __slots__ = ("stream", "pts", "dts", "codec", "_payload")

    __match_args__ = ("stream", "pts", "dts", "codec", "payload")

    def __init__(
        self,
        *,
        stream: StreamId,
        pts: Pts90khz,
        dts: Optional[Pts90khz],
        codec: SubtitleCodec,
        payload,
    ) -> None:
        object.__setattr__(self, "stream", stream)
        object.__setattr__(self, "pts", pts)
        object.__setattr__(self, "dts", dts)
        object.__setattr__(self, "codec", codec)
        # Accept either a pre-built holder (the demuxer path) or raw bytes
        # (direct construction with `payload=b"..."`).
        object.__setattr__(
            self,
            "_payload",
            payload if isinstance(payload, _native.RawBytes) else _native.RawBytes(payload),
        )

    @property
    def payload(self) -> bytes:
        """Raw subtitle PES payload bytes. Materialized on first access and cached."""
        return self._payload.value

    def __setattr__(self, name, value):
        raise AttributeError(f"cannot assign to field {name!r}")

    def __delattr__(self, name):
        raise AttributeError(f"cannot delete field {name!r}")

    def __eq__(self, other) -> bool:
        if other.__class__ is not self.__class__:
            return NotImplemented
        return (
            self.stream == other.stream
            and self.pts == other.pts
            and self.dts == other.dts
            and self.codec == other.codec
            and self._payload == other._payload  # content equality
        )

    def __hash__(self) -> int:
        return hash((self.stream, self.pts, self.dts, self.codec, hash(self._payload)))

    def __repr__(self) -> str:
        return (
            f"DemuxEvent.Subtitle(stream={self.stream!r}, pts={self.pts!r}, "
            f"dts={self.dts!r}, codec={self.codec!r}, payload={self.payload!r})"
        )


@dataclass(frozen=True, slots=True)
class _MetadataEvent(DemuxEvent):
    stream: StreamId
    pts: Pts90khz
    kind: MetadataKindTag
    payload: bytes
    # Multi-cell AU reassembly surface (H.222.0 §2.12.4.2). `False` + `1`
    # on single-cell (Complete) AUs and on non-KlvSyncAuCell metadata
    # events. Defaults preserve backward compatibility for any callers
    # that construct `_MetadataEvent` directly (the Rust converter always
    # populates these explicitly).
    was_reassembled: bool = False
    cell_count: int = 1

    def parse(self, *, strict: bool = False):
        """Opt-in: decode `payload` into a typed KLV set, dispatched by
        universal label via `tstrans.klv.parse_klv_universal`. Returns
        `UasDatalinkLs | SecurityLs | PrecisionTimeStampPack | VmtiLs`,
        or `None` when the UL matches no known set. Raises
        `tstrans.exceptions.KlvError` on a malformed record. Mirrors
        the raw-first `Video.parse()` / `Audio.parse()` naming."""
        from tstrans import klv as _klv

        return _klv.parse_klv_universal(self.payload, strict=strict)


class _UnknownSampleEvent(DemuxEvent):
    """Sample on a PID whose stream_type the demuxer does not classify
    as Video / Audio / Subtitle / KLV. The raw stream_type byte (per
    PMT) and the unparsed PES payload are preserved verbatim so callers
    can archive, forward, or post-process.

    Audit-2 finding #1 — prior versions collapsed unknown samples into
    a NonConformant diagnostic and discarded the payload bytes.

    Hand-written frozen class (see `_VideoEvent`) so `.payload` can be a
    lazily-materialized property; `SamplePayload::Unknown.raw` is
    SharedBytes in tst-core so the demuxer path avoids an eager copy.
    """

    __slots__ = ("stream", "pts", "dts", "stream_type", "_payload")

    __match_args__ = ("stream", "pts", "dts", "stream_type", "payload")

    def __init__(
        self,
        *,
        stream: StreamId,
        pts: Pts90khz,
        dts: Optional[Pts90khz],
        stream_type: int,
        payload,
    ) -> None:
        object.__setattr__(self, "stream", stream)
        object.__setattr__(self, "pts", pts)
        object.__setattr__(self, "dts", dts)
        object.__setattr__(self, "stream_type", stream_type)
        # Accept either a pre-built holder (the demuxer path) or raw bytes
        # (direct construction with `payload=b"..."`).
        object.__setattr__(
            self,
            "_payload",
            payload if isinstance(payload, _native.RawBytes) else _native.RawBytes(payload),
        )

    @property
    def payload(self) -> bytes:
        """Raw PES payload bytes for the unknown stream type. Materialized on first access and cached."""
        return self._payload.value

    def __setattr__(self, name, value):
        raise AttributeError(f"cannot assign to field {name!r}")

    def __delattr__(self, name):
        raise AttributeError(f"cannot delete field {name!r}")

    def __eq__(self, other) -> bool:
        if other.__class__ is not self.__class__:
            return NotImplemented
        return (
            self.stream == other.stream
            and self.pts == other.pts
            and self.dts == other.dts
            and self.stream_type == other.stream_type
            and self._payload == other._payload  # content equality
        )

    def __hash__(self) -> int:
        return hash(
            (self.stream, self.pts, self.dts, self.stream_type, hash(self._payload))
        )

    def __repr__(self) -> str:
        return (
            f"DemuxEvent.UnknownSample(stream={self.stream!r}, pts={self.pts!r}, "
            f"dts={self.dts!r}, stream_type={self.stream_type!r}, payload={self.payload!r})"
        )


@dataclass(frozen=True, slots=True)
class _DiscontinuityEvent(DemuxEvent):
    stream: StreamId
    kind: DiscontinuityKindTag


@dataclass(frozen=True, slots=True)
class _NonConformantEvent(DemuxEvent):
    stream: StreamId
    issue: str
    kind: NonConformantKind
    # Typed reason set only when `kind == NonConformantKind.MULTI_CELL_AU`;
    # `None` for all other issues. Default keeps existing constructors
    # working without changes.
    multi_cell_au_reason: Optional["MultiCellAuReason"] = None
    # Typed CFI bits set only when
    # `kind == NonConformantKind.CFI_TOLERATED`; `None`
    # for all other issues. `observed_cfi` is the wire value the demuxer
    # read; `treated_as` is the value substituted (always
    # `CellFragmentIndication.COMPLETE` today). Both default to None so
    # existing constructors keep working.
    observed_cfi: Optional["CellFragmentIndication"] = None
    treated_as: Optional["CellFragmentIndication"] = None


@dataclass(frozen=True, slots=True)
class _ReconnectDiscontinuityEvent(DemuxEvent):
    pass


# Expose subclasses as attributes of the base. This gives the
# `DemuxEvent.Video(...)` access syntax in the spec while keeping
# the subclasses defined as top-level dataclasses (so dataclass
# decorators work properly).
DemuxEvent.ProgramMap = _ProgramMapEvent
DemuxEvent.Video = _VideoEvent
DemuxEvent.Audio = _AudioEvent
DemuxEvent.Subtitle = _SubtitleEvent
DemuxEvent.Metadata = _MetadataEvent
# DemuxEvent.Klv is a deprecated alias for DemuxEvent.Metadata — it is the
# SAME class object, so `isinstance(e, DemuxEvent.Klv)` and
# `case DemuxEvent.Klv(...)` keep working unchanged. Remove at 1.0.
DemuxEvent.Klv = _MetadataEvent  # deprecated alias, remove at 1.0
_KlvEvent = _MetadataEvent  # deprecated alias, remove at 1.0
DemuxEvent.UnknownSample = _UnknownSampleEvent
DemuxEvent.Discontinuity = _DiscontinuityEvent
DemuxEvent.NonConformant = _NonConformantEvent
DemuxEvent.ReconnectDiscontinuity = _ReconnectDiscontinuityEvent


# Default caps mirror `tst_core::mpegts::demux::DemuxerConfig`.
# Update both sides together if Rust changes its defaults.
_DEFAULT_PES_CAP_PER_PID: int = 4 * 1024 * 1024
_DEFAULT_PES_CAP_TOTAL: int = 64 * 1024 * 1024
_DEFAULT_SYNC_BUF_CAP: int = 4 * 1024 * 1024


@dataclass(frozen=True, slots=True)
class DemuxerConfig:
    """Demuxer configuration. Mirrors Rust's
    `tst_core::mpegts::demux::DemuxerConfig` for the knobs currently
    exposed to Python:

    - `strict_mode` — Off / TimingOnly / PsiOnly / Full ladder.
    - `pes_cap_per_pid`, `pes_cap_total` — reassembly memory caps.
    - `cfi_tolerance` — lenient AU-cell CFI substitution; **default
      `True`** since the 2026-05-24 default flip (industry-wide
      producer bug — see field docstring on `cfi_tolerance` below for
      the rationale, and `MultiCellAuReason` / `CellFragmentIndication`
      for the underlying enums).
    - `av1_carriage` — AV1 PES carriage mode the demuxer expects;
      `None` (the default) defers to the Rust default
      (`Av1CarriageMode.MPEG2_TS_BINDING`). Set to
      `Av1CarriageMode.INTEROP_RAW_OBU` when receiving from
      ffmpeg / libaom / hls.js / mediamtx senders. A mismatched value
      against the wire carriage surfaces as
      `NonConformantKind.AV1_WRONG_STREAM_ID` plus
      `NonConformantKind.AV1_MISSING_TS_OBU_FRAMING`; the Sample
      still arrives via the lenient raw-OBU fallback.
    - `au_cell_cap_per_pid` — per-PID cap on the in-flight
      sync-metadata AU cell reassembly buffer in bytes; `None` defers
      to the Rust default of 1 MiB. Breach surfaces as
      `NonConformantKind.MULTI_CELL_AU` with
      `multi_cell_au_reason = MultiCellAuReason.OVERFLOW`.
    - `lenient_psi_reassembly` — when False (default), PSI section
      reassembly drops the partial section on continuity-counter
      jumps and emits `NonConformantKind.PSI_CC_DISCONTINUITY`
      (matches ffmpeg `mpegts.c:3118-3142`). When True, continuation
      packets are accepted across jumps (today's permissive
      behavior — the section either passes by luck or fails its CRC).
    - `sync_buf_cap` — ceiling in bytes on the pre-sync ingress buffer.
      A single `feed()` call larger than this ceiling raises
      `DemuxError` (kind `SYNC_LOSS`) before any bytes are consumed;
      feed in smaller chunks and drain events between feeds, or raise
      this ceiling. Default is 4 MiB (`4 * 1024 * 1024`).
    - `unwrap_timestamps` — opt-in PTS/DTS unwrap onto a continuous
      per-PID timeline that carries across the 33-bit rollover;
      **default `False`**. See the field docstring below for the
      accumulator model.

    `link_klv` and `treat_as` overrides (per-PID PMT-bypass knobs)
    remain Rust-only today; open an issue if your use case needs them.
    """

    strict_mode: StrictMode = StrictMode.OFF
    pes_cap_per_pid: int = _DEFAULT_PES_CAP_PER_PID
    pes_cap_total: int = _DEFAULT_PES_CAP_TOTAL
    # When True (default), the demuxer treats orphan Middle/Last AU
    # cells as Complete if the inner payload independently validates as
    # one complete KLV record (SMPTE 336M UL + BER length match). Always
    # emits a `_NonConformantEvent` with kind `CFI_TOLERATED` alongside
    # the rescued metadata event, so the producer-side malformation
    # stays visible to validators.
    #
    # Default is True because corpus-wide validation showed the
    # producer-side CFI=00-on-single-cell-AU bug is dominant in
    # real-world STANAG 4609 traffic (~99% of NonConformant events),
    # and no other public reference decoder enforces CFI either.
    # Set to False for spec-strict conformance testing: orphan cells
    # then surface only as `MULTI_CELL_AU{ORPHAN}` with no metadata
    # event.
    cfi_tolerance: bool = True
    # `None` defers to the Rust default
    # (`Av1CarriageMode::Mpeg2TsBinding`). Storing the Python
    # `Av1CarriageMode` value (not its `.value` string) lets the
    # `build_demuxer()` plumbing call `value` for the
    # already-existing Rust mapping.
    av1_carriage: Optional[Av1CarriageMode] = None
    # `None` defers to the Rust default of 1 MiB. Any positive int
    # caps the in-flight AU cell reassembly buffer at that many bytes.
    au_cell_cap_per_pid: Optional[int] = None
    # See class docstring above for the spec-strict vs lenient PSI
    # reassembly trade-off.
    lenient_psi_reassembly: bool = False
    # Pre-sync ingress buffer ceiling in bytes. Feeds larger than this
    # ceiling raise DemuxError (kind SYNC_LOSS) before any bytes are
    # consumed; feed in smaller chunks, or raise this value.
    sync_buf_cap: int = _DEFAULT_SYNC_BUF_CAP
    # Unwrap the demuxer's raw 33-bit 90 kHz PTS/DTS onto a continuous
    # timeline that carries across the rollover, per PID. Default False:
    # off, PTS/DTS pass through as the raw wire value in 0..2^33.
    # When True, a per-PID accumulator carries each sample's signed
    # wrap-aware delta from the previous raw value onto the previous
    # unwrapped value, so crossing the rollover grows the offset by a
    # full `1 << 33` while a genuine backward step — an out-of-order
    # arrival, including a pre-wrap value delivered after the wrap —
    # steps back by its true distance instead of being mistaken for
    # another wrap. PIDs of the same program stay directly comparable
    # on the one continuous timeline (independent programs never
    # cross-anchor). The accumulator resets alongside all other per-PID
    # parse state on a resync (e.g. a reconnect), restarting the
    # unwrap timeline. Mirrors Rust's
    # `tst_core::mpegts::demux::DemuxerConfig::unwrap_timestamps`.
    unwrap_timestamps: bool = False

    def __post_init__(self) -> None:
        # F10 — fail-fast on primitive-shape violations at construction.
        # Without this, invalid values fail deep inside `build_demuxer`
        # in Rust (e.g. negative `pes_cap_per_pid` raises an opaque
        # `OverflowError` from `usize` extraction) instead of pointing
        # at the user's construction site. Mirrors the pattern used by
        # `Pts90khz` (mpegts.py:39-47) and the KLV dataclasses.
        if not isinstance(self.strict_mode, StrictMode):
            raise TypeError(
                f"strict_mode must be a StrictMode enum value; "
                f"got {type(self.strict_mode).__name__}={self.strict_mode!r}"
            )
        # `bool` is a subclass of `int` in Python — exclude it explicitly
        # so `DemuxerConfig(pes_cap_per_pid=True)` doesn't silently pass.
        if isinstance(self.pes_cap_per_pid, bool) or not isinstance(
            self.pes_cap_per_pid, int
        ):
            raise TypeError(
                f"pes_cap_per_pid must be int; "
                f"got {type(self.pes_cap_per_pid).__name__}"
            )
        if self.pes_cap_per_pid <= 0:
            # Rust accepts 0 but a 0-byte cap means no PES can ever
            # reassemble — effectively unusable. Reject loudly.
            raise ValueError(
                f"pes_cap_per_pid must be > 0; got {self.pes_cap_per_pid}"
            )
        if isinstance(self.pes_cap_total, bool) or not isinstance(
            self.pes_cap_total, int
        ):
            raise TypeError(
                f"pes_cap_total must be int; "
                f"got {type(self.pes_cap_total).__name__}"
            )
        if self.pes_cap_total <= 0:
            raise ValueError(
                f"pes_cap_total must be > 0; got {self.pes_cap_total}"
            )
        if not isinstance(self.cfi_tolerance, bool):
            raise TypeError(
                f"cfi_tolerance must be bool; "
                f"got {type(self.cfi_tolerance).__name__}"
            )
        if self.av1_carriage is not None and not isinstance(
            self.av1_carriage, Av1CarriageMode
        ):
            raise TypeError(
                f"av1_carriage must be None or Av1CarriageMode; "
                f"got {type(self.av1_carriage).__name__}"
            )
        if self.au_cell_cap_per_pid is not None:
            if isinstance(self.au_cell_cap_per_pid, bool) or not isinstance(
                self.au_cell_cap_per_pid, int
            ):
                raise TypeError(
                    f"au_cell_cap_per_pid must be None or int; "
                    f"got {type(self.au_cell_cap_per_pid).__name__}"
                )
            if self.au_cell_cap_per_pid <= 0:
                raise ValueError(
                    f"au_cell_cap_per_pid must be > 0 if set; "
                    f"got {self.au_cell_cap_per_pid}"
                )
        if not isinstance(self.lenient_psi_reassembly, bool):
            raise TypeError(
                f"lenient_psi_reassembly must be bool; "
                f"got {type(self.lenient_psi_reassembly).__name__}"
            )
        if isinstance(self.sync_buf_cap, bool) or not isinstance(self.sync_buf_cap, int):
            raise TypeError(
                f"sync_buf_cap must be int; got {type(self.sync_buf_cap).__name__}"
            )
        if self.sync_buf_cap <= 0:
            raise ValueError(f"sync_buf_cap must be > 0; got {self.sync_buf_cap}")
        if not isinstance(self.unwrap_timestamps, bool):
            raise TypeError(
                f"unwrap_timestamps must be bool; "
                f"got {type(self.unwrap_timestamps).__name__}"
            )


# Re-export NalUnit / Obu / ObuExtension so callers can import them
# from `tstrans.mpegts` without also importing from `tstrans.codec`.
# These are the types returned by `DemuxEvent.Video.parse()` (and by
# `tstrans.codec.split_units`).
from tstrans.codec import NalUnit, Obu, ObuExtension

# Re-export the Rust-side PyDemuxer class. The Rust impl lives in
# bindings/python/src/mpegts.rs and is exposed via `_native.Demuxer`.
from tstrans import _native as _native_mod

Demuxer = _native_mod.Demuxer

# `MultiCellAuReason` — PyO3 `eq_int` enum mirroring
# `tst_core::mpegts::demux::event::MultiCellAuReason`. Re-exported here so
# Python users can `from tstrans.mpegts import MultiCellAuReason`. Set on
# `_NonConformantEvent.multi_cell_au_reason` when the issue is
# `MULTI_CELL_AU`; `None` otherwise.
MultiCellAuReason = _native_mod.MultiCellAuReason

# `CellFragmentIndication` — PyO3 `eq_int` enum mirroring
# `tst_core::mpegts::au_cell::CellFragmentIndication`. Re-exported here so
# Python users can `from tstrans.mpegts import CellFragmentIndication`.
# Set on `_NonConformantEvent.observed_cfi` and `_NonConformantEvent.treated_as`
# when the issue is `CFI_TOLERATED`; `None` otherwise.
# Discriminant values match the wire bits exactly: MIDDLE=0, LAST=1,
# FIRST=2, COMPLETE=3 (per H.222.0 V9 Table 2-157).
CellFragmentIndication = _native_mod.CellFragmentIndication

# Stream handle newtypes. Rust impls live in
# bindings/python/src/mux.rs as `Py{Video,Audio,Klv,Subtitle,Data}StreamHandle`,
# exposed on `_native` under the names below via `#[pyclass(name=...)]`.
VideoStreamHandle = _native_mod.VideoStreamHandle
AudioStreamHandle = _native_mod.AudioStreamHandle
KlvStreamHandle = _native_mod.KlvStreamHandle
SubtitleStreamHandle = _native_mod.SubtitleStreamHandle
DataStreamHandle = _native_mod.DataStreamHandle

# Program-level config + builder. Rust impls in
# bindings/python/src/mux.rs as `PyMuxerProgramConfig` /
# `PyMuxerProgramConfigBuilder`, exposed on `_native` under the
# names below via `#[pyclass(name=...)]`.
MuxerProgramConfig = _native_mod.MuxerProgramConfig
MuxerProgramConfigBuilder = _native_mod.MuxerProgramConfigBuilder

# Top-level muxer config + builder. Rust impls in
# bindings/python/src/mux.rs as `PyMuxerConfig` / `PyMuxerConfigBuilder`,
# exposed on `_native` under the names below via `#[pyclass(name=...)]`.
# `MuxerConfigBuilder.build()` runs Rust-side validation and raises
# `tstrans.exceptions.MuxError` on failure.
MuxerConfig = _native_mod.MuxerConfig
MuxerConfigBuilder = _native_mod.MuxerConfigBuilder

# Muxer base (init + pull + pending + capacity). Rust impl in
# bindings/python/src/mux.rs as `PyMuxer`, exposed on `_native` under
# the name below via `#[pyclass(name=...)]`. Constructor takes a
# `MuxerConfig` and re-runs Rust-side validation, surfacing failures
# as `tstrans.exceptions.MuxError`.
Muxer = _native_mod.Muxer

# MuxerStats snapshot type. Rust impl in bindings/python/src/mux.rs as
# `PyMuxerStats`, exposed on `_native` under the name below via
# `#[pyclass(name=...)]`. Frozen — returned by `Muxer.stats()`. The
# `per_stream` BTreeMap on the Rust side is not surfaced in v1 (the
# per-PID `StreamStats` shape exists but isn't yet wrapped); the
# scalar counters cover the common dashboard case.
MuxerStats = _native_mod.MuxerStats


# StreamCodecStats tagged union. Pure-Python dataclasses (no PyO3
# wrap) because the Rust enum has 3+ struct variants and PyO3 lacks
# ergonomic enum support — the `Muxer.stream_codec_stats(pid)`
# accessor constructs the right subclass on each call. `Some(Unknown)`
# from Rust (configured-but-no-data PID) is rendered as `None` from
# Python in v1; callers distinguish "configured and pushed" (typed
# subclass) from "either unconfigured or never pushed" (`None`).

@dataclass(frozen=True, slots=True)
class StreamCodecStats:
    """Abstract base for per-stream codec counter snapshots.

    Returned by `Muxer.stream_codec_stats(pid)`. Concrete subclasses
    are `VideoStreamCodecStats`, `KlvStreamCodecStats`, and
    `AudioStreamCodecStats` — match on the subclass to read the
    typed counters. Mirrors Rust
    `tst_core::mpegts::stats::StreamCodecStats`.
    """


@dataclass(frozen=True, slots=True)
class VideoStreamCodecStats(StreamCodecStats):
    """H.264 / H.265 / H.266 (NALs) or AV1 (OBUs) counters."""

    nals_or_obus: int
    random_access_aus: int


@dataclass(frozen=True, slots=True)
class KlvStreamCodecStats(StreamCodecStats):
    """KLV metadata counters — one bump per `push_klv` call."""

    records: int


@dataclass(frozen=True, slots=True)
class AudioStreamCodecStats(StreamCodecStats):
    """Audio frame counters — populated for MP2 + AAC-ADTS only; LATM
    and AC-3 PIDs return `None` (no frame iterator in v1)."""

    frames: int

# MuxerFileSink + MuxerDrainProxy + `Muxer.write_file`.
# Pure-Python sink: opens a file in `wb` mode and drains pending TS
# packets after every `push_*` call made on the proxy object the `with`
# statement yields. The proxy uses `__getattr__` to forward every other
# attribute to the wrapped Muxer untouched, so callers see a
# near-transparent Muxer surface plus the implicit drain-to-disk
# behavior.
#
# THE drain contract (v0.2.0 #6): only pushes routed through the proxy
# drain. The original Muxer object is not modified — pushing on it while
# a sink is active bypasses the drain entirely and overflows once
# `buffer_packets` (default 10_000) accumulate. The proxy cannot
# intercept those calls; they go straight to the native muxer.

# Drain chunk size: 7 packets × 188 = 1316 bytes — matches the common SRT
# payload (and UDP-like MPEG-TS bundle) size of 7×188, so callers that
# tee the file output to a transport without re-chunking get
# packet-aligned writes for free. Small enough to keep memory footprint
# low; large enough to amortize the pull() call cost.
_DRAIN_CHUNK_PACKETS = 7
_DRAIN_CHUNK_BYTES = _DRAIN_CHUNK_PACKETS * 188


def _drain_muxer_to_file(muxer: "Muxer", fh) -> None:
    """Pull pending packets in `_DRAIN_CHUNK_PACKETS`-packet chunks and
    write to `fh`.

    Stops when pull() returns 0 or pending drops to 0. Used by both
    `MuxerDrainProxy` (after each push) and `MuxerFileSink.__exit__`
    (final drain on close).
    """

    buf = bytearray(_DRAIN_CHUNK_BYTES)
    # F11 — hoist memoryview outside the loop. `view[:n]` is a zero-copy
    # slice; `bytes(buf[:n])` (the prior form) allocated + copied n bytes
    # per chunk. Real file handles (`io.BufferedWriter`) accept any
    # buffer-protocol object, including memoryview slices, so this
    # works transparently.
    view = memoryview(buf)
    while muxer.pending_packets() > 0:
        n = muxer.pull(buf)
        if n == 0:
            break
        fh.write(view[:n])


class MuxerDrainProxy:
    """Returned by `MuxerFileSink.__enter__`. Delegates every attribute
    access to the wrapped `Muxer`; intercepts the `push_*` methods to
    drain pending packets to the sink's file after each push call.

    Non-push methods (e.g. `pending_packets()`, `video_handles()`,
    `stats()`) pass through unchanged via `__getattr__`. This keeps the
    proxy near-transparent — code inside the `with` block can treat
    `proxy` as a Muxer for read-only inspection while the implicit
    drain happens behind the scenes on every push.

    The wrapping is one-way: the original Muxer is untouched, so a
    `push_*` call made directly on it (instead of on this proxy) does
    NOT drain and will overflow the packet buffer in a long push loop.
    Always push on the proxy while a sink is active.
    """

    __slots__ = ("_muxer", "_fh")

    # The set of methods to wrap with a post-push drain. Kept in sync
    # with `PyMuxer`'s push_* surface — structurally locked by
    # tests/test_mpegts_muxer_sink.py::
    # test_drain_proxy_push_methods_match_muxer_surface, which fails
    # if a `push_*` method exists on `Muxer` but is missing here (the
    # silent-BufferFull gap class) or vice versa.
    _PUSH_METHODS = frozenset({
        "push_video", "push_video_to", "push_video_to_with_dts",
        "push_video_wire_to", "push_video_misp_to",
        "push_audio", "push_audio_to",
        "push_klv", "push_klv_to",
        "push_subtitle", "push_subtitle_to",
        "push_data", "push_data_to",
    })

    def __init__(self, muxer, fh) -> None:
        # `object.__setattr__` because we use `__slots__` and want to
        # bypass any future `__setattr__` override (defensive).
        object.__setattr__(self, "_muxer", muxer)
        object.__setattr__(self, "_fh", fh)

    def __getattr__(self, name: str) -> Any:
        # `__getattr__` only fires for attrs NOT found via the normal
        # lookup chain — so `_muxer` and `_fh` (slot descriptors) hit
        # the fast path and never recurse here.
        attr = getattr(self._muxer, name)
        if name in MuxerDrainProxy._PUSH_METHODS:
            fh = self._fh
            muxer = self._muxer

            def wrapper(*args, **kwargs):
                result = attr(*args, **kwargs)
                _drain_muxer_to_file(muxer, fh)
                return result

            return wrapper
        return attr


class MuxerFileSink:
    """Context manager owning a file handle + a draining proxy for an
    external `Muxer`. Always flushes + closes on `__exit__`; user
    exceptions inside the `with` body are re-raised unchanged (the
    sink does NOT suppress them, but it DOES still drain whatever's
    pending and close the file so partial output is preserved).

    `__enter__` returns a `MuxerDrainProxy`, and the per-push drain
    applies ONLY to pushes made on that proxy. Pushing on the original
    Muxer while the sink is active bypasses the drain and overflows
    `buffer_packets` in a long push loop — bind the `with` target and
    push on it.

    **Non-atomic exit behavior (default, `atomic=False`):** on
    exception inside the `with` block, the destination file may exist
    as a valid TS prefix (whatever was drained before the exception).
    The caller is responsible for unlinking it if partial output is
    unwanted. For atomic semantics — file appears at destination only
    on successful exit — use `Muxer.write_file(path, atomic=True)`.

    **Atomic mode (`atomic=True`):** on `__enter__`, opens a
    `*.partial` temp file in the same directory as `path` (so the
    final rename stays on the same filesystem). On successful exit,
    drains + closes + `os.replace(tmp, path)` (atomic on both POSIX
    and Windows). On exception, drains + closes + unlinks the temp
    file so nothing appears at the destination. The user's exception
    is re-raised either way.

    Construct via `Muxer.write_file(path)` or
    `Muxer.write_file(path, atomic=True)`, not directly. The Muxer
    itself is borrowed, not owned — it remains usable after the `with`
    block exits, including for further `write_file(...)` calls.
    """

    __slots__ = ("_muxer", "_path", "_fh", "_proxy", "_atomic", "_tmp_path")

    def __init__(self, muxer, path, *, atomic: bool = False) -> None:
        self._muxer = muxer
        # `Path()` accepts str, os.PathLike, and Path itself.
        self._path = Path(path)
        self._fh = None
        self._proxy = None
        self._atomic = atomic
        self._tmp_path = None

    def __enter__(self) -> MuxerDrainProxy:
        if self._atomic:
            # Tempfile in the SAME directory as the destination so the
            # eventual `os.replace` stays on one filesystem (rename
            # across filesystems is not atomic). `delete=False` so the
            # NamedTemporaryFile wrapper doesn't try to unlink on close
            # — we manage the lifetime ourselves in `__exit__`.
            tmp = tempfile.NamedTemporaryFile(
                dir=self._path.parent,
                suffix=".partial",
                delete=False,
            )
            self._tmp_path = Path(tmp.name)
            self._fh = tmp
        else:
            self._fh = self._path.open("wb")
        self._proxy = MuxerDrainProxy(self._muxer, self._fh)
        return self._proxy

    def __exit__(self, exc_type, exc, tb) -> None:
        # Audit-2 #2: drive cleanup with one outer try/finally so that
        # even if drain/close raises, the atomic-mode .partial file is
        # always removed (or replaced on success). Do not suppress the
        # caller's exception.
        drain_or_close_failed = False
        try:
            try:
                _drain_muxer_to_file(self._muxer, self._fh)
            finally:
                # Always close, even if drain raised. Both exceptions
                # propagate at the end; close-time exception chains onto
                # the drain-time one via Python's implicit __context__.
                self._fh.close()
        except BaseException:
            drain_or_close_failed = True
            if self._atomic:
                try:
                    os.unlink(self._tmp_path)
                except FileNotFoundError:
                    pass
            raise
        finally:
            # Only on the success path: promote .partial → final dest.
            if self._atomic and exc_type is None and not drain_or_close_failed:
                os.replace(self._tmp_path, self._path)
            elif self._atomic and exc_type is not None and not drain_or_close_failed:
                # User-body exception (drain succeeded): discard partial.
                try:
                    os.unlink(self._tmp_path)
                except FileNotFoundError:
                    pass
        # Returning None — never suppress.


def _muxer_write_file(self, path, *, atomic: bool = False) -> MuxerFileSink:
    """Open `path` for writing (mode `wb`) and return a context manager
    whose `with` statement yields a draining proxy: each `push_*` call
    made ON THE PROXY drains pending TS packets to the file, and exit
    drains whatever remains. The Muxer is borrowed, not owned — callers
    can reuse it for further `write_file(...)` calls after the `with`
    block exits.

        with mux.write_file("out.ts") as proxy:
            for au in access_units:
                proxy.push_video(au.nal, pts=au.pts)   # drains per push

    Push on the proxy, NOT on the original Muxer. The original object
    is not modified by the sink, so `mux.push_video(...)` inside the
    block bypasses the per-push drain and raises
    `MuxError(BACKPRESSURE)` once `buffer_packets` (default 10_000)
    accumulate — the classic long-push-loop footgun.

    Pass `atomic=True` to write via a `*.partial` tempfile in the same
    directory and `os.replace` to `path` only on successful exit. On
    exception, the tempfile is removed and no file appears at the
    destination. See `MuxerFileSink` for the full atomic-vs-default
    contract.

    Equivalent to constructing `MuxerFileSink(self, path, atomic=...)`
    directly.
    """

    return MuxerFileSink(self, path, atomic=atomic)


# Bind `write_file` onto the PyO3 Muxer class. PyO3 #[pyclass] types
# in abi3 mode without `subclass` may reject attribute assignment; if
# that happens, switch to the `#[pyclass(..., subclass)]` + subclass
# pattern (see task notes).
Muxer.write_file = _muxer_write_file  # type: ignore[attr-defined]


# Population happens task-by-task. __all__ accumulates as types land.
__all__: list[str] = [
    "Pts90khz",
    "VideoCodec",
    "AudioCodec",
    "SubtitleCodec",
    "KlvStreamType",
    "Av1CarriageMode",
    "StreamSpec",
    "VideoStreamSpec",
    "KlvStreamSpec",
    "AudioStreamSpec",
    "SubtitleStreamSpec",
    "DataStreamSpec",
    "DvbSubtitlingConfig",
    "DvbTeletextConfig",
    "Cea708StandaloneConfig",
    "WebVttInTsConfig",
    "SubtitleCodecConfig",
    "StreamKindTag",
    "MetadataKindTag",
    "DiscontinuityKindTag",
    "NonConformantKind",
    "StrictMode",
    "LinkSource",
    "Codec",
    "StreamId",
    "RawDescriptor",
    "StreamInfo",
    "KlvLink",
    "ProgramMap",
    "DemuxEvent",
    "DemuxerConfig",
    "Demuxer",
    "MultiCellAuReason",
    "CellFragmentIndication",
    "VideoStreamHandle",
    "AudioStreamHandle",
    "KlvStreamHandle",
    "SubtitleStreamHandle",
    "DataStreamHandle",
    "MuxerProgramConfig",
    "MuxerProgramConfigBuilder",
    "MuxerConfig",
    "MuxerConfigBuilder",
    "Muxer",
    "MuxerStats",
    "StreamCodecStats",
    "VideoStreamCodecStats",
    "KlvStreamCodecStats",
    "AudioStreamCodecStats",
    "MuxerFileSink",
    "MuxerDrainProxy",
    # NalUnit / Obu / ObuExtension re-exported from tstrans.codec for
    # convenient import from tstrans.mpegts (returned by
    # DemuxEvent.Video.parse() / tstrans.codec.split_units).
    "NalUnit",
    "Obu",
    "ObuExtension",
]
