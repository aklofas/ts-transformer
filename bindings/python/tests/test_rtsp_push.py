"""tstrans.rtp — publishing to an RTSP server: `RtspClient.push`,
`PushDescription`, `RtspPushSession`, `RtspPushStats`.

Covers:
- An MPEG-TS push, through a `MuxSender` and directly, reaching a publish
  mount of the library's own `RtspServer`; an H.264 + KLV push whose mount
  aligns the two tracks by the sender reports.
- The ends of a session: the `with` exit (drains, then TEARDOWN), a
  reporting `teardown()` against a server that refuses it, a cancel or a
  `close()` from another thread while a send is parked, collection.
- The refusals: calls after `close()`, a wrong-track or empty unit, a
  conversion of an elementary push, a refused program config, H.265 (the
  library's server answers `415`), UDP and RTSP 2.0.

The servers that misbehave are plain sockets speaking RTSP by hand. Every
wait has a deadline and every parked call has a rescue path, so a
regression fails instead of hanging the suite.
"""

from __future__ import annotations

import base64
import gc
import os
import socket
import subprocess
import sys
import textwrap
import threading
import time
from typing import Any, Callable, Optional

import pytest

from tstrans.exceptions import (
    MuxError,
    MuxErrorKind,
    RtpError,
    RtpErrorKind,
    RtspError,
    RtspErrorKind,
)
from tstrans.mpegts import DemuxEvent, Pts90khz
from tstrans.rtp import (
    ClockAlignment,
    PublishShape,
    PushDescription,
    RtspCancelHandle,
    RtspClient,
    RtspClientConfig,
    RtspPushSession,
    RtspPushStats,
    RtspServer,
    RtspServerConfig,
    RtspVersion,
    TransportPref,
)
from tstrans.shells import MuxSender, Sender

from _builders.mux_programs import masquerade_program, video_only_program
import tstrans

TS_PACKET = bytes([0x47, 0x1F, 0xFF, 0x10]) + b"\xff" * 184
TS_BUNDLE = TS_PACKET * 7  # 1316 bytes: the largest `send_ts` payload

# An H.264 SPS and PPS the library's server parses (Baseline, 160x120).
SPS = base64.b64decode("Z/QADJGWgUH7ARAAAAMAEAAAAwHg8UKq")
PPS = base64.b64decode("aM4PGSA=")
START = b"\x00\x00\x00\x01"
KLV_UNIT = bytes([0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01]) + bytes(9)

# Bounded waits. Generous: they only ever expire on a regression.
WAIT_S = 5.0


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _start_server() -> RtspServer:
    return RtspServer.start(
        RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    )


def _url(server: RtspServer, path: str) -> str:
    addr = server.local_addr()
    assert addr is not None
    return f"rtsp://127.0.0.1:{addr.rsplit(':', 1)[1]}{path}"


def _wait_for(cond: Callable[[], bool], limit: float = WAIT_S) -> bool:
    """Poll `cond` every 10 ms until it holds or `limit` seconds pass."""
    deadline = time.monotonic() + limit
    while time.monotonic() < deadline:
        if cond():
            return True
        time.sleep(0.01)
    return cond()


def _access_unit(i: int) -> bytes:
    """Access unit `i` in Annex B form: a key frame with its parameter sets
    first, then P slices large enough to be fragmented."""
    slice_nal = bytes([0x65 if i == 0 else 0x41]) + bytes(
        (k % 250) + 1 for k in range(3000)
    )
    nals = [SPS, PPS, slice_nal] if i == 0 else [slice_nal]
    return b"".join(START + n for n in nals)


