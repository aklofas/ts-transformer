"""Wave A T4 — verify every `SrtErrorKind` variant is raisable from a
real `tstrans.srt` code path.

Each test triggers a real failure (bad URL, unreachable port, wrong
mode, etc.) and asserts the resulting `SrtError` carries the expected
`.kind`. Variants that have no tractable trigger from pure-Python
loopback are exercised via the Rust-side `_raise_srt_error_for_test`
hook so the test suite stays as a second-line check on top of the
`import tstrans` startup check (`raise.rs::check_error_kinds`, which is
the primary guarantee that every kind resolves to a real member).
"""

from __future__ import annotations

import pytest

import tstrans
from tstrans._native import _raise_srt_error_for_test
from tstrans.exceptions import SrtError, SrtErrorKind
from tstrans.srt import Receiver, Sender


# --------------------------------------------------------------------------- #
# CONFIG_INVALID — most accessible via bad URLs / wrong-mode rejections       #
# --------------------------------------------------------------------------- #


def test_config_invalid_from_bad_url() -> None:
    """A URL that doesn't even parse → CONFIG_INVALID (UrlError catchall)."""
    with pytest.raises(SrtError) as ei:
        Sender.from_url("not-an-srt-url")
    assert ei.value.kind == SrtErrorKind.CONFIG_INVALID


def test_config_invalid_wrong_scheme() -> None:
    """`http://...` is not `srt://...` — UrlError::WrongScheme."""
    with pytest.raises(SrtError) as ei:
        Sender.from_url("http://127.0.0.1:9000")
    assert ei.value.kind == SrtErrorKind.CONFIG_INVALID


def test_config_invalid_listener_url_to_sender() -> None:
    """`Sender.from_url` rejects `?mode=listener` URLs — explicit
    CONFIG_INVALID before any socket touches the network."""
    with pytest.raises(SrtError) as ei:
        Sender.from_url("srt://127.0.0.1:9000?mode=listener")
    assert ei.value.kind == SrtErrorKind.CONFIG_INVALID


def test_config_invalid_caller_url_to_receiver() -> None:
    """Mirror of above — Receiver rejects `?mode=caller` URLs."""
    with pytest.raises(SrtError) as ei:
        Receiver.from_url("srt://127.0.0.1:9000?mode=caller")
    assert ei.value.kind == SrtErrorKind.CONFIG_INVALID


# --------------------------------------------------------------------------- #
# CONNECT_FAILED / TIMEOUT — accessible via unreachable peer addresses        #
# --------------------------------------------------------------------------- #


def test_connect_failed_or_timeout_on_unreachable_port() -> None:
    """Caller against a port with no listener — libsrt may surface this
    as either CONNECT_FAILED (RST-like) or TIMEOUT depending on the
    platform and timing. Both are acceptable."""
    with pytest.raises(SrtError) as ei:
        Sender.from_url(
            "srt://127.0.0.1:1?mode=caller&connect_timeout=300&latency=20"
        )
    assert ei.value.kind in (SrtErrorKind.CONNECT_FAILED, SrtErrorKind.TIMEOUT)


def test_timeout_via_test_net_1() -> None:
    """`192.0.2.0/24` is the IETF TEST-NET-1 block (RFC 5737) — guaranteed
    non-routable. The SRT handshake never completes; expect TIMEOUT
    (or, more rarely, a system CONNECT_FAILED from a host-unreachable
    ICMP)."""
    with pytest.raises(SrtError) as ei:
        Sender.from_url(
            "srt://192.0.2.1:9000?mode=caller&connect_timeout=500&latency=20"
        )
    assert ei.value.kind in (SrtErrorKind.TIMEOUT, SrtErrorKind.CONNECT_FAILED)


# --------------------------------------------------------------------------- #
# CLOSED — accessible via close() then a subsequent socket-stats call         #
# --------------------------------------------------------------------------- #


def test_closed_after_sender_explicit_close() -> None:
    """Constructing a Sender against a never-listening port fails at
    handshake; we instead force CLOSED by using the test-helper hook.
    The startup check ensures `CLOSED` resolves on `SrtErrorKind`; this
    test confirms the variant routes through Python correctly."""
    with pytest.raises(SrtError) as ei:
        _raise_srt_error_for_test("CLOSED", "transport closed by caller")
    assert ei.value.kind == SrtErrorKind.CLOSED
    assert "closed" in str(ei.value).lower()


# --------------------------------------------------------------------------- #
# BROKEN / BACKPRESSURE / ACCEPT_FAILED / IO — covered via the test helper.  #
# These variants need a live SRT session to trigger naturally — covered by    #
# the T2 transport tests; here we use the helper as a second-line check on    #
# the kind-string → exception-class wiring.                                   #
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize(
    "kind_str,expected",
    [
        ("BROKEN", SrtErrorKind.BROKEN),
        ("BACKPRESSURE", SrtErrorKind.BACKPRESSURE),
        ("ACCEPT_FAILED", SrtErrorKind.ACCEPT_FAILED),
        ("IO", SrtErrorKind.IO),
    ],
)
def test_kind_wiring_via_helper(kind_str: str, expected: SrtErrorKind) -> None:
    """Each kind name in `_raise_srt_error_for_test` lands on the
    corresponding `SrtErrorKind` enum value with the message preserved."""
    with pytest.raises(SrtError) as ei:
        _raise_srt_error_for_test(kind_str, f"sentinel-{kind_str}")
    assert ei.value.kind == expected
    assert ei.value.message == f"sentinel-{kind_str}"
    assert str(ei.value) == f"sentinel-{kind_str}"


# --------------------------------------------------------------------------- #
# Exception class identity — make sure isinstance hooks work                  #
# --------------------------------------------------------------------------- #


def test_srt_error_is_tst_error_subclass() -> None:
    """`SrtError` inherits from `tstrans.exceptions.TstError`."""
    err = tstrans.exceptions.SrtError(kind=SrtErrorKind.IO, message="x")
    assert isinstance(err, tstrans.exceptions.TstError)


def test_srt_error_kind_round_trip() -> None:
    """The `.kind` attribute survives the Rust→Python round trip with
    the right enum identity (not just int equality)."""
    with pytest.raises(SrtError) as ei:
        _raise_srt_error_for_test("TIMEOUT", "round-trip")
    assert ei.value.kind is SrtErrorKind.TIMEOUT
    assert isinstance(ei.value.kind, SrtErrorKind)
