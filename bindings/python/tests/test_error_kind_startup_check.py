"""Spec Arc 2 §3.3: a `BindingErrorKind::name()` the Python enum cannot
resolve is a STARTUP failure, not a runtime one. `tstrans._native`
resolves every member of every domain's `KINDS` at module init.

Mutation test: load `tstrans.exceptions` WITHOUT running the package
`__init__` (so `_native` has not been initialised yet), remove one member
from one domain enum, then import `_native` — it must refuse."""

from __future__ import annotations

import enum
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

import tstrans

PKG_DIR = Path(tstrans.__file__).resolve().parent


def _run(script: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-c", textwrap.dedent(script)],
        capture_output=True,
        text=True,
        timeout=60,
        env={"PYTHONPATH": str(PKG_DIR.parent), "PATH": "/usr/bin:/bin"},
    )


def test_missing_kind_member_fails_at_import() -> None:
    script = f"""
        import enum, sys, types
        pkg = types.ModuleType("tstrans"); pkg.__path__ = [{str(PKG_DIR)!r}]; sys.modules["tstrans"] = pkg
        import tstrans.exceptions as exc
        members = {{m.name: m.value for m in exc.SrtErrorKind if m.name != "BACKPRESSURE"}}
        exc.SrtErrorKind = enum.IntEnum("SrtErrorKind", members)   # BACKPRESSURE removed
        import tstrans._native
        print("IMPORTED")
    """
    r = _run(script)
    assert r.returncode != 0, f"import succeeded with a missing kind member:\n{r.stdout}\n{r.stderr}"
    assert "ImportError" in r.stderr and "SrtErrorKind.BACKPRESSURE" in r.stderr, r.stderr


def test_intact_enums_import_cleanly() -> None:
    r = _run("import tstrans; tstrans._native._check_error_kinds(); print('OK')")
    assert r.returncode == 0, r.stderr
    assert r.stdout.strip() == "OK"


def test_check_error_kinds_names_the_missing_enum_entry(monkeypatch: pytest.MonkeyPatch) -> None:
    import tstrans.exceptions as exc

    members = {m.name: m.value for m in exc.MuxErrorKind if m.name != "INVALID_NAL"}
    monkeypatch.setattr(exc, "MuxErrorKind", enum.IntEnum("MuxErrorKind", members))
    with pytest.raises(ImportError, match=r"tstrans\.exceptions\.MuxErrorKind\.INVALID_NAL is missing"):
        tstrans._native._check_error_kinds()
