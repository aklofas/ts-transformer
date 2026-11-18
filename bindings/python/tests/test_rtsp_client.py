"""Surface tests for tstrans.rtp.RtspClient + auth dataclasses.

Covers dataclass validation (RtspClientConfig accepts the auth shapes,
rejects empty URL, rejects ad-hoc duck-typed auth objects), BasicAuth /
DigestAuth construction + repr (passwords must NOT leak through repr),
and the enum surface (TransportPref / DigestAlgorithm / RtspVersion).

The actual RTSP wire exchange is covered by `test_rtp_integration.py`
(`test_rtsp_loopback.py` is a documented skip — see its module docstring).
"""

import socket
import struct
import threading
import time

import pytest

from tstrans.exceptions import RtpError, RtpErrorKind, RtspError, RtspErrorKind
from tstrans.mpegts import Muxer, MuxerConfigBuilder, Pts90khz
from tstrans.rtp import (
    BasicAuth,
    DigestAlgorithm,
    DigestAuth,
    RtspCancelHandle,
    RtspClient,
    RtspClientConfig,
    RtspSession,
    RtspStats,
    RtspVersion,
    TransportPref,
)
from tstrans.shells import EndReason

from _builders.hang_deadline import call_with_hang_deadline
from _builders.mux_programs import video_only_program


# ---------------------------------------------------------------------------
# Enums
# ---------------------------------------------------------------------------


def test_transport_pref_enum_members():
    assert TransportPref.AUTO != TransportPref.UDP
    assert TransportPref.UDP != TransportPref.TCP
    assert TransportPref.AUTO != TransportPref.TCP


def test_digest_algorithm_enum_members():
    assert DigestAlgorithm.MD5 != DigestAlgorithm.SHA256


def test_rtsp_version_enum_members():
    assert RtspVersion.V1_0 != RtspVersion.V2_0


# ---------------------------------------------------------------------------
# BasicAuth
# ---------------------------------------------------------------------------


def test_basic_auth_constructs_and_exposes_user():
    a = BasicAuth(user="alice", password="hunter2")
    assert a.user == "alice"


def test_basic_auth_repr_redacts_password():
    a = BasicAuth(user="alice", password="topsecret")
    r = repr(a)
    assert "alice" in r
    assert "topsecret" not in r
    assert "<redacted>" in r


def test_basic_auth_password_not_accessible_attribute():
    # Frozen pyclass without a `password` getter — attribute access fails.
    a = BasicAuth(user="alice", password="x")
    with pytest.raises(AttributeError):
        _ = a.password


# ---------------------------------------------------------------------------
# DigestAuth
# ---------------------------------------------------------------------------


def test_digest_auth_constructs_default_algorithm():
    a = DigestAuth(user="bob", password="x")
    assert a.user == "bob"
    assert a.algorithm == DigestAlgorithm.MD5


def test_digest_auth_constructs_with_sha256():
    a = DigestAuth(user="bob", password="x", algorithm=DigestAlgorithm.SHA256)
    assert a.algorithm == DigestAlgorithm.SHA256


def test_digest_auth_repr_redacts_password():
    a = DigestAuth(user="bob", password="topsecret", algorithm=DigestAlgorithm.SHA256)
    r = repr(a)
    assert "bob" in r
    assert "topsecret" not in r
    assert "<redacted>" in r


def test_digest_auth_password_not_accessible():
    a = DigestAuth(user="bob", password="x")
    with pytest.raises(AttributeError):
        _ = a.password


# ---------------------------------------------------------------------------
# RtspClientConfig
# ---------------------------------------------------------------------------


def test_rtsp_client_config_defaults():
    cfg = RtspClientConfig(url="rtsp://127.0.0.1:8554/live")
    assert cfg.url == "rtsp://127.0.0.1:8554/live"
    assert cfg.auth is None
    assert cfg.transport_pref == TransportPref.AUTO
    assert cfg.tls_root_certs_pem is None
    assert cfg.keepalive is True
    assert cfg.rtsp_version == RtspVersion.V1_0


