"""`tstrans.mpegts.Muxer.reconfigure`: the running muxer takes a new program
list. Survivors keep their handles and continuity counters, new streams get
handles, a removed stream's handle is refused, and a refused config changes
nothing."""

from __future__ import annotations

import pytest

from tstrans.exceptions import MuxError, MuxErrorKind
from tstrans.mpegts import (
    Demuxer,
    DemuxerConfig,
    DemuxEvent,
    KlvStreamType,
    Muxer,
    MuxerConfig,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)

# An IDR slice NAL long enough to span several TS packets, so a counter
# that restarted would show as a continuity jump.
NAL_IDR = b"\x00\x00\x00\x01\x65" + bytes(range(256)) * 4
KLV_LS = (
    b"\x06\x0e\x2b\x34\x02\x0b\x01\x01"
    b"\x0e\x01\x03\x01\x01\x00\x00\x00"
    b"\x03\x02\x01\x02"
)


def _config(*programs: object) -> MuxerConfig:
    b = MuxerConfigBuilder()
    for p in programs:
        b = b.add_program(p)
    return b.build()


def _video_program(number: int, pmt_pid: int, video_pid: int) -> object:
    return MuxerProgramConfigBuilder(number, pmt_pid).add_video(video_pid, VideoCodec.H264).build()


