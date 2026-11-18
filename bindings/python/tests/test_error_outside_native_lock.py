"""A refusal builds its exception only after the native mutex is released.

The binding's exception classes are Python classes with a Python
`__init__`, and a refusal constructs one. Built while a native mutex guard
is still alive, that `__init__` is a point where CPython can hand the GIL
to another thread; a thread that then waits for the same mutex with the
GIL held never lets the first one finish its `__init__` and drop the
guard: a two-thread deadlock.

Each case patches the family's exception `__init__` so that the first
construction starts a helper thread calling a sibling method that takes
the same mutex with the GIL held, and joins it with a deadline. The
interleaving is forced, not raced: while the guard is held, the helper
blocks inside the native lock holding the GIL, so the child can never
return from the join. The child therefore runs in a subprocess with a
hard deadline; it exits 0 only if the helper's call returned in time.
"""

from __future__ import annotations

import os
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

import tstrans

PKG_PARENT = str(Path(tstrans.__file__).resolve().parent.parent)

# Under the suite's 60 s pytest-timeout (raised for this test below), so a
# deadlock fails here, with the child's output.
CHILD_TIMEOUT_S = 45.0

# `arm(cls, sibling)`: the first `cls(...)` built in the child runs
# `sibling` on a helper thread and joins it with a deadline before the real
# `__init__`. `state["joined"]` says whether the helper returned in time.
_PRELUDE = """
    import sys, threading
    import tstrans

    JOIN_DEADLINE_S = 10.0

    def arm(exc_cls, sibling):
        original = exc_cls.__init__
        state = {"constructed": False, "joined": False}

        def patched(self, *args, **kwargs):
            if not state["constructed"]:
                state["constructed"] = True
                print("in __init__: starting the sibling call", flush=True)
                helper = threading.Thread(target=sibling, daemon=True)
                helper.start()
                helper.join(JOIN_DEADLINE_S)
                state["joined"] = not helper.is_alive()
                print("in __init__: join returned", flush=True)
            original(self, *args, **kwargs)

        exc_cls.__init__ = patched
        return state

    def quietly(call):
        def run():
            try:
                call()
            except Exception:
                pass
        return run

    def expect_refusal(call, exc_cls, kind, state):
        print("calling the refused method", flush=True)
        try:
            call()
        except exc_cls as e:
            assert e.kind == kind, e.kind
        else:
            raise AssertionError("the call was not refused")
        assert state["constructed"], "the refusal built no exception"
        assert state["joined"], "the sibling call did not return in time"
"""

_SRT_SETUP = """
    from tstrans.exceptions import SrtError, SrtErrorKind

    lst = tstrans.srt.Builder("srt://127.0.0.1:0?mode=listener").listen()
    port = lst.local_addr()[1]
    accepted = []
    acceptor = threading.Thread(
        target=lambda: accepted.append(lst.accept(timeout_ms=10000)), daemon=True
    )
    acceptor.start()
    sock = tstrans.srt.Builder(f"srt://127.0.0.1:{port}").connect()
    acceptor.join(15)
    sock.close()
    state = arm(SrtError, quietly(sock.is_alive))
"""

_SRT_TEARDOWN = """
    for s in accepted:
        s.close()
    lst.close()
"""

_HLS_SETUP = """
    import shutil, tempfile
    from tstrans.exceptions import HlsError, HlsErrorKind
    from tstrans.hls import HlsMode, HlsPublisher

    out = tempfile.mkdtemp()
    pub = (
        HlsPublisher.builder()
        .bind("127.0.0.1:0")
        .output_dir(out)
        .mode(HlsMode.VOD)
        .build()
    )
    pub.push_ts(b"\\x47" + b"\\x00" * 187)
    pub.cut_segment()
    handle = pub.finish_serving()
    handle.shutdown()
    state = arm(HlsError, quietly(lambda: repr(handle)))
"""

_HLS_TEARDOWN = """
    shutil.rmtree(out, ignore_errors=True)
"""

# A hand-rolled RTSP peer: one track described by `SDP`, every request
# answered 200, TCP-interleaved transport echoed; it ends when the client
# closes the connection.
_RTSP_PEER = """
    import socket
    from tstrans.exceptions import RtspError, RtspErrorKind
    from tstrans.rtp import RtspClient, RtspClientConfig, TransportPref

    def header(request, name):
        for line in request.split("\\r\\n")[1:]:
            key, _, value = line.partition(":")
            if key.strip().lower() == name:
                return value.strip()
        return ""

    def serve(listener):
        sock, _ = listener.accept()
        listener.close()
        with sock:
            sock.settimeout(30)
            buf = b""
            try:
                while True:
                    while b"\\r\\n\\r\\n" not in buf:
                        chunk = sock.recv(8192)
                        if not chunk:
                            return
                        buf += chunk
                    head, buf = buf.split(b"\\r\\n\\r\\n", 1)
                    req = head.decode("latin-1")
                    method = req.split(None, 1)[0]
                    extra, body = "", b""
                    if method == "OPTIONS":
                        extra = "Public: OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN\\r\\n"
                    elif method == "DESCRIBE":
                        body = SDP
                        extra = f"Content-Type: application/sdp\\r\\nContent-Length: {len(body)}\\r\\n"
                    elif method == "SETUP":
                        extra = f"Session: DEADBEEF;timeout=60\\r\\nTransport: {header(req, 'transport')}\\r\\n"
                    elif method in ("PLAY", "TEARDOWN"):
                        extra = "Session: DEADBEEF\\r\\n"
                    cseq = header(req, "cseq") or "1"
                    sock.sendall(f"RTSP/1.0 200 OK\\r\\nCSeq: {cseq}\\r\\n{extra}\\r\\n".encode() + body)
            except OSError:
                return

    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    listener.settimeout(30)
    peer = threading.Thread(target=serve, args=(listener,), daemon=True)
    peer.start()
    cfg = RtspClientConfig(
        url=f"rtsp://127.0.0.1:{listener.getsockname()[1]}/x",
        transport_pref=TransportPref.TCP,
        keepalive=False,
    )
"""

