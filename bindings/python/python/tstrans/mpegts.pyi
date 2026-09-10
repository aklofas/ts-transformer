"""Type stubs for tstrans.mpegts — see mpegts.py for the runtime + full
docstrings.

Covers the 90 kHz timestamp wrapper (Pts90khz), the demux/mux enums, the
stream-spec + subtitle-codec-config dataclasses, the PSI value types
(StreamId / RawDescriptor / StreamInfo / KlvLink / ProgramMap), the raw-first DemuxEvent
subclass hierarchy (Video/Audio carry .raw + .parse(), NOT .payload — v0.2.0
item #1), the DemuxerConfig + Demuxer, and the full mux surface (the four
config/builder pairs, the four stream handles, the Muxer, the stats types,
and the MuxerFileSink / MuxerDrainProxy file-drain helpers).

The DemuxEvent subclasses are declared as nested classes (`DemuxEvent.Video`
etc., usable as types for the 3.10+ match-statement syntax); the module-level
`_*Event` names the runtime also exposes are re-bound to them.

The *_np-style accessors are absent here; any numpy-typed value is `Any` so the
stubs never import numpy (an optional [pandas] extra).

mypy --strict clean.
"""
from __future__ import annotations

import enum
from dataclasses import dataclass
from pathlib import Path
from types import TracebackType
from typing import (
    Any,
    ClassVar,
    Dict,
    List,
    Optional,
    Sequence,
    Tuple,
    Type,
    Union,
    final,
)

from tstrans.codec import MispTimestamp, NalUnit, Obu, ObuExtension
from tstrans.klv import (
    PrecisionTimeStampPack,
    SecurityLs,
    UasDatalinkLs,
    VmtiLs,
)

# Bytes-like input accepted by the PyO3 `&[u8]` / `bytes()`-coercion extractors.
# Private (leading underscore) so stubtest doesn't treat it as a runtime member.
_BytesLike = Union[bytes, bytearray, memoryview]

# --- 90 kHz timestamp ---

@dataclass(frozen=True, slots=True)
class Pts90khz:
    raw: int
    def __post_init__(self) -> None: ...
    @classmethod
    def from_raw(cls, ticks: int) -> Pts90khz: ...
    @classmethod
    def from_ms(cls, ms: int) -> Pts90khz: ...
    @classmethod
    def from_seconds(cls, seconds: float) -> Pts90khz: ...
    @property
    def ms(self) -> int: ...
    @property
    def seconds(self) -> float: ...

# --- demux / mux enums (real enum.Enum subclasses, str-valued) ---

class VideoCodec(enum.Enum):
    H264 = "h264"
    H265 = "h265"
    H266 = "h266"
    AV1 = "av1"

class AudioCodec(enum.Enum):
    MP2 = "mp2"
    AAC = "aac"
    AAC_LATM = "aac_latm"
    AC3 = "ac3"

class SubtitleCodec(enum.Enum):
    DVB_SUBTITLING = "dvb_subtitling"
    DVB_TELETEXT = "dvb_teletext"
    CEA708_STANDALONE = "cea708_standalone"
    WEBVTT_IN_TS = "webvtt_in_ts"

class KlvStreamType(enum.Enum):
    PRIVATE_DATA = "private_data"
    SYNCHRONOUS_METADATA = "synchronous_metadata"

class Av1CarriageMode(enum.Enum):
    MPEG2_TS_BINDING = "mpeg2_ts_binding"
    INTEROP_RAW_OBU = "interop_raw_obu"

class StreamKindTag(enum.Enum):
    VIDEO = "video"
    AUDIO = "audio"
    SUBTITLE = "subtitle"
    KLV_SYNC = "klv_sync"
    KLV_ASYNC = "klv_async"
    UNKNOWN = "unknown"

class MetadataKindTag(enum.Enum):
    KLV_SYNC_AU_CELL = "klv_sync_au_cell"
    KLV_ASYNC = "klv_async"
    UNKNOWN = "unknown"

class DiscontinuityKindTag(enum.Enum):
    CONTINUITY_JUMP = "continuity_jump"
    PES_OVERSIZE = "pes_oversize"
    PES_TOTAL_OVERSIZE = "pes_total_oversize"
    ADAPTATION_FIELD_FLAG = "adaptation_field_flag"

class NonConformantKind(enum.Enum):
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
    OFF = "off"
    TIMING_ONLY = "timing_only"
    PSI_ONLY = "psi_only"
    FULL = "full"

