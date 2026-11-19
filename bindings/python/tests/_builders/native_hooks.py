"""Lookup of the native `py-test-hooks` helpers in `tstrans._native.shells`
and `tstrans._native.rtp`.

A build without the `py-test-hooks` feature lacks them: the test skips,
unless `TSTRANS_EXPECT_TEST_HOOKS=1` says the hooks were built, in which
case a missing hook is a failure.
"""

from __future__ import annotations

import os

import pytest

from tstrans import _native


def _hook(module, name):
    f = getattr(module, name, None)
    if f is None:
        if os.environ.get("TSTRANS_EXPECT_TEST_HOOKS") == "1":
            pytest.fail("TSTRANS_EXPECT_TEST_HOOKS=1 but py-test-hooks is not built")
        pytest.skip("built without py-test-hooks")
    return f


def shells_hook(name):
    return _hook(_native.shells, name)


def rtp_hook(name):
    return _hook(_native.rtp, name)
