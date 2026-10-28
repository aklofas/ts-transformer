"""`udp`, `tcp` and `rist` hand out the one `shells.CancelHandle` class."""

from __future__ import annotations

import os
import threading
import time

import pytest

from tstrans import _native, rist, shells, tcp, udp
from _builders.ports import free_tcp_port, free_udp_port


@pytest.mark.parametrize("module", [udp, tcp, rist])
def test_transport_module_cancel_handle_is_the_shells_class(module):
    assert module.CancelHandle is shells.CancelHandle


def test_udp_handles_are_shells_handles_over_shared_state():
    port = free_udp_port()
    rx = udp.RecvTransport.builder().bind_url(f"udp://127.0.0.1:{port}").build()
    try:
        a = rx.cancel_handle()
        b = rx.cancel_handle()
        assert type(a) is shells.CancelHandle
        assert isinstance(b, udp.CancelHandle)
        assert not a.is_cancelled()
        b.cancel()
        assert a.is_cancelled()
    finally:
        rx.close()


def test_tcp_listener_handle_is_the_shells_class():
    port = free_tcp_port()
    listener = tcp.Listener.builder().bind(f"127.0.0.1:{port}").build()
    try:
        handle = listener.cancel_handle()
        assert type(handle) is shells.CancelHandle
        handle.cancel()
        assert handle.is_cancelled()
    finally:
        listener.close()


def _hook(name):
    f = getattr(_native.shells, name, None)
    if f is None:
        if os.environ.get("TSTRANS_EXPECT_TEST_HOOKS") == "1":
            pytest.fail("TSTRANS_EXPECT_TEST_HOOKS=1 but py-test-hooks is not built")
        pytest.skip("built without py-test-hooks")
    return f


def test_cancel_releases_the_gil_while_the_transport_cancel_runs():
    h = _hook("_slow_cancel_handle_for_test")(300)
    started, stop = threading.Event(), threading.Event()
    ticks = []

    def heartbeat():
        started.set()
        while not stop.is_set():
            ticks.append(time.perf_counter())
            time.sleep(0.005)

    t = threading.Thread(target=heartbeat, daemon=True)
    t.start()
    started.wait()
    begin = time.perf_counter()
    h.cancel()  # the native cancel sleeps 300 ms
    end = time.perf_counter()
    stop.set()
    t.join(5.0)
    assert h.is_cancelled()
    # With the GIL held the heartbeat cannot tick inside the native call; the
    # margins keep a tick racing the call's entry or exit from counting.
    inside = [x for x in ticks if begin + 0.05 < x < end - 0.05]
    assert inside, "the heartbeat never ran: cancel() held the GIL"