class ScriptedRecordServer:
    """One connection, RTSP spoken by hand: ANNOUNCE, SETUP and RECORD are
    answered `200`, TEARDOWN with `teardown_status`, anything else `200`.
    Interleaved frames are read and dropped. With `stop_reading`, nothing
    more is read after the RECORD reply, so the client's writes back up."""

    def __init__(self, *, teardown_status: int = 200, stop_reading: bool = False) -> None:
        self.teardown_status = teardown_status
        self.stop_reading = stop_reading
        self.methods: list[str] = []
        self.done = threading.Event()
        self.lsock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        if stop_reading:
            # A small window, inherited by the accepted socket: the client's
            # writes back up after a few kilobytes on the server's side.
            self.lsock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
        self.lsock.bind(("127.0.0.1", 0))
        self.lsock.listen(1)
        self.lsock.settimeout(WAIT_S)
        self.url = f"rtsp://127.0.0.1:{self.lsock.getsockname()[1]}/cam"
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def _run(self) -> None:
        try:
            conn, _ = self.lsock.accept()
        except OSError:
            return
        conn.settimeout(0.05)
        buf = b""
        try:
            while not self.done.is_set():
                if self.stop_reading and "RECORD" in self.methods:
                    self.done.wait(0.05)
                    continue
                try:
                    chunk = conn.recv(65536)
                except socket.timeout:
                    continue
                if not chunk:
                    return
                buf += chunk
                buf = self._answer(conn, buf)
        except OSError:
            return
        finally:
            conn.close()

    def _answer(self, conn: socket.socket, buf: bytes) -> bytes:
        """Answer every whole request in `buf`; return what is left."""
        while buf:
            if buf[:1] == b"$":
                if len(buf) < 4:
                    return buf
                size = 4 + int.from_bytes(buf[2:4], "big")
                if len(buf) < size:
                    return buf
                buf = buf[size:]
                continue
            end = buf.find(b"\r\n\r\n")
            if end < 0:
                return buf
            head = buf[:end].decode(errors="replace")
            length = 0
            headers: dict[str, str] = {}
            for line in head.split("\r\n")[1:]:
                k, _, v = line.partition(":")
                headers[k.strip().lower()] = v.strip()
            length = int(headers.get("content-length", "0"))
            if len(buf) < end + 4 + length:
                return buf
            buf = buf[end + 4 + length :]
            method = head.split(" ", 1)[0]
            self.methods.append(method)
            status = self.teardown_status if method == "TEARDOWN" else 200
            reply = f"RTSP/1.0 {status} X\r\nCSeq: {headers.get('cseq', '0')}\r\nSession: 1234ABCD\r\n"
            if method == "SETUP":
                reply += f"Transport: {headers.get('transport', '')}\r\n"
            conn.sendall((reply + "\r\n").encode())
        return buf

    def close(self) -> None:
        self.done.set()
        self.thread.join(WAIT_S)
        self.lsock.close()


def _push_until_parked(push: RtspPushSession) -> dict[str, Any]:
    """Send on a daemon thread until a send has not returned for 300 ms (it
    is parked: the server stopped reading). The dict receives `exc` and
    `returned_at` when the parked send ends."""
    state: dict[str, Any] = {"sent": 0}
    ended = threading.Event()

    def run() -> None:
        try:
            while True:
                push.send_ts(TS_BUNDLE)
                state["sent"] += 1
        except BaseException as e:  # noqa: BLE001
            state["exc"] = e
            state["returned_at"] = time.monotonic()
        finally:
            ended.set()

    state["thread"] = threading.Thread(target=run, daemon=True)
    state["ended"] = ended
    state["thread"].start()
    deadline = time.monotonic() + WAIT_S
    last, last_change = -1, time.monotonic()
    while time.monotonic() < deadline and not ended.is_set():
        now = state["sent"]
        if now != last:
            last, last_change = now, time.monotonic()
        elif time.monotonic() - last_change >= 0.3:
            return state
        ended.wait(0.02)
    raise AssertionError(f"the send never parked: {state!r}")


# ---------------------------------------------------------------------------
# Against the library's own server
# ---------------------------------------------------------------------------


def test_mp2t_push_through_mux_sender_reaches_the_mount() -> None:
    with _start_server() as server:
        mount = server.add_publish_mount("/cam")
        rx = mount.into_demux_receiver()
        watchdog = threading.Timer(10.0, mount.cancel)
        watchdog.start()
        try:
            push = RtspClient.push(
                RtspClientConfig(_url(server, "/cam")), PushDescription.mp2t()
            )
            assert isinstance(push, RtspPushSession)
            info = mount.publisher()
            assert info is not None and info.shape is PublishShape.MP2T
            sender = push.into_mux_sender(video_only_program())
            assert isinstance(sender, MuxSender)
            for i in range(30):
                sender.send_video(
                    START + bytes([0x65 if i == 0 else 0x41]) + bytes(range(1, 200)),
                    pts=Pts90khz.from_raw(i * 3003),
                    key_frame=(i == 0),
                )
            video = None
            for ev in rx:
                if isinstance(ev, DemuxEvent.Video):
                    video = ev
                    break
            assert video is not None
            assert START + b"\x65" in video.raw, video.raw[:16].hex()
            sender.close()
            # Closing the sender closes the push: the mount is free again.
            assert _wait_for(lambda: mount.publisher() is None)
            # The session gave its push to the sender.
            with pytest.raises(RtpError) as ei:
                push.send_ts(TS_BUNDLE)
            assert ei.value.kind is RtpErrorKind.CLOSED
        finally:
            watchdog.cancel()
            rx.close()


