"""tstrans.rtp — the RTSP publisher role (ANNOUNCE / RECORD ingest).

Covers:
- `RtspServerConfig.accept_unregistered_publishers` default + type check.
- `RtspServer.add_publish_mount` / `PublishMount` getters before any publisher.
- A raw-socket MP2T publisher (ANNOUNCE, SETUP `mode=record` over
  TCP-interleaved, RECORD) feeding null-packet bundles → `PublishMount.stats()`,
  `publisher()`, and the `ServerStats` publisher counters.
- Muxer-built bundles → a `DemuxEvent.Video` out of `into_demux_receiver()`.
- Take-once: a second `into_demux_receiver()` → `RtspError(CLOSED)`.
- `remove_mount`: the parked receiver ends (`StopIteration`, the
  end-of-stream shape); a repeat → `RtspError(MOUNT)`.
- `PublishMount.cancel()` → the receiver's next read raises `RtpError(CLOSED)`.
- On-demand mounts through `next_publisher()`; `None` on timeout.
- The context-manager exit wakes a parked `next_publisher` (`RtspError(SERVER)`)
  and ends a parked publish-mount receiver.
- `next_publisher` releases the GIL while it waits (structural probe).

The publisher is a plain `socket` speaking RTSP by hand: the library's
RTSP client only plays. Every socket read has a timeout and every parked
call is woken from a side thread or a watchdog with a bounded join, so a
regression fails instead of hanging the suite.
"""

from __future__ import annotations

import _thread
import socket
import struct
import sys
import threading
import time
from time import perf_counter
from typing import Any, Callable, Optional

import pytest

from tstrans.exceptions import RtpError, RtpErrorKind, RtspError, RtspErrorKind
from tstrans.mpegts import (
    DemuxEvent,
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)
from tstrans.rtp import (
    ClockAlignment,
    DemuxReceiver,
    PublisherInfo,
    PublishMount,
    PublishMountStats,
    PublishShape,
    RtspServer,
    RtspServerConfig,
    StreamEndReason,
)

SDP_MP2T = (
    "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=publish\r\nc=IN IP4 127.0.0.1\r\n"
    "t=0 0\r\nm=video 0 RTP/AVP 33\r\na=rtpmap:33 MP2T/90000\r\n"
    "a=control:streamid=0\r\n"
)

# A valid 7-packet bundle of MPEG-TS null packets (PID 0x1FFF): the demuxer
# accepts it and emits no events.
NULL_BUNDLE = (bytes([0x47, 0x1F, 0xFF, 0x10]) + b"\xff" * 184) * 7

# Bounded joins for side threads. Generous: they only ever expire on a
# regression.
JOIN_S = 10.0


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _start(accept_unregistered: bool = False) -> RtspServer:
    cfg = RtspServerConfig(
        bind_addr="127.0.0.1:0",
        graceful_shutdown_drain_ms=50,
        accept_unregistered_publishers=accept_unregistered,
    )
    return RtspServer.start(cfg)


def _port(server: RtspServer) -> int:
    addr = server.local_addr()
    assert addr is not None
    return int(addr.rsplit(":", 1)[1])


def _wait_for(cond: Callable[[], bool], limit: float = 5.0) -> bool:
    """Poll `cond` every 20 ms until it holds or `limit` seconds pass."""
    deadline = time.monotonic() + limit
    while time.monotonic() < deadline:
        if cond():
            return True
        time.sleep(0.02)
    return cond()


