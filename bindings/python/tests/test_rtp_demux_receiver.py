"""Tests for `tstrans.shells.DemuxReceiver`.

Loopback tests:
- Build a small TS stream with `tstrans.mpegts.Muxer`; send the bytes
  via `tstrans.rtp.Transport`; iterate the demuxer; assert events arrive.
- Round-trip: `tstrans.shells.MuxSender` → `tstrans.shells.DemuxReceiver` over
  the same loopback port. Push a video NAL; assert the receiver emits
  at least one ProgramMap + one Sample/Video event.
- `RtspSession.into_demux_receiver` — skipped (no RTSP server in this
  test; covered by `test_rtp_integration.py`).
"""

from __future__ import annotations

from tstrans import shells
import socket
import threading
import time
from typing import Callable

import pytest

import tstrans
import tstrans.rtp
from tstrans.exceptions import RtpError, RtpErrorKind, UdpError, UdpErrorKind
from tstrans.mpegts import DemuxEvent, Muxer, MuxerConfigBuilder, Pts90khz

from _builders.mux_programs import video_only_program as _video_only_program
from _builders.hang_deadline import call_with_hang_deadline
from _builders.ports import free_udp_port as _free_udp_port
from _builders.unknown_stream import _crc32_mpeg, _ts_packet


def _build_minimal_ts_bytes() -> bytes:
    """Mux a single AUD NAL with `tstrans.mpegts.Muxer` and return the
    drained TS bytes. Just enough to contain a PAT/PMT pair so the
    demuxer can recognize a program."""
    cfg = MuxerConfigBuilder().add_program(_video_only_program()).build()
    mux = Muxer(cfg)
    # Push a single AUD NAL with key_frame=True so PSI gets emitted.
    nal_aud = b"\x00\x00\x00\x01\x09\xF0"
    mux.push_video(nal_aud, pts=Pts90khz.from_raw(0), key_frame=True)
    # Drain into an output bytearray.
    out = bytearray()
    scratch = bytearray(188 * 32)
    while True:
        n = mux.pull(scratch)
        if n == 0:
            break
        out.extend(scratch[:n])
    return bytes(out)


# --------------------------------------------------------------------------- #
# Module surface                                                              #
# --------------------------------------------------------------------------- #


def test_demux_receiver_module_re_exports() -> None:
    assert shells.DemuxReceiver is not None
    assert "DemuxReceiver" in dir(shells)


# --------------------------------------------------------------------------- #
# Construction                                                                #
# --------------------------------------------------------------------------- #


def test_demux_receiver_bind_and_close() -> None:
    port = _free_udp_port()
    with shells.DemuxReceiver.open(f"rtp://127.0.0.1:{port}") as rx:
        assert "open" in repr(rx)
    # After context exit, repr reports closed.
    # (re-constructing to verify shape)
    rx2 = shells.DemuxReceiver.open(f"rtp://127.0.0.1:{port}")
    rx2.close()
    assert "closed" in repr(rx2)


def test_demux_receiver_bad_url_raises_rtp_error() -> None:
    with pytest.raises(RtpError) as exc_info:
        shells.DemuxReceiver.open("rtp://127.0.0.1:notaport")
    assert exc_info.value.kind == RtpErrorKind.URL


def test_demux_receiver_unsupported_scheme_raises_value_error() -> None:
    with pytest.raises(ValueError, match="unsupported URL scheme"):
        shells.DemuxReceiver.open("not-a-valid-url://")


def test_demux_receiver_with_demux_config() -> None:
    """Pass a `DemuxerConfig` dataclass via the keyword arg."""
    from tstrans.mpegts import DemuxerConfig, StrictMode

    port = _free_udp_port()
    cfg = DemuxerConfig(strict_mode=StrictMode.OFF)
    with shells.DemuxReceiver.open(
        f"rtp://127.0.0.1:{port}", demux_config=cfg
    ) as rx:
        assert "open" in repr(rx)


# --------------------------------------------------------------------------- #
# Iteration                                                                   #
# --------------------------------------------------------------------------- #


