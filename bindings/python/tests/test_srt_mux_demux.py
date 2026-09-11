"""Tests for `tstrans.srt.MuxSender` + `tstrans.srt.DemuxReceiver`
(Wave B Task 5).

Loopback-only tests against a libsrt caller<->listener pair on a single
host. Each test spawns a listener-side worker on a background thread
before the caller-side construct connects; the test joins the worker at
the end. No external network is required.
"""

from __future__ import annotations

import threading
import time
from typing import Optional, Tuple

import pytest

import tstrans
import tstrans.srt
from tstrans.exceptions import (
    MuxError,
    MuxErrorKind,
    SrtError,
    SrtErrorKind,
)
from tstrans.mpegts import (
    DemuxEvent,
    KlvStreamType,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)

from _builders.mux_programs import video_only_program as _video_only_program
from _builders.ports import free_tcp_port as _free_tcp_port


# --------------------------------------------------------------------------- #
# Helpers                                                                     #
# --------------------------------------------------------------------------- #


def _video_klv_program() -> object:
    return (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )


def _video_data_program() -> object:
    """Video + one private data stream (bare PES-private 0x06, W3)."""
    return (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_data(0x1F0, 0x06, carries_pts=True)
        .build()
    )


def _make_mux_demux_pair(
    port: int, *, program: Optional[object] = None
) -> Tuple[tstrans.srt.MuxSender, tstrans.srt.DemuxReceiver]:
    """Spawn a listener-mode DemuxReceiver on a background thread; once
    it's accepting, connect a caller-mode MuxSender from the main
    thread. Returns (mux_sender, demux_receiver). Callers must
    `.close()` both."""
    prog = program if program is not None else _video_only_program()
    listener_url = f"srt://:{port}?mode=listener"
    caller_url = f"srt://127.0.0.1:{port}?mode=caller"

    rx_box: list[tstrans.srt.DemuxReceiver] = []
    rx_err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            r = tstrans.srt.DemuxReceiver.from_url(listener_url)
            rx_box.append(r)
        except BaseException as exc:  # noqa: BLE001
            rx_err.append(exc)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    # libsrt's bind+listen is fast but not instant; sleep before
    # connecting from the main thread to avoid racing the listener.
    time.sleep(0.1)
    sender = tstrans.srt.MuxSender.from_url(caller_url, prog)
    t.join(timeout=5.0)
    if rx_err:
        sender.close()
        raise rx_err[0]
    if not rx_box:
        sender.close()
        raise RuntimeError("DemuxReceiver listener thread did not accept within 5 s")
    return sender, rx_box[0]


# Minimal Annex-B IDR NAL (start code + nal_unit_type=5).
NAL_IDR = b"\x00\x00\x00\x01\x65\xBB"
# Minimal AU delimiter NAL.
NAL_AUD = b"\x00\x00\x00\x01\x09\xF0"
# A 17-byte KLV LS with UL=ST 0601 (universal label only, no payload).
KLV_UL_ZERO = (
    b"\x06\x0E\x2B\x34\x02\x0B\x01\x01"
    b"\x0E\x01\x03\x01\x01\x00\x00\x00\x00"
)
# Opaque private-data record — the muxer applies no framing or
# inspection, so any byte string works.
DATA_RECORD = b"\x01\x02\x03\x04private-record"


# --------------------------------------------------------------------------- #
# Module surface                                                              #
# --------------------------------------------------------------------------- #


def test_mux_demux_module_re_exports() -> None:
    """`tstrans.srt.{MuxSender,DemuxReceiver}` must be exposed after T5."""
    assert tstrans.srt.MuxSender is not None
    assert tstrans.srt.DemuxReceiver is not None
    assert "MuxSender" in tstrans.srt.__all__
    assert "DemuxReceiver" in tstrans.srt.__all__


# --------------------------------------------------------------------------- #
# Construction-time errors                                                    #
# --------------------------------------------------------------------------- #


