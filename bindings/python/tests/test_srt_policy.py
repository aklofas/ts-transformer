"""`tstrans.srt.ReconnectPolicy` / `BackoffStrategy` /
`OverflowPolicy`.

Pure-Python ergonomics: construction, defaults, accessors, validation,
and `__repr__` shape. No transport interaction, no error mapping
involvement — this is the ergonomic surface the managed shells
(`ManagedSender` etc.) consume.

Defaults must match `tst_pipeline::ReconnectPolicy::default()` to keep
the binding symmetric with the Rust crate:

    max_attempts        = Some(10)
    backoff             = BackoffStrategy::Exponential { 100ms, 10_000ms }
    gap_buffer_capacity = 256
    overflow_policy     = OverflowPolicy::DropOldest
"""

from __future__ import annotations

import pytest

from tstrans.srt import BackoffStrategy, OverflowPolicy, ReconnectMode, ReconnectPolicy


# --------------------------------------------------------------------------- #
# Defaults match Rust                                                         #
# --------------------------------------------------------------------------- #


def test_reconnect_policy_defaults_match_rust():
    """`ReconnectPolicy()` with no kwargs reproduces
    `tst_pipeline::ReconnectPolicy::default()`."""
    p = ReconnectPolicy()
    assert p.max_attempts == 10
    assert p.gap_buffer_capacity == 256
    assert p.overflow_policy == OverflowPolicy.DROP_OLDEST
    assert p.mode == ReconnectMode.BLOCKING
    # Default backoff is exponential 100ms..=10_000ms.
    assert p.backoff.kind == "exponential"
    assert p.backoff.base_ms == 100
    assert p.backoff.max_ms == 10_000


# --------------------------------------------------------------------------- #
# ReconnectMode enum-shape semantics                                          #
# --------------------------------------------------------------------------- #


def test_reconnect_mode_round_trips_through_policy():
    """`mode=ReconnectMode.BACKGROUND` must round-trip through the
    `.mode` getter, compared with `==` (pyclass enum, not IntEnum
    identity)."""
    p = ReconnectPolicy(mode=ReconnectMode.BACKGROUND)
    assert p.mode == ReconnectMode.BACKGROUND
    assert p.mode != ReconnectMode.BLOCKING


def test_reconnect_mode_default_is_blocking():
    """Omitting `mode=` defaults to `ReconnectMode.BLOCKING`, matching
    `tst_pipeline::ReconnectMode::default()`."""
    p = ReconnectPolicy()
    assert p.mode == ReconnectMode.BLOCKING


# --------------------------------------------------------------------------- #
# BackoffStrategy.constant                                                    #
# --------------------------------------------------------------------------- #


def test_backoff_strategy_constant():
    """Constant variant — `base_ms == max_ms == ms`."""
    b = BackoffStrategy.constant(ms=500)
    assert b.kind == "constant"
    assert b.base_ms == 500
    assert b.max_ms == 500


# --------------------------------------------------------------------------- #
# BackoffStrategy.exponential                                                 #
# --------------------------------------------------------------------------- #


def test_backoff_strategy_exponential():
    """Exponential variant — distinct base + max."""
    b = BackoffStrategy.exponential(base_ms=100, max_ms=10_000)
    assert b.kind == "exponential"
    assert b.base_ms == 100
    assert b.max_ms == 10_000


def test_backoff_strategy_exponential_max_less_than_base_raises():
    """`exponential(base_ms=200, max_ms=100)` must raise ValueError —
    a max-cap below the base is meaningless."""
    with pytest.raises(ValueError, match="max_ms must be >= base_ms"):
        BackoffStrategy.exponential(base_ms=200, max_ms=100)


def test_backoff_strategy_exponential_equal_base_and_max_ok():
    """`base_ms == max_ms` is allowed (degenerate, but well-defined)."""
    b = BackoffStrategy.exponential(base_ms=500, max_ms=500)
    assert b.kind == "exponential"
    assert b.base_ms == 500
    assert b.max_ms == 500


# --------------------------------------------------------------------------- #
# ReconnectPolicy validation                                                  #
# --------------------------------------------------------------------------- #


def test_reconnect_policy_zero_capacity_raises():
    """`gap_buffer_capacity=0` is meaningless (no room to queue gap
    bytes during outage) and must raise ValueError."""
    with pytest.raises(ValueError, match="gap_buffer_capacity must be > 0"):
        ReconnectPolicy(gap_buffer_capacity=0)


def test_reconnect_policy_none_max_attempts_means_retry_forever():
    """`max_attempts=None` is the "retry forever" sentinel — the
    Python None must round-trip through the getter."""
    p = ReconnectPolicy(max_attempts=None)
    assert p.max_attempts is None


def test_reconnect_policy_custom_backoff_propagates():
    """Passing a custom `BackoffStrategy` to the constructor must
    surface back through the `.backoff` getter unchanged."""
    custom = BackoffStrategy.constant(ms=250)
    p = ReconnectPolicy(backoff=custom)
    assert p.backoff.kind == "constant"
    assert p.backoff.base_ms == 250
    assert p.backoff.max_ms == 250


# --------------------------------------------------------------------------- #
# ReconnectPolicy.__repr__                                                    #
# --------------------------------------------------------------------------- #


def test_reconnect_policy_repr_includes_all_four_fields():
    """The `__repr__` is a debugging aid — it must show all four
    configured fields, including the embedded `BackoffStrategy` form."""
    p = ReconnectPolicy(
        max_attempts=5,
        backoff=BackoffStrategy.constant(ms=750),
        gap_buffer_capacity=128,
        overflow_policy=OverflowPolicy.REJECT,
    )
    r = repr(p)
    assert "ReconnectPolicy(" in r
    assert "max_attempts=5" in r
    assert "BackoffStrategy.constant(ms=750)" in r
    assert "gap_buffer_capacity=128" in r
    assert "OverflowPolicy.REJECT" in r


def test_reconnect_policy_repr_includes_mode():
    """`__repr__` also shows the configured `mode`."""
    p = ReconnectPolicy(mode=ReconnectMode.BACKGROUND)
    assert "ReconnectMode.BACKGROUND" in repr(p)


def test_reconnect_policy_repr_handles_none_max_attempts():
    """`max_attempts=None` reads as `max_attempts=None` in repr (not
    `Some(...)`)."""
    p = ReconnectPolicy(max_attempts=None)
    assert "max_attempts=None" in repr(p)


# --------------------------------------------------------------------------- #
# OverflowPolicy enum-shape semantics                                         #
# --------------------------------------------------------------------------- #


def test_overflow_policy_int_values():
    """`OverflowPolicy` is an IntEnum-shaped frozen PyClass — its
    variants compare equal to their underlying int codes."""
    assert OverflowPolicy.DROP_OLDEST == 0
    assert OverflowPolicy.REJECT == 1


def test_overflow_policy_variants_distinct():
    """The two variants are not equal to each other."""
    assert OverflowPolicy.DROP_OLDEST != OverflowPolicy.REJECT


# --------------------------------------------------------------------------- #
# BackoffStrategy.__repr__                                                    #
# --------------------------------------------------------------------------- #


def test_backoff_strategy_repr_constant():
    assert repr(BackoffStrategy.constant(ms=500)) == "BackoffStrategy.constant(ms=500)"


def test_backoff_strategy_repr_exponential():
    assert (
        repr(BackoffStrategy.exponential(base_ms=100, max_ms=10_000))
        == "BackoffStrategy.exponential(base_ms=100, max_ms=10000)"
    )
