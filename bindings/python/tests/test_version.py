"""Smoke test: tstrans imports cleanly and exposes __version__."""

import tstrans


def test_version_is_str():
    assert isinstance(tstrans.__version__, str)


def test_version_matches_packaged_release():
    # Bump this assertion when tst-py's version in Cargo.toml changes.
    assert tstrans.__version__ == "0.7.0"


def test_native_submodule_loads():
    from tstrans import _native
    assert _native.__version__ == tstrans.__version__