def test_h264_klv_push_reaches_the_mount_with_sender_report_alignment() -> None:
    units = 30
    with _start_server() as server:
        mount = server.add_publish_mount("/cam")
        desc = PushDescription.h264([SPS, PPS], klv=True)
        with RtspClient.push(RtspClientConfig(_url(server, "/cam")), desc) as push:
            info = mount.publisher()
            assert info is not None
            assert info.shape is PublishShape.ELEMENTARY and info.klv is True
            for i in range(units):
                pts = Pts90khz.from_raw(i * 3003)
                push.send_video(_access_unit(i), pts=pts)
                push.send_klv(KLV_UNIT + bytes([i]), pts=pts)
            # The last access unit is held until the next one starts.
            assert _wait_for(
                lambda: mount.stats().aus_emitted >= units - 1
                and mount.stats().klv_units_emitted >= units - 1
            ), mount.stats()
            s = mount.stats()
            assert s.malformed_packets == 0
            assert s.alignment is ClockAlignment.SENDER_REPORT
            assert s.alignment_steps == 0


def test_with_exit_drains_and_closes() -> None:
    bundles = 200
    with _start_server() as server:
        mount = server.add_publish_mount("/cam")
        with RtspClient.push(
            RtspClientConfig(_url(server, "/cam")), PushDescription.mp2t()
        ) as push:
            assert push.session_id
            handle = push.cancel_handle()
            for _ in range(bundles):
                push.send_ts(TS_BUNDLE)
        # Every queued packet was written before the TEARDOWN.
        assert _wait_for(lambda: mount.stats().rtp_packets_received == bundles), mount.stats()
        assert mount.stats().malformed_packets == 0
        assert _wait_for(lambda: mount.publisher() is None)
        # The closed client's cancel is set, as for every closed client.
        assert handle.is_cancelled()


def test_stats_counts_packets_and_reports() -> None:
    with _start_server() as server:
        server.add_publish_mount("/cam")
        with RtspClient.push(
            RtspClientConfig(_url(server, "/cam")), PushDescription.mp2t()
        ) as push:
            s0 = push.stats()
            assert isinstance(s0, RtspPushStats)
            assert (s0.packets_sent, s0.bytes_sent, s0.sender_reports_sent) == (0, 0, 0)
            for _ in range(10):
                push.send_ts(TS_BUNDLE)
            s = push.stats()
            assert s.packets_sent == 10
            assert s.bytes_sent == 10 * (12 + len(TS_BUNDLE))
            # One sender report goes ahead of the track's first packet.
            assert s.sender_reports_sent >= 1
            for name in (
                "receiver_reports",
                "fraction_lost_q8",
                "cumulative_lost",
                "frames_dropped_inbound",
                "frames_dropped_at_close",
            ):
                assert isinstance(getattr(s, name), int), name
            assert "packets_sent=10" in repr(s)
            assert push.is_session_alive()


def test_close_while_stats_is_read_does_not_cancel() -> None:
    """`stats()` holds the session's lock only for a moment: a `teardown()`
    that finds it held waits for it instead of cancelling the session, so
    the queued media is written and the TEARDOWN is answered. Threads read
    `stats()` in a loop through every round, so many teardowns meet a
    held lock (about one round in 250 meets it on a desktop: 4000 rounds,
    or 5 s of them on a slow machine)."""
    rounds, bundles, readers = 4000, 50, 8
    give_up = time.monotonic() + 5.0
    with _start_server() as server:
        for r in range(rounds):
            if time.monotonic() > give_up:
                break
            mount = server.add_publish_mount(f"/cam{r}")
            push = RtspClient.push(
                RtspClientConfig(_url(server, f"/cam{r}")), PushDescription.mp2t()
            )
            stop = threading.Event()

            def read_stats() -> None:
                while not stop.is_set():
                    try:
                        push.stats()
                    except RtspError:
                        return  # the session closed under the loop

            threads = [threading.Thread(target=read_stats, daemon=True) for _ in range(readers)]
            for t in threads:
                t.start()
            try:
                for _ in range(bundles):
                    push.send_ts(TS_BUNDLE)
                # Returns only when the server answered the TEARDOWN `2xx`
                # on a push that was not cancelled.
                push.teardown()
            finally:
                stop.set()
                for t in threads:
                    t.join(WAIT_S)
                push.close()
            assert _wait_for(
                lambda: mount.stats().rtp_packets_received == bundles
            ), (r, mount.stats())
            assert _wait_for(lambda: mount.publisher() is None), r