def _video_klv_program(number: int, pmt_pid: int, video_pid: int, klv_pid: int) -> object:
    return (
        MuxerProgramConfigBuilder(number, pmt_pid)
        .add_video(video_pid, VideoCodec.H264)
        .add_klv(klv_pid, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )


def _drain(m: Muxer) -> bytes:
    buf = bytearray(max(m.pending_packets(), 1) * 188)
    n = m.pull(buf)
    assert n % 188 == 0
    return bytes(buf[:n])


def _events(ts: bytes) -> list[object]:
    d = Demuxer(DemuxerConfig())
    d.feed(ts)
    d.flush()
    return list(d)


def _push_frames(m: Muxer, handle: object, first_pts: int, count: int) -> bytes:
    out = b""
    for i in range(count):
        m.push_video_to(handle, NAL_IDR, pts=Pts90khz.from_raw(first_pts + i * 3000), key_frame=i == 0)
        out += _drain(m)
    return out


def test_a_surviving_stream_keeps_continuity_across_reconfigure() -> None:
    m = Muxer(_config(_video_program(1, 0x100, 0x101)))
    [v] = m.video_handles()
    ts = _push_frames(m, v, 90_000, 5)
    # Program 1 gains a KLV stream and program 2 appears; the video survives.
    m.reconfigure(
        _config(
            _video_klv_program(1, 0x100, 0x101, 0x102),
            _video_program(2, 0x200, 0x201),
        )
    )
    assert m.video_handles_for_program(1) == [v]
    ts += _push_frames(m, v, 90_000 + 5 * 3000, 5)

    events = _events(ts)
    jumps = [e for e in events if isinstance(e, DemuxEvent.Discontinuity) and e.stream.pid == 0x101]
    assert jumps == []
    videos = [e for e in events if isinstance(e, DemuxEvent.Video) and e.stream.pid == 0x101]
    assert len(videos) == 10
    assert all(e.raw.endswith(NAL_IDR[5:]) for e in videos)


def test_an_added_stream_gets_a_handle() -> None:
    m = Muxer(_config(_video_program(1, 0x100, 0x101)))
    assert m.klv_handles_for_program(1) == []
    m.reconfigure(_config(_video_klv_program(1, 0x100, 0x101, 0x102)))
    [k] = m.klv_handles_for_program(1)
    [v] = m.video_handles_for_program(1)
    m.push_video_to(v, NAL_IDR, pts=Pts90khz.from_raw(90_000), key_frame=True)
    m.push_klv_to(k, KLV_LS, pts=Pts90khz.from_raw(90_000))
    m.push_video_to(v, NAL_IDR, pts=Pts90khz.from_raw(93_000))
    metadata = [
        e for e in _events(_drain(m)) if isinstance(e, DemuxEvent.Metadata) and e.stream.pid == 0x102
    ]
    assert [e.payload for e in metadata] == [KLV_LS]


def test_a_removed_streams_handle_raises_invalid_usage() -> None:
    m = Muxer(_config(_video_program(1, 0x100, 0x101), _video_program(2, 0x200, 0x201)))
    [gone] = m.video_handles_for_program(2)
    [kept] = m.video_handles_for_program(1)
    m.reconfigure(_config(_video_program(1, 0x100, 0x101)))
    with pytest.raises(MuxError) as ei:
        m.push_video_to(gone, NAL_IDR, pts=Pts90khz.from_raw(90_000), key_frame=True)
    assert ei.value.kind == MuxErrorKind.INVALID_USAGE
    m.push_video_to(kept, NAL_IDR, pts=Pts90khz.from_raw(90_000), key_frame=True)
    with pytest.raises(MuxError) as ei:
        m.video_handles_for_program(2)
    assert ei.value.kind == MuxErrorKind.INVALID_USAGE


@pytest.mark.parametrize(
    ("refused", "reason"),
    [
        # The PMT PID of a surviving program moved.
        pytest.param(
            lambda: _config(_video_program(1, 0x110, 0x101)),
            "changes its PMT PID",
            id="pmt-pid-moved",
        ),
        # The same PID now carries another codec.
        pytest.param(
            lambda: _config(
                MuxerProgramConfigBuilder(1, 0x100).add_video(0x101, VideoCodec.H265).build()
            ),
            "changes its stream type or codec",
            id="codec-changed",
        ),
        # The PID moved to another program.
        pytest.param(
            lambda: _config(_video_program(1, 0x100, 0x111), _video_program(2, 0x200, 0x101)),
            "moves from program 1 to program 2",
            id="pid-moved-program",
        ),
        # The PCR moved to 0x102 while 0x101, which carried it, stays.
        pytest.param(
            lambda: _config(
                MuxerProgramConfigBuilder(1, 0x100)
                .add_video(0x101, VideoCodec.H264)
                .add_video(0x102, VideoCodec.H264)
                .pcr_pid(0x102)
                .build()
            ),
            "changes its PCR PID",
            id="pcr-moved-old-stays",
        ),
        # A muxer-wide setting differs from the running one.
        pytest.param(
            lambda: MuxerConfigBuilder()
            .add_program(_video_program(1, 0x100, 0x101))
            .pcr_interval_ms(20)
            .build(),
            "pcr_interval_ms changes",
            id="muxer-setting-changed",
        ),
    ],
)
def test_a_refused_config_raises_config_invalid_and_changes_nothing(refused, reason: str) -> None:
    start = _config(
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_video(0x102, VideoCodec.H264)
        .build()
    )
    m = Muxer(start)
    twin = Muxer(start)
    v = m.video_handles()[0]
    tv = twin.video_handles()[0]
    assert _push_frames(m, v, 90_000, 3) == _push_frames(twin, tv, 90_000, 3)
    bad = refused()  # valid on its own: only reconfigure refuses it
    handles = m.video_handles()
    with pytest.raises(MuxError) as ei:
        m.reconfigure(bad)
    assert ei.value.kind == MuxErrorKind.CONFIG_INVALID
    # The case is refused for its own reason, not another the start config hit.
    assert reason in str(ei.value)
    assert m.video_handles() == handles
    after = _push_frames(m, v, 99_000, 3)
    assert after == _push_frames(twin, tv, 99_000, 3)


def test_the_pcr_moves_when_its_stream_leaves_in_the_same_call() -> None:
    # 0x101 carries the PCR (the first video stream); removing it lets the
    # PCR move to 0x102, which stays.
    m = Muxer(
        _config(
            MuxerProgramConfigBuilder(1, 0x100)
            .add_video(0x101, VideoCodec.H264)
            .add_video(0x102, VideoCodec.H264)
            .build()
        )
    )
    first, second = m.video_handles()
    ts = _push_frames(m, first, 90_000, 2)
    m.reconfigure(_config(_video_program(1, 0x100, 0x102)))
    assert m.video_handles() == [second]
    ts += _push_frames(m, second, 96_000, 2)
    maps = [e for e in _events(ts) if isinstance(e, DemuxEvent.ProgramMap)]
    assert [p.pcr_pid for p in maps[0].programs] == [0x101]
    assert [p.pcr_pid for p in maps[-1].programs] == [0x102]
