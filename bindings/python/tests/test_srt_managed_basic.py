"""Tests for `tstrans.srt.ManagedSender` / `ManagedReceiver`.

Auto-reconnect ergonomics on top of `tst_pipeline::ManagedTransport
<SrtTransport>` (send side) and `ManagedRecvTransport<SrtTransport>`
(recv side). Tests verify:

1. Basic loopback round-trip through `ManagedSender` (caller mode).
2. Basic loopback round-trip through `ManagedReceiver` (listener mode).
3. `ReconnectPolicy` ergonomics — kwargs accepted, `reconnect_attempts`
   reads 0 before any break.
4. `is_alive()` flips from True → False after `close()` on both ends.

The end-to-end reconnect path (kill listener mid-stream + spawn new
listener; verify recovery) is brittle in unit-test form because libsrt
takes seconds to notice the break and the reconnect timing is
policy-dependent. The plan permits a `@pytest.mark.skip` placeholder
for that scenario.
"""

from __future__ import annotations

import threading
import time
from typing import Optional, Tuple

import pytest

import tstrans
import tstrans.srt
from tstrans.exceptions import SrtError, SrtErrorKind
from tstrans.srt import ManagedTransportStats

from _builders.ports import free_tcp_port as _free_tcp_port


def _make_managed_pair(
    port: int,
) -> Tuple[tstrans.srt.ManagedSender, tstrans.srt.ManagedReceiver]:
    """Spawn a listener-mode ManagedReceiver on a background thread;
    once it's accepting, connect a caller-mode ManagedSender from the
    main thread. Mirrors test_srt_transport._make_loopback_pair."""
    listener_url = f"srt://:{port}?mode=listener"
    caller_url = f"srt://127.0.0.1:{port}?mode=caller"

    receiver_box: list[tstrans.srt.ManagedReceiver] = []
    receiver_err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            r = tstrans.srt.ManagedReceiver.from_url(listener_url)
            receiver_box.append(r)
        except BaseException as exc:  # noqa: BLE001
            receiver_err.append(exc)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)  # let listener thread bind before we try to connect

    sender = tstrans.srt.ManagedSender.from_url(caller_url)
    t.join(timeout=5.0)
    if receiver_err:
        sender.close()
        raise receiver_err[0]
    if not receiver_box:
        sender.close()
        raise RuntimeError("listener thread did not accept within 5 s")
    return sender, receiver_box[0]


# --------------------------------------------------------------------------- #
# Module re-exports                                                           #
# --------------------------------------------------------------------------- #


def test_module_re_exports() -> None:
    """`tstrans.srt` must expose both managed-basic PyClasses."""
    assert tstrans.srt.ManagedSender is not None
    assert tstrans.srt.ManagedReceiver is not None
    assert "ManagedSender" in tstrans.srt.__all__
    assert "ManagedReceiver" in tstrans.srt.__all__


# --------------------------------------------------------------------------- #
# 1. ManagedSender.from_url round-trip                                        #
# --------------------------------------------------------------------------- #


def test_managed_sender_round_trip_via_loopback() -> None:
    """A `ManagedSender` to a live listener should send bytes that the
    paired `ManagedReceiver` reads back. Verifies the wrap doesn't add
    behavioral drift over the plain `Sender`."""
    port = _free_tcp_port()
    sender, receiver = _make_managed_pair(port)
    try:
        # 1316 bytes = SRT_TS_BUNDLE_BYTES, a multiple of 188.
        # `Sender::send_ts` bundles 7×188 internally.
        payload = b"\x47" + b"\x00" * 187  # one 188-byte TS packet
        for _ in range(7):
            sender.send_bytes(payload)
        # Receive at least the first packet — opportunistic drain
        # quantum from the plain Receiver's recv_bytes semantics.
        received = receiver.recv_bytes(max_len=1500)
        assert len(received) >= 188
        assert len(received) % 188 == 0
        # First byte must be the TS sync byte we wrote.
        assert received[0] == 0x47
    finally:
        sender.close()
        receiver.close()


# --------------------------------------------------------------------------- #
# 2. ManagedReceiver.from_url round-trip                                      #
# --------------------------------------------------------------------------- #


