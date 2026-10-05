"""Fixed-width dataclasses must fail at construction time
for primitive-shape violations, not later at PyO3/encoder boundary."""

import pytest
from tstrans.mpegts import Pts90khz
from tstrans.klv import TimeStatus, UasDatalinkLs, VTargetPack, ST_0601_UL


def test_pts90khz_rejects_overflow_i64() -> None:
    Pts90khz(raw=(1 << 63) - 1)  # max i64 OK
    Pts90khz(raw=-(1 << 63))     # min i64 OK
    with pytest.raises((ValueError, OverflowError)):
        Pts90khz(raw=1 << 63)     # i64 + 1 — must reject


def test_time_status_rejects_out_of_byte_range() -> None:
    TimeStatus(raw=0)
    TimeStatus(raw=255)
    with pytest.raises(ValueError):
        TimeStatus(raw=-1)
    with pytest.raises(ValueError):
        TimeStatus(raw=256)


def test_uas_datalink_ls_universal_label_must_be_16_bytes() -> None:
    """universal_label is a user-settable bytes field (default b"\\x00" * 16).
    Any value that is not exactly 16 bytes must be rejected at construction."""
    # Valid: exactly 16 bytes
    UasDatalinkLs(universal_label=ST_0601_UL)
    UasDatalinkLs(universal_label=b"\x00" * 16)
    # Invalid: wrong lengths
    with pytest.raises(ValueError, match="universal_label"):
        UasDatalinkLs(universal_label=b"\x00" * 15)
    with pytest.raises(ValueError, match="universal_label"):
        UasDatalinkLs(universal_label=b"\x00" * 17)
    with pytest.raises(ValueError, match="universal_label"):
        UasDatalinkLs(universal_label=b"")


def test_vtarget_pack_target_color_must_be_rgb_triple() -> None:
    """target_color is an optional (R, G, B) tuple. When provided, it must be
    a 3-tuple with each element in 0..=255."""
    # Valid: None and correct 3-tuple
    VTargetPack(target_id=1, target_color=None)
    VTargetPack(target_id=1, target_color=(0, 128, 255))
    VTargetPack(target_id=1, target_color=(0, 0, 0))
    VTargetPack(target_id=1, target_color=(255, 255, 255))
    # Invalid: wrong length
    with pytest.raises(ValueError, match="target_color"):
        VTargetPack(target_id=1, target_color=(1, 2))        # type: ignore[arg-type]
    with pytest.raises(ValueError, match="target_color"):
        VTargetPack(target_id=1, target_color=(1, 2, 3, 4))  # type: ignore[arg-type]
    # Invalid: channel values out of byte range
    with pytest.raises(ValueError, match="target_color"):
        VTargetPack(target_id=1, target_color=(-1, 0, 0))    # type: ignore[arg-type]
    with pytest.raises(ValueError, match="target_color"):
        VTargetPack(target_id=1, target_color=(0, 256, 0))   # type: ignore[arg-type]
