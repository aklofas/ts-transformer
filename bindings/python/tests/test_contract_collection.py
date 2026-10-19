"""Hard-fail guard: the cross-binding scenario contract suite must collect the
pilot scenarios. A regression that makes parametrization empty (broken manifest
path, parse error) must fail here, not silently vanish."""
from __future__ import annotations

import tomllib

from conftest import scenarios_dir

PILOT_IDS = {"h264-st0601-mp", "video-roundtrip", "strict-rejection"}


def _load_ids() -> set[str]:
    path = scenarios_dir() / "scenarios.toml"
    assert path.is_file(), f"scenarios.toml not found at {path}"
    with open(path, "rb") as fh:
        data = tomllib.load(fh)
    return {e["id"] for e in data.get("scenario", [])}


def test_manifest_loads_pilot_scenarios() -> None:
    ids = _load_ids()
    assert len(ids) >= 3, f"expected >=3 scenarios, got {sorted(ids)}"
    assert PILOT_IDS <= ids, f"missing pilot scenarios: {sorted(PILOT_IDS - ids)}"