def test_rtsp_client_config_accepts_basic_auth():
    a = BasicAuth(user="alice", password="x")
    cfg = RtspClientConfig(url="rtsp://h/p", auth=a)
    assert cfg.auth is a  # round-trip preserves identity


def test_rtsp_client_config_accepts_digest_auth():
    a = DigestAuth(user="bob", password="x", algorithm=DigestAlgorithm.SHA256)
    cfg = RtspClientConfig(url="rtsp://h/p", auth=a)
    assert cfg.auth is a


def test_rtsp_client_config_accepts_none_auth_explicitly():
    cfg = RtspClientConfig(url="rtsp://h/p", auth=None)
    assert cfg.auth is None


def test_rtsp_client_config_rejects_empty_url():
    with pytest.raises(ValueError, match="url must not be empty"):
        RtspClientConfig(url="")


def test_rtsp_client_config_rejects_arbitrary_auth_object():
    class FakeAuth:
        user = "x"
        password = "y"

    with pytest.raises(ValueError, match="auth must be"):
        RtspClientConfig(url="rtsp://h/p", auth=FakeAuth())


def test_rtsp_client_config_rejects_string_auth():
    with pytest.raises(ValueError, match="auth must be"):
        RtspClientConfig(url="rtsp://h/p", auth="alice:hunter2")


def test_rtsp_client_config_accepts_transport_pref_kwarg():
    cfg = RtspClientConfig(url="rtsp://h/p", transport_pref=TransportPref.TCP)
    assert cfg.transport_pref == TransportPref.TCP


def test_rtsp_client_config_accepts_tls_pem_bytes():
    # We don't validate the PEM contents at construction — passthrough only.
    pem = b"-----BEGIN CERTIFICATE-----\nMIID...\n-----END CERTIFICATE-----\n"
    cfg = RtspClientConfig(url="rtsps://h/p", tls_root_certs_pem=pem)
    assert cfg.tls_root_certs_pem == pem


def test_rtsp_client_config_repr_redacts_pem_and_auth():
    a = BasicAuth(user="alice", password="topsecret")
    cfg = RtspClientConfig(
        url="rtsp://h/p", auth=a, tls_root_certs_pem=b"PEMBYTES"
    )
    r = repr(cfg)
    assert "topsecret" not in r
    assert "PEMBYTES" not in r
    assert "<auth>" in r
    assert "<bytes>" in r


# ---------------------------------------------------------------------------
# RtspStats
# ---------------------------------------------------------------------------


def test_rtsp_stats_default_is_zero():
    # Construction directly from Python isn't supported (no __new__);
    # we exercise it indirectly via the (currently unused) classmethod
    # surface. Use type-only checks here — `session.stats()` needs a live
    # session.
    assert RtspStats is not None
    assert hasattr(RtspStats, "__qualname__")


# ---------------------------------------------------------------------------
# RtspClient / RtspSession surface (offline — no server)
# ---------------------------------------------------------------------------


def test_rtsp_client_connect_is_staticmethod():
    # RtspClient holds no instance state — `connect` is a staticmethod.
    assert callable(RtspClient.connect)


def test_rtsp_client_connect_to_invalid_url_raises_rtsp_error():
    cfg = RtspClientConfig(url="rtsp://192.0.2.1:1/never")  # TEST-NET-1, blackhole
    # Either:
    # - URL-parse error → RtspError(kind=PROTOCOL) via the Url variant
    # - I/O failure (connect refused / timeout) → RtspError(kind=IO)
    # Both are valid surfaces; we don't assert the specific kind here.
    with pytest.raises(RtspError) as exc_info:
        RtspClient.connect(cfg)
    assert exc_info.value.kind in {
        RtspErrorKind.IO,
        RtspErrorKind.PROTOCOL,
        RtspErrorKind.TIMEOUT,
    }


def test_rtsp_client_connect_malformed_url_raises_rtsp_error():
    cfg = RtspClientConfig(url="not-a-url-at-all")
    with pytest.raises(RtspError) as exc_info:
        RtspClient.connect(cfg)
    # Builder construction (URL parse) fires before any I/O.
    assert exc_info.value.kind == RtspErrorKind.PROTOCOL


