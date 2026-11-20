"""A call parked on another thread, proved by the receiver's slot.

`parked` returns only once the call holds the object's slot (the
`_slot_held_for_test` hook, `py-test-hooks` builds only), so a getter read
next is read while the call is parked, not before it has started.
"""

from __future__ import annotations

import threading
from typing import Callable

from _builders.native_hooks import shells_hook
from _builders.wait import wait_for


def slot_held(obj: object) -> bool:
    """Whether a call on another thread holds `obj`'s slot (never waits)."""
    return shells_hook("_slot_held_for_test")(obj)


def parked(obj: object, call: Callable[[], object], limit: float = 15.0) -> threading.Thread:
    """Run `call` on its own thread and return once it holds `obj`'s slot.

    The hook is looked up before the thread starts, so a build without it
    skips with no thread left behind."""
    held = shells_hook("_slot_held_for_test")
    t = threading.Thread(target=call, daemon=True)
    t.start()
    wait_for("the call holding the slot", lambda: held(obj), limit)
    return t
