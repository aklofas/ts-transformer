"""The `x-silentpeertimeout` URL key through the Python binding.

The key arms silent-peer detection on a managed SRT sender opened from a
URL (`ManagedSender.from_url`, `ManagedMuxSender.from_url`). Every other
SRT open refuses it with `CONFIG_INVALID` and names the key: a value under
the 1 000 ms floor, a managed receiver, and the low-level `srt.Builder`'s
`connect()` and `listen()`. Nothing a Python caller can read tells an
armed sender from an unarmed one without a multi-second live trip, so
the open tests assert what a caller sees: the open succeeds and a send
arrives. That the key arms the sender and that the window trips is
pinned at the Rust level in `tst-srt`; these tests pin that each Python
door accepts or refuses the key and that each refusal keeps its kind.
"""

from __future__ import annotations

import threading
import time
from typing import Callable, TypeVar

import pytest

import tstrans.srt
from tstrans.exceptions import SrtError, SrtErrorKind
from tstrans.mpegts import Pts90khz
from tstrans.srt import ManagedMuxSender, ManagedReceiver, ManagedSender

from _builders.mux_programs import video_only_program as _video_only_program
from _builders.ports import free_udp_port as _free_udp_port

KEY = "x-silentpeertimeout"
NAL_IDR = b"\x00\x00\x00\x01\x65\xBB"
T = TypeVar("T")


def _open_listener(port: int) -> tuple[threading.Thread, list[ManagedReceiver], list[BaseException]]:
    """Open a plain listener-mode `ManagedReceiver` on a background thread."""
    box: list[ManagedReceiver] = []
    err: list[BaseException] = []

    def work() -> None:
        try:
            box.append(ManagedReceiver.from_url(f"srt://:{port}?mode=listener"))
        except BaseException as exc:  # noqa: BLE001
            err.append(exc)

    t = threading.Thread(target=work, daemon=True)
    t.start()
    return t, box, err


def _dial(open_sender: Callable[[], T]) -> T:
    """Call a caller-mode open until the listener is up, within 10 s."""
    deadline = time.monotonic() + 10.0
    while True:
        try:
            return open_sender()
        except SrtError as exc:
            # Only the connect race is retried; a refused configuration is final.
            if exc.kind == SrtErrorKind.CONFIG_INVALID or time.monotonic() >= deadline:
                raise
            time.sleep(0.05)


def _assert_refused_naming_the_key(excinfo: pytest.ExceptionInfo[SrtError]) -> None:
    assert excinfo.value.kind == SrtErrorKind.CONFIG_INVALID
    assert KEY in str(excinfo.value)


def _wait_box(t: threading.Thread, box: list[ManagedReceiver], err: list[BaseException]) -> ManagedReceiver:
    t.join(timeout=10.0)
    assert not err, err
    assert box, "the listener did not accept within 10 s"
    return box[0]


def _receive_one(receiver: ManagedReceiver, t: threading.Thread, err: list[BaseException]) -> bytes:
    t.join(timeout=10.0)
    assert not err, err
    data = receiver.recv_bytes()
    assert len(data) >= 188
    assert data[0] == 0x47
    return data


def test_managed_sender_with_the_key_opens_and_sends() -> None:
    port = _free_udp_port()
    t, box, err = _open_listener(port)
    url = f"srt://127.0.0.1:{port}?mode=caller&{KEY}=1500"
    sender = _dial(lambda: ManagedSender.from_url(url))
    try:
        for _ in range(7):
            sender.send_bytes(b"\x47" + b"\x00" * 187)
        _receive_one(_wait_box(t, box, err), t, err)
        assert sender.reconnect_stats().reconnect_attempts == 0
    finally:
        sender.close()
        for r in box:
            r.close()


def test_managed_mux_sender_with_the_key_opens_and_sends() -> None:
    port = _free_udp_port()
    t, box, err = _open_listener(port)
    url = f"srt://127.0.0.1:{port}?mode=caller&{KEY}=1500"
    sender = _dial(lambda: ManagedMuxSender.from_url(url, _video_only_program()))
    try:
        receiver = _wait_box(t, box, err)
        for i in range(32):
            sender.send_video(NAL_IDR, pts=Pts90khz.from_raw(i * 3000), key_frame=(i % 4 == 0))
        _receive_one(receiver, t, err)
    finally:
        sender.close()
        for r in box:
            r.close()


def test_managed_sender_refuses_a_window_under_the_floor() -> None:
    url = f"srt://127.0.0.1:{_free_udp_port()}?mode=caller&{KEY}=999"
    with pytest.raises(SrtError) as ei:
        ManagedSender.from_url(url)
    _assert_refused_naming_the_key(ei)


def test_managed_mux_sender_refuses_a_window_under_the_floor() -> None:
    url = f"srt://127.0.0.1:{_free_udp_port()}?mode=caller&{KEY}=999"
    with pytest.raises(SrtError) as ei:
        ManagedMuxSender.from_url(url, _video_only_program())
    _assert_refused_naming_the_key(ei)


def test_managed_receiver_refuses_the_key() -> None:
    url = f"srt://:{_free_udp_port()}?mode=listener&{KEY}=1500"
    with pytest.raises(SrtError) as ei:
        ManagedReceiver.from_url(url)
    _assert_refused_naming_the_key(ei)


def test_builder_connect_refuses_the_key() -> None:
    url = f"srt://127.0.0.1:{_free_udp_port()}?mode=caller&{KEY}=1500"
    with pytest.raises(SrtError) as ei:
        tstrans.srt.Builder(url).connect()
    _assert_refused_naming_the_key(ei)


def test_builder_listen_refuses_the_key() -> None:
    url = f"srt://:{_free_udp_port()}?mode=listener&{KEY}=1500"
    with pytest.raises(SrtError) as ei:
        tstrans.srt.Builder(url).listener().listen()
    _assert_refused_naming_the_key(ei)