class LinkSource(enum.Enum):
    DECLARED = "declared"
    INFERRED = "inferred"
    OVERRIDE = "override"

# --- PyO3 eq_int enums (plain classes; members are instances, int-comparable) ---

@final
class MultiCellAuReason:
    ORPHAN: MultiCellAuReason
    SEQUENCE_GAP: MultiCellAuReason
    CONCURRENT_FIRST: MultiCellAuReason
    OVERFLOW: MultiCellAuReason
    OVERFLOW_TOTAL: MultiCellAuReason
    TOO_MANY_PIDS: MultiCellAuReason

@final
class CellFragmentIndication:
    MIDDLE: CellFragmentIndication
    LAST: CellFragmentIndication
    FIRST: CellFragmentIndication
    COMPLETE: CellFragmentIndication

# --- mux-side StreamSpec hierarchy (frozen dataclasses) ---

@dataclass(frozen=True, slots=True)
class StreamSpec:
    pid: int

@dataclass(frozen=True)
class VideoStreamSpec(StreamSpec):
    # Explicit __slots__ (and no `slots=True`): a slotted dataclass that
    # inherits a field only adds its OWN fields to __slots__ at runtime; mypy's
    # `slots=True` synthesis would otherwise include the inherited `pid` and
    # diverge from the runtime tuple. Declaring __slots__ here matches the
    # runtime exactly without tripping the "both __slots__ and slots=True" rule.
    __slots__ = ("codec",)
    codec: VideoCodec

@dataclass(frozen=True)
class KlvStreamSpec(StreamSpec):
    __slots__ = ("stream_type", "carries_pts")
    stream_type: KlvStreamType
    carries_pts: bool

@dataclass(frozen=True)
class AudioStreamSpec(StreamSpec):
    __slots__ = ("codec", "language")
    codec: AudioCodec
    language: Optional[bytes] = ...

@dataclass(frozen=True)
class SubtitleStreamSpec(StreamSpec):
    __slots__ = ("codec",)
    codec: SubtitleCodec

@dataclass(frozen=True)
class DataStreamSpec(StreamSpec):
    __slots__ = ("stream_type", "carries_pts")
    stream_type: int
    carries_pts: bool

# --- mux-side subtitle codec config dataclasses ---

@dataclass(frozen=True, slots=True)
class DvbSubtitlingConfig:
    language: bytes
    subtitling_type: int
    composition_page_id: int
    ancillary_page_id: int
    def __post_init__(self) -> None: ...

@dataclass(frozen=True, slots=True)
class DvbTeletextConfig:
    language: bytes
    teletext_type: int
    magazine_number: int
    page_number: int
    def __post_init__(self) -> None: ...

@dataclass(frozen=True, slots=True)
class Cea708StandaloneConfig: ...

@dataclass(frozen=True, slots=True)
class WebVttInTsConfig: ...

# Discriminated union accepted by MuxerProgramConfigBuilder.add_subtitle.
SubtitleCodecConfig = Union[
    DvbSubtitlingConfig,
    DvbTeletextConfig,
    Cea708StandaloneConfig,
    WebVttInTsConfig,
]

# "Any codec enum or None" — resolved by the StreamKindTag discriminator.
Codec = Union[VideoCodec, AudioCodec, SubtitleCodec, None]

# --- PSI value types (frozen dataclasses) ---

@dataclass(frozen=True, slots=True)
class StreamId:
    pid: int
    kind: StreamKindTag
    codec: Codec
    program_number: int

@dataclass(frozen=True, slots=True)
class RawDescriptor:
    tag: int
    data: bytes

@dataclass(frozen=True, slots=True)
class StreamInfo:
    pid: int
    stream_type: int
    kind: StreamKindTag
    codec: Codec
    program_number: int
    raw_descriptors: Tuple[RawDescriptor, ...] = ...

@dataclass(frozen=True, slots=True)
class KlvLink:
    klv_pid: int
    video_pid: int
    source: LinkSource

@dataclass(frozen=True, slots=True)
class ProgramMap:
    program_number: int
    pcr_pid: int
    pmt_pid: int
    streams: Tuple[Any, ...]
    klv_links: Tuple[Any, ...]

# Unambiguous alias for the PSI `ProgramMap` dataclass above, so the nested
# `DemuxEvent.ProgramMap` event class can name it as an element type without the
# bare `ProgramMap` reference *looking* like the nested class. (mypy already
# resolves a nested-class-body name to module scope — verified via reveal_type —
# but the alias makes the intent obvious to human readers and future refactors.)
_ProgramMapData = ProgramMap

