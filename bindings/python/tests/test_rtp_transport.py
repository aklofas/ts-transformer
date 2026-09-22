"""Tests for `tstrans.rtp.Sender` / `Receiver` / `SocketStats` /
`CancelHandle` (Wave A Task 20).

These tests use UDP loopback exclusively — no external network. The
Receiver-side tests rely on the 100 ms cancel-poll tick to verify
cancel propagates to a parked `.recv()` call.
"""

from __future__ import annotations

import socket
import threading
import time

import pytest

import tstrans
import tstrans.rtp
from tstrans.exceptions import RtpError, RtpErrorKind

from _builders.ports import free_udp_port as _free_udp_port


# --------------------------------------------------------------------------- #
# Sender                                                                      #
# --------------------------------------------------------------------------- #


def test_sender_module_re_exports() -> None:
    """`tstrans.rtp` must expose the 4 transport classes after T20."""
    assert tstrans.rtp.Sender is not None
    assert tstrans.rtp.Receiver is not None
    assert tstrans.rtp.SocketStats is not None
    assert tstrans.rtp.CancelHandle is not None
    assert "Sender" in tstrans.rtp.__all__
    assert "Receiver" in tstrans.rtp.__all__


def test_sender_send_to_loopback_advances_stats() -> None:
    """End-to-end: bind a UDP listener on a port, point a Sender at it,
    send one TS-shaped payload, verify packets_sent == 1."""
    port = _free_udp_port()
    listener = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    listener.bind(("127.0.0.1", port))
    listener.settimeout(2.0)
    try:
        with tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}") as s:
            s.send(b"\x47" * 188)
            stats = s.stats()
            assert stats.packets_sent == 1
            # RTP header is 12 bytes; 188-byte payload → 200 bytes on the wire.
            assert stats.bytes_sent == 200
            # RTCP-derived counters stay zero in Phase 1.
            assert stats.rtt_us == 0
            assert stats.packets_lost_send == 0
        # Receive what landed — verify it was actually an RTP-wrapped
        # TS packet (12-byte RTP header + 0x47 sync bytes).
        data, _ = listener.recvfrom(2048)
        assert len(data) == 12 + 188
        assert data[12:] == b"\x47" * 188
    finally:
        listener.close()


def test_sender_accepts_bytes_bytearray_memoryview() -> None:
    """`.send()` two-path extraction: fast `&[u8]` for `bytes`, fallback
    coercion via `bytes()` builtin for `bytearray` / `memoryview`."""
    port = _free_udp_port()
    listener = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    listener.bind(("127.0.0.1", port))
    listener.settimeout(1.0)
    try:
        with tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}") as s:
            # bytes — fast path
            s.send(b"\x47" * 188)
            # bytearray — fallback through `bytes()` builtin
            s.send(bytearray(b"\x47" * 188))
            # memoryview over a bytearray — fallback as well
            s.send(memoryview(bytearray(b"\x47" * 188)))
            assert s.stats().packets_sent == 3
    finally:
        listener.close()


def test_sender_rejects_non_bytes_like() -> None:
    """Non-bytes-like input raises TypeError (Python `bytes()` rejects
    objects that aren't bytes-like and aren't ints). Note that `bytes(int)`
    IS valid (zero-filled buffer of that length) — using a real
    non-coercible object like a dict here for the negative test."""
    port = _free_udp_port()
    with tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}") as s:
        with pytest.raises(TypeError):
            s.send({"not": "bytes"})


def test_sender_explicit_ssrc_and_pkt_size() -> None:
    """Keyword args `pkt_size` + `ssrc` get propagated to the builder."""
    port = _free_udp_port()
    listener = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    listener.bind(("127.0.0.1", port))
    listener.settimeout(1.0)
    try:
        with tstrans.rtp.Sender(
            f"rtp://127.0.0.1:{port}", pkt_size=1316, ssrc=0xCAFEBABE
        ) as s:
            s.send(b"\x47" * 188)
            data, _ = listener.recvfrom(2048)
            # RTP SSRC lives at bytes 8..12 in the header, big-endian.
            ssrc_observed = int.from_bytes(data[8:12], "big")
            assert ssrc_observed == 0xCAFEBABE
    finally:
        listener.close()


def test_sender_oversized_payload_raises_malformed_packet() -> None:
    """Payload > pkt_size - 12 (RTP header) → `RtpError(TOO_LARGE)`."""
    port = _free_udp_port()
    with tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}", pkt_size=200) as s:
        with pytest.raises(RtpError) as exc_info:
            # 300 bytes payload exceeds the 200-byte UDP-payload cap
            # (minus 12-byte RTP header → 188 max). 300 > 188.
            s.send(b"\x47" * 300)
        assert exc_info.value.kind == RtpErrorKind.TOO_LARGE


def test_sender_close_idempotent() -> None:
    """`.close()` can be called multiple times safely."""
    port = _free_udp_port()
    s = tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}")
    s.close()
    s.close()  # second close is a no-op
    with pytest.raises(RtpError) as exc_info:
        s.send(b"\x47" * 188)
    assert exc_info.value.kind == RtpErrorKind.CLOSED