_SDP_MP2T = """
    SDP = (
        b"v=0\\r\\no=- 0 0 IN IP4 127.0.0.1\\r\\ns=t\\r\\nt=0 0\\r\\n"
        b"a=control:*\\r\\nm=video 0 RTP/AVP 33\\r\\na=control:trackID=0\\r\\n"
    )
"""

_SDP_H264 = """
    SDP = (
        b"v=0\\r\\no=- 0 0 IN IP4 127.0.0.1\\r\\ns=t\\r\\nt=0 0\\r\\n"
        b"a=control:*\\r\\nm=video 0 RTP/AVP 96\\r\\na=rtpmap:96 H264/90000\\r\\n"
        b"a=fmtp:96 packetization-mode=1\\r\\na=control:trackID=0\\r\\n"
    )
"""

_RTSP_TEARDOWN = """
    session.close()
    peer.join(30)
"""

# id -> (setup, the refused call, its exception class and kind, teardown).
# The sibling is chosen in the setup: one that locks the same mutex with
# the GIL held.
_CASES = {
    "srt-socket-local-addr": (
        _SRT_SETUP,
        "expect_refusal(sock.local_addr, SrtError, SrtErrorKind.CLOSED, state)",
        _SRT_TEARDOWN,
    ),
    "srt-socket-peer-addr": (
        _SRT_SETUP,
        "expect_refusal(sock.peer_addr, SrtError, SrtErrorKind.CLOSED, state)",
        _SRT_TEARDOWN,
    ),
    "srt-socket-into-sender": (
        _SRT_SETUP,
        "expect_refusal(sock.into_sender, SrtError, SrtErrorKind.CLOSED, state)",
        _SRT_TEARDOWN,
    ),
    "hls-server-handle-local-addr": (
        _HLS_SETUP,
        "expect_refusal(handle.local_addr, HlsError, HlsErrorKind.FINISHED, state)",
        _HLS_TEARDOWN,
    ),
    "hls-server-handle-local-port": (
        _HLS_SETUP,
        "expect_refusal(handle.local_port, HlsError, HlsErrorKind.FINISHED, state)",
        _HLS_TEARDOWN,
    ),
    # The data plane already taken: the consumed check holds the session
    # mutex; the sibling's own consumed check waits for it.
    "rtsp-session-data-plane-consumed": (
        _SDP_MP2T
        + _RTSP_PEER
        + """
    session = RtspClient.connect(cfg)
    rx = session.into_demux_receiver()
    state = arm(RtspError, quietly(session.into_demux_receiver))
""",
        "expect_refusal(session.into_demux_receiver, RtspError, RtspErrorKind.CLOSED, state)",
        "\n    rx.close()\n" + _RTSP_TEARDOWN,
    ),
    # An MP2T session has no H.264 config: the refusal holds that slot's
    # mutex; the sibling waits for it after its consumed check.
    "rtsp-session-h264-receiver-on-mp2t": (
        _SDP_MP2T
        + _RTSP_PEER
        + """
    session = RtspClient.connect(cfg)
    state = arm(RtspError, quietly(session.into_h264_receiver))
""",
        "expect_refusal(session.into_h264_receiver, RtspError, RtspErrorKind.PROTOCOL, state)",
        _RTSP_TEARDOWN,
    ),
    # An H.264 session refuses the MP2T receiver while it reads the same
    # config slot.
    "rtsp-session-demux-receiver-on-h264": (
        _SDP_H264
        + _RTSP_PEER
        + """
    session = RtspClient.connect_h264(cfg)
    state = arm(RtspError, quietly(session.into_demux_receiver))
""",
        "expect_refusal(session.into_demux_receiver, RtspError, RtspErrorKind.PROTOCOL, state)",
        _RTSP_TEARDOWN,
    ),
}


def _run_child(script: str) -> subprocess.CompletedProcess[str]:
    """Run `script` in a child interpreter. A timeout is the deadlock this
    module exists to catch: it fails with the child's output so far."""
    try:
        return subprocess.run(
            [sys.executable, "-c", script],
            capture_output=True,
            text=True,
            timeout=CHILD_TIMEOUT_S,
            env={**os.environ, "PYTHONPATH": PKG_PARENT},
        )
    except subprocess.TimeoutExpired as e:
        out = e.stdout.decode() if isinstance(e.stdout, bytes) else (e.stdout or "")
    pytest.fail(
        f"child did not finish within {CHILD_TIMEOUT_S}s (deadlocked); "
        f"its output so far:\n{out}"
    )


@pytest.mark.timeout(90)
@pytest.mark.parametrize("case", sorted(_CASES))
def test_refusal_builds_its_exception_after_releasing_the_native_lock(case: str) -> None:
    setup, call, teardown = _CASES[case]
    script = textwrap.dedent(_PRELUDE + setup + "\n    " + call + "\n" + teardown)
    script += '\nprint("OK", flush=True)\n'
    r = _run_child(script)
    assert r.returncode == 0, f"child failed:\n{r.stdout}\n{r.stderr}"
    assert r.stdout.rstrip().endswith("OK"), r.stdout
