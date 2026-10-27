"""Sender splits a bundle at the transport's payload ceiling.

Seven TS packets through `udp://...?pkt_size=1128` (six packets) must
arrive as a 1128-byte datagram then a 188-byte one.
"""

import socket

from tstrans.shells import Sender


def _ts_packets(n: int) -> bytes:
    out = bytearray()
    for i in range(n):
        out.append(0x47)
        out.extend(((i * 188 + j) % 251) | 1 for j in range(1, 188))
    return bytes(out)


def test_sender_splits_bundle_at_pkt_size_ceiling() -> None:
    rx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        rx.bind(("127.0.0.1", 0))
        rx.settimeout(5.0)
        port = rx.getsockname()[1]

        data = _ts_packets(7)
        tx = Sender.open(f"udp://127.0.0.1:{port}?pkt_size=1128")
        try:
            tx.send_bytes(data)
            tx.flush()
        finally:
            tx.close()

        first = rx.recv(2048)
        second = rx.recv(2048)
        assert [len(first), len(second)] == [1128, 188]
        assert first + second == data
    finally:
        rx.close()