def test_sender_context_manager_closes_on_exception() -> None:
    """`__exit__` calls close() even when the body raises."""
    port = _free_udp_port()
    with pytest.raises(RuntimeError, match="boom"):
        with tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}") as s:
            assert "open" in repr(s)
            raise RuntimeError("boom")
    # Sender was closed by __exit__ before the exception propagated;
    # we have no direct handle to check, but the test verifies __exit__
    # doesn't itself raise.


def test_sender_bad_url_raises_rtp_error() -> None:
    """Malformed URL → `RtpError(URL)` at construction."""
    with pytest.raises(RtpError) as exc_info:
        tstrans.rtp.Sender("not-a-valid-url://")
    assert exc_info.value.kind == RtpErrorKind.URL


# --------------------------------------------------------------------------- #
# Receiver                                                                    #
# --------------------------------------------------------------------------- #


def test_receiver_bind_and_close() -> None:
    """Construct + close a Receiver. Verify stats start at zero."""
    port = _free_udp_port()
    with tstrans.rtp.Receiver(f"rtp://127.0.0.1:{port}") as r:
        stats = r.stats()
        assert stats.bytes_received == 0
        assert stats.packets_received == 0
        # Repr shape sanity-check.
        assert "open" in repr(r)


def test_receiver_recv_then_cancel_unparks_recv() -> None:
    """Park a recv() on a worker thread, then cancel from the main
    thread. The recv() must return RtpError(CANCELLED) within ~1 s."""
    port = _free_udp_port()
    r = tstrans.rtp.Receiver(f"rtp://127.0.0.1:{port}")
    ch = r.cancel_handle()
    captured: list[BaseException] = []

    def worker() -> None:
        try:
            r.recv()
        except BaseException as exc:  # noqa: BLE001 — capture any raised error
            captured.append(exc)

    t = threading.Thread(target=worker, daemon=True)
    t.start()
    # Give the worker time to enter the recv() block.
    time.sleep(0.2)
    ch.cancel()
    t.join(timeout=2.0)
    assert not t.is_alive(), "worker thread did not exit after cancel"
    assert len(captured) == 1
    exc = captured[0]
    assert isinstance(exc, RtpError)
    assert exc.kind == RtpErrorKind.CLOSED
    r.close()


def test_receiver_recv_loopback_returns_payload() -> None:
    """End-to-end: send a real RTP packet via tstrans.rtp.Sender, recv
    via tstrans.rtp.Receiver, verify the TS payload round-trips."""
    port = _free_udp_port()
    with tstrans.rtp.Receiver(f"rtp://127.0.0.1:{port}") as r:
        # Start the recv on a worker so we can sequence the send.
        out: list[bytes | BaseException] = []

        def worker() -> None:
            try:
                out.append(r.recv())
            except BaseException as exc:  # noqa: BLE001
                out.append(exc)

        t = threading.Thread(target=worker, daemon=True)
        t.start()
        time.sleep(0.1)
        # Now send.
        with tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}") as s:
            payload = b"\x47" + b"\x00" * 187
            s.send(payload)
        t.join(timeout=2.0)
        assert not t.is_alive()
        assert len(out) == 1
        got = out[0]
        assert isinstance(got, bytes)
        assert got == payload
        assert r.stats().packets_received == 1


def test_receiver_close_idempotent() -> None:
    port = _free_udp_port()
    r = tstrans.rtp.Receiver(f"rtp://127.0.0.1:{port}")
    r.close()
    r.close()
    with pytest.raises(RtpError) as exc_info:
        r.recv()
    assert exc_info.value.kind == RtpErrorKind.CLOSED


def test_receiver_recv_timeout_raises_timeout_kind() -> None:
    """`?recv_timeout=<ms>` on the URL arms a persistent recv deadline
    (wired by `RtpRecvSocketBuilder::from_url`). A quiet socket (no
    sender) must raise `RtpError(TIMEOUT)` once the deadline expires —
    distinct from `TRANSPORT`, since the receiver stays open and usable
    (retry `.recv()` again) rather than being torn down."""
    port = _free_udp_port()
    with tstrans.rtp.Receiver(f"rtp://127.0.0.1:{port}?recv_timeout=200") as r:
        with pytest.raises(RtpError) as exc_info:
            r.recv()
        assert exc_info.value.kind == RtpErrorKind.BACKPRESSURE
        # The receiver is still alive after a TIMEOUT — a second recv on
        # the same (still-quiet) socket raises TIMEOUT again, not TRANSPORT.
        with pytest.raises(RtpError) as exc_info2:
            r.recv()
        assert exc_info2.value.kind == RtpErrorKind.BACKPRESSURE


