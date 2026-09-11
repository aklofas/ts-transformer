"""Tests for `tstrans.srt.Sender` / `Receiver` / `SocketStats` /
`SrtStats` / `CancelHandle` (Wave A T2).

Uses real libsrt loopback sockets — no external network. A listener-mode
Receiver is spun up on a background thread before the Sender connects.
Cross-thread cancel tests verify that closing the libsrt socket from
another thread wakes a parked recv within a few seconds.
"""

from __future__ import annotations

import threading
import time
from typing import Optional, Tuple

import pytest

import tstrans
import tstrans.srt
from tstrans.exceptions import SrtError, SrtErrorKind

from _builders.ports import free_tcp_port as _free_tcp_port


def _make_loopback_pair(
    port: int, payload_size: Optional[int] = None
) -> Tuple[tstrans.srt.Sender, tstrans.srt.Receiver]:
    """Spawn a listener-mode Receiver on a background thread; once it's
    accepting, connect a caller-mode Sender from the main thread.
    Returns (sender, receiver). Callers must `.close()` both."""
    extra = f"&payloadsize={payload_size}" if payload_size else ""
    listener_url = f"srt://:{port}?mode=listener{extra}"
    caller_url = f"srt://127.0.0.1:{port}?mode=caller{extra}"

    receiver_box: list[tstrans.srt.Receiver] = []
    receiver_err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            r = tstrans.srt.Receiver.from_url(listener_url)
            receiver_box.append(r)
        except BaseException as exc:  # noqa: BLE001
            receiver_err.append(exc)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()

    # Tiny sleep before connect — libsrt's bind+listen is fast but not
    # instant; without this the caller can hit "connection refused"
    # racing the listener startup.
    time.sleep(0.1)

    sender = tstrans.srt.Sender.from_url(caller_url)
    t.join(timeout=5.0)
    if receiver_err:
        sender.close()
        raise receiver_err[0]
    if not receiver_box:
        sender.close()
        raise RuntimeError("listener thread did not accept within 5 s")
    return sender, receiver_box[0]


# --------------------------------------------------------------------------- #
# Module exports                                                              #
# --------------------------------------------------------------------------- #


def test_module_re_exports() -> None:
    """`tstrans.srt` must expose the 5 T2 classes after Wave A T2."""
    assert tstrans.srt.Sender is not None
    assert tstrans.srt.Receiver is not None
    assert tstrans.srt.SocketStats is not None
    assert tstrans.srt.SrtStats is not None
    assert tstrans.srt.CancelHandle is not None
    assert "Sender" in tstrans.srt.__all__
    assert "Receiver" in tstrans.srt.__all__
    assert "SrtStats" in tstrans.srt.__all__


# --------------------------------------------------------------------------- #
# Construction / closure                                                      #
# --------------------------------------------------------------------------- #


def test_sender_construct_and_close() -> None:
    """Sender to an unreachable port raises SrtError. libsrt's connect
    times out on a dead listener; conntimeo defaults to 3 s — we override
    via the URL key to keep the test responsive."""
    port = _free_tcp_port()
    # No listener on this port. With a short conntimeo, the SRT
    # handshake fails fast.
    with pytest.raises(SrtError) as exc_info:
        tstrans.srt.Sender.from_url(
            f"srt://127.0.0.1:{port}?mode=caller&conntimeo=500"
        )
    # Either TIMEOUT (handshake didn't complete) or CONNECT_FAILED
    # (Refused / Rejected) — both are valid for "nobody home".
    assert exc_info.value.kind in (SrtErrorKind.TIMEOUT, SrtErrorKind.CONNECT_FAILED)


def test_sender_bad_url_raises_config_invalid() -> None:
    """Malformed URL → SrtError(CONFIG_INVALID) before any socket op."""
    with pytest.raises(SrtError) as exc_info:
        tstrans.srt.Sender.from_url("not-a-valid-url")
    assert exc_info.value.kind == SrtErrorKind.CONFIG_INVALID


def test_sender_wrong_mode_raises_config_invalid() -> None:
    """Sender.from_url requires mode=caller (default); explicit
    listener → SrtError(CONFIG_INVALID)."""
    port = _free_tcp_port()
    with pytest.raises(SrtError) as exc_info:
        tstrans.srt.Sender.from_url(f"srt://127.0.0.1:{port}?mode=listener")
    assert exc_info.value.kind == SrtErrorKind.CONFIG_INVALID


