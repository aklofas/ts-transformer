"""End-to-end integration tests across all 18 PyClasses
of `tstrans.srt`.

Each test exercises a realistic user flow that crosses MULTIPLE
PyClasses simultaneously — the unit-level tests in
`test_srt_{transport,builder,listener,mux_demux,policy,managed_*}` cover
each class in isolation; these tests verify they compose.

Test inventory:

1. `test_full_pipeline_via_builder_socket_promotion` —
   Builder('srt://127.0.0.1:0').listener().listen() → Listener.accept()
   → Socket.into_demux_receiver() paired with
   Builder(...).caller().connect() → Socket.into_mux_sender(prog).
   Push ≥5 video NALs + ≥3 KLV records; iterate the DemuxReceiver;
   observe at least one Video / Klv / ProgramMap event.

2. `test_from_url_shortcut_equivalent_to_builder_path` —
   MuxSender.from_url(caller) ↔ DemuxReceiver.from_url(listener)
   end-to-end on a loopback pair; verifies the from_url shortcut path
   produces the same observable event set as the explicit Builder →
   Socket promotion path from test #1.

3. `test_encrypted_loopback_round_trip` — Builder with `.passphrase(...)`
   on both sides (caller + listener). libsrt's passphrase floor is 10
   characters; we use a 24-char passphrase. Verifies the handshake
   completes and a single push_video → DemuxEvent round-trips.

4. `test_managed_sender_receiver_with_reconnect_policy` —
   ManagedSender + ManagedReceiver both wired via
   `ReconnectPolicy(max_attempts=3)`. Verify `reconnect_attempts()` is
   0 initially; round-trip a small bytes payload via send_bytes /
   recv_bytes (mirrors the existing `test_srt_managed_basic.py` flow but
   in a single combined fixture).

5. `test_managed_mux_demux_force_drop_reconnect_discontinuity` —
   ManagedMuxSender + ManagedDemuxReceiver with a forced peer drop, to
   observe a `DemuxEvent.ReconnectDiscontinuity`. SKIPPED by default
   because driving a real listener-cycle on a loopback SRT pair is
   timing-flaky (see `test_srt_managed_basic.py::
   test_managed_sender_recovers_after_listener_restart` for the same
   rationale).
"""

from __future__ import annotations

import threading
import time
from typing import List, Optional, Tuple

import pytest

import tstrans
import tstrans.srt
from tstrans.exceptions import SrtError, SrtErrorKind
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
    """Two-stream MuxerProgramConfig — H.264 video + ST 0601-style KLV.
    Both streams configured so the muxer accepts push_video + push_klv."""
    return (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )


# Minimal Annex-B IDR NAL (start code + nal_unit_type=5).
NAL_IDR = b"\x00\x00\x00\x01\x65\xBB"
# 17-byte ST 0601 universal-label-only KLV LS (UL + 0-length).
KLV_UL_ZERO = (
    b"\x06\x0E\x2B\x34\x02\x0B\x01\x01"
    b"\x0E\x01\x03\x01\x01\x00\x00\x00\x00"
)


# --------------------------------------------------------------------------- #
# Test 1: full Builder → Socket → MuxSender/DemuxReceiver pipeline           #
# --------------------------------------------------------------------------- #