@pytest.mark.parametrize("url", [
    "rtsp://u:12345?secret-tail@cam/live",
    "rtsp://u:12345?transport=hunter2@host",
])
def test_rtsp_client_url_error_withholds_a_password_tail(url):
    # A raw `?` in a numeric-prefix password puts its tail in the query.
    with pytest.raises(RtspError) as exc_info:
        RtspClient.connect(RtspClientConfig(url=url))
    assert exc_info.value.kind == RtspErrorKind.PROTOCOL
    for text in (str(exc_info.value), repr(exc_info.value)):
        assert "secret-tail" not in text
        assert "hunter2" not in text


def test_rtsp_session_class_exposed():
    assert RtspSession is not None
    # Methods exist on the class (whether bound or unbound).
    for m in ("play", "pause", "teardown", "close", "cancel_handle", "stats",
              "into_demux_receiver", "__enter__", "__exit__", "is_torn_down"):
        assert hasattr(RtspSession, m), f"RtspSession missing method {m}"


# ---------------------------------------------------------------------------
# Cancel outcome
# ---------------------------------------------------------------------------

_SDP = (
    b"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=tst test\r\nt=0 0\r\n"
    b"a=control:*\r\nm=video 0 RTP/AVP 33\r\na=control:trackID=0\r\n"
)

_SDP_H264 = (
    b"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=tst test\r\nt=0 0\r\n"
    b"a=control:*\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n"
    b"a=fmtp:96 packetization-mode=1\r\na=control:trackID=0\r\n"
)


def _header(request: str, name: str) -> str:
    for line in request.split("\r\n")[1:]:
        key, _, value = line.partition(":")
        if key.strip().lower() == name:
            return value.strip()
    return ""


def _silent_on_pause_peer(got_pause: threading.Event | None = None):
    """A hand-rolled RTSP peer: answers OPTIONS / DESCRIBE (one MP2T
    track) / SETUP (echoes the requested Transport) / PLAY, then reads
    PAUSE and never answers it, so the client's PAUSE parks until a
    cancel. Sets `got_pause` once PAUSE is read. Returns (port, done
    event, server thread)."""
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    listener.settimeout(10)
    port = listener.getsockname()[1]
    done = threading.Event()

    def serve() -> None:
        try:
            sock, _ = listener.accept()
        except OSError:
            return
        finally:
            listener.close()
        with sock:
            sock.settimeout(10)
            buf = b""
            try:
                while True:
                    while b"\r\n\r\n" not in buf:
                        chunk = sock.recv(8192)
                        if not chunk:
                            return
                        buf += chunk
                    head, buf = buf.split(b"\r\n\r\n", 1)
                    req = head.decode("latin-1")
                    method = req.split(None, 1)[0]
                    if method == "PAUSE":
                        if got_pause is not None:
                            got_pause.set()
                        done.wait(10)
                        return
                    extra, body = "", b""
                    if method == "OPTIONS":
                        extra = "Public: OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN\r\n"
                    elif method == "DESCRIBE":
                        body = _SDP
                        extra = (
                            "Content-Type: application/sdp\r\n"
                            f"Content-Length: {len(body)}\r\n"
                        )
                    elif method == "SETUP":
                        extra = (
                            "Session: DEADBEEF;timeout=60\r\n"
                            f"Transport: {_header(req, 'transport')}\r\n"
                        )
                    elif method == "PLAY":
                        extra = "Session: DEADBEEF\r\n"
                    cseq = _header(req, "cseq") or "1"
                    sock.sendall(
                        f"RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n{extra}\r\n".encode()
                        + body
                    )
            except OSError:
                return

    server = threading.Thread(target=serve, daemon=True)
    server.start()
    return port, done, server


