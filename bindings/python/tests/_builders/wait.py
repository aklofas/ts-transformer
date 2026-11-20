"""Waiting on a condition with a deadline: the suite's one helper for it.

A test never sleeps to synchronise with another thread; it polls the
condition it needs until a deadline, then fails naming what it waited for.
"""

from __future__ import annotations

import time
from typing import Callable

import pytest


def wait_until(cond: Callable[[], bool], limit: float, poll: float = 0.01) -> bool:
    """Poll `cond` until it holds or `limit` seconds pass; whether it held
    (it is asked once more at the deadline)."""
    deadline = time.monotonic() + limit
    while time.monotonic() < deadline:
        if cond():
            return True
        time.sleep(poll)
    return bool(cond())


def wait_for(what: str, cond: Callable[[], bool], limit: float, poll: float = 0.01) -> None:
    """Poll `cond` until it holds; fail naming `what` if `limit` seconds
    pass first."""
    if not wait_until(cond, limit, poll):
        pytest.fail(f"{what}: not seen within {limit} s")
