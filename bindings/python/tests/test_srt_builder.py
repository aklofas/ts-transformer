"""Tests for `tstrans.srt.Builder`.

Builder is the hybrid fluent + kwargs SRT URL constructor. These tests
cover construction (URL-only + kwargs), mode setter chaining, URL
precedence (URL beats kwargs), passphrase redaction, mode mismatch
errors, and live connect/listen pairing.

Networking tests follow the same pattern as `test_srt_transport.py`:
free TCP-port probe for ephemeral SRT UDP port, listener thread spun
up before caller connects.
"""

from __future__ import annotations

import threading
import time
from typing import Optional

import pytest

import tstrans
import tstrans.srt
from tstrans.exceptions import SrtError, SrtErrorKind

from _builders.ports import free_tcp_port as _free_tcp_port


# --------------------------------------------------------------------------- #
# Module surface                                                              #
# --------------------------------------------------------------------------- #


def test_builder_is_exported() -> None:
    """Builder must be re-exported from `tstrans.srt`."""
    assert tstrans.srt.Builder is not None
    assert "Builder" in tstrans.srt.__all__


# --------------------------------------------------------------------------- #
# Construction                                                                #
# --------------------------------------------------------------------------- #


def test_construct_with_url_only_defaults() -> None:
    """Builder('srt://...') with no kwargs is a valid construction."""
    b = tstrans.srt.Builder("srt://127.0.0.1:9000")
    assert b is not None
    # __repr__ should not error and should NOT mention a passphrase being set.
    r = repr(b)
    assert "Builder(" in r
    assert "passphrase=None" in r


def test_construct_with_all_kwargs() -> None:
    """All 7 documented kwargs accepted at construction."""
    b = tstrans.srt.Builder(
        "srt://127.0.0.1:9000",
        latency_ms=150,
        passphrase="strong-passphrase",
        stream_id="cam-01",
        congestion="live",
        connect_timeout_ms=2000,
        recv_timeout_ms=500,
        send_timeout_ms=500,
    )
    # Passphrase should be redacted in repr.
    assert "<redacted>" in repr(b)
    # No exception means all kwargs successfully landed in the configs.


def test_construct_bad_passphrase_raises_config_invalid() -> None:
    """SRT passphrases are 10-79 chars — a 5-char one should be rejected."""
    with pytest.raises(SrtError) as exc_info:
        tstrans.srt.Builder("srt://127.0.0.1:9000", passphrase="short")
    assert exc_info.value.kind == SrtErrorKind.CONFIG_INVALID


def test_construct_bad_congestion_raises_config_invalid() -> None:
    """`Congestion::from_str_strict` rejects unknown values."""
    with pytest.raises(SrtError) as exc_info:
        tstrans.srt.Builder("srt://127.0.0.1:9000", congestion="bogus")
    assert exc_info.value.kind == SrtErrorKind.CONFIG_INVALID


# --------------------------------------------------------------------------- #
# Fluent chaining                                                             #
# --------------------------------------------------------------------------- #


def test_caller_setter_returns_self() -> None:
    """`.caller()` chainable — returns the same PyClass instance."""
    b = tstrans.srt.Builder("srt://127.0.0.1:9000")
    assert b.caller() is b


def test_listener_setter_returns_self() -> None:
    """`.listener()` chainable."""
    b = tstrans.srt.Builder("srt://0.0.0.0:9000?mode=listener")
    assert b.listener() is b


def test_rendezvous_setter_returns_self() -> None:
    """`.rendezvous()` is callable for forward-compat but finalize will
    raise CONFIG_INVALID — see test_rendezvous_finalize_rejects."""
    b = tstrans.srt.Builder("srt://127.0.0.1:9000")
    assert b.rendezvous() is b


def test_knob_setters_chain() -> None:
    """All knob setters return self and can be chained."""
    b = tstrans.srt.Builder("srt://127.0.0.1:9000")
    chained = (
        b.latency_ms(120)
        .stream_id("alpha")
        .congestion("live")
        .connect_timeout_ms(3000)
        .recv_timeout_ms(500)
        .send_timeout_ms(500)
        .peer_latency_ms(120)
        .recv_latency_ms(120)
        .max_bandwidth_bps(10_000_000)
        .mss(1500)
        .payload_size(1316)
    )
    assert chained is b


def test_passphrase_setter_redacts_in_repr() -> None:
    """`.passphrase()` chainable; the stored value is redacted in repr."""
    b = tstrans.srt.Builder("srt://127.0.0.1:9000")
    b2 = b.passphrase("strong-passphrase-2026")
    assert b2 is b
    assert "<redacted>" in repr(b)
    # Critical: the real passphrase string must NOT appear in repr.
    assert "strong-passphrase-2026" not in repr(b)


# --------------------------------------------------------------------------- #
# Mode mismatch                                                               #
# --------------------------------------------------------------------------- #


def test_connect_rejects_listener_override() -> None:
    """Builder marked listener-mode cannot connect."""
    b = tstrans.srt.Builder("srt://127.0.0.1:9000?mode=listener").listener()
    with pytest.raises(SrtError) as exc_info:
        b.connect()
    assert exc_info.value.kind == SrtErrorKind.CONFIG_INVALID