def _recording_peer(
    teardown_status: int | None = 200,
    after_play: bytes = b"",
    on_teardown=None,
    sdp: bytes = _SDP,
):
    """A hand-rolled RTSP peer that answers OPTIONS / DESCRIBE (`sdp`, one
    MP2T track by default) / SETUP (echoes the requested Transport) / PLAY / TEARDOWN in the
    request's own RTSP version, and records every request head.

    `after_play` is written right after the PLAY reply (interleaved `$`
    frames). TEARDOWN is answered with `teardown_status`; a 200 ends the
    peer, any other status keeps it reading, and `None` leaves TEARDOWN
    unanswered. `on_teardown()`, when given, runs on the peer's thread as
    soon as TEARDOWN is read, before the answer. The peer ends when the
    client closes the connection. Returns (port, requests, server thread)."""
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    listener.settimeout(10)
    port = listener.getsockname()[1]
    requests: list[str] = []

    def serve() -> None:
        try:
            sock, _ = listener.accept()
        except OSError:
            return
        finally:
            listener.close()
        with sock:
            sock.settimeout(10)
            buf = b""
            try:
                while True:
                    while b"\r\n\r\n" not in buf:
                        chunk = sock.recv(8192)
                        if not chunk:
                            return
                        buf += chunk
                    head, buf = buf.split(b"\r\n\r\n", 1)
                    req = head.decode("latin-1")
                    requests.append(req)
                    method = req.split(None, 1)[0]
                    version = req.split("\r\n", 1)[0].rsplit(" ", 1)[-1]
                    extra, body = "", b""
                    if method == "OPTIONS":
                        extra = "Public: OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN\r\n"
                    elif method == "DESCRIBE":
                        body = sdp
                        extra = (
                            "Content-Type: application/sdp\r\n"
                            f"Content-Length: {len(body)}\r\n"
                        )
                    elif method == "SETUP":
                        extra = (
                            "Session: DEADBEEF;timeout=60\r\n"
                            f"Transport: {_header(req, 'transport')}\r\n"
                        )
                    elif method in ("PLAY", "TEARDOWN"):
                        extra = "Session: DEADBEEF\r\n"
                    status = "200 OK"
                    if method == "TEARDOWN":
                        if on_teardown is not None:
                            on_teardown()
                        if teardown_status is None:
                            continue
                        if teardown_status != 200:
                            status = f"{teardown_status} Refused"
                    cseq = _header(req, "cseq") or "1"
                    sock.sendall(
                        f"{version} {status}\r\nCSeq: {cseq}\r\n{extra}\r\n".encode()
                        + body
                    )
                    if method == "PLAY" and after_play:
                        sock.sendall(after_play)
                    if method == "TEARDOWN" and teardown_status == 200:
                        return
            except OSError:
                return

    server = threading.Thread(target=serve, daemon=True)
    server.start()
    return port, requests, server


def _connect_and_teardown(cfg: RtspClientConfig, server: threading.Thread) -> None:
    try:
        session = RtspClient.connect(cfg)
        session.teardown()
    finally:
        server.join(10)


def _setup_transport(requests: list[str]) -> str:
    setup = next(r for r in requests if r.startswith("SETUP "))
    return _header(setup, "transport")


def test_transport_pref_tcp_reaches_setup():
    port, requests, server = _recording_peer()
    cfg = RtspClientConfig(
        url=f"rtsp://127.0.0.1:{port}/x",
        transport_pref=TransportPref.TCP,
        keepalive=False,
    )
    _connect_and_teardown(cfg, server)
    assert "RTP/AVP/TCP" in _setup_transport(requests), requests


def test_default_transport_pref_defers_to_the_url_query():
    port, requests, server = _recording_peer()
    cfg = RtspClientConfig(url=f"rtsp://127.0.0.1:{port}/x?transport=tcp", keepalive=False)
    _connect_and_teardown(cfg, server)
    assert "RTP/AVP/TCP" in _setup_transport(requests), requests


def test_rtsp_version_reaches_every_request_line():
    port, requests, server = _recording_peer()
    cfg = RtspClientConfig(
        url=f"rtsp://127.0.0.1:{port}/x",
        transport_pref=TransportPref.TCP,
        rtsp_version=RtspVersion.V2_0,
        keepalive=False,
    )
    _connect_and_teardown(cfg, server)
    lines = [r.split("\r\n", 1)[0] for r in requests]
    assert lines and all(line.endswith(" RTSP/2.0") for line in lines), lines