# --- DemuxEvent hierarchy (raw-first; Video/Audio carry .raw + .parse()) ---
#
# At runtime the subclasses are module-level `_*Event` dataclasses surfaced as
# `DemuxEvent.Video` etc. via class attributes. The stub declares them as nested
# classes (so `DemuxEvent.Video` is usable as a type, per the 3.10+ match-syntax
# contract) and re-binds each module-level `_*Event` name to the nested class —
# at runtime those names ARE the same objects, so stubtest stays clean.

class DemuxEvent:
    @dataclass(frozen=True, slots=True)
    class ProgramMap(DemuxEvent):
        programs: Tuple[_ProgramMapData, ...]

    # `Video` / `Audio` / `Subtitle` / `UnknownSample` are hand-written frozen
    # classes at runtime (NOT dataclasses) because their raw access unit /
    # payload is lazily materialized through the native `RawBytes` holder —
    # a value cannot be both a dataclass field and a same-named property.
    # `Video` and `Audio` expose this as `.raw`; `Subtitle` and
    # `UnknownSample` expose it as `.payload`. The stub mirrors the runtime:
    # a keyword-only `__init__`, the access property as a read-only `bytes`
    # property, and explicit `__match_args__`.
    class Video(DemuxEvent):
        stream: StreamId
        pts: Pts90khz
        dts: Optional[Pts90khz]
        codec: VideoCodec
        random_access_indicator: bool
        av1_carriage: Optional[Av1CarriageMode]
        __match_args__: ClassVar[Tuple[str, ...]]
        def __init__(
            self,
            *,
            stream: StreamId,
            pts: Pts90khz,
            dts: Optional[Pts90khz],
            codec: VideoCodec,
            raw: bytes,
            random_access_indicator: bool,
            av1_carriage: Optional[Av1CarriageMode] = ...,
        ) -> None: ...
        @property
        def raw(self) -> bytes: ...
        # Always a flat list of units (NalUnit | Obu); strict=True raises
        # ValueError on the first ES-conformance issue. For (units, issues),
        # call tstrans.codec.split_units(raw, codec) instead.
        def parse(self, *, strict: bool = ...) -> List[Any]: ...

    class Audio(DemuxEvent):
        stream: StreamId
        pts: Pts90khz
        dts: Optional[Pts90khz]
        codec: AudioCodec
        __match_args__: ClassVar[Tuple[str, ...]]
        def __init__(
            self,
            *,
            stream: StreamId,
            pts: Pts90khz,
            dts: Optional[Pts90khz],
            codec: AudioCodec,
            raw: bytes,
        ) -> None: ...
        @property
        def raw(self) -> bytes: ...
        # List[AdtsFrame] | List[Mpeg2AudioFrame] | [] — element type erased.
        # strict=True raises CodecError on the first malformed frame.
        def parse(self, *, strict: bool = ...) -> List[Any]: ...

    class Subtitle(DemuxEvent):
        stream: StreamId
        pts: Pts90khz
        dts: Optional[Pts90khz]
        codec: SubtitleCodec
        __match_args__: ClassVar[Tuple[str, ...]]
        def __init__(
            self,
            *,
            stream: StreamId,
            pts: Pts90khz,
            dts: Optional[Pts90khz],
            codec: SubtitleCodec,
            payload: bytes,
        ) -> None: ...
        @property
        def payload(self) -> bytes: ...

    @dataclass(frozen=True, slots=True)
    class Metadata(DemuxEvent):
        stream: StreamId
        pts: Pts90khz
        kind: MetadataKindTag
        payload: bytes
        was_reassembled: bool = ...
        cell_count: int = ...
        def parse(
            self, *, strict: bool = ...
        ) -> Optional[
            Union[UasDatalinkLs, SecurityLs, PrecisionTimeStampPack, VmtiLs]
        ]: ...

    # Deprecated alias for Metadata — same class object at runtime; remove at 1.0.
    # Declared as a full class so stubtest can check the runtime attribute.
    @dataclass(frozen=True, slots=True)
    class Klv(DemuxEvent):
        stream: StreamId
        pts: Pts90khz
        kind: MetadataKindTag
        payload: bytes
        was_reassembled: bool = ...
        cell_count: int = ...
        def parse(
            self, *, strict: bool = ...
        ) -> Optional[
            Union[UasDatalinkLs, SecurityLs, PrecisionTimeStampPack, VmtiLs]
        ]: ...

    class UnknownSample(DemuxEvent):
        stream: StreamId
        pts: Pts90khz
        dts: Optional[Pts90khz]
        stream_type: int
        __match_args__: ClassVar[Tuple[str, ...]]
        def __init__(
            self,
            *,
            stream: StreamId,
            pts: Pts90khz,
            dts: Optional[Pts90khz],
            stream_type: int,
            payload: bytes,
        ) -> None: ...
        @property
        def payload(self) -> bytes: ...

    @dataclass(frozen=True, slots=True)
    class Discontinuity(DemuxEvent):
        stream: StreamId
        kind: DiscontinuityKindTag

    @dataclass(frozen=True, slots=True)
    class NonConformant(DemuxEvent):
        stream: StreamId
        issue: str
        kind: NonConformantKind
        multi_cell_au_reason: Optional[MultiCellAuReason] = ...
        observed_cfi: Optional[CellFragmentIndication] = ...
        treated_as: Optional[CellFragmentIndication] = ...

    @dataclass(frozen=True, slots=True)
    class ReconnectDiscontinuity(DemuxEvent): ...