def test_pause_then_resume_against_the_library_server() -> None:
    with _start_server() as server:
        mount = server.add_publish_mount("/cam")
        with RtspClient.push(
            RtspClientConfig(_url(server, "/cam")), PushDescription.mp2t()
        ) as push:
            push.send_ts(TS_BUNDLE)
            push.pause()
            # The pause wrote the queued media first.
            assert _wait_for(lambda: mount.stats().rtp_packets_received == 1), mount.stats()
            with pytest.raises(RtspError) as ei:
                push.pause()  # already paused: refused locally
            assert ei.value.kind is RtspErrorKind.PROTOCOL
            push.resume()
            with pytest.raises(RtspError) as ei:
                push.resume()  # not paused
            assert ei.value.kind is RtspErrorKind.PROTOCOL
            push.send_ts(TS_BUNDLE)
            assert _wait_for(lambda: mount.stats().rtp_packets_received == 2), mount.stats()
            assert push.is_session_alive()


def test_calls_after_close_raise_closed() -> None:
    with _start_server() as server:
        server.add_publish_mount("/cam")
        push = RtspClient.push(
            RtspClientConfig(_url(server, "/cam")), PushDescription.mp2t()
        )
        sid = push.session_id
        push.send_ts(TS_BUNDLE)
        push.close()
        push.close()  # idempotent
        assert push.session_id == sid
        for send in (
            lambda: push.send_ts(TS_BUNDLE),
            lambda: push.send_video(START + b"\x65\x00", pts=Pts90khz.from_raw(0)),
            lambda: push.send_klv(KLV_UNIT, pts=Pts90khz.from_raw(0)),
        ):
            with pytest.raises(RtpError) as rtp_ei:
                send()
            assert rtp_ei.value.kind is RtpErrorKind.CLOSED
        for call in (
            push.pause,
            push.resume,
            push.teardown,
            push.stats,
            push.is_session_alive,
            push.cancel_handle,
            push.into_sender,
            lambda: push.into_mux_sender(video_only_program()),
        ):
            with pytest.raises(RtspError) as ei:
                call()
            assert ei.value.kind is RtspErrorKind.CLOSED


def test_into_mux_sender_on_an_elementary_push_is_value_error_and_keeps_the_session() -> None:
    with _start_server() as server:
        mount = server.add_publish_mount("/cam")
        desc = PushDescription.h264([SPS, PPS])
        with RtspClient.push(RtspClientConfig(_url(server, "/cam")), desc) as push:
            with pytest.raises(ValueError):
                push.into_mux_sender(video_only_program())
            with pytest.raises(ValueError):
                push.into_sender()
            # Nothing was consumed: the session still sends.
            for i in range(3):
                push.send_video(_access_unit(i), pts=Pts90khz.from_raw(i * 3003))
            assert push.is_session_alive()
            assert _wait_for(lambda: mount.stats().aus_emitted >= 2), mount.stats()


def test_a_refused_program_config_keeps_the_session() -> None:
    with _start_server() as server:
        mount = server.add_publish_mount("/cam")
        push = RtspClient.push(RtspClientConfig(_url(server, "/cam")), PushDescription.mp2t())
        with pytest.raises(MuxError) as ei:
            push.into_mux_sender(masquerade_program())
        assert ei.value.kind is MuxErrorKind.CONFIG_INVALID
        # Still the session's: it sends, and converts once the config is good.
        push.send_ts(TS_BUNDLE)
        assert _wait_for(lambda: mount.stats().rtp_packets_received == 1), mount.stats()
        sender = push.into_sender()
        assert isinstance(sender, Sender)
        sender.send_bytes(TS_BUNDLE)
        sender.close()
        assert _wait_for(lambda: mount.stats().rtp_packets_received == 2), mount.stats()
        assert _wait_for(lambda: mount.publisher() is None)
        push.close()  # nothing left to close