def test_mux_sender_wrong_mode_raises_config_invalid() -> None:
    """`MuxSender.from_url` requires `?mode=caller`; explicit listener
    URL → SrtError(CONFIG_INVALID)."""
    port = _free_tcp_port()
    with pytest.raises(SrtError) as exc_info:
        tstrans.srt.MuxSender.from_url(
            f"srt://127.0.0.1:{port}?mode=listener", _video_only_program()
        )
    assert exc_info.value.kind == SrtErrorKind.CONFIG_INVALID


def test_demux_receiver_wrong_mode_raises_config_invalid() -> None:
    """`DemuxReceiver.from_url` requires `?mode=listener`; explicit
    caller URL → SrtError(CONFIG_INVALID)."""
    port = _free_tcp_port()
    with pytest.raises(SrtError) as exc_info:
        tstrans.srt.DemuxReceiver.from_url(
            f"srt://127.0.0.1:{port}?mode=caller"
        )
    assert exc_info.value.kind == SrtErrorKind.CONFIG_INVALID


def test_mux_sender_bad_url_raises_config_invalid() -> None:
    with pytest.raises(SrtError) as exc_info:
        tstrans.srt.MuxSender.from_url("not-a-valid-url", _video_only_program())
    assert exc_info.value.kind == SrtErrorKind.CONFIG_INVALID


# --------------------------------------------------------------------------- #
# Test 1: from_url round-trip — MuxSender + DemuxReceiver via loopback         #
# --------------------------------------------------------------------------- #


def test_from_url_round_trip_via_loopback() -> None:
    """End-to-end: MuxSender.from_url(caller) <-> DemuxReceiver.from_url
    (listener). Push a video NAL; verify the DemuxReceiver iterates a
    Sample/Video event without raising.

    Pushes a large enough burst that libsrt's bundle (7 × 188 = 1316 B)
    is fully fed by the time the receiver-side close+EOF arrives. A
    short sleep after the burst gives the kernel time to drain the
    send queue before close() races the in-flight bytes."""
    port = _free_tcp_port()
    sender, receiver = _make_mux_demux_pair(port)

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
    # Sleep so the receiver thread is parked inside recv_event before
    # we start pushing bytes.
    time.sleep(0.2)
    # Push a generous burst of key NALs — PSI is emitted every key
    # frame and the bundle threshold is 7 TS packets. Drive enough that
    # the demuxer crosses several PSI+sample cycles before close.
    try:
        for i in range(32):
            sender.send_video(
                NAL_IDR, pts=Pts90khz.from_raw(i * 3000), key_frame=(i % 4 == 0)
            )
        # Let libsrt drain the send queue before close() — close races
        # in-flight bytes and a fast close on a fast loopback often
        # tears the connection mid-stream from the receiver's POV.
        time.sleep(0.3)
    finally:
        sender.close()
    t.join(timeout=5.0)
    receiver.close()
    # A "connection broken" after the bytes drain is OK if we already
    # captured at least one event; otherwise it's a genuine failure.
    saw_video_or_pmt = any(
        isinstance(e, (DemuxEvent.Video, DemuxEvent.ProgramMap)) for e in events
    )
    if not saw_video_or_pmt and consumer_err:
        pytest.fail(f"consumer raised before any event: {consumer_err}")
    assert saw_video_or_pmt, (
        f"expected at least one Video or ProgramMap event; got: "
        f"{[type(e).__name__ for e in events]}"
    )


# --------------------------------------------------------------------------- #
# Test 2: send_klv via loopback + KLV handle accessor                         #
# --------------------------------------------------------------------------- #


