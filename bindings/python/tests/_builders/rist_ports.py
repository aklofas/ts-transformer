"""Bind a RIST endpoint on a free even loopback port, or skip.

librist's Simple profile needs even ports; when none of the probed ports
binds (librist unavailable), the calling test skips like the raw RIST tests.
"""

from __future__ import annotations

from typing import Callable

import pytest

from tstrans.exceptions import RistError


def bind_even_port_or_skip(open_fn: Callable[[int], object]) -> tuple[object, int]:
    """Return `(open_fn(port), port)` for the first even port that binds."""
    for port in range(34110, 34150, 2):
        try:
            return open_fn(port), port
        except RistError:
            continue
    pytest.skip("could not bind any even RIST port (librist unavailable)")
