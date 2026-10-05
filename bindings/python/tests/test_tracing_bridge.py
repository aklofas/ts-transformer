"""Tests for the `TSTRANS_LOG` tracing-to-stderr bridge.

The Rust core (`tst-core`/`tst-rtp`/`tst-pipeline`) emits `tracing`
events, but nothing installs a subscriber for the Python extension —
so those events are silently discarded unless the embedding process
happens to have installed one itself. `TSTRANS_LOG` opts a bare
`import tstrans` process into a stderr subscriber at module-init time.

Runs the check in a subprocess (not in-process) because a `tracing`
subscriber can only be installed once per process, and we need to
observe both the "installed" and "not installed" states cleanly.
"""

from __future__ import annotations

import os
import subprocess
import sys

from _builders.ports import free_udp_port as _free_udp_port


# `DemuxReceiver.__init__` (crates/tst-pipeline/src/demux_receiver.rs)
# emits `tracing::info!("DemuxReceiver opened")` unconditionally on
# construction — no packets need to flow. Binding to a locally-freed
# ephemeral UDP port keeps this deterministic and network-free.
_TRACED_OP = """
import tstrans.rtp
rx = tstrans.rtp.DemuxReceiver("rtp://127.0.0.1:{port}")
rx.close()
"""


def _run(*, tstrans_log: str | None, port: int) -> subprocess.CompletedProcess[str]:
    env = dict(os.environ)
    if tstrans_log is None:
        env.pop("TSTRANS_LOG", None)
    else:
        env["TSTRANS_LOG"] = tstrans_log
    return subprocess.run(
        [sys.executable, "-c", _TRACED_OP.format(port=port)],
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )


def test_tstrans_log_set_emits_to_stderr() -> None:
    port = _free_udp_port()
    result = _run(tstrans_log="trace", port=port)
    assert result.returncode == 0, result.stderr
    assert result.stderr.strip() != ""
    # Specificity: the emitted line names the tracing target, not just
    # any stray stderr noise.
    assert "tst_pipeline" in result.stderr


def test_tstrans_log_unset_emits_nothing() -> None:
    port = _free_udp_port()
    result = _run(tstrans_log=None, port=port)
    assert result.returncode == 0, result.stderr
    assert result.stderr.strip() == ""