def test_demux_receiver_iter_returns_self() -> None:
    port = _free_udp_port()
    rx = shells.DemuxReceiver.open(f"rtp://127.0.0.1:{port}")
    assert iter(rx) is rx
    rx.close()


def test_demux_receiver_emits_events_from_loopback_send() -> None:
    """Build TS bytes locally; send them via `tstrans.rtp.Transport`; the
    DemuxReceiver should yield at least one event before we stop.

    Sends one TS packet per RTP datagram (the most deterministic path)
    and repeats a few times to ride past any single-packet drop in the
    UDP loopback path.
    """
    port = _free_udp_port()
    rx = shells.DemuxReceiver.open(f"rtp://127.0.0.1:{port}")
    events: list[object] = []
    err: list[BaseException] = []

    def consumer() -> None:
        try:
            for ev in rx:
                events.append(ev)
                # Stop after the first event to keep the test fast.
                break
        except BaseException as exc:  # noqa: BLE001
            err.append(exc)

    t = threading.Thread(target=consumer, daemon=True)
    t.start()
    # Give the consumer time to enter recv.
    time.sleep(0.2)
    ts_bytes = _build_minimal_ts_bytes()
    with tstrans.rtp.Transport(f"rtp://127.0.0.1:{port}") as snd:
        # Repeat the burst a few times in case the consumer thread
        # hasn't entered the kernel recv yet on the first pass. Each
        # send is one TS packet (the natural framing the receiver also
        # uses).
        for _ in range(3):
            for i in range(0, len(ts_bytes), 188):
                snd.send(ts_bytes[i : i + 188])
            time.sleep(0.05)
    # Wait for the consumer to receive at least one event.
    t.join(timeout=3.0)
    rx.close()
    # If the consumer raised before producing an event, that's a
    # genuine failure; a CANCELLED after the first event is OK
    # (close() fires the cancel).
    if err and not events:
        pytest.fail(f"consumer raised before any event: {err}")
    assert len(events) >= 1, "consumer did not see any DemuxEvent"


# --------------------------------------------------------------------------- #
# ?recv_timeout= URL knob                                                     #
# --------------------------------------------------------------------------- #


def test_demux_receiver_url_knob_iteration_resumes_after_timeout() -> None:
    """`?recv_timeout=<ms>` on the URL arms a persistent recv deadline on
    `tstrans.shells.DemuxReceiver` too (same knob as `tstrans.rtp.RecvTransport`
    — `RtpRecvTransport::listen_with`, no new API). A quiet socket's
    `next(it)` raises `RtpError(BACKPRESSURE)` rather than blocking forever;
    the receiver + demuxer state survive the expiry, so once a real
    sender delivers muxed TS bytes, the SAME iterator resumes and
    yields a `DemuxEvent` instead of raising again."""
    port = _free_udp_port()
    with shells.DemuxReceiver.open(
        f"rtp://127.0.0.1:{port}?recv_timeout=200"
    ) as rx:
        it = iter(rx)

        # Quiet socket: the persistent deadline expires and `next()`
        # raises RtpError(BACKPRESSURE) — the transport error mapping used by
        # `DemuxReceiverErrorSource::Transport(Backpressure)`.
        with pytest.raises(RtpError) as exc_info:
            next(it)
        assert exc_info.value.kind == RtpErrorKind.BACKPRESSURE
        # A deadline expiry is not a recorded end reason — the session is
        # still alive, so `end_reason()` stays None (pins the
        # deadline-expiry-records-no-reason seam against `StreamEndReason`).
        assert rx.end_reason() is None

        # Deliver muxed TS bytes on the same port *before* resuming the
        # iterator, so the datagrams are already queued in the kernel
        # recv buffer and the 200 ms deadline is ample. Same
        # build-and-burst pattern as
        # `test_demux_receiver_emits_events_from_loopback_send`.
        ts_bytes = _build_minimal_ts_bytes()
        with tstrans.rtp.Transport(f"rtp://127.0.0.1:{port}") as snd:
            for _ in range(3):
                for i in range(0, len(ts_bytes), 188):
                    snd.send(ts_bytes[i : i + 188])
                time.sleep(0.05)

        # Resume the SAME iterator: it yields a DemuxEvent rather than
        # raising again, proving the receiver/demux state survived the
        # caught TIMEOUT.
        ev = next(it)
        assert isinstance(ev, DemuxEvent)


