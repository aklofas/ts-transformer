"""Implicit drops of the native-owning classes.

A class collected without `close()` / `stop()` / `teardown()` still runs its
native teardown: an SRT close waiting out the linger, a server drain, an
RTSP TEARDOWN with thread joins, the WHEP server threads' joins. That
teardown runs with the GIL released, so other threads keep running while
the collecting thread waits. An `hls.MuxPublisher` collected unfinished is
abortive: bytes a failed push retained are discarded.

Each test pins one of: the GIL is released during the drop; the drop
still frees the resource (a port, a session, a libsrt handle); the drop
of an already-closed or already-consumed object does nothing.
"""

from __future__ import annotations

import gc
import os
import subprocess
import sys
import textwrap
import threading
import time
from pathlib import Path
from typing import Callable, List, Optional, Tuple

import pytest

import tstrans

from _builders.ports import free_tcp_port as _free_tcp_port
from _builders.native_hooks import shells_hook as _hook

PKG_PARENT = str(Path(tstrans.__file__).resolve().parent.parent)


def _retry(call: Callable[[], object], budget_s: float = 2.0) -> object:
    """Retry `call` until it stops raising, up to `budget_s`."""
    deadline = time.monotonic() + budget_s
    while True:
        try:
            return call()
        except Exception:  # noqa: BLE001
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)


# --------------------------------------------------------------------------- #
# The GIL is released during an implicit drop                                 #
# --------------------------------------------------------------------------- #


def test_implicit_drop_releases_the_gil():
    obj = _hook("_slow_drop_for_test")(300)
    started, stop = threading.Event(), threading.Event()
    ticks: List[float] = []

    def heartbeat():
        started.set()
        while not stop.is_set():
            ticks.append(time.perf_counter())
            time.sleep(0.005)

    t = threading.Thread(target=heartbeat, daemon=True)
    t.start()
    started.wait()
    begin = time.perf_counter()
    del obj  # the native drop sleeps 300 ms
    gc.collect()
    end = time.perf_counter()
    stop.set()
    t.join(5.0)
    assert end - begin >= 0.25, "the hook's drop did not run in the collector"
    inside = [x for x in ticks if begin + 0.05 < x < end - 0.05]
    assert inside, "the heartbeat never ran: the implicit drop held the GIL"


# --------------------------------------------------------------------------- #
# srt.Socket                                                                  #
# --------------------------------------------------------------------------- #


def _srt_pair() -> Tuple[object, object, object]:
    """(listener, accepted, caller) over loopback. The accepted socket
    inherits the listener's 5 s receive timeout."""
    import tstrans.srt

    port = _free_tcp_port()
    lst = tstrans.srt.Builder(
        f"srt://127.0.0.1:{port}?mode=listener", recv_timeout_ms=5000
    ).listen()
    box: List[Optional[object]] = [None]
    err: List[BaseException] = []

    def accept_worker() -> None:
        try:
            box[0] = lst.accept(timeout_ms=5000)
        except BaseException as exc:  # noqa: BLE001
            err.append(exc)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)
    caller = tstrans.srt.Builder(f"srt://127.0.0.1:{port}").connect()
    t.join(timeout=5.0)
    assert not err, err
    assert box[0] is not None
    return lst, box[0], caller


def test_gc_dropped_socket_closes_the_libsrt_handle():
    pytest.importorskip("tstrans.srt", exc_type=ImportError)
    from tstrans.exceptions import SrtError, SrtErrorKind

    lst, peer, caller = _srt_pair()
    try:
        del caller
        gc.collect()
        receiver = peer.into_receiver()
        try:
            # The peer sees the close, not the 5 s TIMEOUT.
            with pytest.raises(SrtError) as ei:
                receiver.recv_bytes()
            assert ei.value.kind in (SrtErrorKind.BROKEN, SrtErrorKind.CLOSED), ei.value.kind
        finally:
            receiver.close()
    finally:
        lst.close()


def test_gc_after_into_sender_closes_nothing():
    pytest.importorskip("tstrans.srt", exc_type=ImportError)
    lst, peer, caller = _srt_pair()
    try:
        sender = caller.into_sender()
        del caller
        gc.collect()
        sender.send_bytes(b"\x47" + b"\x00" * 187)
        sender.close()
    finally:
        peer.close()
        lst.close()


def test_exit_with_an_unclosed_socket_exits_cleanly():
    pytest.importorskip("tstrans.srt", exc_type=ImportError)
    port = _free_tcp_port()
    script = f"""
        import threading, time
        import tstrans.srt

        lst = tstrans.srt.Builder("srt://127.0.0.1:{port}?mode=listener").listen()
        box = []
        t = threading.Thread(target=lambda: box.append(lst.accept(timeout_ms=5000)), daemon=True)
        t.start()
        time.sleep(0.1)
        caller = tstrans.srt.Builder("srt://127.0.0.1:{port}").connect()
        t.join(5.0)
        assert box, "accept did not return"
        accepted = box[0]
        # listener, caller and accepted are all left unclosed.
    """
    try:
        r = subprocess.run(
            [sys.executable, "-c", textwrap.dedent(script)],
            capture_output=True,
            text=True,
            timeout=30.0,
            env={**os.environ, "PYTHONPATH": PKG_PARENT},
        )
    except subprocess.TimeoutExpired:
        pytest.fail("interpreter did not exit within 30s with unclosed SRT sockets")
    assert r.returncode == 0, (r.returncode, r.stderr)