def test_send_klv_without_a_klv_track_is_value_error() -> None:
    with _start_server() as server:
        mount = server.add_publish_mount("/cam")
        desc = PushDescription.h264([SPS, PPS])
        with RtspClient.push(RtspClientConfig(_url(server, "/cam")), desc) as push:
            with pytest.raises(ValueError, match="KLV"):
                push.send_klv(KLV_UNIT, pts=Pts90khz.from_raw(0))
            with pytest.raises(ValueError):
                push.send_video(b"", pts=Pts90khz.from_raw(0))
            with pytest.raises(ValueError):
                push.send_ts(TS_BUNDLE)
            # The refusals sent nothing and left the session open.
            assert push.stats().packets_sent == 0
            push.send_video(_access_unit(0), pts=Pts90khz.from_raw(0))
            push.send_video(_access_unit(1), pts=Pts90khz.from_raw(3003))
            assert _wait_for(lambda: mount.stats().aus_emitted >= 1), mount.stats()
        # On an MPEG-TS push the elementary calls name a track it lacks.
        with RtspClient.push(
            RtspClientConfig(_url(server, "/cam")), PushDescription.mp2t()
        ) as push:
            with pytest.raises(ValueError):
                push.send_video(_access_unit(0), pts=Pts90khz.from_raw(0))
            with pytest.raises(ValueError):
                push.send_klv(KLV_UNIT, pts=Pts90khz.from_raw(0))


def test_from_key_frame_without_parameter_sets_is_value_error() -> None:
    with pytest.raises(ValueError):
        PushDescription.h264_from_key_frame(START + b"\x65\x00\x01")
    with pytest.raises(ValueError):
        PushDescription.h265_from_key_frame(START + b"\x26\x01\x00")
    desc = PushDescription.h264_from_key_frame(_access_unit(0), klv=True)
    assert isinstance(desc, PushDescription)


def test_h265_push_is_refused_415_by_the_library_server() -> None:
    with _start_server() as server:
        mount = server.add_publish_mount("/cam")
        desc = PushDescription.h265(b"\x40\x01\x0c", b"\x42\x01\x01", b"\x44\x01\xc0")
        with pytest.raises(RtspError) as ei:
            RtspClient.push(RtspClientConfig(_url(server, "/cam")), desc)
        assert ei.value.kind is RtspErrorKind.PROTOCOL
        assert "415" in str(ei.value)
        assert mount.publisher() is None


def test_transport_udp_is_unsupported_transport() -> None:
    with _start_server() as server:
        mount = server.add_publish_mount("/cam")
        cfg = RtspClientConfig(_url(server, "/cam"), transport_pref=TransportPref.UDP)
        with pytest.raises(RtspError) as ei:
            RtspClient.push(cfg, PushDescription.mp2t())
        assert ei.value.kind is RtspErrorKind.UNSUPPORTED_TRANSPORT
        assert _wait_for(lambda: mount.publisher() is None)


def test_rtsp_version_2_is_refused_locally() -> None:
    server = ScriptedRecordServer()
    try:
        cfg = RtspClientConfig(server.url, rtsp_version=RtspVersion.V2_0)
        with pytest.raises(RtspError) as ei:
            RtspClient.push(cfg, PushDescription.mp2t())
        assert ei.value.kind is RtspErrorKind.PROTOCOL
        # Refused before a request was written: the server's thread has read
        # the connection to its end, so a late request would be counted.
        server.thread.join(WAIT_S)
        assert not server.thread.is_alive(), "the client kept the connection"
        assert server.methods == []
    finally:
        server.close()


def test_a_gc_dropped_session_cancels() -> None:
    with _start_server() as server:
        mount = server.add_publish_mount("/cam")
        push: Optional[RtspPushSession] = RtspClient.push(
            RtspClientConfig(_url(server, "/cam")), PushDescription.mp2t()
        )
        assert push is not None
        handle = push.cancel_handle()
        assert isinstance(handle, RtspCancelHandle)
        assert not handle.is_cancelled()
        push = None
        gc.collect()
        assert handle.is_cancelled()
        # The dropped client wrote its TEARDOWN: the mount is free.
        assert _wait_for(lambda: mount.publisher() is None)


# ---------------------------------------------------------------------------
# Against scripted servers
# ---------------------------------------------------------------------------