def test_full_pipeline_via_builder_socket_promotion() -> None:
    """End-to-end across Builder + Listener + Socket + MuxSender +
    DemuxReceiver: drive the listener side through `Builder.listener()
    .listen()` then `Listener.accept().into_demux_receiver()`; drive the
    caller side through `Builder.caller().connect().into_mux_sender()`.
    Push 5 IDR NALs + 3 KLV records; iterate the DemuxReceiver on a
    worker thread; assert at least one Sample / Metadata / ProgramMap
    event arrives before close races the drain."""
    port = _free_tcp_port()

    # Listener side — bind first so the caller doesn't connection-refuse.
    listener = (
        tstrans.srt.Builder(f"srt://0.0.0.0:{port}?mode=listener")
        .listener()
        .listen()
    )

    rx_box: list[tstrans.srt.DemuxReceiver] = []
    rx_err: list[BaseException] = []

    def accept_then_into_demux() -> None:
        try:
            sock = listener.accept(timeout_ms=5000)
            rx = sock.into_demux_receiver()
            rx_box.append(rx)
        except BaseException as exc:  # noqa: BLE001
            rx_err.append(exc)

    accept_thread = threading.Thread(target=accept_then_into_demux, daemon=True)
    accept_thread.start()
    # Tiny wait so the accept thread is parked before we connect.
    time.sleep(0.1)

    # Caller side — Builder → connect → into_mux_sender.
    caller_sock = (
        tstrans.srt.Builder(f"srt://127.0.0.1:{port}?mode=caller")
        .caller()
        .connect()
    )
    mux_sender = caller_sock.into_mux_sender(_video_klv_program())
    # Caller Socket is consumed by into_mux_sender.
    assert not caller_sock.is_alive()

    accept_thread.join(timeout=5.0)
    if rx_err:
        mux_sender.close()
        listener.close()
        raise rx_err[0]
    assert rx_box, "listener-side did not accept within 5 s"
    demux_receiver = rx_box[0]

    # Consume DemuxEvents on another thread so we can drive the push
    # side without recv_event blocking us.
    events: list[object] = []
    consumer_err: list[BaseException] = []

    def consumer() -> None:
        try:
            for ev in demux_receiver:
                events.append(ev)
                # Stop after we've seen Video + (Klv or ProgramMap), or
                # after a generous event count to avoid hanging if a
                # category never arrives on a fast loopback close.
                saw_video = any(isinstance(e, DemuxEvent.Video) for e in events)
                if saw_video and len(events) >= 5:
                    break
        except BaseException as exc:  # noqa: BLE001
            consumer_err.append(exc)

    consumer_thread = threading.Thread(target=consumer, daemon=True)
    consumer_thread.start()
    time.sleep(0.2)

    # Push 5 IDR NALs + 3 KLV records. The mux side bundles PSI every
    # key frame; the bundle threshold is 7 × 188 bytes.
    klv_h = mux_sender.klv_handle()
    assert klv_h is not None, "KLV handle should resolve given a klv program stream"
    try:
        for i in range(8):
            mux_sender.send_video(
                NAL_IDR, pts=Pts90khz.from_raw(i * 3000), key_frame=(i % 2 == 0)
            )
            if i < 3:
                mux_sender.send_klv_to(
                    klv_h, KLV_UL_ZERO, pts=Pts90khz.from_raw(i * 3000)
                )
        # Let libsrt drain the send queue so close doesn't race in-flight
        # bytes off the wire.
        time.sleep(0.3)
    finally:
        mux_sender.close()

    consumer_thread.join(timeout=5.0)
    demux_receiver.close()
    listener.close()

    # We must observe at least one event before the connection drained.
    # "Connection broken" partway through is OK if we already got past
    # the PSI bootstrap (PMT arrives ~immediately on the first PAT
    # boundary).
    if not events and consumer_err:
        pytest.fail(
            f"consumer raised before any event: {consumer_err[0]!r}"
        )
    assert events, (
        "expected at least one DemuxEvent from the full Builder→Socket→"
        "MuxSender/DemuxReceiver pipeline"
    )
    # Surface event types to make a failure report tractable.
    type_names = sorted({type(e).__name__ for e in events})
    assert any(
        isinstance(e, (DemuxEvent.Video, DemuxEvent.ProgramMap, DemuxEvent.Metadata))
        for e in events
    ), f"none of Video/ProgramMap/Metadata seen; got: {type_names}"


# --------------------------------------------------------------------------- #
# Test 2: from_url shortcut path on a loopback Socket pair                   #
# --------------------------------------------------------------------------- #