# --------------------------------------------------------------------------- #
# rtp.RtspServer / rtp.RtspSession                                            #
# --------------------------------------------------------------------------- #


def _single_video_program():
    from tstrans.mpegts import MuxerProgramConfigBuilder, VideoCodec

    return MuxerProgramConfigBuilder(1, 0x100).add_video(0x101, VideoCodec.H264).build()


def test_gc_dropped_rtsp_server_releases_its_port():
    pytest.importorskip("tstrans.rtp", exc_type=ImportError)
    from tstrans.rtp import RtspServer, RtspServerConfig

    server = RtspServer.start(
        RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    )
    port = server.local_addr().rsplit(":", 1)[1]
    del server
    gc.collect()
    again = _retry(
        lambda: RtspServer.start(
            RtspServerConfig(bind_addr=f"127.0.0.1:{port}", graceful_shutdown_drain_ms=50)
        )
    )
    again.stop()


def test_gc_after_stop_is_immediate():
    pytest.importorskip("tstrans.rtp", exc_type=ImportError)
    from tstrans.rtp import RtspServer, RtspServerConfig

    server = RtspServer.start(
        RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    )
    server.stop()
    begin = time.perf_counter()
    del server
    gc.collect()
    end = time.perf_counter()
    assert end - begin < 1.0, f"the drop of a stopped server took {end - begin:.3f}s"


def test_gc_dropped_rtsp_session_tears_down():
    pytest.importorskip("tstrans.rtp", exc_type=ImportError)
    from tstrans.rtp import RtspClient, RtspClientConfig, RtspServer, RtspServerConfig

    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        server.add_unicast_mount("/live", _single_video_program())
        port = server.local_addr().rsplit(":", 1)[1]
        session = RtspClient.connect(
            RtspClientConfig(url=f"rtsp://127.0.0.1:{port}/live", keepalive=False)
        )
        assert server.stats().active_sessions == 1
        del session
        gc.collect()
        deadline = time.monotonic() + 2.0
        while server.stats().active_sessions != 0 and time.monotonic() < deadline:
            time.sleep(0.02)
        assert server.stats().active_sessions == 0


# --------------------------------------------------------------------------- #
# webrtc.WhepServer                                                           #
# --------------------------------------------------------------------------- #


def test_gc_dropped_whep_server_releases_its_port():
    pytest.importorskip("tstrans.webrtc", exc_type=ImportError)
    from tstrans.webrtc import WhepServer

    s = WhepServer.bind("whep://127.0.0.1:0")
    addr = s.local_addr()
    del s
    gc.collect()
    again = _retry(lambda: WhepServer.bind(f"whep://{addr}"))
    again.close()


# --------------------------------------------------------------------------- #
# hls.MuxPublisher                                                            #
# --------------------------------------------------------------------------- #

NAL_IDR = b"\x00\x00\x00\x01\x65\xBB"


def _segment_sizes(d: Path) -> dict:
    return {p.name: p.stat().st_size for p in d.iterdir()}


def test_gc_dropped_unfinished_mux_publisher_is_abortive(tmp_path):
    pytest.importorskip("tstrans.hls", exc_type=ImportError)
    # POSIX only: a read-only directory refuses the segment open.
    if sys.platform == "win32" or os.geteuid() == 0:
        pytest.skip("needs a directory the process cannot write")
    from tstrans.exceptions import HlsError
    from tstrans.hls import HlsPublisher, MuxPublisher
    from tstrans.mpegts import Pts90khz

    pub = HlsPublisher.builder().bind("127.0.0.1:0").output_dir(str(tmp_path)).build()
    mp = MuxPublisher.with_config_hls(pub, _single_video_program())
    os.chmod(tmp_path, 0o500)
    try:
        # The segment open fails; the shell retains the chunk.
        with pytest.raises(HlsError):
            mp.push_video(NAL_IDR, pts=Pts90khz.from_raw(0), key_frame=True)
    finally:
        os.chmod(tmp_path, 0o700)
    assert not list(tmp_path.glob("*.ts"))
    del mp
    gc.collect()
    assert not list(tmp_path.glob("*.ts")), "the implicit drop re-offered the retained segment bytes"


def test_gc_after_finish_into_publisher_touches_nothing(tmp_path):
    pytest.importorskip("tstrans.hls", exc_type=ImportError)
    from tstrans.hls import HlsPublisher, MuxPublisher
    from tstrans.mpegts import Pts90khz

    pub = HlsPublisher.builder().bind("127.0.0.1:0").output_dir(str(tmp_path)).build()
    mp = MuxPublisher.with_config_hls(pub, _single_video_program())
    for i in range(3):
        mp.push_video(NAL_IDR, pts=Pts90khz.from_raw(i * 360_000), key_frame=True)
    recovered = mp.finish_into_publisher()
    recovered.finish()
    before = _segment_sizes(tmp_path)
    assert any(name.endswith(".ts") for name in before)
    del mp
    gc.collect()
    assert _segment_sizes(tmp_path) == before
