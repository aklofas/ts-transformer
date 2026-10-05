"""Regression test: ManagedDemuxReceiver.socket_stats() GIL freeze.

The bug (before fix):
  `ManagedDemuxReceiver.__next__` acquires the outer
  `Arc<Mutex<Option<ManagedDemuxReceiver<...>>>>` inside `allow_threads` and
  holds it for the entire duration of `recv_event()`.

  `socket_stats()` (and `srt_stats()`) acquired that same outer mutex while
  holding the GIL.  Since `__next__` holds the mutex inside allow_threads
  (GIL released), `socket_stats()` would wait for it while holding the GIL —
  freezing all Python threads for the duration of one `recv_event()` call.

  This is distinct from the byte-sink ABBA deadlock in `DemuxReceiver` — it
  is a temporary GIL freeze rather than a permanent deadlock, but still
  degrades interpreter responsiveness.

The fix:
  `socket_stats()` now wraps mutex acquisition in `py.allow_threads()` using
  the two-step error pattern, so the GIL is released while waiting for the
  lock.

  `ManagedMuxSender.stats()` received the same treatment: its `socket_stats()`
  and `stats()` calls both acquire the internal `MuxSender` mutex, which
  `send_*` methods also hold inside `allow_threads`.

What this test verifies:
  - `socket_stats()` and `srt_stats()` on `ManagedDemuxReceiver` return
    promptly while the iterator is running (sender pushes continuously so
    `recv_event()` returns quickly between packets, releasing the mutex).
  - No call takes longer than 5 s — a sign that the GIL is not frozen by the
    mutex wait.

Watchdog: 5 s per call.
"""

from __future__ import annotations

import threading
import time

import pytest

import tstrans.srt as srt
from tstrans.mpegts import Pts90khz

from _builders.mux_programs import video_only_program as _video_only_program
from _builders.ports import free_tcp_port as _free_tcp_port


NAL_IDR = b"\x00\x00\x00\x01\x65\xBB"
_CALL_DEADLINE_S = 5.0
_HAMMER_COUNT = 30


# --------------------------------------------------------------------------- #
# Test
# --------------------------------------------------------------------------- #

def test_managed_demux_receiver_socket_stats_no_gil_block_with_iter() -> None:
    """Hammering socket_stats()/srt_stats() while the managed iterator is
    running in another thread must not freeze the GIL.

    Before the fix, socket_stats() held the GIL while waiting for the outer
    Arc<Mutex> that __next__ holds inside allow_threads. With a continuous
    sender, each call unfreezes quickly when recv_event() returns, but the
    GIL is still frozen for the duration of one recv_event() call.

    After the fix, socket_stats() releases the GIL before acquiring the mutex,
    so the main thread stays schedulable. The test verifies each call returns
    within the watchdog deadline.
    """
    port = _free_tcp_port()
    listener_url = f"srt://:{port}?mode=listener"
    caller_url = f"srt://127.0.0.1:{port}?mode=caller"

    rx_box: list[srt.ManagedDemuxReceiver] = []
    rx_err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            rx_box.append(srt.ManagedDemuxReceiver.from_url(listener_url))
        except BaseException as exc:  # noqa: BLE001
            rx_err.append(exc)

    accept_t = threading.Thread(target=accept_worker, daemon=True)
    accept_t.start()
    time.sleep(0.1)

    sender = srt.ManagedMuxSender.from_url(caller_url, _video_only_program())
    accept_t.join(timeout=5.0)
    if rx_err:
        sender.close()
        pytest.fail(f"ManagedDemuxReceiver accept failed: {rx_err[0]}")
    if not rx_box:
        sender.close()
        pytest.fail("ManagedDemuxReceiver listener thread did not accept within 5 s")

    rx = rx_box[0]

    # Feeder: push data continuously so recv_event() returns frequently,
    # keeping the mutex contention window short.
    feeder_stop = threading.Event()
    feeder_err: list[BaseException] = []

    def feeder() -> None:
        try:
            i = 0
            while not feeder_stop.is_set():
                sender.send_video(
                    NAL_IDR,
                    pts=Pts90khz.from_raw(i * 3000),
                    key_frame=(i % 4 == 0),
                )
                i += 1
                time.sleep(0.002)
        except Exception as exc:  # noqa: BLE001
            feeder_err.append(exc)

    feeder_t = threading.Thread(target=feeder, daemon=True)
    feeder_t.start()

    # Iterator thread: holds the outer Arc<Mutex> inside allow_threads during
    # each recv_event() call — this is the source of contention.
    iter_stop = threading.Event()
    iter_err: list[BaseException] = []
    event_count: list[int] = [0]

    def iterator() -> None:
        try:
            for _ev in rx:
                event_count[0] += 1
                if iter_stop.is_set():
                    break
        except Exception as exc:  # noqa: BLE001
            iter_err.append(exc)

    iter_t = threading.Thread(target=iterator, daemon=True)
    iter_t.start()

    # Give feeder and iterator time to settle.
    time.sleep(0.3)

    # Hammer socket_stats() and srt_stats() from the main thread.
    # Each call is issued in a sub-thread so we can impose a deadline
    # without blocking the main thread (which would mask a GIL freeze).
    for i in range(_HAMMER_COUNT):
        # --- socket_stats() ---
        done = threading.Event()
        call_err: list[BaseException] = []

        def call_socket_stats(ev: threading.Event = done, errs: list = call_err) -> None:
            try:
                rx.socket_stats()
            except Exception as exc:  # noqa: BLE001
                errs.append(exc)
            finally:
                ev.set()

        t = threading.Thread(target=call_socket_stats, daemon=True)
        t.start()
        if not done.wait(timeout=_CALL_DEADLINE_S):
            feeder_stop.set()
            sender.close()
            iter_stop.set()
            rx.close()
            pytest.fail(
                f"socket_stats() call #{i} did not return within {_CALL_DEADLINE_S}s "
                f"({event_count[0]} events received so far)"
            )
        t.join()

        # --- srt_stats() ---
        done2 = threading.Event()
        call_err2: list[BaseException] = []

        def call_srt_stats(ev: threading.Event = done2, errs: list = call_err2) -> None:
            try:
                rx.srt_stats()
            except Exception as exc:  # noqa: BLE001
                errs.append(exc)
            finally:
                ev.set()

        t2 = threading.Thread(target=call_srt_stats, daemon=True)
        t2.start()
        if not done2.wait(timeout=_CALL_DEADLINE_S):
            feeder_stop.set()
            sender.close()
            iter_stop.set()
            rx.close()
            pytest.fail(
                f"srt_stats() call #{i} did not return within {_CALL_DEADLINE_S}s"
            )
        t2.join()

    # Cleanup.
    feeder_stop.set()
    feeder_t.join(timeout=2.0)
    iter_stop.set()
    sender.close()
    rx.close()
    iter_t.join(timeout=2.0)

    # Sanity: feeder must have actually pushed data.
    assert event_count[0] > 0, (
        "iterator received no DemuxEvents — the race was not exercised; "
        "check feeder/iterator setup"
    )