def test_send_klv_via_loopback() -> None:
    """Send a KLV blob through a video+klv program; assert the KLV
    handle accessor returns something + the demux side sees the event
    (Metadata or ProgramMap before stopping)."""
    port = _free_tcp_port()
    sender, receiver = _make_mux_demux_pair(port, program=_video_klv_program())

    events: list[object] = []
    consumer_err: list[BaseException] = []

    def consumer() -> None:
        try:
            for ev in receiver:
                events.append(ev)
                if isinstance(ev, (DemuxEvent.Metadata, DemuxEvent.Video)):
                    break
        except BaseException as exc:  # noqa: BLE001
            consumer_err.append(exc)

    t = threading.Thread(target=consumer, daemon=True)
    t.start()
    time.sleep(0.2)
    try:
        # KLV handle accessor must resolve since a KLV stream is configured.
        klv_h = sender.klv_handle()
        assert klv_h is not None
        # Drive enough PSI+video+klv triplets that libsrt has streamed
        # several full bundles before close() can race.
        for i in range(32):
            sender.send_video(
                NAL_IDR, pts=Pts90khz.from_raw(i * 3000), key_frame=(i % 4 == 0)
            )
            sender.send_klv_to(klv_h, KLV_UL_ZERO, pts=Pts90khz.from_raw(i * 3000))
        time.sleep(0.3)
    finally:
        sender.close()
    t.join(timeout=5.0)
    receiver.close()
    if not events and consumer_err:
        pytest.fail(f"consumer raised before any event: {consumer_err}")
    assert len(events) >= 1, "consumer did not observe any DemuxEvent"


# --------------------------------------------------------------------------- #
# Test 2b: send_data via loopback + data handle accessor (W3)                 #
# --------------------------------------------------------------------------- #


def test_send_data_via_loopback() -> None:
    """Send opaque private-data records through a video+data program;
    assert the data handle accessor returns something + the demux side
    sees an event (UnknownSample or Video before stopping). Exercises
    both `send_data` (single-stream shorthand) and `send_data_to`."""
    port = _free_tcp_port()
    sender, receiver = _make_mux_demux_pair(port, program=_video_data_program())

    events: list[object] = []
    consumer_err: list[BaseException] = []

    def consumer() -> None:
        try:
            for ev in receiver:
                events.append(ev)
                if isinstance(ev, (DemuxEvent.UnknownSample, DemuxEvent.Video)):
                    break
        except BaseException as exc:  # noqa: BLE001
            consumer_err.append(exc)

    t = threading.Thread(target=consumer, daemon=True)
    t.start()
    time.sleep(0.2)
    try:
        # Data handle accessor must resolve since a data stream is configured.
        data_h = sender.data_handle()
        assert data_h is not None
        # Drive enough PSI+video+data triplets that libsrt has streamed
        # several full bundles before close() can race.
        for i in range(32):
            sender.send_video(
                NAL_IDR, pts=Pts90khz.from_raw(i * 3000), key_frame=(i % 4 == 0)
            )
            # Alternate the single-stream shorthand and the explicit
            # handle variant so both code paths see traffic.
            if i % 2 == 0:
                sender.send_data(DATA_RECORD, pts=Pts90khz.from_raw(i * 3000))
            else:
                sender.send_data_to(
                    data_h, DATA_RECORD, pts=Pts90khz.from_raw(i * 3000)
                )
        time.sleep(0.3)
    finally:
        sender.close()
    t.join(timeout=5.0)
    receiver.close()
    if not events and consumer_err:
        pytest.fail(f"consumer raised before any event: {consumer_err}")
    assert len(events) >= 1, "consumer did not observe any DemuxEvent"


# --------------------------------------------------------------------------- #
# Test 3: handle getters + send_video_to + bytes-like extraction              #
# --------------------------------------------------------------------------- #


