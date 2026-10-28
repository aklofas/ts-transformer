"""Type stubs for tstrans.io — see io.py for the runtime + full docstrings.

mypy --strict clean."""
from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from types import TracebackType
from typing import Any, Iterator, Optional, Sequence, Tuple, Type, Union

from tstrans.klv import UasDatalinkLs
from tstrans.mpegts import (
    AudioCodec,
    DemuxEvent,
    DemuxerConfig,
    ProgramMap,
    Pts90khz,
    StreamKindTag,
    SubtitleCodec,
    VideoCodec,
)

@dataclass(frozen=True, slots=True)
class ProbeResult:
    size_bytes: int
    programs: Tuple[ProgramMap, ...]
    pids: Tuple[int, ...]
    video_codecs: Tuple[VideoCodec, ...]
    audio_codecs: Tuple[AudioCodec, ...]
    subtitle_codecs: Tuple[SubtitleCodec, ...]
    has_klv: bool
    packet_count: int

def parse_file(
    path: Union[str, Path],
    config: Optional[DemuxerConfig] = ...,
) -> Iterator[DemuxEvent]: ...
def probe(
    path: Union[str, Path],
    *,
    config: Optional[DemuxerConfig] = ...,
) -> ProbeResult: ...
def extract_klv(
    path: Union[str, Path],
    *,
    with_pts: bool = ...,
    parsed: bool = ...,
    skip_unknown: bool = ...,
    skip_malformed: bool = ...,
    config: Optional[DemuxerConfig] = ...,
) -> Iterator[Any]: ...
def iter_uas_datalink(
    path: Union[str, Path],
    *,
    strict: bool = ...,
    config: Optional[DemuxerConfig] = ...,
) -> Iterator[Tuple[Pts90khz, int, UasDatalinkLs]]: ...

class Transmuxer:
    def __init__(
        self,
        src: Union[str, Path],
        dst: Union[str, Path],
        *,
        drop: Sequence[StreamKindTag] = ...,
        atomic: bool = ...,
        config: Optional[DemuxerConfig] = ...,
    ) -> None:
        """With `atomic=False`, raises `ValueError` when `dst` is the same file
        as `src` (symlinks and hard links included); `atomic=True` allows an
        in-place rewrite."""
    def __enter__(self) -> Transmuxer: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc: Optional[BaseException],
        tb: Optional[TracebackType],
    ) -> None: ...
    def __iter__(self) -> Iterator[DemuxEvent]: ...
    def write(self, ev: DemuxEvent) -> None: ...
    def write_klv(
        self, ev: DemuxEvent.Metadata, new_bytes: Union[bytes, bytearray, memoryview]
    ) -> None: ...

def transmux(
    src: Union[str, Path],
    dst: Union[str, Path],
    *,
    drop: Sequence[StreamKindTag] = ...,
    atomic: bool = ...,
    config: Optional[DemuxerConfig] = ...,
) -> Transmuxer:
    """With `atomic=False`, raises `ValueError` when `dst` is the same file
    as `src` (symlinks and hard links included); `atomic=True` allows an
    in-place rewrite."""

__all__ = [
    "parse_file",
    "probe",
    "extract_klv",
    "iter_uas_datalink",
    "transmux",
    "ProbeResult",
    "Transmuxer",
]