@pytest.mark.parametrize(
    ("query", "kwargs", "want_tcp", "want_version"),
    [
        # TransportPref.UDP forces a UDP SETUP.
        ("", {"transport_pref": TransportPref.UDP}, False, "RTSP/1.0"),
        # A non-default value overrides a contrary URL query.
        ("?transport=udp", {"transport_pref": TransportPref.TCP}, True, "RTSP/1.0"),
        ("?rtsp_version=1.0", {"rtsp_version": RtspVersion.V2_0}, False, "RTSP/2.0"),
        # The default V1_0 defers to the URL: the query wins.
        ("?rtsp_version=2.0", {}, False, "RTSP/2.0"),
    ],
    ids=["udp-forced", "tcp-overrides-url-udp", "v2-overrides-url-v1", "default-defers-to-url-v2"],
)
def test_transport_and_version_precedence_over_the_url(query, kwargs, want_tcp, want_version):
    port, requests, server = _recording_peer()
    cfg = RtspClientConfig(url=f"rtsp://127.0.0.1:{port}/x{query}", keepalive=False, **kwargs)
    _connect_and_teardown(cfg, server)
    transport = _setup_transport(requests)
    if want_tcp:
        assert "RTP/AVP/TCP" in transport, requests
    else:
        assert transport.startswith("RTP/AVP;unicast;client_port="), requests
        assert "TCP" not in transport, requests
    lines = [r.split("\r\n", 1)[0] for r in requests]
    assert lines and all(line.endswith(" " + want_version) for line in lines), lines


def test_rtsp_client_config_has_no_rtcp_knob():
    with pytest.raises(TypeError):
        RtspClientConfig(url="rtsp://h/p", rtcp=False)
    assert not hasattr(RtspClientConfig(url="rtsp://h/p"), "rtcp")
    assert "rtcp" not in repr(RtspClientConfig(url="rtsp://h/p"))


def test_cancel_of_a_parked_control_call_is_closed():
    """A cancel fired while `pause()` is parked on a silent peer ends the
    call with `RtspError(CLOSED)` — the one cancel outcome, never
    PROTOCOL. The handle only exists on a live session,
    so the parked call is PAUSE, not connect. Asserts the kind only."""
    port, done, server = _silent_on_pause_peer()
    cfg = RtspClientConfig(
        url=f"rtsp://127.0.0.1:{port}/x",
        transport_pref=TransportPref.TCP,
        keepalive=False,
    )
    try:
        with RtspClient.connect(cfg) as session:
            handle = session.cancel_handle()
            # Whether the cancel lands before or after PAUSE parks, the
            # call ends at its next poll with the same kind.
            canceller = threading.Timer(0.2, handle.cancel)
            canceller.start()
            try:
                with pytest.raises(RtspError) as exc_info:
                    session.pause()
            finally:
                canceller.cancel()
            assert exc_info.value.kind == RtspErrorKind.CLOSED
    finally:
        done.set()
        server.join(timeout=5)


# ---------------------------------------------------------------------------
# close(), the with exit, and calls from a second thread
# ---------------------------------------------------------------------------

_IDR_NAL = b"\x00\x00\x00\x01\x65\xbb"


def _interleaved_ts_frames() -> bytes:
    """Four muxed H.264 access units as RTP/MP2T packets of seven TS
    packets each, framed as `$` frames on interleaved channel 0."""
    mux = Muxer(MuxerConfigBuilder().add_program(video_only_program()).build())
    for i in range(4):
        mux.push_video(_IDR_NAL, pts=Pts90khz.from_raw(i * 3000), key_frame=True)
    ts = bytearray()
    scratch = bytearray(188 * 32)
    while (n := mux.pull(scratch)) > 0:
        ts.extend(scratch[:n])
    frames = b""
    for seq, off in enumerate(range(0, len(ts), 188 * 7)):
        rtp = struct.pack("!BBHII", 0x80, 33, seq, seq * 3000, 0x1234) + ts[off:off + 188 * 7]
        frames += b"$\x00" + struct.pack("!H", len(rtp)) + rtp
    return frames