def test_handle_getters_and_send_to() -> None:
    """Verify handle accessors + the `_to` send variants on the SRT
    MuxSender side. We don't need a live receiver for this — the
    listener thread accepts then the receiver sits idle while we
    exercise the sender API surface."""
    port = _free_tcp_port()
    sender, receiver = _make_mux_demux_pair(port)
    try:
        # Single-video program: video_handle resolves, others are None.
        vh = sender.video_handle()
        assert vh is not None
        assert sender.klv_handle() is None
        assert sender.audio_handle() is None
        assert sender.subtitle_handle() is None
        assert sender.data_handle() is None
        # _to variant works.
        sender.send_video_to(vh, NAL_AUD, pts=Pts90khz.from_raw(0))
        # bytes-like coercion: bytearray + memoryview both round-trip
        # through the audit-#10 two-path bytes-like helper.
        sender.send_video(bytearray(NAL_AUD), pts=Pts90khz.from_raw(3000))
        sender.send_video(memoryview(bytearray(NAL_AUD)), pts=Pts90khz.from_raw(6000))
        # libsrt does not flush its `pktSentTotal` counter synchronously
        # with `srt_send`; on a fast loopback the stats snapshot can race
        # the send queue and read 0. Give the send path a moment to drain
        # before sampling (matches the drain-before-stats convention in
        # test_srt_transport.py and the pre-close sleeps elsewhere here).
        time.sleep(0.3)
        sock_stats, mux_stats = sender.stats()
        assert sock_stats.packets_sent >= 1
        assert mux_stats.programs_configured == 1
    finally:
        sender.close()
        receiver.close()


# --------------------------------------------------------------------------- #
# Test 4: stats() returns (SocketStats, MuxerStats)                           #
# --------------------------------------------------------------------------- #


def test_mux_sender_stats_tuple_shape() -> None:
    """`MuxSender.stats()` returns a 2-tuple of (SocketStats,
    MuxerStats)."""
    port = _free_tcp_port()
    sender, receiver = _make_mux_demux_pair(port)
    try:
        stats = sender.stats()
        assert isinstance(stats, tuple)
        assert len(stats) == 2
        sock_stats, mux_stats = stats
        # tstrans.srt.SocketStats class identity.
        assert type(sock_stats).__name__ == "SocketStats"
        # MuxerStats lives in tstrans.mpegts but is re-used by the
        # MuxSender wrapper.
        assert hasattr(mux_stats, "ts_packets_emitted")
        assert hasattr(mux_stats, "programs_configured")
        assert mux_stats.programs_configured == 1
    finally:
        sender.close()
        receiver.close()


def test_demux_receiver_stats_tuple_shape() -> None:
    """`DemuxReceiver.stats()` returns a 2-tuple of (SocketStats,
    MuxerStats). Mirror of MuxSender for shape parity."""
    port = _free_tcp_port()
    sender, receiver = _make_mux_demux_pair(port)
    try:
        stats = receiver.stats()
        assert isinstance(stats, tuple)
        assert len(stats) == 2
        sock_stats, mux_stats = stats
        assert hasattr(sock_stats, "packets_received")
        assert hasattr(mux_stats, "ts_packets_emitted")
    finally:
        sender.close()
        receiver.close()


# --------------------------------------------------------------------------- #
# Test 4b: last_seen_micros(pid)                                              #
# --------------------------------------------------------------------------- #


def test_last_seen_micros_via_loopback() -> None:
    """Before any traffic, `last_seen_micros(0x101)` is None. After a
    Video event for pid 0x101 has been consumed, it reports an
    epoch-microsecond int; a PID that never carried traffic (0x1FFF)
    stays None even once the stream is flowing."""
    port = _free_tcp_port()
    sender, receiver = _make_mux_demux_pair(port)
    try:
        assert receiver.last_seen_micros(0x101) is None

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
        saw_video = any(isinstance(e, DemuxEvent.Video) for e in events)
        if not saw_video and consumer_err:
            pytest.fail(f"consumer raised before a Video event: {consumer_err}")
        assert saw_video, (
            f"expected a Video event, got: {[type(e).__name__ for e in events]}"
        )
        seen = receiver.last_seen_micros(0x101)
        assert isinstance(seen, int) and seen > 0
        assert receiver.last_seen_micros(0x1FFF) is None
    finally:
        receiver.close()


# --------------------------------------------------------------------------- #
# Test 5: Socket.into_mux_sender + Socket.into_demux_receiver promotion       #
# --------------------------------------------------------------------------- #