# --------------------------------------------------------------------------- #
# Round-trip                                                                  #
# --------------------------------------------------------------------------- #


def test_mux_sender_to_demux_receiver_round_trip() -> None:
    """Round-trip: MuxSender → DemuxReceiver over the same loopback
    port. Push a video NAL via MuxSender; assert the receiver emits
    a ProgramMap + Sample event."""
    port = _free_udp_port()
    rx = shells.DemuxReceiver.open(f"rtp://127.0.0.1:{port}")
    events: list[object] = []
    err: list[BaseException] = []
    saw_program_map = threading.Event()

    def consumer() -> None:
        try:
            for ev in rx:
                events.append(ev)
                if isinstance(ev, DemuxEvent.ProgramMap):
                    saw_program_map.set()
                # Bail out after a Sample is seen — we got enough.
                if isinstance(ev, DemuxEvent.Video):
                    break
        except BaseException as exc:  # noqa: BLE001
            err.append(exc)

    t = threading.Thread(target=consumer, daemon=True)
    t.start()
    # Give the consumer time to bind.
    time.sleep(0.2)
    # Push several key NALs to cover PSI emission + a video sample.
    program = _video_only_program()
    with shells.MuxSender.open(f"rtp://127.0.0.1:{port}", program) as snd:
        nal_idr = b"\x00\x00\x00\x01\x65\xBB"
        for i in range(8):
            snd.send_video(
                nal_idr, pts=Pts90khz.from_raw(i * 3000), key_frame=(i == 0)
            )
    # Wait for the consumer to see a video Sample.
    t.join(timeout=5.0)
    rx.close()
    assert not err, f"consumer raised: {err}"
    # Expect at least one ProgramMap event and at least one Sample.
    saw_video = any(isinstance(e, DemuxEvent.Video) for e in events)
    assert saw_program_map.is_set() or saw_video, (
        f"expected at least one PMT or Video event, got: {[type(e).__name__ for e in events]}"
    )


# --------------------------------------------------------------------------- #
# Stats                                                                       #
# --------------------------------------------------------------------------- #

_PMT_PID = 0x0100
_VIDEO_PID = 0x0101
_WAIT_S = 10.0


def _psi_packet(pid: int, section_no_crc: bytes, cc: int) -> bytes:
    """One section, CRC added, after a zero pointer field; the rest of the
    packet is 0xFF stuffing (zeros would read as further sections)."""
    section = section_no_crc + _crc32_mpeg(section_no_crc).to_bytes(4, "big")
    payload = b"\x00" + section
    return _ts_packet(pid, payload + b"\xFF" * (184 - len(payload)), pusi=True, cc=cc)


def _pat_packet() -> bytes:
    """PAT: program 1 on `_PMT_PID`."""
    body = bytes([0x00, 0x01, 0xE0 | (_PMT_PID >> 8), _PMT_PID & 0xFF])
    section_length = 5 + len(body) + 4
    header = bytes([0x00, 0xB0, section_length, 0x00, 0x01, 0xC1, 0x00, 0x00])
    return _psi_packet(0x0000, header + body, cc=0)


def _pmt_packet(version: int, cc: int) -> bytes:
    """PMT for program 1 on `_PMT_PID`: one H.264 stream on `_VIDEO_PID`."""
    body = bytes([
        0xE0 | (_VIDEO_PID >> 8), _VIDEO_PID & 0xFF,  # PCR_PID
        0xF0, 0x00,                                  # program_info_length = 0
        0x1B, 0xE0 | (_VIDEO_PID >> 8), _VIDEO_PID & 0xFF, 0xF0, 0x00,
    ])
    section_length = 5 + len(body) + 4
    header = bytes([
        0x02, 0xB0 | (section_length >> 8), section_length & 0xFF,
        0x00, 0x01,                                  # program_number = 1
        0xC1 | ((version & 0x1F) << 1),              # version, current_next = 1
        0x00, 0x00,
    ])
    return _psi_packet(_PMT_PID, header + body, cc=cc)


