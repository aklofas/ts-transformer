"""Tests for `tstrans.srt` listen rules: `Listener.rules()`, `ListenRule`,
`ListenRules`, `RejectReason` and `user_defined`.

Networking tests follow `test_srt_builder.py`: a free TCP-port probe for
the ephemeral SRT UDP port, the listener's `accept` on a thread before the
caller connects.
"""

from __future__ import annotations

import threading
import time
from typing import Optional

import pytest

import tstrans
import tstrans.srt
from tstrans.exceptions import SrtError, SrtErrorKind
from tstrans.srt import ListenRule, RejectReason, user_defined

from _builders.ports import free_tcp_port as _free_tcp_port


def _listen() -> tuple[tstrans.srt.Listener, int]:
    port = _free_tcp_port()
    lst = tstrans.srt.Builder(f"srt://0.0.0.0:{port}?mode=listener").listen()
    return lst, port


class _AcceptThread:
    """`accept(timeout_ms)` on a daemon thread; keeps the socket or error."""

    def __init__(self, lst: tstrans.srt.Listener, timeout_ms: int) -> None:
        self.sock: Optional[tstrans.srt.Socket] = None
        self.err: Optional[BaseException] = None

        def run() -> None:
            try:
                self.sock = lst.accept(timeout_ms=timeout_ms)
            except BaseException as exc:  # noqa: BLE001
                self.err = exc

        self.thread = threading.Thread(target=run, daemon=True)
        self.thread.start()
        # Let the thread enter accept() before the caller connects.
        time.sleep(0.1)

    def join(self) -> None:
        self.thread.join(timeout=10.0)
        assert not self.thread.is_alive()


def test_module_exports_listen_rule_names() -> None:
    for name in ("ListenRule", "ListenRules", "RejectReason"):
        assert name in tstrans.srt.__all__
        assert hasattr(tstrans.srt, name)


def test_reject_rule_refuses_caller_with_reason() -> None:
    lst, port = _listen()
    try:
        rules = lst.rules()
        rules.set("publish:amber", ListenRule.reject(RejectReason.FORBIDDEN))
        acc = _AcceptThread(lst, 2000)
        with pytest.raises(SrtError) as ei:
            tstrans.srt.Builder(
                f"srt://127.0.0.1:{port}?streamid=publish:amber"
            ).connect()
        assert ei.value.kind == SrtErrorKind.CONNECT_FAILED
        assert "Forbidden" in str(ei.value)
        acc.join()
        assert isinstance(acc.err, SrtError)
        assert acc.err.kind == SrtErrorKind.TIMEOUT
    finally:
        lst.close()


def test_passphrase_rule_round_trip() -> None:
    lst, port = _listen()
    try:
        lst.rules().set("a", ListenRule.accept_with("secretsecretsecret"))

        acc = _AcceptThread(lst, 2000)
        sock = tstrans.srt.Builder(
            f"srt://127.0.0.1:{port}?streamid=a", passphrase="secretsecretsecret"
        ).connect()
        acc.join()
        try:
            if acc.err is not None:
                raise acc.err
            assert acc.sock is not None
            assert sock.is_alive()
        finally:
            sock.close()
            if acc.sock is not None:
                acc.sock.close()

        acc = _AcceptThread(lst, 2000)
        with pytest.raises(SrtError) as ei:
            tstrans.srt.Builder(
                f"srt://127.0.0.1:{port}?streamid=a", passphrase="wrongwrongwrong"
            ).connect()
        assert ei.value.kind == SrtErrorKind.CONNECT_FAILED
        assert "BadSecret" in str(ei.value)
        acc.join()
    finally:
        lst.close()


def test_rules_edit_while_accept_parked() -> None:
    lst, _port = _listen()
    rules = lst.rules()
    cancel = lst.cancel_handle()
    acc = _AcceptThread(lst, 60_000)
    try:
        start = time.monotonic()
        rules.set_default(ListenRule.reject(RejectReason.UNAUTHORIZED))
        rules.set("a", ListenRule.accept())
        assert len(rules) == 1
        assert rules.remove("a") is True
        assert rules.remove("a") is False
        assert len(rules) == 0
        rules.clear()
        assert time.monotonic() - start < 1.0
    finally:
        cancel.cancel()
        acc.join()
        lst.close()
    assert isinstance(acc.err, SrtError)
    assert acc.err.kind == SrtErrorKind.CLOSED


def test_closed_listener() -> None:
    lst, _port = _listen()
    rules = lst.rules()
    lst.close()
    with pytest.raises(SrtError) as ei:
        rules.set("a", ListenRule.accept())
    assert ei.value.kind == SrtErrorKind.CLOSED
    # Every edit and the length raise CLOSED, as on the JVM.
    for call in (lambda: rules.remove("a"), rules.clear, lambda: len(rules)):
        with pytest.raises(SrtError) as ei:
            call()
        assert ei.value.kind == SrtErrorKind.CLOSED
    with pytest.raises(SrtError) as ei:
        lst.rules()
    assert ei.value.kind == SrtErrorKind.CLOSED


def test_validation() -> None:
    with pytest.raises(ValueError):
        ListenRule.reject(10)
    with pytest.raises(ValueError):
        user_defined(1999)
    assert user_defined(2000) == 2000
    # Any code >= 1000 is accepted, typed or not.
    ListenRule.reject(1000)
    ListenRule.reject(user_defined(4242))

    lst, _port = _listen()
    try:
        with pytest.raises(SrtError) as ei:
            lst.rules().set("a", ListenRule.accept_with("short"))
        assert ei.value.kind == SrtErrorKind.CONFIG_INVALID
    finally:
        lst.close()

    rule = ListenRule.accept_with("secretsecretsecret")
    assert "secretsecretsecret" not in repr(rule)
    assert "Forbidden" in repr(ListenRule.reject(RejectReason.FORBIDDEN))


def test_reject_reason_table_matches_native() -> None:
    native = dict(tstrans.srt._native_reject_reason_table())
    assert native == {m.value: m.name for m in RejectReason}
    assert len(native) == 21
