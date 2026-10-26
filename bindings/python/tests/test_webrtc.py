"""tstrans.webrtc — the WHEP playback server.

Covers bind / add_mount / push / stats / close semantics and the error
model. A real WebRTC viewer is the Rust test viewer of tst-webrtc's own
loopback tests, reached through the `py-test-hooks` probe: a WHEP client
written in Python would be a second implementation.
"""

from __future__ import annotations

import http.client
import os
import threading
import time
from pathlib import Path

import pytest

from tstrans import _native
from tstrans.exceptions import WebrtcError, WebrtcErrorKind
from tstrans.klv import UasDatalinkLs, encode_uas_datalink
from tstrans.mpegts import Pts90khz, VideoCodec
from tstrans.webrtc import (
    AudioPolicy,
    KlvCarriage,
    MountConfig,
    MountHandle,
    MountKind,
    MountStats,
    ServerStats,
    SessionStats,
    WhepServer,
)

# A minimal Annex-B access unit: SPS, PPS and an IDR slice header.
_TINY_IDR_AU = bytes(
    [
        0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1F, 0xE9, 0x40,
        0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80,
        0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33,
    ]
)

_GOP = (
    Path(__file__).resolve().parents[3]
    / "crates"
    / "tst-webrtc"
    / "tests"
    / "fixtures"
    / "h264"
    / "gop.h264"
)


def _gop_access_units() -> list[bytes]:
    """The 30-frame H.264 GOP of tst-webrtc's fixtures, split at each AUD."""
    data = _GOP.read_bytes()
    starts = [
        i
        for i in range(len(data) - 4)
        if data[i : i + 4] == b"\x00\x00\x00\x01" and data[i + 4] & 0x1F == 9
    ]
    return [data[s:e] for s, e in zip(starts, starts[1:] + [len(data)])]


def _klv_record(heading: float) -> bytes:
    return encode_uas_datalink(UasDatalinkLs(platform_heading_deg=heading))


@pytest.fixture
def server():
    s = WhepServer.bind("whep://127.0.0.1:0")
    yield s
    s.close()


# ---------------------------------------------------------------------------
# Bind, mounts, pushes, stats
# ---------------------------------------------------------------------------


def test_bind_reports_kernel_picked_addresses(server):
    addr = server.local_addr()
    host, port = addr.rsplit(":", 1)
    assert host == "127.0.0.1"
    assert int(port) > 0
    media_host, media_port = server.media_addr().rsplit(":", 1)
    assert media_host == "127.0.0.1"
    assert int(media_port) > 0


def test_preflight_does_not_advertise_patch(server):
    # Neither trickle ICE nor ICE restart is supported; a client that sees
    # PATCH in the method list switches to trickle and then needs an ETag.
    conn = http.client.HTTPConnection(*server.local_addr().rsplit(":", 1))
    try:
        conn.request("OPTIONS", "/cam")
        resp = conn.getresponse()
        assert resp.status == 204
        assert resp.getheader("Access-Control-Allow-Methods") == "POST, DELETE, OPTIONS"
    finally:
        conn.close()


def test_add_mount_push_and_stats(server):
    stats = server.stats()
    assert isinstance(stats, ServerStats)
    assert stats.mounts == 0

    m = server.add_mount("/cam")
    assert isinstance(m, MountHandle)
    assert m.mount_path() == "/cam"
    m.push_video(_TINY_IDR_AU, pts=Pts90khz(0), key_frame=True)
    m.push_video(bytearray(_TINY_IDR_AU), pts=Pts90khz(3000), key_frame=True)
    m.push_video(memoryview(_TINY_IDR_AU), pts=Pts90khz(6000), key_frame=True)
    m.push_klv(_klv_record(10.0), pts=Pts90khz(6000))
    m.push_klv(_klv_record(20.0), pts=Pts90khz(6000), metadata_service_id=255)

    assert server.stats().mounts == 1
    ms = m.stats()
    assert isinstance(ms, MountStats)
    assert ms.frames_pushed == 3
    assert ms.keyframes_pushed == 3
    assert ms.keyframe_flag_mismatch == 0
    assert ms.klv_records_pushed == 2
    assert ms.viewers == 0
    assert ms.audio_frames_pushed == 0
    assert m.sessions() == []

    server.remove_mount("/cam")
    assert server.stats().mounts == 0


def test_add_mount_with_explicit_config(server):
    cfg = MountConfig(
        kind=MountKind.ELEMENTARY,
        video=VideoCodec.H265,
        audio=AudioPolicy.PASSTHROUGH_OPUS,
        klv=KlvCarriage.DATA_CHANNEL_ONLY,
        sei_budget_bytes=1024,
        viewer_queue_packets=512,
    )
    m = server.add_mount("/h265", cfg)
    assert m.mount_path() == "/h265"
    m.push_audio(b"\xfc\xff\xfe", pts=Pts90khz(0))
    assert m.stats().audio_frames_pushed == 1
    mp2t = server.add_mount("/ts", MountConfig(kind=MountKind.MP2T))
    mp2t.push_ts(b"\x47\x1f\xff\x10" + b"\xff" * 184)
    assert server.stats().mounts == 2