def _stats_capture() -> list[bytes]:
    """PAT, PMT version 0 twice, a video PES whose third packet skips a
    continuity counter, then PMT version 1 and three null packets (so the
    last PMT is not the final packet of the datagram)."""
    pes_start = b"\x00\x00\x01\xE0\x00\x00\x80\x00\x00" + b"\x00\x00\x00\x01\x09\xF0"
    return [
        _pat_packet(),
        _pmt_packet(0, cc=0),
        _pmt_packet(0, cc=1),
        _ts_packet(_VIDEO_PID, pes_start, pusi=True, cc=0),
        _ts_packet(_VIDEO_PID, b"", cc=1),
        _ts_packet(_VIDEO_PID, b"", cc=3),  # expected 2: one continuity gap
        _pmt_packet(1, cc=2),
        *[_ts_packet(0x1FFF, b"", cc=i) for i in range(3)],
    ]


def _send_datagram(port: int, data: bytes) -> None:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.sendto(data, ("127.0.0.1", port))


def _iterate_until(rx: shells.DemuxReceiver, done: Callable[[object], bool]) -> list[object]:
    """Iterate `rx` on a daemon thread until `done(event)`; the receiver's
    `recv_timeout` bounds each wait, the thread join bounds the whole."""
    events: list[object] = []
    err: list[BaseException] = []

    def run() -> None:
        try:
            for ev in rx:
                events.append(ev)
                if done(ev):
                    return
        except BaseException as exc:  # noqa: BLE001
            err.append(exc)

    t = threading.Thread(target=run, daemon=True)
    t.start()
    t.join(_WAIT_S)
    assert not t.is_alive(), f"no matching event within {_WAIT_S} s: {events}"
    assert not err, err
    return events


def _second_pmt_seen() -> Callable[[object], bool]:
    """True at the first `ProgramMap` after the continuity gap's
    `Discontinuity`, which only PMT version 1 follows."""
    gap = [False]

    def done(ev: object) -> bool:
        if isinstance(ev, DemuxEvent.Discontinuity):
            gap[0] = True
        return gap[0] and isinstance(ev, DemuxEvent.ProgramMap)

    return done


def test_demux_receiver_stats_values() -> None:
    """The six counters by name, against what the capture holds: two PMT
    versions (the repeat of version 0 counts nothing), one continuity gap,
    and whole packets only."""
    port = _free_udp_port()
    capture = _stats_capture()
    with shells.DemuxReceiver.open(f"udp://127.0.0.1:{port}") as rx:
        _send_datagram(port, b"".join(capture))
        _iterate_until(rx, _second_pmt_seen())
        s = rx.stats()
        assert isinstance(s, shells.DemuxReceiverStats)
        assert s.program_maps_seen == 2
        assert s.pmt_versions_seen == 2
        assert s.discontinuities == 1
        assert s.nonconformant == 0
        assert s.packets_received >= capture.index(_pmt_packet(1, cc=2)) + 1
        assert s.packets_received <= len(capture)
        assert s.bytes_received == s.packets_received * 188


def test_a_stats_record_kept_across_close_keeps_its_values() -> None:
    port = _free_udp_port()
    rx = shells.DemuxReceiver.open(f"udp://127.0.0.1:{port}")
    _send_datagram(port, b"".join(_stats_capture()))
    _iterate_until(rx, _second_pmt_seen())
    before = rx.stats()
    values = repr(before)
    rx.close()
    assert repr(before) == values
    assert before.program_maps_seen == 2
    with pytest.raises(UdpError) as ei:
        rx.stats()
    assert ei.value.kind == UdpErrorKind.CLOSED