# Module-level names that the runtime exposes for the subclasses (the nested
# classes above ARE these objects at runtime).
_ProgramMapEvent = DemuxEvent.ProgramMap
_VideoEvent = DemuxEvent.Video
_AudioEvent = DemuxEvent.Audio
_SubtitleEvent = DemuxEvent.Subtitle
_MetadataEvent = DemuxEvent.Metadata
_KlvEvent = DemuxEvent.Klv
_UnknownSampleEvent = DemuxEvent.UnknownSample
_DiscontinuityEvent = DemuxEvent.Discontinuity
_NonConformantEvent = DemuxEvent.NonConformant
_ReconnectDiscontinuityEvent = DemuxEvent.ReconnectDiscontinuity

# --- DemuxerConfig + Demuxer ---

@dataclass(frozen=True, slots=True)
class DemuxerConfig:
    strict_mode: StrictMode = ...
    pes_cap_per_pid: int = ...
    pes_cap_total: int = ...
    cfi_tolerance: bool = ...
    av1_carriage: Optional[Av1CarriageMode] = ...
    au_cell_cap_per_pid: Optional[int] = ...
    lenient_psi_reassembly: bool = ...
    sync_buf_cap: int = ...
    unwrap_timestamps: bool = ...
    def __post_init__(self) -> None: ...

@final
class Demuxer:
    def __new__(cls, config: Optional[DemuxerConfig] = ...) -> Demuxer: ...
    def feed(self, bytes: _BytesLike) -> None: ...
    def flush(self) -> None: ...
    def next_event(self) -> Optional[DemuxEvent]: ...
    def __iter__(self) -> Demuxer: ...
    def __next__(self) -> DemuxEvent: ...
    def stats(self) -> Dict[str, int]: ...
    def reset_stats(self) -> None: ...

# --- mux: stream handles ---

@final
class VideoStreamHandle:
    @staticmethod
    def from_raw(raw: int) -> VideoStreamHandle: ...
    @property
    def raw(self) -> int: ...
    def unpack(self) -> Tuple[int, int]: ...

@final
class AudioStreamHandle:
    @staticmethod
    def from_raw(raw: int) -> AudioStreamHandle: ...
    @property
    def raw(self) -> int: ...
    def unpack(self) -> Tuple[int, int]: ...

@final
class KlvStreamHandle:
    @staticmethod
    def from_raw(raw: int) -> KlvStreamHandle: ...
    @property
    def raw(self) -> int: ...
    def unpack(self) -> Tuple[int, int]: ...

@final
class SubtitleStreamHandle:
    @staticmethod
    def from_raw(raw: int) -> SubtitleStreamHandle: ...
    @property
    def raw(self) -> int: ...
    def unpack(self) -> Tuple[int, int]: ...

@final
class DataStreamHandle:
    @staticmethod
    def from_raw(raw: int) -> DataStreamHandle: ...
    @property
    def raw(self) -> int: ...
    def unpack(self) -> Tuple[int, int]: ...

# --- mux: program config + builder ---

@final
class MuxerProgramConfig:
    @property
    def program_number(self) -> int: ...
    @property
    def pmt_pid(self) -> int: ...
    @property
    def pcr_pid(self) -> Optional[int]: ...
    @property
    def streams(self) -> Tuple[StreamSpec, ...]: ...
    @property
    def program_descriptors(self) -> Tuple[bytes, ...]: ...
    @property
    def stream_descriptors(self) -> Tuple[Tuple[bytes, ...], ...]: ...