def test_managed_receiver_round_trip_via_loopback() -> None:
    """Symmetric of the above — same loopback pair, but driving from the
    receiver side: verify `recv_bytes` returns what the paired sender
    pushed."""
    port = _free_tcp_port()
    sender, receiver = _make_managed_pair(port)
    try:
        payload = b"\x47" + b"\xff" * 187  # 188 bytes, distinctive content
        for _ in range(7):
            sender.send_bytes(payload)
        received = receiver.recv_bytes(max_len=1500)
        assert len(received) >= 188
        # Sync byte preserved across the loopback.
        assert received[0] == 0x47
        assert received[1] == 0xFF
    finally:
        sender.close()
        receiver.close()


# --------------------------------------------------------------------------- #
# 3. ReconnectPolicy ergonomics                                               #
# --------------------------------------------------------------------------- #


def test_managed_sender_accepts_reconnect_policy() -> None:
    """`ManagedSender.from_url(url, policy=ReconnectPolicy(...))` must
    accept the ReconnectPolicy PyClass directly. The initial connect doesn't trigger
    any reconnect, so the underlying counter isn't exposed on the
    sender (drift documented in module rustdoc)."""
    port = _free_tcp_port()
    policy = tstrans.srt.ReconnectPolicy(max_attempts=3)
    assert policy.max_attempts == 3

    # Spin up a listener so the initial connect succeeds.
    listener_url = f"srt://:{port}?mode=listener"
    caller_url = f"srt://127.0.0.1:{port}?mode=caller"
    receiver_box: list[tstrans.srt.ManagedReceiver] = []

    def accept_worker() -> None:
        try:
            r = tstrans.srt.ManagedReceiver.from_url(listener_url, policy=policy)
            receiver_box.append(r)
        except BaseException:  # noqa: BLE001
            pass

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)

    sender = tstrans.srt.ManagedSender.from_url(caller_url, policy=policy)
    t.join(timeout=5.0)
    try:
        assert sender.is_alive()
        # On the receive side, `reconnect_attempts()` reads 0 before any
        # break has happened — the initial bind+accept does NOT count.
        if receiver_box:
            assert receiver_box[0].reconnect_attempts() == 0
    finally:
        sender.close()
        if receiver_box:
            receiver_box[0].close()


def test_managed_sender_default_policy() -> None:
    """Omitting `policy=` falls back to `ReconnectPolicy()` (the
    defaults). Verifies the kwarg is optional, not required."""
    port = _free_tcp_port()
    listener_url = f"srt://:{port}?mode=listener"
    caller_url = f"srt://127.0.0.1:{port}?mode=caller"
    receiver_box: list[tstrans.srt.ManagedReceiver] = []

    def accept_worker() -> None:
        try:
            r = tstrans.srt.ManagedReceiver.from_url(listener_url)
            receiver_box.append(r)
        except BaseException:  # noqa: BLE001
            pass

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)

    sender = tstrans.srt.ManagedSender.from_url(caller_url)  # no policy=
    t.join(timeout=5.0)
    try:
        assert sender.is_alive()
    finally:
        sender.close()
        if receiver_box:
            receiver_box[0].close()


# --------------------------------------------------------------------------- #
# 4. is_alive flips on close                                                  #
# --------------------------------------------------------------------------- #


def test_is_alive_flips_after_close() -> None:
    """Both `ManagedSender` and `ManagedReceiver` should report
    `is_alive() == True` while the inner transport is held, and
    `False` after `close()`. Verifies the close path also latches
    cancel on the recv side."""
    port = _free_tcp_port()
    sender, receiver = _make_managed_pair(port)
    try:
        assert sender.is_alive() is True
        assert receiver.is_alive() is True
        sender.close()
        receiver.close()
        assert sender.is_alive() is False
        assert receiver.is_alive() is False
    finally:
        # Idempotent close — safe to call again.
        sender.close()
        receiver.close()


# --------------------------------------------------------------------------- #
# 5. Construction errors                                                      #
# --------------------------------------------------------------------------- #


