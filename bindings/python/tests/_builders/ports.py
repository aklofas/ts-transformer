"""Ephemeral-port allocators shared by SRT/RTP/TCP loopback tests.

Ask the OS for a free port, then release it immediately. Use the
allocator matching the protocol under test — `free_tcp_port()` for a
TCP listener/connect, `free_udp_port()` for a UDP bind (SRT, RTP and
RIST all bind UDP). A port free
for one protocol is not guaranteed free for the other (the kernel
tracks UDP/TCP ports in separate namespaces, so cross-protocol reuse
is best-effort, not guaranteed). The tiny TOCTOU window between
release and rebind is fine since pytest runs these tests sequentially
by default.
"""

from __future__ import annotations

import socket


def free_tcp_port() -> int:
    """Ask the OS for an ephemeral TCP port, then release it."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def free_udp_port() -> int:
    """Ask the OS for an ephemeral UDP port, then release it."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]