def test_mount_config_defaults_mirror_rust():
    d = MountConfig()
    assert d.kind is MountKind.ELEMENTARY
    assert d.video is VideoCodec.H264
    assert d.audio is AudioPolicy.DROP
    assert d.klv is KlvCarriage.SEI_AND_DATA_CHANNEL
    assert d.sei_budget_bytes == 64 * 1024
    assert d.viewer_queue_packets == 2048
    assert [m.name for m in MountKind] == ["ELEMENTARY", "MP2T"]
    assert [m.name for m in AudioPolicy] == ["DROP", "PASSTHROUGH_OPUS"]
    assert [m.name for m in KlvCarriage] == [
        "SEI_AND_DATA_CHANNEL",
        "SEI_ONLY",
        "DATA_CHANNEL_ONLY",
    ]


def test_stats_records_carry_every_counter(server):
    m = server.add_mount("/cam")
    assert len(_fields(m.stats())) == 20
    assert len(_fields(server.stats())) == 11
    assert "MountStats(" in repr(m.stats())
    assert "ServerStats(" in repr(server.stats())


def _fields(record: object) -> list[str]:
    return [n for n in dir(record) if not n.startswith("_")]


def test_session_stats_class_is_exported():
    assert SessionStats.__module__ == "tstrans.webrtc"


def test_repr_names_the_addresses_and_path(server):
    m = server.add_mount("/cam")
    assert server.local_addr() in repr(server)
    assert "/cam" in repr(m)


# ---------------------------------------------------------------------------
# Close semantics
# ---------------------------------------------------------------------------


def test_stop_is_idempotent_and_close_after_stop_is_fine():
    s = WhepServer.bind("whep://127.0.0.1:0")
    s.stop()
    s.stop()
    s.close()
    s.close()


def test_push_after_stop_is_closed(server):
    m = server.add_mount("/cam")
    server.stop()
    with pytest.raises(WebrtcError) as exc:
        m.push_video(_TINY_IDR_AU, pts=Pts90khz(0), key_frame=True)
    assert exc.value.kind == WebrtcErrorKind.CLOSED


def test_use_after_close_raises_closed():
    s = WhepServer.bind("whep://127.0.0.1:0")
    m = s.add_mount("/cam")
    s.close()
    calls = [
        s.stats,
        s.local_addr,
        s.media_addr,
        s.stop,
        lambda: s.add_mount("/other"),
        lambda: s.remove_mount("/cam"),
    ]
    for call in calls:
        with pytest.raises(WebrtcError) as exc:
            call()
        assert exc.value.kind == WebrtcErrorKind.CLOSED
    m.close()
    m.close()
    mount_calls = [
        m.stats,
        m.sessions,
        m.mount_path,
        lambda: m.push_video(_TINY_IDR_AU, pts=Pts90khz(0), key_frame=True),
        lambda: m.push_klv(_klv_record(1.0), pts=Pts90khz(0)),
        lambda: m.push_audio(b"\x00", pts=Pts90khz(0)),
        lambda: m.push_ts(b"\x47" + b"\xff" * 187),
    ]
    for call in mount_calls:
        with pytest.raises(WebrtcError) as exc:
            call()
        assert exc.value.kind == WebrtcErrorKind.CLOSED
    assert "closed" in repr(s)
    assert "closed" in repr(m)


def test_dropping_the_server_stops_its_mounts():
    # A mount handle does not keep its server alive: the unreferenced
    # server is collected at once and stops.
    m = WhepServer.bind("whep://127.0.0.1:0").add_mount("/cam")
    with pytest.raises(WebrtcError) as exc:
        m.push_video(_TINY_IDR_AU, pts=Pts90khz(0), key_frame=True)
    assert exc.value.kind == WebrtcErrorKind.CLOSED
    assert m.stats().frames_pushed == 0


def test_context_manager_closes():
    with WhepServer.bind("whep://127.0.0.1:0") as s:
        s.add_mount("/cam")
        assert s.stats().mounts == 1
    with pytest.raises(WebrtcError) as exc:
        s.stats()
    assert exc.value.kind == WebrtcErrorKind.CLOSED


# ---------------------------------------------------------------------------
# Error model
# ---------------------------------------------------------------------------


def _kind_of(call) -> WebrtcErrorKind:
    with pytest.raises(WebrtcError) as exc:
        call()
    return exc.value.kind


def test_hostname_bind_is_url():
    assert _kind_of(lambda: WhepServer.bind("whep://localhost:1")) == WebrtcErrorKind.URL


def test_refused_bind_is_io(server):
    taken = server.local_addr()
    assert _kind_of(lambda: WhepServer.bind(f"whep://{taken}")) == WebrtcErrorKind.IO


