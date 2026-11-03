"""SRT caller/listener loopback pair: a `MuxSender` and the `DemuxReceiver`
its listener accepts."""

from __future__ import annotations

import threading
import time
from typing import Tuple

import tstrans
from tstrans import shells

from _builders.mux_programs import video_only_program as _video_only_program


def make_srt_pair(
    port: int,
) -> Tuple[tstrans.shells.MuxSender, tstrans.shells.DemuxReceiver]:
    listener_url = f"srt://:{port}?mode=listener"
    caller_url = f"srt://127.0.0.1:{port}?mode=caller"

    rx_box: list[tstrans.shells.DemuxReceiver] = []
    rx_err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            rx_box.append(shells.DemuxReceiver.open(listener_url))
        except BaseException as exc:  # noqa: BLE001
            rx_err.append(exc)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)
    sender = shells.MuxSender.open(caller_url, _video_only_program())
    t.join(timeout=5.0)
    if rx_err:
        sender.close()
        raise rx_err[0]
    if not rx_box:
        sender.close()
        raise RuntimeError("SRT DemuxReceiver did not accept within 5 s")
    return sender, rx_box[0]
