"""`ts_bytes_demuxed()` on `shells.DemuxReceiver` and `srt.ManagedDemuxReceiver`.

The counter is the aligned TS bytes the demuxer was fed. It is an atomic
read, so it answers while a call is parked in `__next__` and after `close()`.
"""

from __future__ import annotations

import threading
import time

import pytest

import tstrans.exceptions
import tstrans.rtp
from tstrans import shells
from tstrans.mpegts import DemuxEvent, Pts90khz
from tstrans.srt import ManagedDemuxReceiver

from _builders.ports import free_tcp_port as _free_tcp_port
from _builders.ports import free_udp_port as _free_udp_port
from _builders.srt_pair import make_srt_pair as _make_srt_pair
from test_byte_sink import _build_minimal_ts_bytes
from test_srt_managed_convenience import NAL_IDR, _make_managed_pair


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
    sender, rx = _make_srt_pair(_free_tcp_port())
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
        sender, rx = _make_srt_pair(_free_tcp_port())
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
    assert rx.ts_bytes_demuxed() == n


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
    sender, rx = _make_managed_pair(_free_tcp_port())
    assert isinstance(rx, ManagedDemuxReceiver)
    try:
        _send_32_and_wait_first_video(sender, rx)
        n = rx.ts_bytes_demuxed()
        assert n > 0
        assert n % 188 == 0
    finally:
        rx.close()
    assert rx.ts_bytes_demuxed() == n