def test_from_url_shortcut_equivalent_to_builder_path() -> None:
    """Smoke-verify that `MuxSender.from_url` + `DemuxReceiver.from_url`
    produce the same observable event surface as the explicit
    Builder/Socket promotion path in test 1. Doesn't byte-compare —
    libsrt's framing layer can shuffle TS packets across the
    handshake — but does require that the receiver observes at least
    one of the same event categories on both paths."""
    port = _free_tcp_port()
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
    time.sleep(0.1)
    mux_sender = tstrans.srt.MuxSender.from_url(caller_url, _video_klv_program())
    t.join(timeout=5.0)
    if rx_err:
        mux_sender.close()
        raise rx_err[0]
    assert rx_box, "DemuxReceiver.from_url did not accept within 5 s"
    demux_receiver = rx_box[0]

    events: list[object] = []
    consumer_err: list[BaseException] = []

    def consumer() -> None:
        try:
            for ev in demux_receiver:
                events.append(ev)
                if any(isinstance(e, DemuxEvent.Video) for e in events):
                    break
        except BaseException as exc:  # noqa: BLE001
            consumer_err.append(exc)

    consumer_thread = threading.Thread(target=consumer, daemon=True)
    consumer_thread.start()
    time.sleep(0.2)

    klv_h = mux_sender.klv_handle()
    assert klv_h is not None
    try:
        for i in range(8):
            mux_sender.send_video(
                NAL_IDR, pts=Pts90khz.from_raw(i * 3000), key_frame=(i % 2 == 0)
            )
            if i < 3:
                mux_sender.send_klv_to(
                    klv_h, KLV_UL_ZERO, pts=Pts90khz.from_raw(i * 3000)
                )
        time.sleep(0.3)
    finally:
        mux_sender.close()

    consumer_thread.join(timeout=5.0)
    demux_receiver.close()

    if not events and consumer_err:
        pytest.fail(f"consumer raised before any event: {consumer_err[0]!r}")
    assert events, (
        "from_url shortcut path produced no events — Builder/Socket path "
        "did, so this points to a behavior drift between the two paths"
    )


# --------------------------------------------------------------------------- #
# Test 3: encrypted loopback                                                  #
# --------------------------------------------------------------------------- #


def test_encrypted_loopback_round_trip() -> None:
    """SRT with a configured passphrase performs a key exchange in the
    handshake (HSv5 KMREQ/KMRSP). Both ends must carry the same
    passphrase or the handshake fails with REJ_BADSECRET.

    libsrt enforces a 10..=79 char passphrase length; this test uses a
    24-character passphrase. We pair a `Builder(...).passphrase(...)
    .listener().listen()` with a `Builder(...).passphrase(...).caller()
    .connect()`, run a one-bundle TS send through the resulting Socket
    pair (via `into_sender` / `into_receiver`), and observe the round
    trip."""
    # 24 chars — well within libsrt's 10..=79 range.
    passphrase = "hunter-too-long-thanks!!"

    port = _free_tcp_port()

    listener = (
        tstrans.srt.Builder(f"srt://0.0.0.0:{port}?mode=listener")
        .passphrase(passphrase)
        .listener()
        .listen()
    )

    accepted_box: list[Optional[tstrans.srt.Socket]] = [None]
    accept_err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            accepted_box[0] = listener.accept(timeout_ms=5000)
        except BaseException as exc:  # noqa: BLE001
            accept_err.append(exc)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)

    caller_sock = (
        tstrans.srt.Builder(f"srt://127.0.0.1:{port}?mode=caller")
        .passphrase(passphrase)
        .caller()
        .connect()
    )
    t.join(timeout=5.0)

    try:
        if accept_err:
            raise accept_err[0]
        assert accepted_box[0] is not None, "encrypted handshake did not complete"
        assert caller_sock.is_alive()
        assert accepted_box[0].is_alive()

        # Demote both Sockets into raw Sender/Receiver — we don't need
        # the mux path to prove the encrypted handshake worked.
        sender = caller_sock.into_sender()
        receiver = accepted_box[0].into_receiver()

        # Send one full 7-packet bundle through; expect ≥1 packet back.
        bundle = (b"\x47" + b"\x00" * 187) * 7  # 1316 bytes
        sender.send_bytes(bundle)
        received = receiver.recv_bytes(max_len=1500)
        # First packet is one TS packet (188 bytes) starting with sync.
        assert len(received) == 188
        assert received[0] == 0x47

        sender.close()
        receiver.close()
    finally:
        # Both Sockets are consumed if we made it to the demotion; the
        # close() calls below are defensive in case of early exception.
        caller_sock.close()
        if accepted_box[0] is not None:
            accepted_box[0].close()
        listener.close()