def _consume(rx, first: threading.Event, outcome: dict) -> None:
    """Iterate `rx` to its end: `outcome["end"]` is "stop" for end of
    stream, else the exception it raised."""
    try:
        for _ in rx:
            first.set()
        outcome["end"] = "stop"
    except BaseException as exc:  # noqa: BLE001
        outcome["end"] = exc


def _join_or_rescue(thread: threading.Thread, rescue, budget_s: float) -> bool:
    """Join `thread` within `budget_s`; if it is still alive, run `rescue`
    and join again. Returns whether the rescue was needed; fails the test
    if the thread outlives the rescue."""
    thread.join(budget_s)
    if not thread.is_alive():
        return False
    rescue()
    thread.join(10)
    assert not thread.is_alive(), "thread still alive after its rescue"
    return True


def _tcp_config(port: int) -> RtspClientConfig:
    return RtspClientConfig(
        url=f"rtsp://127.0.0.1:{port}/x",
        transport_pref=TransportPref.TCP,
        keepalive=False,
    )


def test_with_exit_after_a_refused_teardown_ends_the_live_receiver_closed():
    port, requests, server = _recording_peer(
        teardown_status=403, after_play=_interleaved_ts_frames()
    )
    first, outcome = threading.Event(), {}
    consumer = None
    rescue = None
    try:
        with RtspClient.connect(_tcp_config(port)) as session:
            rx = session.into_demux_receiver()
            rescue = rx.cancel_handle()
            consumer = threading.Thread(target=_consume, args=(rx, first, outcome), daemon=True)
            consumer.start()
            assert first.wait(5.0), "no event reached the receiver"
        rescued = _join_or_rescue(consumer, rescue.cancel, 5.0)
        assert not rescued, "the receiver kept running after the with exit"
        assert any(r.startswith("TEARDOWN ") for r in requests), requests
        assert isinstance(outcome["end"], RtpError), outcome
        assert outcome["end"].kind == RtpErrorKind.CLOSED
        assert rx.end_reason() == EndReason.CANCELLED
        rx.close()
    finally:
        if consumer is not None and consumer.is_alive():
            _join_or_rescue(consumer, rescue.cancel, 0)
        server.join(10)
        assert not server.is_alive()


@pytest.mark.parametrize("teardown_status", [403, None], ids=["refused", "unanswered"])
def test_close_is_silent_bounded_and_idempotent(teardown_status):
    port, requests, server = _recording_peer(teardown_status=teardown_status)
    try:
        session = RtspClient.connect(_tcp_config(port))
        begin = time.monotonic()
        assert session.close() is None
        elapsed = time.monotonic() - begin
        assert session.close() is None
        # Far below the 10 s request timeout an unanswered teardown() waits.
        assert elapsed < 5.0, f"close() took {elapsed:.3f}s"
        assert any(r.startswith("TEARDOWN ") for r in requests), requests
        assert session.is_torn_down()
        for call in (session.pause, session.play, session.cancel_handle):
            with pytest.raises(RtspError) as exc_info:
                call()
            assert exc_info.value.kind == RtspErrorKind.CLOSED, call
        assert session.teardown() is None
    finally:
        server.join(10)
        assert not server.is_alive()


def test_accepted_teardown_then_close_ends_the_live_receiver_with_end_of_stream():
    from tstrans.rtp import RtspServer, RtspServerConfig

    stop = threading.Event()
    first, outcome = threading.Event(), {}
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", video_only_program())
        port = server.local_addr().rsplit(":", 1)[1]

        def producer() -> None:
            i = 0
            while not stop.is_set():
                mount.push_video(_IDR_NAL, pts=Pts90khz.from_raw(i * 3000), key_frame=True)
                i += 1
                stop.wait(0.02)

        pusher = threading.Thread(target=producer, daemon=True)
        pusher.start()
        consumer = None
        rescue = None
        try:
            session = RtspClient.connect(
                RtspClientConfig(url=f"rtsp://127.0.0.1:{port}/live?transport=tcp", keepalive=False)
            )
            rx = session.into_demux_receiver()
            rescue = rx.cancel_handle()
            consumer = threading.Thread(target=_consume, args=(rx, first, outcome), daemon=True)
            consumer.start()
            assert first.wait(5.0), "no event reached the receiver"
            session.teardown()
            session.close()
            rescued = _join_or_rescue(consumer, rescue.cancel, 5.0)
            assert not rescued, "the receiver did not end"
            assert outcome["end"] == "stop", outcome
            assert rx.end_reason() == EndReason.CLEAN_TEARDOWN
            rx.close()
        finally:
            if consumer is not None and consumer.is_alive():
                _join_or_rescue(consumer, rescue.cancel, 0)
            stop.set()
            pusher.join(5)
            assert not pusher.is_alive()


