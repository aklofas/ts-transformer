"""A reconnect policy with zero backoff and unlimited attempts is refused at
open, before any connect, with the config-invalid kind."""

import time

import pytest

from tstrans.exceptions import SrtError, SrtErrorKind
from tstrans.srt import BackoffStrategy, ManagedSender, ReconnectPolicy


def _unpaced_unlimited() -> ReconnectPolicy:
    return ReconnectPolicy(max_attempts=None, backoff=BackoffStrategy.constant(ms=0))


def test_managed_sender_refuses_an_unpaced_unlimited_policy_before_connecting() -> None:
    # Nothing listens on port 1: a connect attempt would take the connect
    # timeout, so a fast refusal proves no socket was touched.
    started = time.monotonic()
    with pytest.raises(SrtError) as info:
        ManagedSender.from_url("srt://127.0.0.1:1", policy=_unpaced_unlimited())
    assert info.value.kind == SrtErrorKind.CONFIG_INVALID
    assert time.monotonic() - started < 2.0


def test_zero_backoff_with_bounded_attempts_is_accepted_by_the_constructor() -> None:
    policy = ReconnectPolicy(max_attempts=3, backoff=BackoffStrategy.constant(ms=0))
    assert policy.max_attempts == 3