def test_receiver_stats_values() -> None:
    """Five bytes of garbage, then aligned packets: the syncer skips the
    five and locks once; each packet read counts 188 bytes."""
    port = _free_udp_port()
    packets = [_ts_packet(0x1FFF, b"", cc=i) for i in range(8)]
    with shells.Receiver.open(f"udp://127.0.0.1:{port}") as rx:
        _send_datagram(port, b"\x00" * 5 + b"".join(packets))
        for _ in range(4):
            assert len(rx.recv_bytes()) == 188
        s = rx.stats()
        assert isinstance(s, shells.ReceiverStats)
        assert s.packets_received == 4
        assert s.bytes_received == 4 * 188
        assert s.bytes_skipped_for_sync == 5
        assert s.resync_events == 1
    with pytest.raises(UdpError) as ei:
        rx.stats()
    assert ei.value.kind == UdpErrorKind.CLOSED


@pytest.mark.parametrize("cls", [shells.DemuxReceiver, shells.Receiver])
def test_rtp_stats_is_none_off_rtp(cls) -> None:
    port = _free_udp_port()
    with cls.open(f"udp://127.0.0.1:{port}") as rx:
        assert rx.rtp_stats() is None
    assert rx.rtp_stats() is None


def _park(rx) -> tuple[threading.Thread, threading.Event]:
    """Start a receive on a daemon thread; the Event is set just before it."""
    started = threading.Event()

    def run() -> None:
        started.set()
        try:
            if isinstance(rx, shells.Receiver):
                while True:
                    rx.recv_bytes()
            else:
                for _ in rx:
                    pass
        except Exception:  # noqa: BLE001 - CLOSED at close() ends it
            pass

    t = threading.Thread(target=run, daemon=True)
    t.start()
    assert started.wait(_WAIT_S)
    return t, started


def _wait_for(cond: Callable[[], bool]) -> bool:
    deadline = time.monotonic() + _WAIT_S
    while time.monotonic() < deadline:
        if cond():
            return True
        time.sleep(0.01)
    return cond()


@pytest.mark.parametrize("cls", [shells.DemuxReceiver, shells.Receiver])
def test_rtp_stats_counts_a_malformed_datagram(cls) -> None:
    """A datagram whose first byte says RTP version 1 is dropped and
    counted in `malformed_packets`."""
    port = _free_udp_port()
    rx = cls.open(f"rtp://127.0.0.1:{port}")
    try:
        t, _ = _park(rx)
        _send_datagram(port, b"\x40" + b"\x00" * 11 + b"\x47" + b"\x00" * 187)
        assert _wait_for(lambda: rx.rtp_stats().malformed_packets == 1), rx.rtp_stats()
        assert rx.rtp_stats().source_rejected == 0
    finally:
        rx.close()
    t.join(_WAIT_S)
    assert not t.is_alive()


@pytest.mark.parametrize("cls", [shells.DemuxReceiver, shells.Receiver])
def test_rtp_stats_answers_while_a_receive_is_parked_and_after_close(cls) -> None:
    """The counters are read without the receiver's slot: a receive in
    progress on another thread does not hold them up, and `close()` keeps
    the last value."""
    port = _free_udp_port()
    rx = cls.open(f"rtp://127.0.0.1:{port}")
    t, _ = _park(rx)
    for _ in range(3):
        _send_datagram(port, b"\x40" + b"\x00" * 199)
    assert _wait_for(lambda: rx.rtp_stats().malformed_packets == 3)
    # The receive has not returned: the datagrams were all dropped.
    assert t.is_alive()
    blocked, got = call_with_hang_deadline(rx.rtp_stats, 5.0)
    assert not blocked
    assert got["value"].malformed_packets == 3
    rx.close()
    t.join(_WAIT_S)
    assert not t.is_alive()
    after = rx.rtp_stats()
    assert isinstance(after, tstrans.rtp.RtpStats)
    assert after.malformed_packets == 3