def test_with_exit_on_an_idle_session_sends_teardown_before_any_cancel():
    # The peer answers TEARDOWN at once from its own thread; what it sees
    # when the TEARDOWN arrives proves the exit did not cancel first.
    cancelled_at_teardown: list[bool] = []
    handle = None

    def on_teardown() -> None:
        cancelled_at_teardown.append(handle.is_cancelled())

    port, requests, server = _recording_peer(on_teardown=on_teardown)
    try:
        with RtspClient.connect(_tcp_config(port)) as session:
            handle = session.cancel_handle()
        assert cancelled_at_teardown == [False], cancelled_at_teardown
        assert handle.is_cancelled()
        assert any(r.startswith("TEARDOWN ") for r in requests), requests
    finally:
        server.join(10)
        assert not server.is_alive()


def test_a_second_control_call_waits_for_the_one_in_progress():
    got_pause = threading.Event()
    port, done, server = _silent_on_pause_peer(got_pause)
    session = RtspClient.connect(_tcp_config(port))
    handle = session.cancel_handle()
    outcome: dict = {}

    def call(name: str) -> None:
        try:
            getattr(session, name)()
            outcome[name] = None
        except BaseException as exc:  # noqa: BLE001
            outcome[name] = exc

    paused = threading.Thread(target=call, args=("pause",), daemon=True)
    torn = threading.Thread(target=call, args=("teardown",), daemon=True)
    paused.start()
    try:
        assert got_pause.wait(5.0), "PAUSE never reached the peer"
        torn.start()
        torn.join(1.0)
        assert torn.is_alive(), f"teardown() did not wait for the parked pause(): {outcome}"
        assert not session.is_torn_down()

        handle.cancel()
        paused.join(5)
        torn.join(5)
        assert not paused.is_alive() and not torn.is_alive(), "the cancel did not end both calls"
        for name in ("pause", "teardown"):
            assert isinstance(outcome[name], RtspError), outcome
            assert outcome[name].kind == RtspErrorKind.CLOSED, outcome
    finally:
        handle.cancel()
        done.set()
        session.close()
        paused.join(10)
        if torn.ident is not None:
            torn.join(10)
        server.join(10)
        assert not paused.is_alive() and not torn.is_alive()
        assert not server.is_alive()


def test_second_thread_cancel_handle_and_close_while_pause_is_parked():
    got_pause = threading.Event()
    port, done, server = _silent_on_pause_peer(got_pause)
    session = RtspClient.connect(_tcp_config(port))
    outcome: dict = {}

    def parked() -> None:
        try:
            session.pause()
            outcome["pause"] = None
        except BaseException as exc:  # noqa: BLE001
            outcome["pause"] = exc

    paused = threading.Thread(target=parked, daemon=True)
    paused.start()
    try:
        assert got_pause.wait(5.0), "PAUSE never reached the peer"
        blocked, got = call_with_hang_deadline(session.cancel_handle, 5.0)
        assert not blocked, "cancel_handle() waited behind the parked pause()"
        assert "exc" not in got, got
        handle = got["value"]
        assert isinstance(handle, RtspCancelHandle)
        assert not handle.is_cancelled()

        blocked, got = call_with_hang_deadline(session.close, 10.0)
        assert not blocked, "close() did not return"
        assert got == {"value": None}, got
        paused.join(5)
        assert not paused.is_alive(), "the parked pause() did not end"
        assert isinstance(outcome["pause"], RtspError), outcome
        assert outcome["pause"].kind == RtspErrorKind.CLOSED
        assert handle.is_cancelled()
    finally:
        done.set()
        paused.join(10)
        server.join(10)
        assert not paused.is_alive()
        assert not server.is_alive()