def test_teardown_reports_a_refused_teardown() -> None:
    server = ScriptedRecordServer(teardown_status=501)
    try:
        push = RtspClient.push(RtspClientConfig(server.url), PushDescription.mp2t())
        push.send_ts(TS_BUNDLE)
        with pytest.raises(RtspError) as ei:
            push.teardown()
        assert ei.value.kind is RtspErrorKind.PROTOCOL
        assert "501" in str(ei.value)
        assert server.methods[:3] == ["ANNOUNCE", "SETUP", "RECORD"]
        assert "TEARDOWN" in server.methods
        # The session is closed after a teardown, refused or not.
        push.close()
        with pytest.raises(RtpError) as rtp_ei:
            push.send_ts(TS_BUNDLE)
        assert rtp_ei.value.kind is RtpErrorKind.CLOSED
    finally:
        server.close()


def test_cancel_from_a_second_thread_releases_a_parked_send_as_closed() -> None:
    server = ScriptedRecordServer(stop_reading=True)
    push = RtspClient.push(RtspClientConfig(server.url), PushDescription.mp2t())
    try:
        handle = push.cancel_handle()
        state = _push_until_parked(push)
        cancelled_at = time.monotonic()
        handle.cancel()
        assert state["ended"].wait(WAIT_S), "the parked send did not end"
        assert state["returned_at"] - cancelled_at < 1.0
        exc = state["exc"]
        assert isinstance(exc, RtpError), exc
        assert exc.kind is RtpErrorKind.CLOSED
        # cancel_handle() answered while the send was parked; it still does.
        assert push.cancel_handle().is_cancelled()
    finally:
        server.close()
        push.close()


def test_close_during_a_parked_send_cancels_first() -> None:
    server = ScriptedRecordServer(stop_reading=True)
    push = RtspClient.push(RtspClientConfig(server.url), PushDescription.mp2t())
    try:
        handle = push.cancel_handle()
        state = _push_until_parked(push)
        closed = threading.Event()
        closer = threading.Thread(target=lambda: (push.close(), closed.set()), daemon=True)
        close_at = time.monotonic()
        closer.start()
        # The close fires the cancel before it waits for the send's lock, so
        # the send ends as a cancel, well inside the 5 s stall that would
        # end it as BROKEN.
        assert state["ended"].wait(WAIT_S), "the parked send did not end"
        assert state["returned_at"] - close_at < 1.0
        exc = state["exc"]
        assert isinstance(exc, RtpError), exc
        assert exc.kind is RtpErrorKind.CLOSED
        assert handle.is_cancelled()
        # The close then ends the session within its bounds (TEARDOWN 500 ms,
        # the client's thread 2 s).
        assert closed.wait(WAIT_S), "close() did not return"
    finally:
        server.close()
        push.close()


def test_interpreter_exit_cancels_an_open_session() -> None:
    """The exit hook (`_fire_cancel_sources_at_exit`, run by `atexit`)
    fires the cancel of a session left open, so a send parked in it ends
    before the interpreter finalises. Run in a child interpreter: the hook
    marks the process as exiting."""
    script = textwrap.dedent(
        """
        from tstrans._native import _fire_cancel_sources_at_exit
        from tstrans.rtp import PushDescription, RtspClient, RtspClientConfig
        from tstrans.rtp import RtspServer, RtspServerConfig

        server = RtspServer.start(RtspServerConfig(bind_addr="127.0.0.1:0"))
        server.add_publish_mount("/cam")
        url = "rtsp://" + server.local_addr() + "/cam"
        push = RtspClient.push(RtspClientConfig(url), PushDescription.mp2t())
        handle = push.cancel_handle()
        print("BEFORE=%s" % handle.is_cancelled(), flush=True)
        _fire_cancel_sources_at_exit()
        print("AFTER=%s" % handle.is_cancelled(), flush=True)
        """
    )
    pkg_parent = os.path.dirname(os.path.dirname(os.path.abspath(tstrans.__file__)))
    r = subprocess.run(
        [sys.executable, "-c", script],
        capture_output=True,
        text=True,
        timeout=30.0,
        env={**os.environ, "PYTHONPATH": pkg_parent},
    )
    assert "BEFORE=False" in r.stdout, f"{r.stdout}\n{r.stderr}"
    assert "AFTER=True" in r.stdout, f"{r.stdout}\n{r.stderr}"
    assert r.returncode == 0, f"exit={r.returncode}\n{r.stdout}\n{r.stderr}"