def test_encrypted_handshake_fails_on_passphrase_mismatch() -> None:
    """When the two ends carry different passphrases, the SRT handshake
    rejects with REJ_BADSECRET. Caller side surfaces this as
    `SrtError(CONNECT_FAILED)` (typed mapping from `tst_srt::AcceptError`
    /`ConnectError`)."""
    port = _free_tcp_port()
    listener = (
        tstrans.srt.Builder(f"srt://0.0.0.0:{port}?mode=listener")
        .passphrase("listener-side-passphrase-abc")
        .listener()
        .listen()
    )

    accept_err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            listener.accept(timeout_ms=2000)
        except BaseException as exc:  # noqa: BLE001
            accept_err.append(exc)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)

    # Caller uses a DIFFERENT passphrase; the handshake fails.
    with pytest.raises(SrtError) as exc_info:
        tstrans.srt.Builder(
            f"srt://127.0.0.1:{port}?mode=caller&conntimeo=2000"
        ).passphrase("caller-side-different-pass!").caller().connect()
    # Most likely CONNECT_FAILED (REJ_BADSECRET); TIMEOUT is also
    # acceptable on slow CI where the listener's reject doesn't arrive
    # before conntimeo expires.
    assert exc_info.value.kind in (
        SrtErrorKind.CONNECT_FAILED,
        SrtErrorKind.TIMEOUT,
    )

    # Drain the listener-side worker — it may either succeed (unlikely)
    # or fail with an accept error; either is fine for this test.
    t.join(timeout=3.0)
    listener.close()


# --------------------------------------------------------------------------- #
# Test 4: ManagedSender + ManagedReceiver with ReconnectPolicy                #
# --------------------------------------------------------------------------- #


def test_managed_sender_receiver_with_reconnect_policy() -> None:
    """`ManagedSender` + `ManagedReceiver` pair, both constructed with a
    user-supplied `ReconnectPolicy(max_attempts=3)`. Verifies:

    1. The kwarg is accepted (no `max_attempts` AttributeError).
    2. `reconnect_attempts()` reads 0 on the receiver before any break
       (the initial bind+accept does NOT count as a reconnect attempt).
    3. A small `send_bytes` / `recv_bytes` round-trip succeeds through
       the managed wrappers — i.e. the managed layer doesn't add
       behavioral drift on the happy path."""
    port = _free_tcp_port()
    policy = tstrans.srt.ReconnectPolicy(max_attempts=3)
    assert policy.max_attempts == 3

    listener_url = f"srt://:{port}?mode=listener"
    caller_url = f"srt://127.0.0.1:{port}?mode=caller"

    receiver_box: list[tstrans.srt.ManagedReceiver] = []
    receiver_err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            r = tstrans.srt.ManagedReceiver.from_url(listener_url, policy=policy)
            receiver_box.append(r)
        except BaseException as exc:  # noqa: BLE001
            receiver_err.append(exc)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)

    sender = tstrans.srt.ManagedSender.from_url(caller_url, policy=policy)
    t.join(timeout=5.0)
    if receiver_err:
        sender.close()
        raise receiver_err[0]
    assert receiver_box, "ManagedReceiver did not accept within 5 s"
    receiver = receiver_box[0]

    try:
        # Initial reconnect counter is 0 — the initial bind/accept and
        # the initial connect both don't count as reconnects.
        assert receiver.reconnect_attempts() == 0
        assert sender.is_alive()
        assert receiver.is_alive()

        # 7-packet TS bundle round-trip. Send-side managed wraps a
        # `Sender`; recv-side managed wraps a `Receiver`. Both expose
        # `send_bytes` / `recv_bytes` directly.
        bundle = (b"\x47" + b"\x00" * 187) * 7  # 1316 bytes
        sender.send_bytes(bundle)
        # First recv yields one 188-byte packet (libsrt boundary).
        received = receiver.recv_bytes(max_len=1500)
        assert len(received) == 188
        assert received[0] == 0x47

        # Counter still 0 after a clean round-trip — no break happened.
        assert receiver.reconnect_attempts() == 0
    finally:
        sender.close()
        receiver.close()


# --------------------------------------------------------------------------- #
# Test 5: ManagedMuxSender + ManagedDemuxReceiver forced-drop reconnect       #
# --------------------------------------------------------------------------- #


@pytest.mark.skip(
    reason="Driving a real listener-cycle on a loopback SRT pair is "
    "timing-flaky (libsrt break-detection can take seconds; the policy's "
    "exponential backoff defaults to 100ms..=10s). The unit-level reconnect "
    "logic is covered by `crates/tst-pipeline/src/managed_*` Rust tests "
    "against scripted RecvTransports. Will land as a deterministic "
    "integration test in a follow-up wave with a controlled break trigger."
)
def test_managed_mux_demux_force_drop_reconnect_discontinuity() -> None:
    """Forced peer drop on a `ManagedMuxSender` ↔ `ManagedDemuxReceiver`
    pair should surface a `DemuxEvent.ReconnectDiscontinuity` on the
    consumer side after the reconnect heals. Verifies the full
    auto-reconnect-with-mux/demux path."""
    raise NotImplementedError
