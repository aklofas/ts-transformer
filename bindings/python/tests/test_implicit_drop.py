"""Implicit drops of the native-owning classes.

A class collected without `close()` / `stop()` / `teardown()` still runs its
native teardown: an SRT close waiting out the linger, a server drain, an
RTSP TEARDOWN (the RTSP client's thread is told to stop, not joined), the
WHEP server threads' joins. That
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
import socket
import struct
import subprocess
import sys
import textwrap
import threading
import time
from pathlib import Path
from typing import Callable, List, Optional, Tuple

import pytest

import tstrans

from _builders.ports import free_udp_port as _free_udp_port
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

    port = _free_udp_port()
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
    port = _free_udp_port()
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
    port = server.local_addr()[1]
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
        port = server.local_addr()[1]
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


_IDR_NAL = b"\x00\x00\x00\x01\x65\xbb"


def _wait_until(what: str, cond: Callable[[], bool], limit: float = 5.0) -> None:
    end = time.monotonic() + limit
    while not cond():
        if time.monotonic() > end:
            pytest.fail(f"timed out after {limit}s waiting for {what}")
        time.sleep(0.02)


class _Pusher:
    """Pushes an IDR access unit to `mount` every 20 ms until `stop()`."""

    def __init__(self, mount) -> None:  # type: ignore[no-untyped-def]
        from tstrans.mpegts import Pts90khz

        self._stop = threading.Event()

        def run() -> None:
            i = 0
            while not self._stop.is_set():
                mount.push_video(_IDR_NAL, pts=Pts90khz.from_raw(i * 3000), key_frame=True)
                i += 1
                self._stop.wait(0.02)

        self._thread = threading.Thread(target=run, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        self._thread.join(5)
        assert not self._thread.is_alive()


def _rtsp_server():  # type: ignore[no-untyped-def]
    from tstrans.rtp import RtspServer, RtspServerConfig

    server = RtspServer.start(
        RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    )
    mount = server.add_unicast_mount("/live", _single_video_program())
    return server, mount, server.local_addr()[1]


def _receiver_of_a_collected_session(port: int, transport: str):  # type: ignore[no-untyped-def]
    """`RtspClient.connect(cfg).into_demux_receiver()`: the session object is
    gone once this returns. Each `next()` gives up after 5 s with
    `RtpError(BACKPRESSURE)`, so a receiver that gets nothing fails the test
    instead of hanging it."""
    from tstrans.rtp import RtspClient, RtspClientConfig

    rx = RtspClient.connect(
        RtspClientConfig(
            url=f"rtsp://127.0.0.1:{port}/live?transport={transport}&recv_timeout=5000",
            keepalive=False,
        )
    ).into_demux_receiver()
    gc.collect()
    return rx


@pytest.mark.parametrize("transport", ["tcp", "udp"])
def test_a_receiver_outlives_its_collected_session(transport: str):
    """The receiver holds the session's client: media pushed after the
    session object was collected still arrives, and over TCP the receiver
    ends with end of stream (`StopIteration`) when the server stops; the
    client records a server's close of the control connection as
    `CLEAN_TEARDOWN`."""
    pytest.importorskip("tstrans.rtp", exc_type=ImportError)
    from tstrans.shells import EndReason

    server, mount, port = _rtsp_server()
    try:
        rx = _receiver_of_a_collected_session(port, transport)
        try:
            assert server.stats().active_sessions == 1
            pusher = _Pusher(mount)
            try:
                next(rx)
            finally:
                pusher.stop()
            assert rx.end_reason() is None
            if transport == "tcp":
                server.stop()
                for _ in rx:
                    pass
                assert rx.end_reason() == EndReason.CLEAN_TEARDOWN
        finally:
            rx.close()
    finally:
        server.stop()


@pytest.mark.parametrize("release", ["close", "collect"])
def test_gc_dropped_session_leaves_its_live_receiver_receiving(release: str):
    """Collecting the session object no longer ends its TCP-interleaved
    receiver; the receiver's own `close()`, or its collection, releases the
    client, and the server sees the session go."""
    pytest.importorskip("tstrans.rtp", exc_type=ImportError)

    server, mount, port = _rtsp_server()
    try:
        rx = _receiver_of_a_collected_session(port, "tcp")
        pusher = _Pusher(mount)
        try:
            next(rx)
            assert rx.end_reason() is None
            assert server.stats().active_sessions == 1
            if release == "close":
                rx.close()
            else:
                del rx
                gc.collect()
            _wait_until("the server to see the client go", lambda: server.stats().active_sessions == 0)
        finally:
            pusher.stop()
    finally:
        server.stop()


def test_closing_the_session_still_ends_its_receiver():
    """`close()` on the session ends the client and so the receiver (end of
    stream: the server accepted the TEARDOWN), although the receiver still
    holds the client's cell."""
    pytest.importorskip("tstrans.rtp", exc_type=ImportError)
    from tstrans.rtp import RtspClient, RtspClientConfig
    from tstrans.shells import EndReason

    server, mount, port = _rtsp_server()
    pusher = _Pusher(mount)
    try:
        session = RtspClient.connect(
            RtspClientConfig(
                url=f"rtsp://127.0.0.1:{port}/live?transport=tcp&recv_timeout=5000",
                keepalive=False,
            )
        )
        rx = session.into_demux_receiver()
        try:
            next(rx)
            session.close()
            for _ in rx:
                pass
            assert rx.end_reason() == EndReason.CLEAN_TEARDOWN
            _wait_until("the server to see the client go", lambda: server.stats().active_sessions == 0)
        finally:
            rx.close()
    finally:
        pusher.stop()
        server.stop()