def test_receiver_wrong_mode_raises_config_invalid() -> None:
    """Receiver.from_url requires mode=listener; default caller →
    SrtError(CONFIG_INVALID)."""
    port = _free_tcp_port()
    with pytest.raises(SrtError) as exc_info:
        tstrans.srt.Receiver.from_url(f"srt://127.0.0.1:{port}?mode=caller")
    assert exc_info.value.kind == SrtErrorKind.CONFIG_INVALID


# --------------------------------------------------------------------------- #
# Loopback round-trip                                                         #
# --------------------------------------------------------------------------- #


def test_loopback_round_trip_small() -> None:
    """Send one full 7-packet TS bundle (the smallest payload that the
    sender framing layer will push without further data — STRICT-mode
    sync verification needs 377+ bytes plus a full 1316-byte bundle to
    emit). Receive back one 188-byte packet at a time."""
    port = _free_tcp_port()
    sender, receiver = _make_loopback_pair(port)
    try:
        # 1 bundle = 7 × 188 bytes.
        payload = (b"\x47" + b"\x00" * 187) * 7
        sender.send_bytes(payload)
        # First recv returns one 188-byte packet.
        received = receiver.recv_bytes(max_len=1500)
        assert len(received) == 188
        assert received[0] == 0x47
        # Drain the remaining 6 packets so the receiver thread teardown
        # doesn't fight with the recv-buffer cleanup.
        for _ in range(6):
            receiver.recv_bytes(max_len=1500)
    finally:
        sender.close()
        receiver.close()


def test_loopback_round_trip_large() -> None:
    """Send a 1316-byte payload (libsrt's default SRTO_PAYLOADSIZE) =
    7 × 188-byte TS packets. recv_bytes returns one packet per call,
    so we call it 7 times to receive the full payload."""
    port = _free_tcp_port()
    sender, receiver = _make_loopback_pair(port)
    try:
        payload = (b"\x47" + b"\x00" * 187) * 7  # 1316 bytes, 7 TS packets
        sender.send_bytes(payload)
        # Collect 7 packets — one per recv_bytes call.
        collected = b""
        for _ in range(7):
            chunk = receiver.recv_bytes(max_len=1500)
            assert len(chunk) == 188
            assert chunk[0] == 0x47, f"bad sync at offset {len(collected)}"
            collected += chunk
        assert len(collected) == 1316
    finally:
        sender.close()
        receiver.close()


def test_bytes_like_inputs_accepted() -> None:
    """`.send_bytes()` two-path extraction: fast `&[u8]` for `bytes`,
    fallback coercion via `bytes()` builtin for `bytearray` /
    `memoryview`."""
    port = _free_tcp_port()
    sender, receiver = _make_loopback_pair(port)
    try:
        # Three full TS bundles (one per shape) so each send actually
        # flushes through the sender framing layer to the wire.
        bundle = (b"\x47" + b"\x00" * 187) * 7
        # bytes — fast path
        sender.send_bytes(bundle)
        # bytearray — fallback through `bytes()` builtin
        sender.send_bytes(bytearray(bundle))
        # memoryview over a bytearray — fallback
        sender.send_bytes(memoryview(bytearray(bundle)))
        # Drain all 21 packets.
        for _ in range(21):
            received = receiver.recv_bytes(max_len=1500)
            assert received[0] == 0x47
    finally:
        sender.close()
        receiver.close()


# --------------------------------------------------------------------------- #
# Stats                                                                       #
# --------------------------------------------------------------------------- #


def test_socket_stats_populated_after_transfer() -> None:
    """After sending a payload, `socket_stats().bytes_sent /
    packets_sent` advance on the sender side."""
    port = _free_tcp_port()
    sender, receiver = _make_loopback_pair(port)
    try:
        payload = (b"\x47" + b"\x00" * 187) * 7  # 1316 bytes
        sender.send_bytes(payload)
        # Receive the payload to drive the receiver-side counters too.
        # Drain all 7 packets so receiver-side counters tick.
        for _ in range(7):
            receiver.recv_bytes(max_len=1500)
        # Stats should reflect at least one packet sent.
        stats = sender.socket_stats()
        assert stats.bytes_sent > 0
        assert stats.packets_sent >= 1
        # Receiver-side stats — libsrt may not flush counters
        # synchronously on a single packet; check the field is present.
        rstats = receiver.socket_stats()
        assert rstats.bytes_received >= 0  # field accessible
    finally:
        sender.close()
        receiver.close()


