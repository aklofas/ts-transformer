"""KLV record DataFrame adapter tests.

Tests `tstrans.pandas.klv_to_dataframe` — the polymorphic dispatcher across
the 4 MISB sets (UasDatalinkLs / SecurityLs / PrecisionTimeStampPack /
VmtiLs). Fixture availability drives skips: only ST 0601 fixtures ship in
the workspace today; ST 0102 / 0605 / 0903 paths skip gracefully.

Composites are exposed via the `frame_center()`,
`sensor_position()`, `sensor_attitude()`, `platform_attitude()`,
`sensor_fov()`, and `corners()` synthesizer methods; the DataFrame
columns track flat scalar dataclass fields (`frame_center_lat_deg`,
`sensor_lat_deg`, etc.) — see test below. `time_status` is a typed
`TimeStatus` object with property accessors.
"""

import pathlib

import pytest

pytestmark = pytest.mark.pandas

# The `pandas` marker filters at run-time, but pytest still imports test
# modules at collection. Skip the whole module when the [pandas] extra
# isn't installed (the python-core CI job runs without it).
pytest.importorskip("pandas")

import pandas as pd  # noqa: E402

from tstrans.klv import (  # noqa: E402
    PrecisionTimeStampPack,
    SecurityLs,
    UasDatalinkLs,
    VmtiLs,
)
from tstrans.pandas import klv_to_dataframe  # noqa: E402


# --- fixtures ------------------------------------------------------------

_FIXTURE_ROOT = pathlib.Path(__file__).parent.parent.parent.parent / "crates" / "tst-core" / "tests" / "fixtures"


def _uas_full():
    """Decode the synthetic full ST 0601 fixture (timestamp + composites populated)."""
    from tstrans.klv import decode_uas_datalink

    blob_path = _FIXTURE_ROOT / "st0601" / "synthetic_full.klv"
    assert blob_path.exists(), f"checked-in fixture missing: {blob_path}"
    return decode_uas_datalink(blob_path.read_bytes())


def _uas_minimal():
    """Decode the synthetic minimal ST 0601 fixture (only ts present)."""
    from tstrans.klv import decode_uas_datalink

    blob_path = _FIXTURE_ROOT / "st0601" / "synthetic_minimal.klv"
    assert blob_path.exists(), f"checked-in fixture missing: {blob_path}"
    return decode_uas_datalink(blob_path.read_bytes())


def _security():
    """Return a synthetic SecurityLs decoded from a spec-minimal ST 0102 record.

    Uses an in-process synthetic
    builder, so the test always runs without requiring a local corpus file.
    The builder calls encode_security() on a known-good SecurityLs, prefixes
    the 16-byte SECURITY_LS_UL, and round-trips through parse_klv_universal().
    """
    from _builders.synthetic_klv_universal import synthetic_security_ls
    from tstrans.klv import parse_klv_universal

    return parse_klv_universal(synthetic_security_ls())


def _vmti():
    """Return a synthetic VmtiLs decoded from a spec-minimal ST 0903 record.

    Uses an in-process synthetic
    builder. Contains 2 VTargetPacks so the targets-mode DataFrame has >0 rows.
    """
    from _builders.synthetic_klv_universal import synthetic_vmti_ls
    from tstrans.klv import parse_klv_universal

    return parse_klv_universal(synthetic_vmti_ls(n_targets=2))


# --- unconditional tests (no fixture deps) -------------------------------


def test_klv_to_dataframe_empty_returns_empty_dataframe():
    df = klv_to_dataframe([])
    assert isinstance(df, pd.DataFrame)
    assert len(df) == 0


def test_klv_to_dataframe_mixed_types_raises():
    """Hand-built records — uses default-constructed objects so no fixture is needed."""
    # PrecisionTimeStampPack accepts (timestamp_us, time_status); VmtiLs has a
    # default constructor. Mix them to trigger the homogeneity check.
    from tstrans.klv import TimeStatus

    ptsp = PrecisionTimeStampPack(timestamp_us=0, time_status=TimeStatus(raw=0xFF))
    vmti = VmtiLs()
    with pytest.raises(TypeError, match="homogeneous"):
        klv_to_dataframe([ptsp, vmti])


def test_klv_to_dataframe_unknown_type_raises():
    with pytest.raises(TypeError, match="unsupported"):
        klv_to_dataframe([object()])


def test_klv_to_dataframe_precision_timestamp_pack_decomposes_time_status():
    """ST 0605 PrecisionTimeStampPack — timestamp + decoded TimeStatus bits."""
    from tstrans.klv import TimeStatus

    rec = PrecisionTimeStampPack(
        timestamp_us=1_700_000_000_000_000,
        time_status=TimeStatus(raw=0xFF),  # 0xFF: discontinuity + reverse + locked=False
    )
    df = klv_to_dataframe([rec])
    assert len(df) == 1
    assert "timestamp_us" in df.columns
    assert "has_discontinuity" in df.columns
    assert "is_locked" in df.columns
    assert "is_reverse_jump" in df.columns
    assert "reserved_bits_valid" in df.columns
    # DatetimeIndex derived from timestamp_us
    assert isinstance(df.index, pd.DatetimeIndex)
    assert df.index.name == "pts"


# --- UAS Datalink (ST 0601) ----------------------------------------------