@final
class MuxerProgramConfigBuilder:
    def __new__(
        cls, program_number: int, pmt_pid: int
    ) -> MuxerProgramConfigBuilder: ...
    def add_video(
        self, pid: int, codec: VideoCodec
    ) -> MuxerProgramConfigBuilder: ...
    def add_klv(
        self, pid: int, stream_type: KlvStreamType, *, carries_pts: bool
    ) -> MuxerProgramConfigBuilder: ...
    def add_audio(
        self, pid: int, codec: AudioCodec
    ) -> MuxerProgramConfigBuilder: ...
    def add_audio_with_language(
        self, pid: int, codec: AudioCodec, *, language: _BytesLike
    ) -> MuxerProgramConfigBuilder: ...
    def add_subtitle(
        self, pid: int, codec_config: SubtitleCodecConfig
    ) -> MuxerProgramConfigBuilder: ...
    def add_data(
        self, pid: int, stream_type: int, *, carries_pts: bool
    ) -> MuxerProgramConfigBuilder: ...
    def pcr_pid(self, pid: int) -> MuxerProgramConfigBuilder: ...
    def program_descriptors(
        self, descs: List[_BytesLike]
    ) -> MuxerProgramConfigBuilder: ...
    def stream_descriptors_for_video(
        self, video_idx: int, descs: List[_BytesLike]
    ) -> MuxerProgramConfigBuilder: ...
    def stream_descriptors_for_klv(
        self, klv_idx: int, descs: List[_BytesLike]
    ) -> MuxerProgramConfigBuilder: ...
    def stream_descriptors_for_audio(
        self, audio_idx: int, descs: List[_BytesLike]
    ) -> MuxerProgramConfigBuilder: ...
    def stream_descriptors_for_subtitle(
        self, subtitle_idx: int, descs: List[_BytesLike]
    ) -> MuxerProgramConfigBuilder: ...
    def stream_descriptors_for_data(
        self, data_idx: int, descs: List[_BytesLike]
    ) -> MuxerProgramConfigBuilder: ...
    def stream_descriptors_for_stream(
        self, abs_idx: int, descs: List[_BytesLike]
    ) -> MuxerProgramConfigBuilder: ...
    def build(self) -> MuxerProgramConfig: ...

# --- mux: top-level config + builder ---

@final
class MuxerConfig:
    @staticmethod
    def builder() -> MuxerConfigBuilder: ...
    @staticmethod
    def from_program_map(
        pm: ProgramMap,
        drop: Optional[Sequence[StreamKindTag]] = ...,
        av1_carriage: Optional[Av1CarriageMode] = ...,
    ) -> MuxerConfig: ...
    @property
    def programs(self) -> Tuple[MuxerProgramConfig, ...]: ...
    @property
    def pcr_interval_ms(self) -> int: ...
    @property
    def psi_interval_ms(self) -> int: ...
    @property
    def buffer_packets(self) -> int: ...
    @property
    def av1_carriage(self) -> Av1CarriageMode: ...

@final
class MuxerConfigBuilder:
    def __new__(cls) -> MuxerConfigBuilder: ...
    def add_program(self, prog: MuxerProgramConfig) -> MuxerConfigBuilder: ...
    def pcr_interval_ms(self, ms: int) -> MuxerConfigBuilder: ...
    def psi_interval_ms(self, ms: int) -> MuxerConfigBuilder: ...
    def buffer_packets(self, n: int) -> MuxerConfigBuilder: ...
    def av1_carriage(self, mode: Av1CarriageMode) -> MuxerConfigBuilder: ...
    def build(self) -> MuxerConfig: ...

# --- mux: stats ---

@final
class MuxerStats:
    @property
    def ts_packets_emitted(self) -> int: ...
    @property
    def ts_bytes_emitted(self) -> int: ...
    @property
    def programs_configured(self) -> int: ...
    @property
    def subtitle_streams_configured(self) -> int: ...

@dataclass(frozen=True, slots=True)
class StreamCodecStats: ...

@dataclass(frozen=True, slots=True)
class VideoStreamCodecStats(StreamCodecStats):
    nals_or_obus: int
    random_access_aus: int

@dataclass(frozen=True, slots=True)
class KlvStreamCodecStats(StreamCodecStats):
    records: int

@dataclass(frozen=True, slots=True)
class AudioStreamCodecStats(StreamCodecStats):
    frames: int

# --- mux: Muxer ---

