"""Arc 2 rider R-EXIT: leaving a shell parked in a native call must not
wedge (or abort) interpreter exit.

`tst_srt` registers `srt_cleanup` with C `atexit`, and `srt_cleanup` joins
libsrt's `SRT:GC` thread, which cannot finish while a socket is still
parked in `accept()` / `recv()`. Before the guard, a script that simply
forgot to `close()` a listener hung forever at exit (observed twice as a
pytest that printed its summary and never returned).

`tstrans._native` registers `_fire_cancel_sources_at_exit` with Python's
`atexit` at import; Python's callbacks run before the C-level ones, so
every live shell is cancelled in time for teardown to complete. The hook
SKIPS sources that already latched (every `close()` goes cancel-first), so
a program that closed everything pays neither the cancel walk nor the
settle window — `test_the_exit_hook_reports_nothing_open_after_a_clean_close`
is the observable for that, via the count the hook returns.

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


def _run(script: str, timeout: float = 30.0, what: str = "") -> subprocess.CompletedProcess[str]:
    """Run `script` in a child interpreter. A `TimeoutExpired` IS the
    regression this module exists to catch, so it becomes a named failure
    rather than an error."""
    import os

    try:
        return subprocess.run(
            [sys.executable, "-c", textwrap.dedent(script)],
            capture_output=True,
            text=True,
            timeout=timeout,
            env={**os.environ, "PYTHONPATH": PKG_PARENT},
        )
    except subprocess.TimeoutExpired:
        pytest.fail(
            f"interpreter did not exit within {timeout}s{what} — "
            "the R-EXIT atexit guard is not firing"
        )


# The latch shape every parked-shell script below uses: set an Event
# immediately before the native call and a second one in a `finally`, then
# assert (bounded) that the call has NOT returned. The assertion — not a
# clock reading — is what proves the worker is inside the native call with
# the GIL released; without it a `sleep()` that lost its race would make
# the whole test vacuous (it would pass with the guard removed).
_PARK_LATCH = """
    import threading, time

    entered = threading.Event()
    returned = threading.Event()

    def _park(call):
        def run():
            entered.set()
            try:
                call()
            finally:
                returned.set()
        return run

    def assert_parked():
        assert entered.wait(10.0), "worker never reached the native call"
        # Bounded: hand the GIL over a few times and require the call to
        # still be outstanding. 200 ms ceiling, no wall-clock assertion.
        for _ in range(20):
            if returned.is_set():
                break
            time.sleep(0.01)
        assert not returned.is_set(), "the native call returned; nothing is parked"
        print("PARKED", flush=True)
"""

PARKED_LISTENER = (
    _PARK_LATCH
    + """
    import tstrans.srt as srt

    lst = srt.Builder("srt://127.0.0.1:0?mode=listener").listen()
    threading.Thread(target=_park(lambda: lst.accept()), daemon=True).start()
    assert_parked()
    # Deliberately no close(): the leaked-parked-thread shape.
"""
)

PARKED_UDP_RECEIVER = (
    _PARK_LATCH
    + """
    import tstrans.udp as udp

    rx = udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0").build()
    threading.Thread(target=_park(lambda: rx.recv()), daemon=True).start()
    assert_parked()
"""
)


def test_exit_is_clean_with_a_listener_parked_in_accept() -> None:
    """The guard's reason for existing. Without it this process never
    exits; the subprocess deadline is the failure mode."""
    r = _run(PARKED_LISTENER, what=" with a listener parked in accept()")
    assert "PARKED" in r.stdout, r.stderr
    # 0 = clean. 124 would be the old hang; 134 (SIGABRT) is the woken
    # thread losing its race with finalisation, which the settle window in
    # `fire_cancel_sources_at_exit` exists to prevent.
    assert r.returncode == 0, f"exit={r.returncode}\n{r.stdout}\n{r.stderr}"


def test_exit_is_clean_with_a_udp_receiver_parked_in_recv() -> None:
    """udp parks in a pure-kernel poll loop with no C `atexit` partner, so
    it exited cleanly even before the guard — pinned so a future WP-D
    change to the udp recv loop cannot regress it."""
    r = _run(PARKED_UDP_RECEIVER, what=" with a udp receiver parked in recv()")
    assert "PARKED" in r.stdout, r.stderr
    assert r.returncode == 0, f"exit={r.returncode}\n{r.stdout}\n{r.stderr}"


def test_the_exit_hook_reports_nothing_open_after_a_clean_close() -> None:
    """A clean exit must be FREE: `close()` latches the shell's
    `CancelSource`, so the hook skips it and never reaches the settle
    window. The hook's return value (how many sources it fired) is the
    observable — 0 here, non-zero in the companion test below."""
    r = _run(
        """
        import tstrans.srt as srt
        from tstrans._native import _fire_cancel_sources_at_exit

        lst = srt.Builder("srt://127.0.0.1:0?mode=listener").listen()
        lst.close()
        print("HOOK=%d" % _fire_cancel_sources_at_exit(), flush=True)
        """,
        timeout=20.0,
        what=" after a clean close",
    )
    assert "HOOK=0" in r.stdout, f"{r.stdout}\n{r.stderr}"
    assert r.returncode == 0, f"exit={r.returncode}\n{r.stdout}\n{r.stderr}"


def test_the_exit_hook_reports_an_unclosed_shell() -> None:
    """The counterpart: an open shell IS fired, so `HOOK=0` above is the
    filter working, not the hook being blind."""
    r = _run(
        """
        import tstrans.srt as srt
        from tstrans._native import _fire_cancel_sources_at_exit

        lst = srt.Builder("srt://127.0.0.1:0?mode=listener").listen()
        print("HOOK=%d" % _fire_cancel_sources_at_exit(), flush=True)
        """,
        timeout=20.0,
        what=" with an unclosed listener",
    )
    fired = [line for line in r.stdout.splitlines() if line.startswith("HOOK=")]
    assert fired, f"{r.stdout}\n{r.stderr}"
    assert int(fired[0].split("=")[1]) >= 1, f"{r.stdout}\n{r.stderr}"
    assert r.returncode == 0, f"exit={r.returncode}\n{r.stdout}\n{r.stderr}"
