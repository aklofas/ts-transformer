"""Tests for `DemuxReceiver.add_byte_sink` on the SRT + RTP wrappers.

§8.1 byte-sink precursor for the tst-jni arc. `add_byte_sink` registers a
fan-out `Callable[[bytes], None]` that receives every 188-byte TS packet
BEFORE demuxing, in registration order. If the callback raises, the
exception is re-raised fail-loud from the next event pull (`__next__`) and
iteration stops.

The SRT side reuses the libsrt caller<->listener loopback pair from
`test_srt_mux_demux.py`; the RTP side reuses the UDP-loopback Sender/
DemuxReceiver pattern from `test_rtp_demux_receiver.py`. Ephemeral ports
throughout (de-flake convention).
"""

from __future__ import annotations

import threading
import time
from typing import Optional, Tuple

import pytest

import tstrans
import tstrans.rtp
import tstrans.srt
from tstrans.exceptions import RtpError, RtpErrorKind, SrtError, SrtErrorKind
from tstrans.mpegts import DemuxEvent, Muxer, MuxerConfigBuilder, Pts90khz

from _builders.mux_programs import video_only_program as _video_only_program
from _builders.ports import free_tcp_port as _free_tcp_port
from _builders.ports import free_udp_port as _free_udp_port


NAL_IDR = b"\x00\x00\x00\x01\x65\xBB"
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


# --------------------------------------------------------------------------- #
# SRT loopback harness (mirror of test_srt_mux_demux.py)                      #
# --------------------------------------------------------------------------- #


def _make_srt_pair(
    port: int,
) -> Tuple[tstrans.srt.MuxSender, tstrans.srt.DemuxReceiver]:
    listener_url = f"srt://:{port}?mode=listener"
    caller_url = f"srt://127.0.0.1:{port}?mode=caller"

    rx_box: list[tstrans.srt.DemuxReceiver] = []
    rx_err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            rx_box.append(tstrans.srt.DemuxReceiver.from_url(listener_url))
        except BaseException as exc:  # noqa: BLE001
            rx_err.append(exc)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)
    sender = tstrans.srt.MuxSender.from_url(caller_url, _video_only_program())
    t.join(timeout=5.0)
    if rx_err:
        sender.close()
        raise rx_err[0]
    if not rx_box:
        sender.close()
        raise RuntimeError("SRT DemuxReceiver did not accept within 5 s")
    return sender, rx_box[0]


# --------------------------------------------------------------------------- #
# Test 1 — fanout happy path (SRT)                                            #
# --------------------------------------------------------------------------- #


def test_srt_byte_sink_fanout_sees_ts_packets() -> None:
    """A registered sink sees > 0 packets, each exactly 188 bytes and a
    valid TS packet (sync byte 0x47)."""
    port = _free_tcp_port()
    sender, receiver = _make_srt_pair(port)

    captured: list[bytes] = []
    receiver.add_byte_sink(captured.append)

    events: list[object] = []
    consumer_err: list[BaseException] = []

    def consumer() -> None:
        try:
            for ev in receiver:
                events.append(ev)
                if isinstance(ev, DemuxEvent.Video):
                    break
        except BaseException as exc:  # noqa: BLE001
            consumer_err.append(exc)

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
    receiver.close()

    if not captured and consumer_err:
        pytest.fail(f"consumer raised before any packet: {consumer_err}")
    assert len(captured) > 0, "byte sink saw no packets"
    assert all(len(pkt) == 188 for pkt in captured), "packet was not 188 bytes"
    assert all(pkt[0] == 0x47 for pkt in captured), "packet missing TS sync byte"
    # The captured stream must re-demux into the same shape (PAT/PMT/video).
    assert any(
        isinstance(e, (DemuxEvent.Video, DemuxEvent.ProgramMap)) for e in events
    )


# --------------------------------------------------------------------------- #
# Test 2 — fail-loud: callback exception crosses back out of __next__ (SRT)   #
# --------------------------------------------------------------------------- #


def test_srt_byte_sink_failure_propagates_fail_loud() -> None:
    """A sink that raises makes the receiver re-raise that exception from
    the iterating call."""
    port = _free_tcp_port()
    sender, receiver = _make_srt_pair(port)

    def boom(_pkt: bytes) -> None:
        raise ValueError("boom")

    receiver.add_byte_sink(boom)

    raised: list[BaseException] = []
    stopped = threading.Event()

    def consumer() -> None:
        try:
            for _ev in receiver:
                pass
        except BaseException as exc:  # noqa: BLE001
            raised.append(exc)
        finally:
            stopped.set()

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
    stopped.wait(timeout=5.0)
    receiver.close()

    assert raised, "no exception propagated out of the iterating call"
    assert any(
        isinstance(e, ValueError) and "boom" in str(e) for e in raised
    ), f"expected the sink's ValueError('boom'); got {raised}"


# --------------------------------------------------------------------------- #
# Test 3 — two sinks fire in registration order (SRT)                         #
# --------------------------------------------------------------------------- #


def test_srt_two_byte_sinks_fire_in_registration_order() -> None:
    port = _free_tcp_port()
    sender, receiver = _make_srt_pair(port)

    log: list[str] = []
    log_lock = threading.Lock()

    def sink_a(_pkt: bytes) -> None:
        with log_lock:
            log.append("a")

    def sink_b(_pkt: bytes) -> None:
        with log_lock:
            log.append("b")

    receiver.add_byte_sink(sink_a)
    receiver.add_byte_sink(sink_b)

    consumer_err: list[BaseException] = []

    def consumer() -> None:
        try:
            for ev in receiver:
                if isinstance(ev, DemuxEvent.Video):
                    break
        except BaseException as exc:  # noqa: BLE001
            consumer_err.append(exc)

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
    receiver.close()

    with log_lock:
        snapshot = list(log)
    assert len(snapshot) >= 2, f"expected both sinks to fire; got {snapshot}"
    # Per packet, 'a' precedes 'b' — so the flattened log is "abab...".
    assert snapshot[0] == "a" and snapshot[1] == "b", (
        f"sinks did not fire in registration order: {snapshot[:4]}"
    )


