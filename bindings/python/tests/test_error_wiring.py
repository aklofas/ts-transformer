"""End-to-end test that PyO3 can raise the Python exception classes
defined in tstrans.exceptions. Uses a Rust-side test helper exposed
only for this purpose."""

import os

import pytest

from tstrans import _native
from tstrans.exceptions import MuxError, TstError, MuxErrorKind

from _builders.hook_names import hook_names
from _builders.native_hooks import native_hook


def test_a_default_build_has_no_test_raisers():
    # `_slow_drop_for_test` is built only with the hooks, so it marks such a build.
    if os.environ.get("TSTRANS_EXPECT_TEST_HOOKS") == "1" or hasattr(
        _native.shells, "_slow_drop_for_test"
    ):
        pytest.skip("built with py-test-hooks")
    assert hook_names() == []


def test_native_can_raise_mux_error():
    with pytest.raises(MuxError) as exc_info:
        native_hook("_raise_mux_error_for_test")("synthetic failure")
    assert "synthetic failure" in str(exc_info.value)


def test_native_mux_error_is_catchable_as_tst_error():
    with pytest.raises(TstError):
        native_hook("_raise_mux_error_for_test")("x")


def test_native_mux_error_kind_attribute_is_set():
    with pytest.raises(MuxError) as exc_info:
        native_hook("_raise_mux_error_for_test")("x")
    assert exc_info.value.kind is MuxErrorKind.INTERNAL