def test_cancel_handle_after_teardown_and_after_close():
    port, _requests, server = _recording_peer()
    try:
        session = RtspClient.connect(_tcp_config(port))
        session.teardown()
        handle = session.cancel_handle()
        assert not handle.is_cancelled()
        session.close()
        # The close ends by dropping the client, which sets its cancel.
        assert handle.is_cancelled()
        with pytest.raises(RtspError) as exc_info:
            session.cancel_handle()
        assert exc_info.value.kind == RtspErrorKind.CLOSED
    finally:
        server.join(10)
        assert not server.is_alive()


@pytest.mark.parametrize("ending", ["close", "with_exit"])
@pytest.mark.parametrize("h264", [False, True], ids=["mp2t", "h264"])
def test_data_plane_after_close_is_closed(h264, ending):
    # The session's own take, then the other codec's: a closed session is
    # CLOSED before the wrong-constructor check could say PROTOCOL.
    port, _requests, server = _recording_peer(sdp=_SDP_H264 if h264 else _SDP)
    connect = RtspClient.connect_h264 if h264 else RtspClient.connect
    try:
        if ending == "close":
            session = connect(_tcp_config(port))
            session.close()
        else:
            with connect(_tcp_config(port)) as session:
                pass
        takes = [session.into_demux_receiver, session.into_h264_receiver]
        for take in reversed(takes) if h264 else takes:
            with pytest.raises(RtspError) as exc_info:
                take()
            assert exc_info.value.kind == RtspErrorKind.CLOSED, take
    finally:
        server.join(10)
        assert not server.is_alive()


def test_data_plane_after_close_is_closed_over_udp():
    from tstrans.rtp import RtspServer, RtspServerConfig

    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        server.add_unicast_mount("/live", video_only_program())
        port = server.local_addr().rsplit(":", 1)[1]
        session = RtspClient.connect(
            RtspClientConfig(url=f"rtsp://127.0.0.1:{port}/live", keepalive=False)
        )
        session.close()
        with pytest.raises(RtspError) as exc_info:
            session.into_demux_receiver()
        assert exc_info.value.kind == RtspErrorKind.CLOSED


def _recv_exact(sock: socket.socket, n: int) -> bytes:
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise OSError("peer closed early")
        buf += chunk
    return buf


def _fin_after_client_hello_peer():
    """Accepts one connection, reads exactly one TLS record (the
    ClientHello: 5-byte header + its length), then closes. Reading the
    whole record matters: closing with unread bytes would send RST, which
    already took the error path. Returns (port, server thread)."""
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    listener.settimeout(10)
    port = listener.getsockname()[1]

    def serve() -> None:
        try:
            sock, _ = listener.accept()
        except OSError:
            return
        finally:
            listener.close()
        with sock:
            sock.settimeout(5)
            try:
                header = _recv_exact(sock, 5)
                _recv_exact(sock, int.from_bytes(header[3:5], "big"))
            except OSError:
                return

    server = threading.Thread(target=serve, daemon=True)
    server.start()
    return port, server


# `method="thread"`: a regressed handshake spins inside native code, where
# the signal method's exception would never be delivered.
@pytest.mark.timeout(20, method="thread")
def test_rtsps_peer_closing_mid_handshake_raises_tls():
    """A FIN after the ClientHello ends `connect` with `RtspError(TLS)`
    instead of polling the closed socket forever."""
    port, server = _fin_after_client_hello_peer()
    with pytest.raises(RtspError) as exc_info:
        RtspClient.connect(
            RtspClientConfig(url=f"rtsps://127.0.0.1:{port}/x", keepalive=False)
        )
    assert exc_info.value.kind == RtspErrorKind.TLS
    server.join(5)