# --------------------------------------------------------------------------- #
# Test 4 — add_byte_sink on a closed receiver raises the closed-error (SRT)   #
# --------------------------------------------------------------------------- #


def test_srt_add_byte_sink_on_closed_raises_closed() -> None:
    port = _free_tcp_port()
    sender, receiver = _make_srt_pair(port)
    sender.close()
    receiver.close()
    with pytest.raises(SrtError) as exc_info:
        receiver.add_byte_sink(lambda _pkt: None)
    assert exc_info.value.kind == SrtErrorKind.CLOSED


# --------------------------------------------------------------------------- #
# RTP — fanout happy path                                                     #
# --------------------------------------------------------------------------- #


def test_rtp_byte_sink_fanout_sees_ts_packets() -> None:
    port = _free_udp_port()
    rx = tstrans.rtp.DemuxReceiver(f"rtp://127.0.0.1:{port}")

    captured: list[bytes] = []
    rx.add_byte_sink(captured.append)

    events: list[object] = []
    err: list[BaseException] = []

    def consumer() -> None:
        try:
            for ev in rx:
                events.append(ev)
                break
        except BaseException as exc:  # noqa: BLE001
            err.append(exc)

    t = threading.Thread(target=consumer, daemon=True)
    t.start()
    time.sleep(0.2)
    ts_bytes = _build_minimal_ts_bytes()
    with tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}") as snd:
        for _ in range(3):
            for i in range(0, len(ts_bytes), 188):
                snd.send(ts_bytes[i : i + 188])
            time.sleep(0.05)
    t.join(timeout=3.0)
    rx.close()

    if err and not captured:
        pytest.fail(f"consumer raised before any packet: {err}")
    assert len(captured) > 0, "byte sink saw no packets"
    assert all(len(pkt) == 188 for pkt in captured)
    assert all(pkt[0] == 0x47 for pkt in captured)


# --------------------------------------------------------------------------- #
# RTP — fail-loud                                                             #
# --------------------------------------------------------------------------- #


def test_rtp_byte_sink_failure_propagates_fail_loud() -> None:
    port = _free_udp_port()
    rx = tstrans.rtp.DemuxReceiver(f"rtp://127.0.0.1:{port}")

    def boom(_pkt: bytes) -> None:
        raise ValueError("boom")

    rx.add_byte_sink(boom)

    raised: list[BaseException] = []
    stopped = threading.Event()

    def consumer() -> None:
        try:
            for _ev in rx:
                pass
        except BaseException as exc:  # noqa: BLE001
            raised.append(exc)
        finally:
            stopped.set()

    t = threading.Thread(target=consumer, daemon=True)
    t.start()
    time.sleep(0.2)
    ts_bytes = _build_minimal_ts_bytes()
    with tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}") as snd:
        for _ in range(3):
            for i in range(0, len(ts_bytes), 188):
                snd.send(ts_bytes[i : i + 188])
            time.sleep(0.05)
    stopped.wait(timeout=5.0)
    rx.close()

    assert raised, "no exception propagated out of the iterating call"
    assert any(
        isinstance(e, ValueError) and "boom" in str(e) for e in raised
    ), f"expected the sink's ValueError('boom'); got {raised}"


# --------------------------------------------------------------------------- #
# RTP — two sinks fire in registration order                                  #
# --------------------------------------------------------------------------- #


def test_rtp_two_byte_sinks_fire_in_registration_order() -> None:
    port = _free_udp_port()
    rx = tstrans.rtp.DemuxReceiver(f"rtp://127.0.0.1:{port}")

    log: list[str] = []
    log_lock = threading.Lock()

    def sink_a(_pkt: bytes) -> None:
        with log_lock:
            log.append("a")

    def sink_b(_pkt: bytes) -> None:
        with log_lock:
            log.append("b")

    rx.add_byte_sink(sink_a)
    rx.add_byte_sink(sink_b)

    err: list[BaseException] = []

    def consumer() -> None:
        try:
            for _ev in rx:
                break
        except BaseException as exc:  # noqa: BLE001
            err.append(exc)

    t = threading.Thread(target=consumer, daemon=True)
    t.start()
    time.sleep(0.2)
    ts_bytes = _build_minimal_ts_bytes()
    with tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}") as snd:
        for _ in range(3):
            for i in range(0, len(ts_bytes), 188):
                snd.send(ts_bytes[i : i + 188])
            time.sleep(0.05)
    t.join(timeout=3.0)
    rx.close()

    with log_lock:
        snapshot = list(log)
    assert len(snapshot) >= 2, f"expected both sinks to fire; got {snapshot}"
    assert snapshot[0] == "a" and snapshot[1] == "b", (
        f"sinks did not fire in registration order: {snapshot[:4]}"
    )


# --------------------------------------------------------------------------- #
# RTP — add_byte_sink on a closed receiver raises the closed-error           #
# --------------------------------------------------------------------------- #


def test_rtp_add_byte_sink_on_closed_raises() -> None:
    port = _free_udp_port()
    rx = tstrans.rtp.DemuxReceiver(f"rtp://127.0.0.1:{port}")
    rx.close()
    with pytest.raises(RtpError) as exc_info:
        rx.add_byte_sink(lambda _pkt: None)
    assert exc_info.value.kind == RtpErrorKind.CLOSED