def test_klv_to_dataframe_uas_datalink_returns_dataframe():
    rec = _uas_full()
    df = klv_to_dataframe([rec])
    assert isinstance(df, pd.DataFrame)
    assert len(df) == 1


def test_klv_to_dataframe_uas_datalink_has_datetime_index():
    rec = _uas_full()
    df = klv_to_dataframe([rec])
    assert isinstance(df.index, pd.DatetimeIndex), f"got {type(df.index)}"
    assert df.index.name == "pts"


def test_klv_to_dataframe_uas_datalink_columns_include_flat_geo_fields():
    """Drift from plan: UasDatalinkLs uses flat scalar fields, not composite
    `platform_position` — the columns track real dataclass fields.

    `frame_center` / `sensor_position` / etc. are method synthesizers on the
    Rust side, not separate `@property` accessors, so the DataFrame only
    sees the underlying scalars.
    """
    rec = _uas_full()
    df = klv_to_dataframe([rec])
    cols = set(df.columns)
    expected_flat = {
        "frame_center_lat_deg",
        "frame_center_lon_deg",
        "frame_center_elev_m",
        "sensor_lat_deg",
        "sensor_lon_deg",
        "sensor_alt_m",
    }
    missing = expected_flat - cols
    assert not missing, f"missing flat-field cols: {missing}; have {sorted(cols)}"


def test_klv_to_dataframe_uas_datalink_field_errors_collapsed_to_string():
    """`field_errors` is a tuple of KlvFieldError on the Rust side; the
    adapter collapses to a comma-joined string column. Pandas may infer
    `object` (2.x) or `StringDtype` (3.x) — both are acceptable."""
    rec = _uas_full()
    df = klv_to_dataframe([rec])
    assert "field_errors" in df.columns
    col = df["field_errors"]
    # All values must be plain Python str instances (collapsed from tuples)
    assert all(isinstance(v, str) for v in col), f"non-str values in field_errors: {list(col)}"


def test_klv_to_dataframe_uas_datalink_skips_method_synthesizers():
    """Method accessors (`frame_center`, `sensor_position`, etc.) must NOT
    appear as columns — they're callables, not data fields."""
    rec = _uas_full()
    df = klv_to_dataframe([rec])
    cols = set(df.columns)
    for forbidden in (
        "frame_center",
        "sensor_position",
        "sensor_attitude",
        "platform_attitude",
        "sensor_fov",
        "corners",
    ):
        assert forbidden not in cols, f"column {forbidden!r} leaked from method"


def test_klv_to_dataframe_uas_datalink_minimal_no_composites():
    """Minimal fixture has only timestamp + version — most columns NaN/None."""
    rec = _uas_minimal()
    df = klv_to_dataframe([rec])
    assert len(df) == 1
    assert isinstance(df.index, pd.DatetimeIndex)


# --- Security (ST 0102) — skipped unless fixture present -----------------


def test_klv_to_dataframe_security_uses_range_index():
    rec = _security()
    df = klv_to_dataframe([rec])
    assert isinstance(df.index, pd.RangeIndex)


# --- VMTI (ST 0903) — synthetic (no fixture) -----------------------------


def test_vmti_summary_with_default_record():
    """Synthetic VmtiLs (no fixture) — covers the summary code path.

    Guards the CRITICAL bug where `precision_time_stamp` was incorrectly
    treated as a `PrecisionTimeStampPack` instead of `int | None`; that
    bug previously hid silently because all VMTI tests depended on a
    fixture that doesn't ship in the workspace.
    """
    df = klv_to_dataframe([VmtiLs(precision_time_stamp=1700000000_000000)])
    assert isinstance(df, pd.DataFrame)
    assert len(df) == 1
    assert "num_targets" in df.columns
    assert df["num_targets"].iloc[0] == 0
    # With precision_time_stamp set, index is DatetimeIndex
    assert isinstance(df.index, pd.DatetimeIndex)


def test_vmti_targets_empty_yields_empty_df():
    """Synthetic VmtiLs with no targets — covers the targets-mode empty path.

    Default-constructed VmtiLs has `targets=()`, so the inner loop emits
    zero rows; the result is an empty DataFrame and the early-exit branch
    (no MultiIndex assignment) takes over.
    """
    df = klv_to_dataframe([VmtiLs()], mode="targets")
    assert isinstance(df, pd.DataFrame)
    assert len(df) == 0


# --- VMTI (ST 0903) — skipped unless fixture present ---------------------


def test_klv_to_dataframe_vmti_summary_mode():
    rec = _vmti()
    df = klv_to_dataframe([rec])  # mode="summary" default
    assert len(df) == 1
    assert "num_targets" in df.columns


def test_klv_to_dataframe_vmti_targets_mode():
    rec = _vmti()
    df = klv_to_dataframe([rec], mode="targets")
    assert isinstance(df.index, pd.MultiIndex)
    assert df.index.names == ["pts", "target_id"]


# --- mode validation -----------------------------------------------------


@pytest.mark.pandas
def test_klv_to_dataframe_rejects_invalid_mode() -> None:
    """Typos like mode='target' must raise, not silently
    fall back to 'summary'."""
    records = []  # empty — function should still validate mode first
    with pytest.raises(ValueError, match="mode must be"):
        klv_to_dataframe(records, mode="target")  # missing 's'
    with pytest.raises(ValueError, match="mode must be"):
        klv_to_dataframe(records, mode="summmary")  # typo
