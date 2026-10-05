"""mypy --strict assert_type smoke for tstrans.mpegts — pins the raw-first
DemuxEvent.Video/.Audio surface (.raw + .parse(), NOT .payload), Pts90khz, and
a few representative mux-side members so accidental signature drift fails mypy
here too (not just under stubtest).
Not collected by pytest (no test_*); checked statically by tests/typing/mypy.ini.

Note: smoke files are static-checked only, never executed — the byte literals
just need to be valid `bytes`, they are never muxed/demuxed at runtime."""
from typing import Any, List, Literal, Optional, Tuple, Union, assert_type

import tstrans.klv as klv_mod
from tstrans.mpegts import (
    DataStreamHandle,
    DemuxEvent,
    Demuxer,
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)

v: DemuxEvent.Video
assert_type(v.raw, bytes)
assert_type(v.random_access_indicator, bool)
assert_type(v.pts, Pts90khz)
assert_type(v.dts, Optional[Pts90khz])
assert_type(v.codec, VideoCodec)
# parse() is mode-polymorphic: strict=False → bare unit list; strict=True →
# (units, issues) tuple. The stub declares the union of both, so just pin the
# declared return type rather than a single concrete shape.
parsed = v.parse(strict=False)
assert_type(parsed, Union[List[Any], Tuple[List[Any], List[str]]])

a: DemuxEvent.Audio
assert_type(a.raw, bytes)
a.parse(strict=False)

# Klv.parse() decode-on-event sugar — UL-dispatched Optional[union], the KLV
# counterpart of the raw-first Video/Audio parse(). strict= is keyword-only.
k: DemuxEvent.Metadata
assert_type(k.payload, bytes)
assert_type(
    k.parse(strict=True),
    Optional[
        Union[
            klv_mod.UasDatalinkLs,
            klv_mod.SecurityLs,
            klv_mod.PrecisionTimeStampPack,
            klv_mod.VmtiLs,
        ]
    ],
)

# Pts90khz surface — a constructor and both derived props.
p = Pts90khz.from_raw(9000)
assert_type(p, Pts90khz)
assert_type(p.ms, int)
assert_type(p.seconds, float)
assert_type(Pts90khz.from_ms(100), Pts90khz)

# Representative mux/demux coverage beyond the raw-first pair — a Demuxer
# method return, a builder-chaining call returning the builder self-type, and
# an enum-member access. These pin the symbols so accidental signature drift
# would fail mypy here. (VideoCodec is a real enum.Enum, so a member access
# narrows to its Literal type.)
d: Demuxer
assert_type(d.next_event(), Optional[DemuxEvent])
b = MuxerConfigBuilder()
assert_type(b.buffer_packets(1024), MuxerConfigBuilder)
assert_type(VideoCodec.H264, Literal[VideoCodec.H264])

# Data-stream surface — builder add_data/stream_descriptors_for_data
# chain, the push_data pair, the handle-accessor trio, and the
# DataStreamHandle members (mirrors how the klv handles are smoked).
pb = MuxerProgramConfigBuilder(1, 0x100)
assert_type(pb.add_data(0x1F0, 0xF0, carries_pts=True), MuxerProgramConfigBuilder)
assert_type(
    pb.stream_descriptors_for_data(0, [b"\xff\x04demo"]), MuxerProgramConfigBuilder
)
mux: Muxer
mux.push_data(b"\x00", pts=Pts90khz.from_raw(0))
assert_type(mux.data_handles(), List[DataStreamHandle])
assert_type(mux.data_handles_for_program(1), List[DataStreamHandle])
assert_type(mux.data_stream_handle(0), Optional[DataStreamHandle])
dh = DataStreamHandle.from_raw(0)
assert_type(dh, DataStreamHandle)
assert_type(dh.raw, int)
assert_type(dh.unpack(), Tuple[int, int])
mux.push_data_to(dh, b"\x00", pts=Pts90khz.from_raw(0))
