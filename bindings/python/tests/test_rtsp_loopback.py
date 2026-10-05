"""Loopback end-to-end test for RtspClient.

The tst-rtp test fixture `crates/tst-rtp/tests/fixtures/rtsp_loopback_server.rs`
is gated under `#[cfg(test)]` and not reachable from a separate crate
(tst-py is a separate crate, even within the same workspace). This
loopback test is therefore SKIPPED — the wire-level RtspClient.connect
path is covered by tst-rtp's own integration tests in `crates/tst-rtp/
tests/rtsp_client_setup_play.rs` and friends, which exercise the exact
same `RtspClient` we wrap, and by `test_rtp_integration.py`, which runs
the Python `RtspServer` → `RtspClient` path end to end.

Running it would need the fixture exposed as `pub` behind a
`test-fixtures` cargo feature on tst-rtp, or spawned via a subprocess
that links the test-built binary. The file stays as a documented
`@pytest.mark.skip` so the test discovery surface is stable.
"""

import pytest

from tstrans.rtp import RtspClient, RtspClientConfig


@pytest.mark.skip(
    reason="needs tst-rtp test-fixtures feature (an RTSP server fixture); "
    "tst-rtp's own integration tests already exercise the wire path"
)
def test_connect_to_unauth_loopback_then_teardown():
    # Sketch (executes once the fixture is reachable):
    #
    # from tstrans._test_fixtures import rtsp_loopback  # NOT YET
    # with rtsp_loopback(port=0, auth=None) as (host, port):
    #     cfg = RtspClientConfig(
    #         url=f"rtsp://{host}:{port}/live",
    #         keepalive=False,
    #     )
    #     with RtspClient.connect(cfg) as session:
    #         assert session.is_torn_down() is False
    #         assert session.stats() is not None
    #     # __exit__ fires teardown best-effort
    #     # session.is_torn_down() is True here

    cfg = RtspClientConfig(url="rtsp://127.0.0.1:0/live")
    with RtspClient.connect(cfg) as session:
        assert session.is_torn_down() is False