class RawPublisher:
    """A minimal MP2T publisher: ANNOUNCE, SETUP `mode=record` over
    TCP-interleaved, RECORD. Holds the mount for as long as the control
    connection stays open."""

    def __init__(self, port: int, path: str) -> None:
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=5.0)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.uri = f"rtsp://127.0.0.1:{port}{path}"
        self.seq = 0
        r = self._request(
            f"ANNOUNCE {self.uri} RTSP/1.0\r\nCSeq: 1\r\n"
            f"Content-Type: application/sdp\r\nContent-Length: {len(SDP_MP2T)}\r\n\r\n"
            f"{SDP_MP2T}"
        )
        assert _status(r) == 200, f"ANNOUNCE refused: {r}"
        r = self._request(
            f"SETUP {self.uri}/streamid=0 RTSP/1.0\r\nCSeq: 2\r\n"
            "Transport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\n\r\n"
        )
        assert _status(r) == 200, f"SETUP refused: {r}"
        session = _header(r, "Session").split(";")[0].strip()
        transport = _header(r, "Transport")
        self.channel = int(
            next(
                p.strip()[len("interleaved="):].split("-")[0]
                for p in transport.split(";")
                if p.strip().startswith("interleaved=")
            )
        )
        r = self._request(
            f"RECORD {self.uri} RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\n\r\n"
        )
        assert _status(r) == 200, f"RECORD refused: {r}"

    def _request(self, req: str) -> str:
        self.sock.sendall(req.encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = self.sock.recv(1024)
            assert chunk, f"server closed before answering {req!r}"
            buf += chunk
        return buf.decode(errors="replace")

    def send_rtp(self, ts_bundle: bytes) -> None:
        """One RTP packet (PT 33) in one RFC 2326 §10.12 interleaved frame."""
        rtp = struct.pack("!BBHII", 0x80, 33, self.seq & 0xFFFF, self.seq * 3003, 0x12345678)
        self.seq += 1
        payload = rtp + ts_bundle
        self.sock.sendall(b"$" + bytes([self.channel]) + struct.pack("!H", len(payload)) + payload)

    def local_addr(self) -> str:
        host, port = self.sock.getsockname()[:2]
        return f"{host}:{port}"

    def close(self) -> None:
        self.sock.close()


def _status(resp: str) -> int:
    return int(resp.split()[1])


def _header(resp: str, name: str) -> str:
    for line in resp.split("\r\n"):
        k, sep, v = line.partition(":")
        if sep and k.strip().lower() == name.lower():
            return v.strip()
    raise AssertionError(f"no {name} header in {resp!r}")


def _muxed_bundles(n: int) -> list[bytes]:
    """`n` bundles of up to 7 TS packets from a real muxer fed synthetic
    H.264 access units, so the demuxer sees PAT, PMT and video."""
    prog = MuxerProgramConfigBuilder(1, 0x1000).add_video(0x1011, VideoCodec.H264).build()
    mux = Muxer(MuxerConfigBuilder().add_program(prog).build())
    buf = bytearray(7 * 188)
    bundles: list[bytes] = []
    i = 0
    while len(bundles) < n:
        au = bytes([0, 0, 0, 1, 0x65 if i == 0 else 0x41]) + bytes(
            ((k + i) & 0xFF) | 1 for k in range(295)
        )
        mux.push_video(au, pts=Pts90khz.from_raw(i * 3003), key_frame=(i == 0))
        while len(bundles) < n:
            got = mux.pull(buf)
            if got == 0:
                break
            bundles.append(bytes(buf[:got]))
        i += 1
    return bundles


def _next_in_thread(rx: DemuxReceiver) -> tuple[threading.Thread, dict[str, Any]]:
    """Park `next(rx)` on a side thread; the dict receives `value` or `exc`."""
    out: dict[str, Any] = {}
    entered = threading.Event()

    def run() -> None:
        entered.set()
        try:
            out["value"] = next(rx)
        except BaseException as e:  # noqa: BLE001 — StopIteration included
            out["exc"] = e

    t = threading.Thread(target=run, daemon=True)
    t.start()
    entered.wait()
    # Give the thread time to reach the native wait. Not an assertion: an
    # early wake-up path reaches the same outcome.
    time.sleep(0.2)
    return t, out


# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------


def test_config_accept_unregistered_publishers_defaults_off():
    assert RtspServerConfig().accept_unregistered_publishers is False
    assert RtspServerConfig(accept_unregistered_publishers=True).accept_unregistered_publishers


def test_config_accept_unregistered_publishers_rejects_non_bool():
    with pytest.raises(TypeError, match="accept_unregistered_publishers"):
        RtspServerConfig(accept_unregistered_publishers=1)  # type: ignore[arg-type]


# ---------------------------------------------------------------------------
# Mount lifecycle without a publisher
# ---------------------------------------------------------------------------


def test_add_publish_mount_getters_before_any_publisher():
    with _start() as server:
        mount = server.add_publish_mount("/cam")
        assert isinstance(mount, PublishMount)
        assert mount.mount_path() == "/cam"
        assert mount.peer_count() == 0
        assert mount.generation() == 0
        assert mount.publisher() is None
        s = mount.stats()
        assert isinstance(s, PublishMountStats)
        assert s.rtp_packets_received == 0
        assert s.alignment is ClockAlignment.NOT_APPLICABLE
        assert "PublishMountStats(" in repr(s)
        assert "/cam" in repr(mount)
        assert server.stats().mounts == 1
        assert server.stats().active_publishers == 0


def test_add_publish_mount_duplicate_and_invalid_paths_are_mount_kind():
    with _start() as server:
        server.add_publish_mount("/cam")
        with pytest.raises(RtspError) as ei:
            server.add_publish_mount("/cam")
        assert ei.value.kind == RtspErrorKind.MOUNT
        with pytest.raises(RtspError) as ei:
            server.add_publish_mount("no-slash")
        assert ei.value.kind == RtspErrorKind.MOUNT


def test_next_publisher_returns_none_on_timeout():
    with _start() as server:
        assert server.next_publisher(0.1) is None
        assert server.next_publisher(0) is None
        with pytest.raises(ValueError, match="timeout"):
            server.next_publisher(-1.0)


def test_remove_mount_twice_is_mount_kind():
    with _start() as server:
        mount = server.add_publish_mount("/cam")
        server.remove_mount("/cam")
        with pytest.raises(RtspError) as ei:
            server.remove_mount("/cam")
        assert ei.value.kind == RtspErrorKind.MOUNT
        # The mount object outlives its removal.
        assert mount.stats().rtp_packets_received == 0
        # The path is free again.
        server.add_publish_mount("/cam")


def test_into_demux_receiver_is_take_once():
    with _start() as server:
        mount = server.add_publish_mount("/cam")
        rx = mount.into_demux_receiver()
        try:
            assert isinstance(rx, DemuxReceiver)
            with pytest.raises(RtspError) as ei:
                mount.into_demux_receiver()
            assert ei.value.kind == RtspErrorKind.CLOSED
            # Still usable for stats after the take.
            assert mount.stats().generation == 0
        finally:
            rx.close()


def test_into_demux_receiver_bad_config_does_not_spend_the_take():
    with _start() as server:
        mount = server.add_publish_mount("/cam")
        with pytest.raises(Exception):
            mount.into_demux_receiver(demux_config=object())  # type: ignore[arg-type]
        rx = mount.into_demux_receiver()
        rx.close()


def test_cancel_closes_the_receiver_with_closed_kind():
    with _start() as server:
        mount = server.add_publish_mount("/cam")
        rx = mount.into_demux_receiver()
        try:
            mount.cancel()
            with pytest.raises(RtpError) as ei:
                next(rx)
            assert ei.value.kind == RtpErrorKind.CLOSED
            assert rx.end_reason() == StreamEndReason.CANCELLED
        finally:
            rx.close()


def test_remove_mount_ends_a_parked_receiver_with_end_of_stream():
    with _start() as server:
        mount = server.add_publish_mount("/cam")
        rx = mount.into_demux_receiver()
        try:
            t, out = _next_in_thread(rx)
            server.remove_mount("/cam")
            t.join(JOIN_S)
            if t.is_alive():
                mount.cancel()  # unpark so the suite does not hang
                t.join(JOIN_S)
                pytest.fail("remove_mount did not wake the parked receiver")
            # END_OF_STREAM reaches an iterator as StopIteration.
            assert isinstance(out.get("exc"), StopIteration), out
            assert rx.end_reason() == StreamEndReason.CLEAN_TEARDOWN
        finally:
            rx.close()


def test_methods_after_stop_raise_server_kind():
    server = _start()
    mount = server.add_publish_mount("/cam")
    server.stop()
    for call in (
        lambda: server.next_publisher(0.1),
        lambda: server.add_publish_mount("/other"),
        lambda: server.remove_mount("/cam"),
    ):
        with pytest.raises(RtspError) as ei:
            call()
        assert ei.value.kind == RtspErrorKind.SERVER
    assert mount.stats().rtp_packets_received == 0


# ---------------------------------------------------------------------------
# With a publisher
# ---------------------------------------------------------------------------


def test_raw_publisher_null_bundles_reach_stats_and_publisher_info():
    with _start() as server:
        mount = server.add_publish_mount("/cam")
        before_ms = int(time.time() * 1000)
        pub = RawPublisher(_port(server), "/cam")
        try:
            rx = mount.into_demux_receiver()
            try:
                for _ in range(5):
                    pub.send_rtp(NULL_BUNDLE)
                assert _wait_for(lambda: mount.stats().rtp_packets_received >= 5), mount.stats()
                s = mount.stats()
                assert s.rtp_packets_received == 5
                assert s.bytes_received == 5 * (12 + len(NULL_BUNDLE))
                assert s.malformed_packets == 0
                assert s.alignment is ClockAlignment.NOT_APPLICABLE

                info = mount.publisher()
                assert isinstance(info, PublisherInfo)
                assert info.peer == pub.local_addr()
                assert info.shape is PublishShape.MP2T
                assert info.klv is False
                assert info.generation == 0
                # A wall-clock instant, not a duration: the ANNOUNCE
                # happened after `before_ms` (1 s slack for clock steps).
                assert info.since_unix_ms >= before_ms - 1000
                assert "MP2T" in repr(info)

                st = server.stats()
                assert st.active_publishers == 1
                assert st.total_rtp_packets_received >= 5
                assert st.total_rtp_bytes_received >= 5 * (12 + len(NULL_BUNDLE))
            finally:
                rx.close()
        finally:
            pub.close()
        # The publisher's end bumps the generation and frees the slot.
        assert _wait_for(lambda: mount.generation() == 1), mount.generation()
        assert _wait_for(lambda: mount.publisher() is None)


def test_muxed_bundles_produce_a_video_event():
    with _start() as server:
        mount = server.add_publish_mount("/cam")
        rx = mount.into_demux_receiver()
        # Watchdog: a regression that never delivers video ends the read
        # with RtpError(CLOSED) instead of hanging.
        watchdog = threading.Timer(JOIN_S, mount.cancel)
        watchdog.start()
        pub = RawPublisher(_port(server), "/cam")
        try:
            for bundle in _muxed_bundles(60):
                pub.send_rtp(bundle)
            video = None
            for ev in rx:
                if isinstance(ev, DemuxEvent.Video):
                    video = ev
                    break
            assert video is not None
            # The first video event is the first pushed AU: an IDR slice
            # (NAL header 0x65) behind an Annex-B start code.
            payload = video.raw
            assert payload, "empty video payload"
            assert b"\x00\x00\x00\x01\x65" in payload, payload[:16].hex()
        finally:
            watchdog.cancel()
            pub.close()
            rx.close()


def test_next_publisher_returns_the_on_demand_mount():
    with _start(accept_unregistered=True) as server:
        pub = RawPublisher(_port(server), "/auto")
        try:
            mount = server.next_publisher(5.0)
            assert mount is not None
            assert mount.mount_path() == "/auto"
            info = mount.publisher()
            assert info is not None and info.shape is PublishShape.MP2T
            rx = mount.into_demux_receiver()
            try:
                pub.send_rtp(NULL_BUNDLE)
                assert _wait_for(lambda: mount.stats().rtp_packets_received >= 1)
            finally:
                rx.close()
            server.remove_mount("/auto")
        finally:
            pub.close()


# ---------------------------------------------------------------------------
# Shutdown wakes parked calls
# ---------------------------------------------------------------------------


def test_context_exit_wakes_parked_next_publisher_and_receiver():
    out: dict[str, Any] = {}
    entered = threading.Event()

    def park(server: RtspServer) -> None:
        entered.set()
        try:
            out["value"] = server.next_publisher(None)
        except BaseException as e:  # noqa: BLE001
            out["exc"] = e

    with _start(accept_unregistered=True) as server:
        mount = server.add_publish_mount("/cam")
        rx = mount.into_demux_receiver()
        t = threading.Thread(target=park, args=(server,), daemon=True)
        t.start()
        entered.wait()
        rt, rout = _next_in_thread(rx)
    try:
        t.join(JOIN_S)
        rt.join(JOIN_S)
        if t.is_alive() or rt.is_alive():
            server.stop()
            mount.cancel()
            t.join(JOIN_S)
            rt.join(JOIN_S)
            pytest.fail("leaving the with-block did not wake the parked calls")
        exc = out.get("exc")
        assert isinstance(exc, RtspError), out
        assert exc.kind == RtspErrorKind.SERVER
        # stop() ends the publish mount's receiver with END_OF_STREAM.
        assert isinstance(rout.get("exc"), StopIteration), rout
    finally:
        rx.close()


def test_next_publisher_none_timeout_is_interruptible():
    """Ctrl-C reaches a `next_publisher(None)` parked on the main thread:
    the wait is sliced and pending signals are handled between slices."""
    server = _start()
    fired = threading.Event()

    def unpark() -> None:
        fired.set()
        server.stop()

    interrupt = threading.Timer(0.2, _thread.interrupt_main)
    # A regression unparks through stop() instead of hanging. The signal
    # stays pending until the call returns, so a KeyboardInterrupt alone
    # does not prove the slice loop handled it: assert the watchdog did
    # not have to fire.
    watchdog = threading.Timer(JOIN_S, unpark)
    try:
        interrupt.start()
        watchdog.start()
        with pytest.raises(KeyboardInterrupt):
            server.next_publisher(None)
        assert not fired.is_set(), "the interrupt was only handled after stop() unparked the call"
    finally:
        interrupt.cancel()
        watchdog.cancel()
        server.stop()


# ---------------------------------------------------------------------------
# GIL release (structural probe — see test_gil_release.py for the proof)
# ---------------------------------------------------------------------------


def test_next_publisher_releases_the_gil_while_waiting():
    """A probe thread stamps once; with the switch interval pinned to an
    hour, the stamp lands inside the `next_publisher` call window only if
    the call released the GIL."""
    with _start() as server:
        go = threading.Event()
        stamp: list[float] = []

        def probe() -> None:
            go.wait()
            stamp.append(perf_counter())

        t = threading.Thread(target=probe, daemon=True)
        t.start()
        prev = sys.getswitchinterval()
        sys.setswitchinterval(3600.0)
        try:
            go.set()
            t0 = perf_counter()
            got: Optional[PublishMount] = server.next_publisher(1.5)
            t1 = perf_counter()
            t.join(JOIN_S)
        finally:
            sys.setswitchinterval(prev)
        assert got is None
        assert stamp, "probe thread never ran"
        assert t0 < stamp[0] < t1, (
            "next_publisher held the GIL while it waited: the probe ran "
            f"{(stamp[0] - t1) * 1000:.1f} ms after the call returned"
        )
