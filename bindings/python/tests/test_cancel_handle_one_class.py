"""`udp`, `tcp` and `rist` hand out the one `shells.CancelHandle` class."""

from __future__ import annotations

import pytest

from tstrans import rist, shells, tcp, udp
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
