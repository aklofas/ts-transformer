"""`ts_bytes_demuxed()` on `shells.DemuxReceiver` and `srt.ManagedDemuxReceiver`.

The counter is the aligned TS bytes the demuxer was fed. It is an atomic
read, so it answers while a call is parked in `__next__` and after `close()`.
"""

from __future__ import annotations

import socket
import threading
import time

import pytest

import tstrans.exceptions
import tstrans.rtp
from tstrans import shells
from tstrans.mpegts import DemuxEvent, Demuxer, Muxer, MuxerConfigBuilder, Pts90khz
from tstrans.srt import ManagedDemuxReceiver

from _builders.mux_programs import video_only_program as _video_only_program
from _builders.ports import free_udp_port as _free_udp_port
from _builders.srt_pair import make_srt_pair as _make_srt_pair
from test_srt_managed_convenience import NAL_IDR, _make_managed_pair

NAL_AUD = b"\x00\x00\x00\x01\x09\xF0"


def _build_minimal_ts_bytes() -> bytes:
    """Mux one key AUD NAL → drained TS bytes (PAT/PMT + a sample)."""
    cfg = MuxerConfigBuilder().add_program(_video_only_program()).build()
    mux = Muxer(cfg)
    mux.push_video(NAL_AUD, pts=Pts90khz.from_raw(0), key_frame=True)
    out = bytearray()
    scratch = bytearray(188 * 32)
    while True:
        n = mux.pull(scratch)
        if n == 0:
            break
        out.extend(scratch[:n])
    return bytes(out)


def _drain(rx) -> None:
    """Iterate until the receiver ends; a `close()` mid-wait raises CLOSED."""
    try:
        for _ in rx:
            pass
    except tstrans.exceptions.RtpError:
        pass


def _send_32_and_wait_first_video(sender, receiver) -> None:
    """Send 32 video AUs and iterate `receiver` to its first `Video`."""
    err: list[BaseException] = []

    def consumer() -> None:
        try:
            for ev in receiver:
                if isinstance(ev, DemuxEvent.Video):
                    break
        except BaseException as exc:  # noqa: BLE001
            err.append(exc)

    t = threading.Thread(target=consumer, daemon=True)
    t.start()
    time.sleep(0.2)
    try:
        for i in range(32):
            sender.send_video(
                NAL_IDR, pts=Pts90khz.from_raw(i * 3000), key_frame=(i % 4 == 0)
            )
        time.sleep(0.3)
    finally:
        sender.close()
    t.join(timeout=5.0)
    assert not t.is_alive(), "consumer never saw a Video event"
    assert not err, err


def test_srt_ts_bytes_demuxed_counts_the_stream() -> None:
    sender, rx = _make_srt_pair(_free_udp_port())
    try:
        _send_32_and_wait_first_video(sender, rx)
        n = rx.ts_bytes_demuxed()
        assert n > 0
        assert n % 188 == 0
        assert n == rx.stats()[0].bytes_received
    finally:
        rx.close()


@pytest.mark.parametrize("kind", ["srt", "rtp"])
def test_ts_bytes_demuxed_after_close_returns_last_value(kind: str) -> None:
    if kind == "srt":
        sender, rx = _make_srt_pair(_free_udp_port())
        _send_32_and_wait_first_video(sender, rx)
    else:
        port = _free_udp_port()
        rx = shells.DemuxReceiver.open(f"rtp://127.0.0.1:{port}")
        ts_bytes = _build_minimal_ts_bytes()
        t = threading.Thread(target=lambda: _drain(rx), daemon=True)
        t.start()
        time.sleep(0.2)
        with tstrans.rtp.Transport(f"rtp://127.0.0.1:{port}") as snd:
            for _ in range(3):
                for i in range(0, len(ts_bytes), 188):
                    snd.send(ts_bytes[i : i + 188])
                time.sleep(0.05)
        deadline = time.monotonic() + 3.0
        while rx.ts_bytes_demuxed() == 0 and time.monotonic() < deadline:
            time.sleep(0.02)
    n = rx.ts_bytes_demuxed()
    assert n > 0
    rx.close()
    # Packets still in flight may be counted between the read above and the
    # close, so the closing value may exceed `n`; after close it is fixed.
    after = rx.ts_bytes_demuxed()
    assert after >= n
    assert after % 188 == 0
    assert rx.ts_bytes_demuxed() == after