def test_socket_into_mux_sender_promotion() -> None:
    """`Builder.connect().into_mux_sender(program_config)` produces a
    fully-functional MuxSender. Verifies the T3 NotImplementedError
    stub is replaced by a real impl."""
    port = _free_tcp_port()
    # Listener side spawned by the helper, but we drive the caller side
    # manually via Builder + into_mux_sender.
    listener_url = f"srt://:{port}?mode=listener"
    rx_box: list[tstrans.srt.DemuxReceiver] = []

    def accept_worker() -> None:
        try:
            r = tstrans.srt.DemuxReceiver.from_url(listener_url)
            rx_box.append(r)
        except BaseException:
            pass

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)
    sock = (
        tstrans.srt.Builder(f"srt://127.0.0.1:{port}?mode=caller")
        .caller()
        .connect()
    )
    program = _video_only_program()
    sender = sock.into_mux_sender(program)
    # Socket should now be closed (consumed).
    assert not sock.is_alive()
    # Pushing should work via the promoted sender.
    sender.send_video(NAL_AUD, pts=Pts90khz.from_raw(0))
    sender.close()
    t.join(timeout=5.0)
    if rx_box:
        rx_box[0].close()


def test_socket_into_demux_receiver_promotion() -> None:
    """`Listener.accept().into_demux_receiver()` produces a
    DemuxReceiver. Verifies the second T3 NotImplementedError stub is
    replaced."""
    port = _free_tcp_port()
    # Spawn the listener-side accept on a worker thread, then connect a
    # MuxSender from the main thread (so libsrt actually accepts the
    # peer and Listener.accept returns).
    bldr = tstrans.srt.Builder(f"srt://:{port}?mode=listener").listener()
    listener = bldr.listen()

    sender_box: list[tstrans.srt.MuxSender] = []

    def caller_worker() -> None:
        try:
            time.sleep(0.1)
            s = tstrans.srt.MuxSender.from_url(
                f"srt://127.0.0.1:{port}?mode=caller", _video_only_program()
            )
            sender_box.append(s)
        except BaseException:
            pass

    t = threading.Thread(target=caller_worker, daemon=True)
    t.start()
    sock = listener.accept()
    receiver = sock.into_demux_receiver()
    assert not sock.is_alive()
    assert "open" in repr(receiver)
    t.join(timeout=5.0)
    if sender_box:
        sender_box[0].close()
    receiver.close()
    listener.close()


# --------------------------------------------------------------------------- #
# Test 6: context manager + idempotent close                                  #
# --------------------------------------------------------------------------- #


def test_mux_sender_context_manager_closes_cleanly() -> None:
    """`with MuxSender(...) as s: ...` closes the sender on exit;
    repeated close is a no-op. Mirror test for DemuxReceiver follows."""
    port = _free_tcp_port()
    sender, receiver = _make_mux_demux_pair(port)
    try:
        with sender as s:
            assert "open" in repr(s)
            s.send_video(NAL_AUD, pts=Pts90khz.from_raw(0))
        # After __exit__, sender is closed.
        assert "closed" in repr(sender)
        # Idempotent close.
        sender.close()
        sender.close()
    finally:
        receiver.close()


def test_demux_receiver_context_manager_closes_cleanly() -> None:
    port = _free_tcp_port()
    sender, receiver = _make_mux_demux_pair(port)
    try:
        with receiver as rx:
            assert "open" in repr(rx)
        # After __exit__, receiver is closed.
        assert "closed" in repr(receiver)
        receiver.close()  # idempotent
    finally:
        sender.close()