def test_srt_stats_populated_after_transfer() -> None:
    """`srt_stats()` exposes the SRT-rich fields, including
    `mbps_estimated_bandwidth` as a float."""
    port = _free_tcp_port()
    sender, receiver = _make_loopback_pair(port)
    try:
        payload = (b"\x47" + b"\x00" * 187) * 7
        sender.send_bytes(payload)
        # Drain all 7 packets.
        for _ in range(7):
            receiver.recv_bytes(max_len=1500)
        stats = sender.srt_stats()
        # mbps_estimated_bandwidth is libsrt's bandwidth probe — should
        # be a non-negative float (often 0.0 on a brand-new socket
        # with one packet sent, but well-defined).
        assert isinstance(stats.mbps_estimated_bandwidth, float)
        assert stats.mbps_estimated_bandwidth >= 0.0
        # bytes_sent must track the abstract SocketStats projection.
        assert stats.bytes_sent == sender.socket_stats().bytes_sent
        # rtt_us is a u32 — loopback RTT is sub-millisecond, well
        # under u32::MAX.
        assert stats.rtt_us < 1_000_000
    finally:
        sender.close()
        receiver.close()


def test_stats_repr_does_not_leak_internals() -> None:
    """The `__repr__` of SocketStats / SrtStats should be a stable,
    grep-friendly summary — not a dump of pointer addresses or libsrt
    handle integers."""
    port = _free_tcp_port()
    sender, receiver = _make_loopback_pair(port)
    try:
        ss = sender.socket_stats()
        srtss = sender.srt_stats()
        ss_repr = repr(ss)
        srtss_repr = repr(srtss)
        assert ss_repr.startswith("SocketStats(")
        assert srtss_repr.startswith("SrtStats(")
        assert "bytes_sent=" in ss_repr
        assert "mbps_estimated_bandwidth=" in srtss_repr
        # No raw pointer-looking hex sequences.
        assert "0x" not in ss_repr.lower()
        assert "0x" not in srtss_repr.lower()
    finally:
        sender.close()
        receiver.close()


# --------------------------------------------------------------------------- #
# Lifecycle                                                                   #
# --------------------------------------------------------------------------- #


def test_context_manager() -> None:
    """`with Sender(...) as s:` calls close() on exit."""
    port = _free_tcp_port()
    receiver_box: list[tstrans.srt.Receiver] = []

    def accept_worker() -> None:
        receiver_box.append(
            tstrans.srt.Receiver.from_url(f"srt://:{port}?mode=listener")
        )

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)

    with tstrans.srt.Sender.from_url(f"srt://127.0.0.1:{port}?mode=caller") as s:
        assert "open" in repr(s)
        assert s.is_alive()
    # After the with-block, send_bytes must raise CLOSED.
    with pytest.raises(SrtError) as exc_info:
        s.send_bytes(b"\x47" + b"\x00" * 187)
    assert exc_info.value.kind == SrtErrorKind.CLOSED
    t.join(timeout=5.0)
    if receiver_box:
        receiver_box[0].close()


def test_close_idempotent() -> None:
    """`.close()` can be called multiple times safely."""
    port = _free_tcp_port()
    sender, receiver = _make_loopback_pair(port)
    sender.close()
    sender.close()  # second close is a no-op
    receiver.close()
    receiver.close()
    with pytest.raises(SrtError) as exc_info:
        sender.send_bytes(b"\x47" + b"\x00" * 187)
    assert exc_info.value.kind == SrtErrorKind.CLOSED


# --------------------------------------------------------------------------- #
# Cancel                                                                      #
# --------------------------------------------------------------------------- #


def test_cancel_handle_cross_thread() -> None:
    """Park a receiver.recv_bytes() on a worker thread, then cancel
    from a Timer thread. The recv must return SrtError(BROKEN or
    CLOSED) within ~2 s — libsrt cancels by closing the underlying
    socket, which surfaces as a connection-broken error."""
    port = _free_tcp_port()
    sender, receiver = _make_loopback_pair(port)
    ch = receiver.cancel_handle()
    captured: list[BaseException] = []

    def worker() -> None:
        try:
            # Block until a packet arrives — none will, the cancel
            # tears down the socket first.
            receiver.recv_bytes(max_len=1316)
        except BaseException as exc:  # noqa: BLE001
            captured.append(exc)

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    # Let the worker park.
    time.sleep(0.2)
    # Cancel from the main thread.
    ch.cancel()
    # Worker should unpark within ~2 s.
    w.join(timeout=5.0)
    assert not w.is_alive(), "worker did not unpark after cancel"
    assert len(captured) == 1, f"expected one error; got {captured!r}"
    err = captured[0]
    assert isinstance(err, SrtError)
    assert err.kind in (SrtErrorKind.BROKEN, SrtErrorKind.CLOSED)
    assert ch.is_cancelled()
    sender.close()
    receiver.close()


