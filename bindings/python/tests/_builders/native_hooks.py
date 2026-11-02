"""Lookup of the native `py-test-hooks` helpers in `tstrans.shells`.

A build without the `py-test-hooks` feature lacks them: the test skips,
unless `TSTRANS_EXPECT_TEST_HOOKS=1` says the hooks were built, in which
case a missing hook is a failure.
"""

from __future__ import annotations

import os

import pytest

from tstrans import _native


def shells_hook(name):
    f = getattr(_native.shells, name, None)
    if f is None:
        if os.environ.get("TSTRANS_EXPECT_TEST_HOOKS") == "1":
            pytest.fail("TSTRANS_EXPECT_TEST_HOOKS=1 but py-test-hooks is not built")
        pytest.skip("built without py-test-hooks")
    return f