def test_managed_sender_rejects_listener_url() -> None:
    """A URL with `mode=listener` passed to `ManagedSender.from_url`
    raises `SrtError(CONFIG_INVALID)` BEFORE any socket operation.
    This is checked up-front so the same misconfiguration doesn't
    quietly route into a reconnect loop."""
    port = _free_tcp_port()
    with pytest.raises(SrtError) as exc_info:
        tstrans.srt.ManagedSender.from_url(f"srt://127.0.0.1:{port}?mode=listener")
    assert exc_info.value.kind == SrtErrorKind.CONFIG_INVALID


def test_managed_receiver_rejects_caller_url() -> None:
    """Symmetric: `ManagedReceiver.from_url` with `mode=caller` raises
    `SrtError(CONFIG_INVALID)`."""
    port = _free_tcp_port()
    with pytest.raises(SrtError) as exc_info:
        tstrans.srt.ManagedReceiver.from_url(f"srt://127.0.0.1:{port}?mode=caller")
    assert exc_info.value.kind == SrtErrorKind.CONFIG_INVALID


# --------------------------------------------------------------------------- #
# 6. Context-manager protocol                                                 #
# --------------------------------------------------------------------------- #


def test_context_manager_closes() -> None:
    """`with ManagedSender.from_url(...) as s:` should close on exit."""
    port = _free_tcp_port()
    listener_url = f"srt://:{port}?mode=listener"
    caller_url = f"srt://127.0.0.1:{port}?mode=caller"
    receiver_box: list[tstrans.srt.ManagedReceiver] = []

    def accept_worker() -> None:
        try:
            r = tstrans.srt.ManagedReceiver.from_url(listener_url)
            receiver_box.append(r)
        except BaseException:  # noqa: BLE001
            pass

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)

    with tstrans.srt.ManagedSender.from_url(caller_url) as sender:
        assert sender.is_alive() is True
    # After context exit, the sender is closed.
    assert sender.is_alive() is False
    t.join(timeout=5.0)
    if receiver_box:
        receiver_box[0].close()


# --------------------------------------------------------------------------- #
# 7. reconnect_stats()                                                        #
# --------------------------------------------------------------------------- #


def test_managed_sender_reconnect_stats_healthy_link() -> None:
    """On a healthy link (no break yet), `reconnect_stats()` must return
    the typed `ManagedTransportStats` object with all counters at zero
    and `reconnecting` False — the deterministic minimum this test can
    assert without forcing an actual outage."""
    port = _free_tcp_port()
    sender, receiver = _make_managed_pair(port)
    try:
        stats = sender.reconnect_stats()
        assert isinstance(stats, ManagedTransportStats)
        assert stats.reconnect_attempts == 0
        assert stats.reconnect_successes == 0
        assert stats.gap_len == 0
        assert stats.gap_messages_dropped == 0
        assert stats.gap_bytes_dropped == 0
        assert stats.reconnecting is False
    finally:
        sender.close()
        receiver.close()


def test_managed_sender_reconnect_stats_closed_raises() -> None:
    """`reconnect_stats()` on a closed sender raises `SrtError(CLOSED)`,
    matching `socket_stats()` / `srt_stats()` symmetry."""
    port = _free_tcp_port()
    sender, receiver = _make_managed_pair(port)
    sender.close()
    receiver.close()
    with pytest.raises(SrtError) as exc_info:
        sender.reconnect_stats()
    assert exc_info.value.kind == SrtErrorKind.CLOSED


# --------------------------------------------------------------------------- #
# 8. Reconnect on listener-cycle (deferred / skipped)                         #
# --------------------------------------------------------------------------- #


@pytest.mark.skip(
    reason="Listener-cycle reconnect requires synchronous teardown + rebuild "
    "of a libsrt listener within the policy's backoff window. The timing is "
    "flaky in unit-test form (libsrt's break-detection can take seconds; "
    "the policy default backoff is 100ms..=10s exponential). Will land as "
    "an integration test in a later wave."
)
def test_managed_sender_recovers_after_listener_restart() -> None:
    """Listener teardown + restart should trigger the reconnect path."""
    raise NotImplementedError