class _H264Peer:
    """A scripted RTSP server for one `connect_h264` client over TCP: it
    answers OPTIONS / DESCRIBE / SETUP / PLAY, then writes one interleaved
    IDR access unit per `send_au()`, closes the connection on `hang_up()`,
    and sets `client_gone` once the client sends TEARDOWN or closes its
    side."""

    def __init__(self) -> None:
        from test_rtsp_client import _SDP_H264, _header

        self._sdp, self._header = _SDP_H264, _header
        self._listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._listener.bind(("127.0.0.1", 0))
        self._listener.listen(1)
        self._listener.settimeout(10)
        self.port = self._listener.getsockname()[1]
        self.playing = threading.Event()
        self.client_gone = threading.Event()
        self._au = threading.Event()
        self._hang_up = threading.Event()
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    def send_au(self) -> None:
        self._au.set()

    def hang_up(self) -> None:
        self._hang_up.set()

    def join(self) -> None:
        self._hang_up.set()
        self._thread.join(10)
        assert not self._thread.is_alive()

    def _reply(self, sock: socket.socket, req: str) -> None:
        method = req.split(None, 1)[0]
        extra, body = "", b""
        if method == "OPTIONS":
            extra = "Public: OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN\r\n"
        elif method == "DESCRIBE":
            body = self._sdp
            extra = f"Content-Type: application/sdp\r\nContent-Length: {len(body)}\r\n"
        elif method == "SETUP":
            extra = f"Session: DEADBEEF;timeout=60\r\nTransport: {self._header(req, 'transport')}\r\n"
        else:
            extra = "Session: DEADBEEF\r\n"
        cseq = self._header(req, "cseq") or "1"
        sock.sendall(f"RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n{extra}\r\n".encode() + body)

    def _serve(self) -> None:
        try:
            sock, _ = self._listener.accept()
        except OSError:
            return
        finally:
            self._listener.close()
        seq = 0
        with sock:
            # Short reads, so the loop also sees the test's requests.
            sock.settimeout(0.05)
            buf = b""
            while not self._hang_up.is_set():
                if self._au.is_set():
                    self._au.clear()
                    seq += 1
                    rtp = struct.pack("!BBHII", 0x80, 0x80 | 96, seq, seq * 3000, 9) + _IDR_NAL[4:]
                    sock.sendall(b"$\x00" + struct.pack("!H", len(rtp)) + rtp)
                try:
                    chunk = sock.recv(8192)
                except socket.timeout:
                    continue
                except OSError:
                    self.client_gone.set()
                    return
                if not chunk:
                    self.client_gone.set()
                    return
                buf += chunk
                while b"\r\n\r\n" in buf:
                    head, buf = buf.split(b"\r\n\r\n", 1)
                    req = head.decode("latin-1")
                    if req.startswith("TEARDOWN"):
                        self.client_gone.set()
                    self._reply(sock, req)
                    if req.startswith("PLAY"):
                        self.playing.set()


def _h264_receiver_of_a_collected_session(peer: _H264Peer):  # type: ignore[no-untyped-def]
    from tstrans.rtp import RtspClient, RtspClientConfig, TransportPref

    rx = RtspClient.connect_h264(
        RtspClientConfig(
            url=f"rtsp://127.0.0.1:{peer.port}/x",
            transport_pref=TransportPref.TCP,
            keepalive=False,
        )
    ).into_h264_receiver()
    gc.collect()
    assert peer.playing.wait(5.0), "the peer never answered PLAY"
    return rx


def test_an_h264_receiver_outlives_its_collected_session():
    """An access unit the server writes after the session object was
    collected still arrives; the receiver ends with end of stream (`None`)
    when the server closes the connection, which the client records as
    `CLEAN_TEARDOWN`."""
    pytest.importorskip("tstrans.rtp", exc_type=ImportError)
    from tstrans.shells import EndReason

    peer = _H264Peer()
    try:
        rx = _h264_receiver_of_a_collected_session(peer)
        try:
            assert not peer.client_gone.is_set()
            peer.send_au()
            au = rx.recv_au(timeout_ms=5000)
            assert au is not None and au.key_frame
            peer.hang_up()
            assert rx.recv_au(timeout_ms=5000) is None
            assert rx.end_reason() == EndReason.CLEAN_TEARDOWN
        finally:
            rx.close()
    finally:
        peer.join()


@pytest.mark.parametrize("release", ["close", "collect"])
def test_releasing_an_h264_receiver_releases_its_client(release: str):
    """With the session object gone, the receiver's `close()` or its
    collection ends the client: the server sees TEARDOWN or the
    connection's end."""
    pytest.importorskip("tstrans.rtp", exc_type=ImportError)

    peer = _H264Peer()
    try:
        rx = _h264_receiver_of_a_collected_session(peer)
        peer.send_au()
        assert rx.recv_au(timeout_ms=5000) is not None
        assert not peer.client_gone.is_set()
        if release == "close":
            rx.close()
        else:
            del rx
            gc.collect()
        assert peer.client_gone.wait(5.0), "the client outlived its receiver"
    finally:
        peer.join()


# --------------------------------------------------------------------------- #
# webrtc.WhepServer                                                           #
# --------------------------------------------------------------------------- #


def test_gc_dropped_whep_server_releases_its_port():
    pytest.importorskip("tstrans.webrtc", exc_type=ImportError)
    from tstrans.webrtc import WhepServer

    s = WhepServer.bind("whep://127.0.0.1:0")
    host, port = s.local_addr()
    del s
    gc.collect()
    again = _retry(lambda: WhepServer.bind(f"whep://{host}:{port}"))
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