def test_close_while_recv_parked_cancels_first() -> None:
    """`close()` from another thread while `recv_bytes()` is parked cancels
    first: it returns promptly (no `RuntimeError: Already borrowed`, no wait
    behind the parked recv) and the parked call ends with `SrtError(BROKEN)`
    — the plain cancel handle closes the libsrt socket, so the parked
    `srt_recvmsg` fails with a connection error. The contract shared with
    the JVM plain `Receiver.close()` and the C ABI's receiver close."""
    port = _free_tcp_port()
    sender, receiver = _make_loopback_pair(port)
    captured: list[BaseException] = []

    def worker() -> None:
        try:
            # Block until a packet arrives — none will; the close under
            # test must end this call.
            receiver.recv_bytes(max_len=1316)
        except BaseException as exc:  # noqa: BLE001
            captured.append(exc)

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    # Let the worker park inside the native receive.
    time.sleep(0.3)

    # close() on its own thread with a bounded join: a regression to
    # waiting behind the parked recv fails the verdict below instead of
    # hanging the test.
    close_err: list[BaseException] = []

    def closer() -> None:
        try:
            receiver.close()
        except BaseException as exc:  # noqa: BLE001
            close_err.append(exc)

    c = threading.Thread(target=closer, daemon=True)
    try:
        c.start()
        c.join(timeout=5.0)
        w.join(timeout=5.0)
        if c.is_alive() or w.is_alive():
            # Rescue: the peer is still connected, so a packet unparks the
            # recv and lets the daemon threads finish rather than sitting in
            # a native read at interpreter exit.
            sender.send_bytes(b"\x47" + bytes(187))
            w.join(timeout=5.0)
            c.join(timeout=5.0)
            pytest.fail("close() did not end the parked recv_bytes() within 5 s")
        assert not close_err, f"close() raised {close_err!r}"
        assert len(captured) == 1, f"expected one error; got {captured!r}"
        err = captured[0]
        assert isinstance(err, SrtError), f"parked recv ended with {err!r}"
        assert err.kind == SrtErrorKind.BROKEN, f"unexpected kind: {err.kind!r}"
        assert not receiver.is_alive()
    finally:
        sender.close()
        receiver.close()


def test_cancel_handle_is_independently_clonable() -> None:
    """Each call to `cancel_handle()` returns a fresh wrapper with its
    own `is_cancelled()` observation, but they all forward `.cancel()`
    into the same underlying socket. Cancelling one wakes the parked
    socket; the other clone's is_cancelled stays False until cancel
    is called through it directly."""
    port = _free_tcp_port()
    sender, receiver = _make_loopback_pair(port)
    try:
        ch1 = sender.cancel_handle()
        ch2 = sender.cancel_handle()
        assert not ch1.is_cancelled()
        assert not ch2.is_cancelled()
        ch1.cancel()
        assert ch1.is_cancelled()
        # ch2 was not the one that received .cancel(), so its local
        # flag stays False. (The underlying transport IS cancelled,
        # but the per-wrapper observation only flips on direct cancel.)
        assert not ch2.is_cancelled()
    finally:
        sender.close()
        receiver.close()


# --------------------------------------------------------------------------- #
# Cross-transport shape parity (skip if rtp unavailable)                      #
# --------------------------------------------------------------------------- #


def test_socket_stats_class_shape_matches_rtp() -> None:
    """`tstrans.srt.SocketStats` and `tstrans.rtp.SocketStats` must
    expose the same property set — bindings often need to read the
    abstract wire stats without knowing which transport they came from.
    Skip when rtp is unavailable (alternative build feature off)."""
    rtp = pytest.importorskip("tstrans.rtp")

    srt_props = {
        name
        for name in dir(tstrans.srt.SocketStats)
        if not name.startswith("_")
    }
    rtp_props = {name for name in dir(rtp.SocketStats) if not name.startswith("_")}
    assert srt_props == rtp_props, (
        f"shape diverged: srt-only={srt_props - rtp_props}, "
        f"rtp-only={rtp_props - srt_props}"
    )


def test_srt_stats_has_advanced_fields_not_in_socket_stats() -> None:
    """`SrtStats` exposes the libsrt-specific extras that don't fit the
    abstract `SocketStats` shape: `mbps_estimated_bandwidth`, the
    symmetric send/recv-side byte-loss split, etc."""
    srt_stats_props = {
        name
        for name in dir(tstrans.srt.SrtStats)
        if not name.startswith("_")
    }
    socket_stats_props = {
        name
        for name in dir(tstrans.srt.SocketStats)
        if not name.startswith("_")
    }
    extras = srt_stats_props - socket_stats_props
    assert "mbps_estimated_bandwidth" in extras
    assert "bytes_lost_send_side" in extras
    assert "bytes_lost_recv_side" in extras
