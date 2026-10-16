"""Run a call on a daemon thread under a hang deadline.

A test that must not wedge the suite when the call under test blocks
behind a parked operation runs the call through this helper, performs its
rescue, and only then asserts on the returned `blocked` flag.
"""

from __future__ import annotations

import threading
from typing import Callable


def call_with_hang_deadline(
    getter: Callable[[], object], deadline_s: float = 30.0
) -> tuple[bool, dict[str, object]]:
    """Run `getter()` on its own daemon thread and join with a generous
    hang deadline (`deadline_s`, 30 s unless the caller
    tightens it). Returns `(blocked, outcome)`; the caller runs its rescue
    BEFORE asserting on `blocked`, so a getter stuck behind a parked call
    fails the test instead of wedging the process."""
    outcome: dict[str, object] = {}

    def getter_worker() -> None:
        try:
            outcome["value"] = getter()
        except BaseException as exc:  # noqa: BLE001
            outcome["exc"] = exc

    g = threading.Thread(target=getter_worker, daemon=True)
    g.start()
    g.join(deadline_s)
    return g.is_alive(), outcome
