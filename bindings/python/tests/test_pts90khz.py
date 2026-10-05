"""Pts90khz wrapper — value semantics over a 90 kHz tick count."""

import pytest

from tstrans.mpegts import Pts90khz


def test_from_raw_round_trip():
    pts = Pts90khz.from_raw(90_000)
    assert pts.raw == 90_000


def test_from_ms_round_trip():
    pts = Pts90khz.from_ms(1000)
    assert pts.raw == 90_000  # 1000 ms × 90 ticks/ms
    assert pts.ms == 1000


def test_from_seconds_round_trip():
    pts = Pts90khz.from_seconds(0.5)
    assert pts.raw == 45_000
    assert pts.seconds == pytest.approx(0.5)


def test_ms_is_int_truncating():
    # 90_001 ticks = 1.000011... s = 1000.0111... ms → int truncates to 1000
    pts = Pts90khz.from_raw(90_001)
    assert pts.ms == 1000


def test_seconds_is_float():
    pts = Pts90khz.from_raw(45_000)
    assert isinstance(pts.seconds, float)
    assert pts.seconds == pytest.approx(0.5)


def test_equality_by_raw_value():
    assert Pts90khz.from_raw(123) == Pts90khz.from_raw(123)
    assert Pts90khz.from_raw(123) != Pts90khz.from_raw(124)


def test_hashable():
    s = {Pts90khz.from_raw(1), Pts90khz.from_raw(1), Pts90khz.from_raw(2)}
    assert len(s) == 2


def test_repr_includes_raw_and_ms():
    pts = Pts90khz.from_raw(90_000)
    r = repr(pts)
    assert "90000" in r
    assert "1000" in r or "1.0" in r  # ms or seconds


def test_negative_raw_allowed():
    # Rust's i64 PTS allows negative for diff arithmetic
    pts = Pts90khz.from_raw(-100)
    assert pts.raw == -100


# Pts90khz.ms must truncate toward zero (Rust integer division
# semantics), not floor toward -inf (Python's `//`). Boundary values around
# ±90 ticks (= 1 ms) prove the divergence: e.g. -1 ticks floor-divided by 90
# is -1 in Python, but truncated-divided is 0 — which is what Rust returns.

@pytest.mark.parametrize(
    "raw,expected_ms",
    [
        # Positive — floor and truncate agree on positives.
        (0, 0),
        (1, 0),
        (89, 0),
        (90, 1),
        (91, 1),
        (100, 1),
        # Negative — these are the discriminators between floor and truncate.
        # int_div(raw, 90) toward zero matches Rust:
        (-1, 0),     # Python floor: -1; Rust trunc: 0
        (-89, 0),    # Python floor: -1; Rust trunc: 0
        (-90, -1),   # exact — both agree
        (-91, -1),   # Python floor: -2; Rust trunc: -1
        (-100, -1),  # Python floor: -2; Rust trunc: -1
    ],
)
def test_ms_truncates_toward_zero(raw, expected_ms):
    assert Pts90khz.from_raw(raw).ms == expected_ms


def test_ms_int_overflow_safe_for_large_negatives():
    # i64 lower bound — use a value far beyond float64's 53-bit mantissa
    # exact-int range so a naive `int(self.raw / 90)` implementation would
    # produce a wrong answer. raw = -(2**60); truncated divide by 90 gives
    # `-(2**60) / 90` truncated toward zero. Compute the expected value via
    # the same sign-aware integer formula the implementation should use:
    raw = -(2**60)
    expected = -((-raw) // 90)  # toward-zero integer divide
    assert Pts90khz.from_raw(raw).ms == expected