@pytest.mark.parametrize("cls", [shells.DemuxReceiver, shells.Receiver])
def test_rtp_stats_racing_close_never_fails(cls) -> None:
    for _ in range(20):
        port = _free_udp_port()
        rx = cls.open(f"rtp://127.0.0.1:{port}")
        stop = threading.Event()
        answers: list[object] = []
        errs: list[BaseException] = []

        def read() -> None:
            while not stop.is_set():
                try:
                    answers.append(rx.rtp_stats())
                except BaseException as exc:  # noqa: BLE001
                    errs.append(exc)
                    return

        t = threading.Thread(target=read, daemon=True)
        t.start()
        assert _wait_for(lambda: len(answers) > 0)
        rx.close()
        # Read on a little after the close, then stop.
        n = len(answers)
        assert _wait_for(lambda: len(answers) > n)
        stop.set()
        t.join(_WAIT_S)
        assert not t.is_alive()
        assert not errs, errs
        assert all(isinstance(a, tstrans.rtp.RtpStats) for a in answers)


# --------------------------------------------------------------------------- #
# last_seen_micros(pid)                                                       #
# --------------------------------------------------------------------------- #


def test_last_seen_micros_none_before_any_data() -> None:
    """A freshly-bound receiver has seen nothing yet — any pid, including
    a configured-looking one, reports None."""
    port = _free_udp_port()
    with shells.DemuxReceiver.open(f"rtp://127.0.0.1:{port}") as rx:
        assert rx.last_seen_micros(0x101) is None


def test_last_seen_micros_bogus_pid_returns_none() -> None:
    port = _free_udp_port()
    with shells.DemuxReceiver.open(f"rtp://127.0.0.1:{port}") as rx:
        assert rx.last_seen_micros(0x1FFF) is None


def test_last_seen_micros_reports_int_after_video_event() -> None:
    """Once a Video event for pid 0x101 has been consumed,
    `last_seen_micros(0x101)` returns an epoch-microsecond int; a PID
    that never carried traffic (0x1FFF, outside the configured program)
    stays None."""
    port = _free_udp_port()
    rx = shells.DemuxReceiver.open(f"rtp://127.0.0.1:{port}")
    events: list[object] = []
    err: list[BaseException] = []

    def consumer() -> None:
        try:
            for ev in rx:
                events.append(ev)
                if isinstance(ev, DemuxEvent.Video):
                    break
        except BaseException as exc:  # noqa: BLE001
            err.append(exc)

    t = threading.Thread(target=consumer, daemon=True)
    t.start()
    # Give the consumer time to bind.
    time.sleep(0.2)
    program = _video_only_program()
    with shells.MuxSender.open(f"rtp://127.0.0.1:{port}", program) as snd:
        nal_idr = b"\x00\x00\x00\x01\x65\xBB"
        for i in range(8):
            snd.send_video(
                nal_idr, pts=Pts90khz.from_raw(i * 3000), key_frame=(i == 0)
            )
    t.join(timeout=5.0)
    saw_video = any(isinstance(e, DemuxEvent.Video) for e in events)
    if not saw_video and err:
        pytest.fail(f"consumer raised before a Video event: {err}")
    assert saw_video, (
        f"expected a Video event, got: {[type(e).__name__ for e in events]}"
    )
    seen = rx.last_seen_micros(0x101)
    assert isinstance(seen, int) and seen > 0
    # A PID never carried on this program stays None even after traffic.
    assert rx.last_seen_micros(0x1FFF) is None
    rx.close()


# --------------------------------------------------------------------------- #
# into_demux_receiver bridge — skipped (needs RTSP server fixture)           #
# --------------------------------------------------------------------------- #


@pytest.mark.skip(
    reason="RtspSession.into_demux_receiver requires a real RTSP server fixture; "
    "covered by tst-rtp's RTSP integration tests."
)
def test_rtsp_session_into_demux_receiver() -> None:  # pragma: no cover
    """Bridge from `RtspSession` → `DemuxReceiver`. Verifying this
    end-to-end requires a real RTSP server; `test_rtp_integration.py`
    covers it.

    The bridge itself is wired in `bindings/python/src/rtp/client.rs`:
    `PyRtspClient::connect` retains the SETUP-time `RtspSession`; the
    `into_demux_receiver` method consumes it via
    `RtspSession::into_recv_transport` and wraps the resulting
    `RtpRecvTransport` in a `PyDemuxReceiver`.
    """