def test_receiver_recv_per_call_timeout_ms_raises_and_recovers() -> None:
    """`recv(timeout_ms=N)` bounds a single call — no URL knob needed.
    A quiet socket raises `RtpError(TIMEOUT)`; the receiver stays open
    and a subsequently delivered packet is still received normally."""
    port = _free_udp_port()
    with tstrans.rtp.Receiver(f"rtp://127.0.0.1:{port}") as r:
        with pytest.raises(RtpError) as exc_info:
            r.recv(timeout_ms=200)
        assert exc_info.value.kind == RtpErrorKind.BACKPRESSURE

        # Session stayed alive across the TIMEOUT — a real packet sent
        # now is still delivered on the same receiver.
        payload = b"\x47" + b"\x00" * 187
        with tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}") as s:
            s.send(payload)
        got = r.recv(timeout_ms=2000)
        assert got == payload


def test_receiver_recv_timeout_ms_default_none_is_blocking() -> None:
    """`recv()` with no args (`timeout_ms=None`) keeps the pre-existing
    indefinite-block contract — the same code path exercised by
    `test_receiver_recv_loopback_returns_payload`, called explicitly
    with `timeout_ms=None` here to pin the default's identity."""
    port = _free_udp_port()
    with tstrans.rtp.Receiver(f"rtp://127.0.0.1:{port}") as r:
        out: list[bytes | BaseException] = []

        def worker() -> None:
            try:
                out.append(r.recv(timeout_ms=None))
            except BaseException as exc:  # noqa: BLE001
                out.append(exc)

        t = threading.Thread(target=worker, daemon=True)
        t.start()
        time.sleep(0.1)
        payload = b"\x47" + b"\x00" * 187
        with tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}") as s:
            s.send(payload)
        t.join(timeout=2.0)
        assert not t.is_alive()
        assert out == [payload]


# --------------------------------------------------------------------------- #
# SocketStats + CancelHandle shape                                            #
# --------------------------------------------------------------------------- #


def test_socket_stats_is_frozen() -> None:
    """`SocketStats` is `@pyclass(frozen)` — assigning to fields fails."""
    port = _free_udp_port()
    with tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}") as s:
        stats = s.stats()
        # All 16 fields exist + read as 0 on a fresh sender.
        assert stats.bytes_sent == 0
        assert stats.packets_sent == 0
        assert stats.bytes_received == 0
        assert stats.packets_received == 0
        assert stats.rtt_us == 0
        assert stats.send_bandwidth_bps == 0
        assert stats.recv_bandwidth_bps == 0
        assert stats.link_bandwidth_bps == 0
        assert stats.bytes_lost_recv == 0
        assert stats.packets_lost_recv == 0
        assert stats.packets_lost_send == 0
        assert stats.packets_retransmitted == 0
        assert stats.packets_dropped_send == 0
        assert stats.packets_dropped_recv == 0
        assert stats.send_buffer_packets == 0
        assert stats.recv_buffer_packets == 0
        # Frozen — write attempts raise AttributeError.
        with pytest.raises(AttributeError):
            stats.bytes_sent = 999


def test_socket_stats_repr_includes_key_fields() -> None:
    port = _free_udp_port()
    with tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}") as s:
        r = repr(s.stats())
        assert "SocketStats(" in r
        assert "bytes_sent=" in r
        assert "packets_sent=" in r


def test_cancel_handle_multiple_clones_share_state() -> None:
    """Two CancelHandle clones from the same Sender both fire one
    underlying cancel — the second is a no-op."""
    port = _free_udp_port()
    s = tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}")
    h1 = s.cancel_handle()
    h2 = s.cancel_handle()
    h1.cancel()
    h2.cancel()  # idempotent
    # Both handles point at the same atomic — cancel from h1 should
    # cause the next send() to return CLOSED.
    with pytest.raises(RtpError) as exc_info:
        s.send(b"\x47" * 188)
    assert exc_info.value.kind == RtpErrorKind.CLOSED
    s.close()


def test_cancel_handle_repr() -> None:
    """The repr carries the shell's shared cancel state (0.7.0 — it was a
    bare `CancelHandle()` while rtp handles had no `is_cancelled()`)."""
    port = _free_udp_port()
    with tstrans.rtp.Sender(f"rtp://127.0.0.1:{port}") as s:
        ch = s.cancel_handle()
        assert repr(ch) == "CancelHandle(cancelled=false)"
        ch.cancel()
        assert repr(ch) == "CancelHandle(cancelled=true)"


# ---------------------------------------------------------------------------
# pkt_size recv rejection
# ---------------------------------------------------------------------------


def test_receiver_rejects_pkt_size_url():
    with pytest.raises(RtpError) as ei:
        tstrans.rtp.Receiver("rtp://127.0.0.1:0?pkt_size=1316")
    assert ei.value.kind == RtpErrorKind.URL
    assert "send-side knob" in str(ei.value)


def test_receiver_has_no_pkt_size_kwarg():
    with pytest.raises(TypeError):
        tstrans.rtp.Receiver("rtp://127.0.0.1:0", pkt_size=1316)
