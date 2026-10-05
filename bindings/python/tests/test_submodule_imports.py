"""Submodule shells exist and import cleanly; this guards their
existence."""

import importlib

import pytest


@pytest.mark.parametrize("name", ["mpegts", "klv", "codec", "io", "exceptions"])
def test_submodule_imports(name):
    mod = importlib.import_module(f"tstrans.{name}")
    assert hasattr(mod, "__all__")
    assert isinstance(mod.__all__, list)


def test_submodules_attached_to_package():
    import tstrans
    for name in ("mpegts", "klv", "codec", "io", "exceptions"):
        assert hasattr(tstrans, name), f"tstrans.{name} not re-exported"