def test_mount_errors(server):
    assert _kind_of(lambda: server.add_mount("nope")) == WebrtcErrorKind.MOUNT
    server.add_mount("/cam")
    assert _kind_of(lambda: server.add_mount("/cam")) == WebrtcErrorKind.MOUNT
    server.remove_mount("/cam")
    assert _kind_of(lambda: server.remove_mount("/cam")) == WebrtcErrorKind.MOUNT


@pytest.mark.parametrize(
    "cfg",
    [
        MountConfig(sei_budget_bytes=0),
        MountConfig(kind=MountKind.MP2T, audio=AudioPolicy.PASSTHROUGH_OPUS),
        MountConfig(video=VideoCodec.AV1),
        MountConfig(viewer_queue_packets=1),
    ],
)
def test_invalid_mount_config_is_mount(server, cfg):
    assert _kind_of(lambda: server.add_mount("/cam", cfg)) == WebrtcErrorKind.MOUNT
    assert server.stats().mounts == 0


def test_out_of_range_counts_are_mount(server):
    cfgs = [
        MountConfig(sei_budget_bytes=-1),
        MountConfig(viewer_queue_packets=2**32),
        MountConfig(sei_budget_bytes=2**70),
        MountConfig(viewer_queue_packets=-(2**70)),
        MountConfig(kind=2**70),
        MountConfig(audio=-(2**70)),
        MountConfig(klv=2**70),
    ]
    kinds = [_kind_of(lambda: server.add_mount("/cam", cfg)) for cfg in cfgs]
    assert kinds == [WebrtcErrorKind.MOUNT] * 7
    assert server.stats().mounts == 0


def test_push_errors(server):
    m = server.add_mount("/cam")
    assert _kind_of(lambda: m.push_ts(b"\x47" + b"\xff" * 187)) == WebrtcErrorKind.PUSH
    assert (
        _kind_of(lambda: m.push_video(b"", pts=Pts90khz(0), key_frame=True))
        == WebrtcErrorKind.PUSH
    )
    with pytest.raises(ValueError):
        m.push_klv(_klv_record(1.0), pts=Pts90khz(0), metadata_service_id=256)
    for bad in (-1, 2**70, -(2**70)):
        with pytest.raises(ValueError):
            m.push_klv(_klv_record(1.0), pts=Pts90khz(0), metadata_service_id=bad)
    with pytest.raises(TypeError):
        m.push_klv(_klv_record(1.0), pts=Pts90khz(0), metadata_service_id="0")
    oversize = b"\x00" * (256 * 1024 + 1)
    assert _kind_of(lambda: m.push_klv(oversize, pts=Pts90khz(0))) == WebrtcErrorKind.PUSH
    assert m.stats().klv_records_pushed == 0
    assert m.stats().klv_dc_dropped_oversize == 1
    mp2t = server.add_mount("/ts", MountConfig(kind=MountKind.MP2T))
    assert (
        _kind_of(lambda: mp2t.push_video(_TINY_IDR_AU, pts=Pts90khz(0), key_frame=True))
        == WebrtcErrorKind.PUSH
    )


def test_raise_for_test_reaches_every_kind():
    for member in WebrtcErrorKind:
        with pytest.raises(WebrtcError) as exc:
            _native._raise_webrtc_error_for_test(member.name, f"test {member.name}")
        assert exc.value.kind == member
        assert f"test {member.name}" in str(exc.value)
    assert len(WebrtcErrorKind) == 8


# ---------------------------------------------------------------------------
# A real viewer (py-test-hooks probe)
# ---------------------------------------------------------------------------


def _probe():
    probe = getattr(_native.webrtc, "_loopback_viewer_probe", None)
    if probe is None:
        if os.environ.get("TSTRANS_EXPECT_TEST_HOOKS") == "1":
            pytest.fail(
                "TSTRANS_EXPECT_TEST_HOOKS=1 but the extension was built without "
                "the py-test-hooks feature"
            )
        pytest.skip("the extension was built without the py-test-hooks feature")
    return probe


def test_a_viewer_receives_frames_pushed_from_python(server):
    probe = _probe()
    aus = _gop_access_units()
    assert len(aus) == 30
    m = server.add_mount("/cam")
    done = threading.Event()
    failures: list[BaseException] = []

    # A viewer joins at a keyframe, so the pusher loops the GOP until the
    # probe returns; a viewer that connects late still gets the next IDR.
    def push() -> None:
        pts = 0
        try:
            while not done.is_set():
                for i, au in enumerate(aus):
                    if done.is_set():
                        break
                    m.push_video(au, pts=Pts90khz(pts), key_frame=i == 0)
                    pts += 3000
                    time.sleep(0.033)
        except BaseException as e:  # noqa: BLE001 - reported below
            failures.append(e)

    pusher = threading.Thread(target=push, name="whep-pusher", daemon=True)
    pusher.start()
    try:
        frames = probe(server, "/cam")
    finally:
        done.set()
        pusher.join(5)
    assert failures == []
    assert frames > 0
    assert m.stats().frames_pushed > 0
    assert server.stats().rtp_packets_sent > 0
    assert server.stats().sessions_total == 1