def test_listen_rejects_caller_override() -> None:
    """Builder marked caller-mode cannot listen."""
    b = tstrans.srt.Builder("srt://127.0.0.1:9000").caller()
    with pytest.raises(SrtError) as exc_info:
        b.listen()
    assert exc_info.value.kind == SrtErrorKind.CONFIG_INVALID


def test_rendezvous_finalize_rejects() -> None:
    """Rendezvous mode is not yet supported by tst-srt."""
    b = tstrans.srt.Builder("srt://127.0.0.1:9000").rendezvous()
    with pytest.raises(SrtError) as exc_info:
        b.connect()
    assert exc_info.value.kind == SrtErrorKind.CONFIG_INVALID


def test_connect_rejects_listener_url() -> None:
    """URL says listener but we asked connect()."""
    b = tstrans.srt.Builder("srt://127.0.0.1:9000?mode=listener")
    with pytest.raises(SrtError) as exc_info:
        b.connect()
    assert exc_info.value.kind == SrtErrorKind.CONFIG_INVALID


def test_listen_rejects_caller_url() -> None:
    """URL is caller (default) but we asked listen()."""
    b = tstrans.srt.Builder("srt://127.0.0.1:9000")
    with pytest.raises(SrtError) as exc_info:
        b.listen()
    assert exc_info.value.kind == SrtErrorKind.CONFIG_INVALID


# --------------------------------------------------------------------------- #
# Listen succeeds + port readback                                             #
# --------------------------------------------------------------------------- #


def test_listen_succeeds_on_kernel_picked_port() -> None:
    """`.listen()` against ?mode=listener with port 0 succeeds; the bound
    port reads back via `local_addr()`."""
    b = tstrans.srt.Builder("srt://0.0.0.0:0?mode=listener")
    with b.listen() as lst:
        host, port = lst.local_addr()
        assert port != 0
        assert host == "0.0.0.0"


# --------------------------------------------------------------------------- #
# Connect against a live listener (real loopback handshake)                   #
# --------------------------------------------------------------------------- #


def test_connect_succeeds_against_live_listener() -> None:
    """Spin up a Listener on a free port via Builder; connect a second
    Builder caller-side; assert both side hold live Socket objects."""
    port = _free_tcp_port()
    listener_b = tstrans.srt.Builder(f"srt://0.0.0.0:{port}?mode=listener")
    lst = listener_b.listen()

    accepted_box: list[Optional[tstrans.srt.Socket]] = [None]
    accept_err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            sock = lst.accept(timeout_ms=5000)
            accepted_box[0] = sock
        except BaseException as exc:  # noqa: BLE001
            accept_err.append(exc)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    # Brief sleep — let the listener thread enter accept() before connect.
    time.sleep(0.1)

    caller_b = tstrans.srt.Builder(f"srt://127.0.0.1:{port}")
    sock = caller_b.connect()
    t.join(timeout=5.0)

    try:
        if accept_err:
            raise accept_err[0]
        assert accepted_box[0] is not None
        # Both sides hold live Sockets.
        assert sock.is_alive()
        assert accepted_box[0].is_alive()
    finally:
        sock.close()
        if accepted_box[0] is not None:
            accepted_box[0].close()
        lst.close()


# --------------------------------------------------------------------------- #
# Timeout on unreachable host                                                 #
# --------------------------------------------------------------------------- #


def test_connect_timeout_triggers_timeout_kind() -> None:
    """Connect against TEST-NET-1 (192.0.2.0/24, RFC 5737 — guaranteed
    no listener) with a short connect_timeout. Either TIMEOUT or
    CONNECT_FAILED is acceptable — both signal the connect did not
    complete."""
    b = tstrans.srt.Builder(
        "srt://192.0.2.1:9000",
        connect_timeout_ms=300,
    )
    with pytest.raises(SrtError) as exc_info:
        b.connect()
    assert exc_info.value.kind in (SrtErrorKind.TIMEOUT, SrtErrorKind.CONNECT_FAILED)


# --------------------------------------------------------------------------- #
# URL precedence (URL wins over kwargs)                                       #
# --------------------------------------------------------------------------- #


def test_url_value_overrides_kwarg_value() -> None:
    """URL-supplied `latency=200` should beat kwarg `latency_ms=100`.

    We can't inspect the final SocketConfig from Python directly; we
    instead verify the URL parses without error AND a connect attempt
    fails at the right boundary (timeout/connect_failed) rather than
    config-invalid. This is a smoke test for the precedence ordering —
    the deeper property (URL wins) is enforced by the unconditional
    overwrite in `UrlOverlay::apply_to_socket` at the Rust layer.
    """
    b = tstrans.srt.Builder(
        "srt://192.0.2.1:9000?latency=200",
        latency_ms=100,
        connect_timeout_ms=300,
    )
    with pytest.raises(SrtError) as exc_info:
        b.connect()
    # The URL parsed without error (CONFIG_INVALID would mean overlay
    # rejected the value). TIMEOUT/CONNECT_FAILED means we got past
    # config to the actual handshake.
    assert exc_info.value.kind in (SrtErrorKind.TIMEOUT, SrtErrorKind.CONNECT_FAILED)
