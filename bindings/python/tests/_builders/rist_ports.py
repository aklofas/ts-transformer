"""Bind a RIST endpoint on a free even port.

librist's Simple profile needs even ports. The port comes from the kernel (a
UDP bind of port 0 on every interface), so two suites on one box do not meet
on it; another process can still take it between the probe and librist's
bind, so a bind refusal is retried on a fresh port, and the test fails, with
the refusals seen, once the attempts run out.
"""

from __future__ import annotations

import socket
from typing import Callable, TypeVar

import pytest

from tstrans.exceptions import RistError, RistErrorKind

T = TypeVar("T")

# What librist answers for a port it cannot bind.
_BIND_REFUSED = (RistErrorKind.CONTEXT_CREATE_FAILED, RistErrorKind.PEER_CREATE_FAILED)


def free_even_udp_port() -> int:
    """An even UDP port free on every interface when this returns."""
    for _ in range(64):
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            s.bind(("0.0.0.0", 0))
            port = s.getsockname()[1]
        if port % 2 == 0:
            return port
    pytest.fail("the kernel offered no even UDP port in 64 binds")


def bind_even_port(open_fn: Callable[[int], T], attempts: int = 8) -> tuple[T, int]:
    """Return `(open_fn(port), port)` for a free even port. A bind refusal
    is retried on another port; any other error is raised."""
    refused: list[str] = []
    for _ in range(attempts):
        port = free_even_udp_port()
        try:
            return open_fn(port), port
        except RistError as e:
            if e.kind not in _BIND_REFUSED:
                raise
            refused.append(f"{port}: {e.kind.name}: {e}")
    pytest.fail(f"no even port bound in {attempts} attempts: " + "; ".join(refused))
