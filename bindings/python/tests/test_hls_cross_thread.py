"""`tstrans.hls` from two threads: a push on one thread must never freeze a
call on another.

The HLS classes guard their inner publisher with a mutex, and every push
releases the GIL for the native work. A method that waits for that mutex
while it still holds the GIL completes a cycle with a push in flight:

  - thread P is inside a push: it holds the mutex, the GIL is released;
  - thread G takes the GIL and calls a getter, which blocks on the mutex;
  - P finishes and needs the GIL back before its guard can drop.

P waits for G, G waits for P, and the interpreter is frozen — the native
call does not have to be slow. So every method takes the mutex with the GIL
released, and the construction constants (`local_addr`, `local_port`,
`repr`) take no lock at all.

Each case runs in a subprocess with a hard deadline: a regression freezes
the interpreter, which no in-process assertion could report.
"""

from __future__ import annotations

import os
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

pytest.importorskip(
    "tstrans.hls",
    reason="tstrans.hls missing = under-built wheel (or --no-default-features build); investigate.",
    exc_type=ImportError,
)

import tstrans

PKG_PARENT = str(Path(tstrans.__file__).resolve().parent.parent)

# Thread P pushes without pause until told to stop; the main thread makes a
# fixed number of calls meanwhile. No clock decides the overlap: every push
# releases the GIL, which hands it straight to the main thread, so its next
# call lands while P is inside the native push. `other(i)` is the call under
# test.
_HARNESS = """
    import tempfile, threading

    from tstrans.hls import HlsPublisher, MuxPublisher
    from tstrans.mpegts import MuxerProgramConfigBuilder, Pts90khz, VideoCodec

    CALLS = 2000
    TS = (b"\\x47" + b"\\x00" * 187) * 64
    NAL = b"\\x00\\x00\\x00\\x01\\x65" + b"\\xbb" * 4096

    def hls_publisher(d):
        return HlsPublisher.builder().bind("127.0.0.1:0").output_dir(d).build()

    def mux_publisher(d):
        program = MuxerProgramConfigBuilder(1, 0x100).add_video(0x101, VideoCodec.H264).build()
        return MuxPublisher.with_config_hls(hls_publisher(d), program)

    def hammer(push, other):
        started = threading.Event()
        stop = threading.Event()
        failed = []

        def pusher():
            try:
                i = 0
                while not stop.is_set():
                    push(i)
                    started.set()
                    i += 1
            except BaseException as exc:
                failed.append(exc)
                started.set()

        t = threading.Thread(target=pusher, daemon=True)
        t.start()
        assert started.wait(10.0), "the pusher never completed a push"
        try:
            for i in range(CALLS):
                other(i)
        finally:
            stop.set()
            t.join(10.0)
        assert not t.is_alive(), "the pusher did not stop"
        assert not failed, failed
        print("DONE", flush=True)
"""

_HLS_PUSH_TS = """
    with tempfile.TemporaryDirectory() as d:
        pub = hls_publisher(d)
        hammer(lambda i: pub.push_ts(TS), lambda i: {call})
        pub.close()
"""

_MUX_SEND_VIDEO = """
    with tempfile.TemporaryDirectory() as d:
        mp = mux_publisher(d)
        hammer(
            lambda i: mp.send_video(NAL, pts=Pts90khz.from_raw(i * 3000), key_frame=(i == 0)),
            lambda i: {call},
        )
        mp.finish_into_publisher().close()
"""

CASES = {
    "HlsPublisher.local_addr": _HLS_PUSH_TS.format(call="pub.local_addr()"),
    "HlsPublisher.local_port": _HLS_PUSH_TS.format(call="pub.local_port()"),
    "HlsPublisher.repr": _HLS_PUSH_TS.format(call="repr(pub)"),
    "HlsPublisher.stats": _HLS_PUSH_TS.format(call="pub.stats()"),
    "HlsPublisher.hls_stats": _HLS_PUSH_TS.format(call="pub.hls_stats()"),
    "HlsPublisher.render_playlist": _HLS_PUSH_TS.format(call="pub.render_playlist()"),
    "HlsPublisher.cut_segment": _HLS_PUSH_TS.format(call="pub.cut_segment()"),
    "HlsPublisher.push_ts": _HLS_PUSH_TS.format(call="pub.push_ts(TS)"),
    "MuxPublisher.repr": _MUX_SEND_VIDEO.format(call="repr(mp)"),
    "MuxPublisher.stats": _MUX_SEND_VIDEO.format(call="mp.stats()"),
    "MuxPublisher.publisher_stats": _MUX_SEND_VIDEO.format(call="mp.publisher_stats()"),
    "MuxPublisher.cut_segment": _MUX_SEND_VIDEO.format(call="mp.cut_segment()"),
}


@pytest.mark.parametrize("name", sorted(CASES))
def test_a_push_in_flight_does_not_freeze_a_call_on_another_thread(name: str) -> None:
    script = textwrap.dedent(_HARNESS) + textwrap.dedent(CASES[name])
    timeout = 30.0
    try:
        r = subprocess.run(
            [sys.executable, "-c", script],
            capture_output=True,
            text=True,
            timeout=timeout,
            env={**os.environ, "PYTHONPATH": PKG_PARENT},
        )
    except subprocess.TimeoutExpired:
        pytest.fail(
            f"interpreter frozen: {name} called while another thread was pushing "
            f"did not return within {timeout}s — it waited for the publisher's "
            "lock while holding the GIL"
        )
    assert "DONE" in r.stdout, f"exit={r.returncode}\n{r.stdout}\n{r.stderr}"
    assert r.returncode == 0, f"exit={r.returncode}\n{r.stdout}\n{r.stderr}"


def test_local_addr_and_repr_answer_from_the_construction_snapshot(tmp_path: Path) -> None:
    """The getters no longer read the live publisher; pin what they say
    across the lifecycle so the snapshot cannot drift from it."""
    from tstrans.exceptions import HlsError, HlsErrorKind
    from tstrans.hls import HlsPublisher

    pub = HlsPublisher.builder().bind("127.0.0.1:0").output_dir(str(tmp_path)).build()
    addr = pub.local_addr()
    assert addr is not None and addr.startswith("127.0.0.1:")
    assert pub.local_port() == int(addr.rsplit(":", 1)[1])
    assert repr(pub) == "HlsPublisher(open)"

    pub.finish()
    assert repr(pub) == "HlsPublisher(finished)"
    for getter in (pub.local_addr, pub.local_port):
        with pytest.raises(HlsError) as ei:
            getter()
        assert ei.value.kind == HlsErrorKind.FINISHED