def test_demux_receiver_close_while_next_parked_cancels_first() -> None:
    """`close()` from another thread while `__next__` is parked cancels
    first: it returns promptly and the parked iteration ends with
    `SrtError(BROKEN)` (the plain cancel handle closes the libsrt socket;
    the managed shell is the one that maps its own cancel to CLOSED). The
    contract shared with the JVM plain `DemuxReceiver.close()` and the C
    ABI's `tst_demux_receiver_close`."""
    port = _free_tcp_port()
    sender, receiver = _make_mux_demux_pair(port)
    outcome: dict[str, object] = {}

    def iterator() -> None:
        try:
            for _ev in receiver:
                pass
            outcome["end"] = "StopIteration"
        except SrtError as exc:
            outcome["end"] = "SrtError"
            outcome["kind"] = exc.kind
        except BaseException as exc:  # noqa: BLE001
            outcome["end"] = type(exc).__name__

    t = threading.Thread(target=iterator, daemon=True)
    t.start()
    # Park the iterator inside recv_event before closing; the peer is
    # connected and silent, so only a cancel can end that receive.
    time.sleep(0.3)

    # close() on its own thread with a bounded join: a regression to
    # waiting behind the parked recv fails the verdict below instead of
    # hanging the test.
    c = threading.Thread(target=receiver.close, daemon=True)
    try:
        c.start()
        c.join(timeout=5.0)
        t.join(timeout=5.0)
        if c.is_alive() or t.is_alive():
            # Rescue: a frame from the still-connected peer unparks the recv
            # so the daemon threads do not sit in a native read at exit.
            sender.send_video(NAL_IDR, pts=Pts90khz.from_raw(0), key_frame=True)
            t.join(timeout=5.0)
            c.join(timeout=5.0)
            pytest.fail("close() did not end the parked iteration within 5 s")
        assert outcome.get("end") == "SrtError", f"iteration ended via {outcome}"
        assert outcome.get("kind") == SrtErrorKind.BROKEN, f"unexpected kind: {outcome}"
    finally:
        sender.close()
        receiver.close()


def test_demux_receiver_iter_returns_self() -> None:
    port = _free_tcp_port()
    sender, receiver = _make_mux_demux_pair(port)
    try:
        assert iter(receiver) is receiver
    finally:
        sender.close()
        receiver.close()


# --------------------------------------------------------------------------- #
# Test 7: error mapping — closed sender, malformed NAL                        #
# --------------------------------------------------------------------------- #


def test_send_video_on_closed_sender_raises_closed() -> None:
    """Sending on a closed MuxSender raises `SrtError(CLOSED)`."""
    port = _free_tcp_port()
    sender, receiver = _make_mux_demux_pair(port)
    sender.close()
    receiver.close()
    with pytest.raises(SrtError) as exc_info:
        sender.send_video(NAL_IDR, pts=Pts90khz.from_raw(0))
    assert exc_info.value.kind == SrtErrorKind.CLOSED


def test_send_video_malformed_nal_raises_mux_error() -> None:
    """Raw bytes without an Annex-B start code → MuxError(INPUT_MALFORMED)."""
    port = _free_tcp_port()
    sender, receiver = _make_mux_demux_pair(port)
    try:
        with pytest.raises(MuxError) as exc_info:
            sender.send_video(b"not annex-b bytes", pts=Pts90khz.from_raw(0))
        assert exc_info.value.kind == MuxErrorKind.INPUT_MALFORMED
    finally:
        sender.close()
        receiver.close()


# --------------------------------------------------------------------------- #
# Test 8: demux_config dataclass propagation                                  #
# --------------------------------------------------------------------------- #


def test_demux_receiver_accepts_demux_config() -> None:
    """Pass an explicit `DemuxerConfig` dataclass to
    `DemuxReceiver.from_url(demux_config=...)`. Smoke — verifies the
    helper routes through `crate::mpegts::build_demuxer_config`."""
    from tstrans.mpegts import DemuxerConfig, StrictMode

    port = _free_tcp_port()
    listener_url = f"srt://:{port}?mode=listener"
    caller_url = f"srt://127.0.0.1:{port}?mode=caller"
    cfg = DemuxerConfig(strict_mode=StrictMode.OFF)

    rx_box: list[tstrans.srt.DemuxReceiver] = []
    rx_err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            r = tstrans.srt.DemuxReceiver.from_url(listener_url, demux_config=cfg)
            rx_box.append(r)
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
    assert rx_box, "demux receiver did not accept under demux_config kwarg"
    sender.close()
    rx_box[0].close()
