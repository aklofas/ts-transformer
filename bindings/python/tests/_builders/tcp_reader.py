"""A plain TCP peer that reads what a shell sends.

`TcpReader` listens on loopback, accepts one connection and reads it to its
end on a thread, so a test sees exactly the bytes that crossed the wire,
never what one of this library's receivers made of them.
"""

from __future__ import annotations

import socket
import threading
from typing import Optional


class TcpReader:
    """Accepts one connection on `url` and collects it in `data` until the
    sender closes it; `to_end()` waits (bounded) for that close."""

    def __init__(self, wait_s: float = 10.0) -> None:
        self.wait_s = wait_s
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(1)
        self.sock.settimeout(wait_s)
        self.url = f"tcp://127.0.0.1:{self.sock.getsockname()[1]}"
        self.data = bytearray()
        self.error: Optional[BaseException] = None
        self.ended = threading.Event()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self) -> None:
        try:
            conn, _ = self.sock.accept()
            with conn:
                conn.settimeout(self.wait_s)
                while chunk := conn.recv(65536):
                    self.data.extend(chunk)
        except BaseException as exc:  # noqa: BLE001
            self.error = exc
        finally:
            self.ended.set()

    def to_end(self) -> bytes:
        assert self.ended.wait(self.wait_s), "the peer did not read to the end"
        assert self.error is None, self.error
        return bytes(self.data)

    def close(self) -> None:
        self.sock.close()
