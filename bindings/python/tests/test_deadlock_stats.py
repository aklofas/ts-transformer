"""Regression test for the GIL-mutex ABBA deadlock in DemuxReceiver.

The deadlock path:
  - Thread A (iterator): holds inner Mutex inside recv_event (under allow_threads),
    then the registered byte sink fires Python::with_gil -> blocks re-taking GIL.
  - Thread B (stats caller): holds GIL (normal Python thread), then calls
    stats() / socket_stats() which lock inner Mutex -> blocks on the lock.
  => permanent deadlock.

Fix: release the GIL before acquiring the inner Mutex in stats() / socket_stats()
(matching the existing pattern in close() / add_byte_sink()).

This test verifies the fix by hammering stats/socket_stats from the main thread
while an iterator thread runs with a registered byte sink. A wall-clock watchdog
asserts no single stats call takes longer than 5 seconds.
"""

from __future__ import annotations

from tstrans import shells
import threading
import time

import pytest

import tstrans.srt
from tstrans.mpegts import DemuxEvent, Pts90khz

from _builders.mux_programs import video_only_program as _video_only_program
from _builders.ports import free_tcp_port as _free_tcp_port


NAL_IDR = b"\x00\x00\x00\x01\x65\xBB"

# Watchdog: any single stats() / socket_stats() call must complete within
# this many seconds. Under the deadlock, neither call ever returns.
_STATS_CALL_DEADLINE_S = 5.0

# How many times to call stats() + socket_stats() from the main thread.
_HAMMER_COUNT = 200


# --------------------------------------------------------------------------- #
# SRT deadlock regression                                                     #
# --------------------------------------------------------------------------- #


def test_srt_demux_receiver_stats_no_deadlock_with_byte_sink() -> None:
    """Hammering stats()/socket_stats() while a byte sink fires in the
    iterator thread must not deadlock. Before the fix, the first stats()
    call would block permanently.

    Construction: MuxSender (caller) → DemuxReceiver (listener), one
    registered byte sink, iterator on a daemon thread, main thread hammers
    stats/socket_stats with a wall-clock deadline per call.
    """
    port = _free_tcp_port()
    listener_url = f"srt://:{port}?mode=listener"
    caller_url = f"srt://127.0.0.1:{port}?mode=caller"

    rx_box: list[tstrans.shells.DemuxReceiver] = []
    rx_err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            rx_box.append(shells.DemuxReceiver.open(listener_url))
        except BaseException as exc:  # noqa: BLE001
            rx_err.append(exc)

    accept_t = threading.Thread(target=accept_worker, daemon=True)
    accept_t.start()
    time.sleep(0.1)
    sender = shells.MuxSender.open(caller_url, _video_only_program())
    accept_t.join(timeout=5.0)
    if rx_err:
        sender.close()
        pytest.fail(f"DemuxReceiver accept failed: {rx_err[0]}")
    if not rx_box:
        sender.close()
        pytest.fail("DemuxReceiver listener thread did not accept within 5 s")

    rx = rx_box[0]

    # Register a byte sink that does real Python work (ensures the
    # GIL-reacquire path fires on every TS packet during recv_event).
    sink_counts: list[int] = [0]

    def counting_sink(_pkt: bytes) -> None:
        sink_counts[0] += 1

    rx.add_byte_sink(counting_sink)

    # Start the feeder: push a continuous stream of NALs so the receiver
    # is always busy and the byte sink fires regularly.
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
                time.sleep(0.001)  # ~1 kpps — fast enough to keep sink firing
        except Exception as exc:  # noqa: BLE001
            feeder_err.append(exc)

    feeder_t = threading.Thread(target=feeder, daemon=True)
    feeder_t.start()

    # Start the iterator thread (holds inner Mutex inside recv_event).
    iter_stop = threading.Event()
    iter_err: list[BaseException] = []

    def iterator() -> None:
        try:
            for ev in rx:
                if iter_stop.is_set():
                    break
                # Break early after the first video event — but keep the
                # thread alive (do NOT close rx) so the mutex stays in play.
                if isinstance(ev, DemuxEvent.Video):
                    pass  # keep iterating; we want the lock contention
        except Exception as exc:  # noqa: BLE001
            iter_err.append(exc)

    iter_t = threading.Thread(target=iterator, daemon=True)
    iter_t.start()

    # Give the feeder and iterator time to settle — we need the sink to be
    # actively firing when we start hammering stats.
    time.sleep(0.3)

    # Main-thread hammer: call stats() + socket_stats() repeatedly with a
    # per-call deadline. Under the deadlock, the first call hangs forever.
    stats_errs: list[str] = []
    for i in range(_HAMMER_COUNT):
        # stats()
        deadline = time.monotonic() + _STATS_CALL_DEADLINE_S
        done_event = threading.Event()
        call_err: list[BaseException] = []

        def call_stats(ev: threading.Event = done_event, errs: list = call_err) -> None:
            try:
                rx.stats()
            except Exception as exc:  # noqa: BLE001
                errs.append(exc)
            finally:
                ev.set()

        stats_thread = threading.Thread(target=call_stats, daemon=True)
        stats_thread.start()
        if not done_event.wait(timeout=_STATS_CALL_DEADLINE_S):
            # The call hung — this is the deadlock. Teardown and fail.
            feeder_stop.set()
            sender.close()
            iter_stop.set()
            rx.close()
            pytest.fail(
                f"stats() call #{i} deadlocked (did not return within "
                f"{_STATS_CALL_DEADLINE_S}s). Byte sink fired {sink_counts[0]} "
                f"times before the hang."
            )
        stats_thread.join()

        # socket_stats()
        done_event2 = threading.Event()
        call_err2: list[BaseException] = []

        def call_sock_stats(
            ev: threading.Event = done_event2, errs: list = call_err2
        ) -> None:
            try:
                rx.socket_stats()
            except Exception as exc:  # noqa: BLE001
                errs.append(exc)
            finally:
                ev.set()

        ss_thread = threading.Thread(target=call_sock_stats, daemon=True)
        ss_thread.start()
        if not done_event2.wait(timeout=_STATS_CALL_DEADLINE_S):
            feeder_stop.set()
            sender.close()
            iter_stop.set()
            rx.close()
            pytest.fail(
                f"socket_stats() call #{i} deadlocked (did not return within "
                f"{_STATS_CALL_DEADLINE_S}s). Byte sink fired {sink_counts[0]} "
                f"times before the hang."
            )
        ss_thread.join()

    # Cleanup.
    feeder_stop.set()
    feeder_t.join(timeout=2.0)
    iter_stop.set()
    sender.close()
    rx.close()
    iter_t.join(timeout=2.0)

    # The byte sink must have actually fired during the hammering — if it
    # didn't, the race wasn't exercised and the test is vacuous.
    assert sink_counts[0] > 0, (
        "byte sink never fired during the test — the race was not exercised; "
        "check the feeder / iterator setup"
    )