@final
class Muxer:
    def __new__(cls, config: MuxerConfig) -> Muxer: ...
    def pull(self, out: bytearray) -> int: ...
    def pending_packets(self) -> int: ...
    def capacity_packets(self) -> int: ...
    def push_video(
        self, nal: _BytesLike, *, pts: Pts90khz, key_frame: bool = ...
    ) -> None: ...
    def push_video_to(
        self,
        handle: VideoStreamHandle,
        nal: _BytesLike,
        *,
        pts: Pts90khz,
        key_frame: bool = ...,
    ) -> None: ...
    def push_video_to_with_dts(
        self,
        handle: VideoStreamHandle,
        nal: _BytesLike,
        *,
        pts: Pts90khz,
        dts: Optional[Pts90khz] = ...,
        key_frame: bool = ...,
    ) -> None: ...
    def push_video_misp_to(
        self,
        handle: VideoStreamHandle,
        nal: _BytesLike,
        *,
        pts: Pts90khz,
        dts: Optional[Pts90khz] = ...,
        key_frame: bool = ...,
        misp: MispTimestamp,
    ) -> None: ...
    def push_video_wire_to(
        self,
        handle: VideoStreamHandle,
        wire: _BytesLike,
        *,
        pts: Pts90khz,
        dts: Optional[Pts90khz] = ...,
        key_frame: bool = ...,
    ) -> None: ...
    def push_audio(self, frames: _BytesLike, *, pts: Pts90khz) -> None: ...
    def push_audio_to(
        self, handle: AudioStreamHandle, frames: _BytesLike, *, pts: Pts90khz
    ) -> None: ...
    def push_klv(
        self, klv: _BytesLike, *, pts: Pts90khz, metadata_service_id: int = ...
    ) -> None: ...
    def push_klv_to(
        self,
        handle: KlvStreamHandle,
        klv: _BytesLike,
        *,
        pts: Pts90khz,
        metadata_service_id: int = ...,
    ) -> None: ...
    def push_subtitle(self, payload: _BytesLike, *, pts: Pts90khz) -> None: ...
    def push_subtitle_to(
        self, handle: SubtitleStreamHandle, payload: _BytesLike, *, pts: Pts90khz
    ) -> None: ...
    def push_data(self, data: _BytesLike, *, pts: Pts90khz) -> None: ...
    def push_data_to(
        self, handle: DataStreamHandle, data: _BytesLike, *, pts: Pts90khz
    ) -> None: ...
    def video_handles(self) -> List[VideoStreamHandle]: ...
    def video_handles_for_program(
        self, program_number: int
    ) -> List[VideoStreamHandle]: ...
    def video_stream_handle(self, index: int) -> Optional[VideoStreamHandle]: ...
    def audio_handles(self) -> List[AudioStreamHandle]: ...
    def audio_handles_for_program(
        self, program_number: int
    ) -> List[AudioStreamHandle]: ...
    def klv_handles(self) -> List[KlvStreamHandle]: ...
    def klv_handles_for_program(
        self, program_number: int
    ) -> List[KlvStreamHandle]: ...
    def klv_stream_handle(self, index: int) -> Optional[KlvStreamHandle]: ...
    def subtitle_handles(self) -> List[SubtitleStreamHandle]: ...
    def subtitle_handles_for_program(
        self, program_number: int
    ) -> List[SubtitleStreamHandle]: ...
    def data_handles(self) -> List[DataStreamHandle]: ...
    def data_handles_for_program(
        self, program_number: int
    ) -> List[DataStreamHandle]: ...
    def data_stream_handle(self, index: int) -> Optional[DataStreamHandle]: ...
    def stats(self) -> MuxerStats: ...
    def reset_stats(self) -> None: ...
    def stream_codec_stats(self, pid: int) -> Optional[StreamCodecStats]: ...
    def write_file(
        self, path: Union[str, Path], *, atomic: bool = ...
    ) -> MuxerFileSink: ...

# --- mux: file-drain helpers (pure Python) ---

class MuxerDrainProxy:
    def __init__(self, muxer: Muxer, fh: Any) -> None: ...
    def __getattr__(self, name: str) -> Any: ...

class MuxerFileSink:
    def __init__(
        self, muxer: Muxer, path: Union[str, Path], *, atomic: bool = ...
    ) -> None: ...
    def __enter__(self) -> MuxerDrainProxy: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc: Optional[BaseException],
        tb: Optional[TracebackType],
    ) -> None: ...

__all__ = [
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
    "NalUnit",
    "Obu",
    "ObuExtension",
]
