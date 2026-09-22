"""Arc 2 rider R-EXIT: leaving a shell parked in a native call must not
wedge (or abort) interpreter exit.

`tst_srt` registers `srt_cleanup` with C `atexit`, and `srt_cleanup` joins
libsrt's `SRT:GC` thread, which cannot finish while a socket is still
parked in `accept()` / `recv()`. Before the guard, a script that simply
forgot to `close()` a listener hung forever at exit (observed twice as a
pytest that printed its summary and never returned).

`tstrans._native` registers `_fire_cancel_sources_at_exit` with Python's
`atexit` at import; Python's callbacks run before the C-level ones, so
every live shell is cancelled in time for teardown to complete.

These run in a subprocess with a hard deadline — a regression here is a
HANG, which no in-process assertion could catch.
"""

from __future__ import annotations

import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

import tstrans

PKG_PARENT = str(Path(tstrans.__file__).resolve().parent.parent)


def _run(script: str, timeout: float = 30.0) -> subprocess.CompletedProcess[str]:
    import os

    return subprocess.run(
        [sys.executable, "-c", textwrap.dedent(script)],
        capture_output=True,
        text=True,
        timeout=timeout,
        env={**os.environ, "PYTHONPATH": PKG_PARENT},
    )


PARKED_LISTENER = """
    import threading, time
    import tstrans.srt as srt

    lst = srt.Builder("srt://127.0.0.1:0?mode=listener").listen()
    t = threading.Thread(target=lambda: lst.accept(), daemon=True)
    t.start()
    time.sleep(0.5)          # parked inside libsrt, GIL released
    print("PARKED", flush=True)
    # Deliberately no close(): the leaked-parked-thread shape.
"""


def test_exit_is_clean_with_a_listener_parked_in_accept() -> None:
    """The guard's reason for existing. Without it this process never
    exits; the 30 s subprocess deadline is the failure mode."""
    try:
        r = _run(PARKED_LISTENER)
    except subprocess.TimeoutExpired:
        pytest.fail(
            "interpreter did not exit with a listener parked in accept() — "
            "the R-EXIT atexit guard is not firing"
        )
    assert "PARKED" in r.stdout, r.stderr
    # 0 = clean. 124 would be the old hang; 134 (SIGABRT) is the woken
    # thread losing its race with finalisation, which the settle window in
    # `fire_cancel_sources_at_exit` exists to prevent.
    assert r.returncode == 0, f"exit={r.returncode}\n{r.stdout}\n{r.stderr}"


def test_exit_is_clean_with_a_udp_receiver_parked_in_recv() -> None:
    """udp parks in a pure-kernel poll loop with no C `atexit` partner, so
    it exited cleanly even before the guard — pinned so a future WP-D
    change to the udp recv loop cannot regress it."""
    r = _run(
        """
        import threading, time
        import tstrans.udp as udp

        rx = udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0").build()
        t = threading.Thread(target=lambda: rx.recv(), daemon=True)
        t.start()
        time.sleep(0.4)
        print("PARKED", flush=True)
        """
    )
    assert "PARKED" in r.stdout, r.stderr
    assert r.returncode == 0, f"exit={r.returncode}\n{r.stdout}\n{r.stderr}"


def test_exit_is_clean_when_nothing_was_left_open() -> None:
    """The guard must be free when every shell was closed properly — it
    returns before touching the GIL-released settle window."""
    r = _run(
        """
        import tstrans.srt as srt

        lst = srt.Builder("srt://127.0.0.1:0?mode=listener").listen()
        lst.close()
        print("CLOSED", flush=True)
        """,
        timeout=20.0,
    )
    assert "CLOSED" in r.stdout, r.stderr
    assert r.returncode == 0, f"exit={r.returncode}\n{r.stdout}\n{r.stderr}"