def test_rtp_ts_bytes_demuxed_counts_the_stream() -> None:
    port = _free_udp_port()
    rx = shells.DemuxReceiver.open(f"rtp://127.0.0.1:{port}")
    t = threading.Thread(target=lambda: _drain(rx), daemon=True)
    t.start()
    time.sleep(0.2)
    ts_bytes = _build_minimal_ts_bytes()
    with tstrans.rtp.Transport(f"rtp://127.0.0.1:{port}") as snd:
        for _ in range(3):
            for i in range(0, len(ts_bytes), 188):
                snd.send(ts_bytes[i : i + 188])
            time.sleep(0.05)
    deadline = time.monotonic() + 3.0
    while rx.ts_bytes_demuxed() == 0 and time.monotonic() < deadline:
        time.sleep(0.02)
    n = rx.ts_bytes_demuxed()
    rx.close()
    t.join(timeout=5.0)
    assert n > 0, "the counter saw no packets"
    assert n % 188 == 0


def _serve_once(payload: bytes) -> tuple[socket.socket, int]:
    """A TCP server that writes `payload` to its one connection, then closes."""
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)

    def run() -> None:
        conn, _ = srv.accept()
        with conn:
            conn.sendall(payload)

    threading.Thread(target=run, daemon=True).start()
    return srv, srv.getsockname()[1]


def _muxed_stream(aus: int = 5) -> bytes:
    """PAT/PMT and `aus` video AUs. The `Receiver` emits nothing before its
    syncer has locked on four aligned packets, so the stream must be longer."""
    mux = Muxer(MuxerConfigBuilder().add_program(_video_only_program()).build())
    for i in range(aus):
        mux.push_video(NAL_IDR, pts=Pts90khz.from_raw(i * 3000), key_frame=True)
    out = bytearray()
    scratch = bytearray(188 * 32)
    while (n := mux.pull(scratch)) > 0:
        out.extend(scratch[:n])
    return bytes(out)


def _event_keys(events: list[object]) -> list[tuple[str, int | None]]:
    """`(type name, pid)`; the pid is read from `stream` when the event has one."""
    return [
        (type(ev).__name__, getattr(getattr(ev, "stream", None), "pid", None))
        for ev in events
    ]


def test_receiver_demuxer_composition_over_tcp() -> None:
    """`Receiver` for the packets, `Demuxer` fed the same bytes for the events:
    the replacement for observing raw TS through a byte sink."""
    buf = _muxed_stream()
    assert len(buf) >= 4 * 188 and len(buf) % 188 == 0

    srv, port = _serve_once(buf)
    chunks: list[bytes] = []
    composed: list[object] = []
    demux = Demuxer()
    try:
        with shells.Receiver.open(f"tcp://127.0.0.1:{port}") as rx:
            while True:
                try:
                    chunk = rx.recv_bytes()
                except tstrans.exceptions.TcpError as exc:
                    assert exc.kind == tstrans.exceptions.TcpErrorKind.END_OF_STREAM, exc
                    break
                assert len(chunk) % 188 == 0
                assert chunk[0] == 0x47
                chunks.append(chunk)
                demux.feed(chunk)
                while (ev := demux.next_event()) is not None:
                    composed.append(ev)
        demux.flush()
        while (ev := demux.next_event()) is not None:
            composed.append(ev)
    finally:
        srv.close()
    assert b"".join(chunks) == buf

    # Reference: a DemuxReceiver over a second identical connection.
    srv2, port2 = _serve_once(buf)
    try:
        with shells.DemuxReceiver.open(f"tcp://127.0.0.1:{port2}") as shell:
            reference = list(shell)
    finally:
        srv2.close()

    assert reference, "the reference produced no events"
    assert _event_keys(composed) == _event_keys(reference)


def test_ts_bytes_demuxed_answers_while_iteration_is_parked() -> None:
    port = _free_udp_port()
    rx = shells.DemuxReceiver.open(f"rtp://127.0.0.1:{port}")
    iterating = threading.Thread(target=lambda: _drain(rx), daemon=True)
    iterating.start()
    time.sleep(0.2)

    got: list[int] = []
    reader = threading.Thread(target=lambda: got.append(rx.ts_bytes_demuxed()), daemon=True)
    reader.start()
    reader.join(timeout=2.0)
    assert not reader.is_alive(), "ts_bytes_demuxed() waited behind a parked __next__"
    assert got == [0]

    rx.close()
    iterating.join(timeout=5.0)
    assert not iterating.is_alive()


def test_managed_ts_bytes_demuxed_counts_and_survives_close() -> None:
    sender, rx = _make_managed_pair(_free_udp_port())
    assert isinstance(rx, ManagedDemuxReceiver)
    try:
        _send_32_and_wait_first_video(sender, rx)
        n = rx.ts_bytes_demuxed()
        assert n > 0
        assert n % 188 == 0
    finally:
        rx.close()
    assert rx.ts_bytes_demuxed() == n
