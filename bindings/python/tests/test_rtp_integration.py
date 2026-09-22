"""Wave C Task 25 — end-to-end integration tests for `tstrans.rtp.*`.

These tests exercise the full Python surface as a real consumer would:

1. `test_rtp_loopback_round_trip_mux_sender_to_demux_receiver`
   The no-RTSP path — `MuxSender` ↔ `DemuxReceiver` over a UDP loopback
   port. Pushes a video NAL on one end, iterates `DemuxEvent` on the
   other, and asserts a `Video` event arrives within a 5 s deadline.
   Validates the data-plane round-trip without the RTSP control plane.

2. `test_full_pipeline_rtsp_server_to_rtsp_client`
   The full RTSP path — `RtspServer.add_unicast_mount(...)` →
   `RtspClient.connect(...)` → `session.into_demux_receiver()` (the
   Wave B T23 bridge). A background producer thread pushes a video NAL
   through the mount; the foreground consumer iterates the demux
   receiver and asserts a `Video` event arrives within a 5 s deadline.
   Closes the client + server cleanly via `__exit__`.

Both tests construct the program shape inline (`_build_test_program`)
to avoid coupling to the Wave A/B test fixtures.
"""

from __future__ import annotations

import threading
import time

import pytest

import tstrans.rtp
from tstrans.mpegts import (
    DemuxEvent,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)
from tstrans.rtp import (
    DemuxReceiver,
    MuxSender,
    RtspClient,
    RtspClientConfig,
    RtspServer,
    RtspServerConfig,
)

from _builders.ports import free_udp_port as _free_udp_port


# --------------------------------------------------------------------------- #
# Shared helpers                                                              #
# --------------------------------------------------------------------------- #


# Annex-B IDR NAL — type 5 (IDR) + filler payload. Reliably triggers a
# Sample event downstream because the muxer emits PSI on the first key
# frame and immediately drains the AU through the elementary-stream PES
# packer. Reused across both integration tests.
_IDR_NAL = bytes([0x00, 0x00, 0x00, 0x01, 0x65, 0xBB])


def _build_test_program():
    """Single H.264 video stream on PID 0x101 in program 1 (PMT PID
    0x100). Matches the shape used by `test_rtp_mux_sender.py` and
    `test_rtsp_server.py` so the integration tests stay aligned with
    the Wave A/B suite."""
    return (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .build()
    )


# --------------------------------------------------------------------------- #
# 1. No-RTSP loopback — MuxSender ↔ DemuxReceiver                             #
# --------------------------------------------------------------------------- #


def test_rtp_loopback_round_trip_mux_sender_to_demux_receiver() -> None:
    """Validates the RTP data-plane round-trip without the RTSP control
    plane. A `MuxSender` pushes video to a `127.0.0.1:<port>` URL; a
    `DemuxReceiver` bound on the same port consumes the loopback
    traffic and yields a `Video` event.

    The consumer thread breaks out of the iterator as soon as a `Video`
    event arrives — that's the success signal. A 5 s join deadline
    bounds the test even when CI hardware is slow.
    """
    port = _free_udp_port()
    rx = DemuxReceiver(f"rtp://127.0.0.1:{port}")
    events: list[object] = []
    err: list[BaseException] = []
    saw_video = threading.Event()

    def consumer() -> None:
        try:
            for ev in rx:
                events.append(ev)
                if isinstance(ev, DemuxEvent.Video):
                    saw_video.set()
                    break
        except BaseException as exc:  # noqa: BLE001
            # `rx.close()` from the main thread fires CANCELLED on the
            # pending recv — that's fine if we've already seen a Video.
            err.append(exc)

    consumer_thread = threading.Thread(target=consumer, daemon=True)
    consumer_thread.start()
    # Give the consumer time to enter the kernel recv before the first
    # send. Without this delay, the first burst can race the consumer
    # bind and disappear into the void on UDP loopback.
    time.sleep(0.2)

    program = _build_test_program()
    with MuxSender(f"rtp://127.0.0.1:{port}", program) as snd:
        # Push several IDRs so PSI tables emit on the first one and the
        # subsequent NALs ride downstream as Sample events. Eight is a
        # comfortable margin without slowing the test.
        for i in range(8):
            snd.send_video(
                _IDR_NAL,
                pts=Pts90khz.from_raw(i * 3000),
                key_frame=(i == 0),
            )

    # Wait for the consumer to see a Video event (5 s deadline).
    saw_video.wait(timeout=5.0)
    rx.close()
    consumer_thread.join(timeout=1.0)

    if not saw_video.is_set() and err and not events:
        pytest.fail(f"consumer raised before any event: {err}")
    assert saw_video.is_set(), (
        f"expected at least one Video event within 5s, got: "
        f"{[type(e).__name__ for e in events]}"
    )


# --------------------------------------------------------------------------- #
# 2. Full pipeline — RtspServer ↔ RtspClient via into_demux_receiver bridge  #
# --------------------------------------------------------------------------- #


def test_full_pipeline_rtsp_server_to_rtsp_client() -> None:
    """End-to-end exercise of the Wave A/B RTSP surface:

    1. Start an `RtspServer` bound to `127.0.0.1:0` (kernel-picked port).
    2. Add a unicast mount with a single H.264 stream.
    3. Read the server's bound port via `local_addr()`.
    4. From the same process, `RtspClient.connect()` to the mount.
    5. `session.into_demux_receiver()` — the T23 bridge.
    6. In a background thread, the mount keeps pushing IDR NALs.
    7. Foreground consumer iterates the demux receiver, asserts a
       `Video` event arrives within 5 s.
    8. Clean shutdown: client teardown via `__exit__`, server stop via
       `__exit__`.

    If the bridge wiring is incomplete (e.g. `into_recv_transport`
    leaves the RTP socket pair dangling for the chosen transport) the
    test's 5 s deadline fires and pytest reports a clear failure.
    """
    server_cfg = RtspServerConfig(
        bind_addr="127.0.0.1:0",
        # Keep the test fast — no need for the default 2 s drain on a
        # local-only loopback teardown.
        graceful_shutdown_drain_ms=50,
    )
    program = _build_test_program()

    saw_video = threading.Event()
    consumer_events: list[object] = []
    consumer_err: list[BaseException] = []
    stop_producer = threading.Event()
    producer_err: list[BaseException] = []

    with RtspServer.start(server_cfg) as server:
        local = server.local_addr()
        assert local is not None, "server.local_addr() returned None"
        # `local_addr()` returns a `host:port` string like
        # `127.0.0.1:54321`. Split off the port for the client URL.
        # (The Rust side stringifies a SocketAddr, never with a scheme.)
        host, _, port_str = local.rpartition(":")
        assert host == "127.0.0.1", f"unexpected bind host: {host!r}"
        port = int(port_str)

        mount = server.add_unicast_mount("/live", program)

        def producer() -> None:
            """Keep pushing IDR NALs until the consumer signals it saw
            a Video event or the producer is stopped. The mount pushes
            succeed even pre-PLAY (broadcast::send returns Err which
            the Rust side suppresses) so we can start before the client
            connects."""
            i = 0
            try:
                while not stop_producer.is_set():
                    mount.push_video(
                        _IDR_NAL,
                        pts=Pts90khz.from_raw(i * 3000),
                        key_frame=(i % 4 == 0),
                    )
                    i += 1
                    time.sleep(0.02)  # ~50 fps
            except BaseException as exc:  # noqa: BLE001
                producer_err.append(exc)

        producer_thread = threading.Thread(target=producer, daemon=True)
        producer_thread.start()

        # Give the producer a head start so the mount has frames
        # queued when the client SETUPs.
        time.sleep(0.1)

        client_cfg = RtspClientConfig(
            url=f"rtsp://{host}:{port}/live",
            # Disable the auto-keepalive thread — the test is short
            # enough that the default 60 s session timeout won't bite
            # and we don't want a stray background thread complicating
            # shutdown ordering.
            keepalive=False,
        )

        try:
            with RtspClient.connect(client_cfg) as session:
                # The T23 bridge: consume the SETUP-time RtspSession
                # into a Wave-B DemuxReceiver that reads from the RTP
                # socket pair (UDP) or the TCP-interleaved mpsc rx.
                demux = session.into_demux_receiver()

                # 0.7.0: the data plane is a consumable handle, so a
                # second take is a closed-handle condition (`CLOSED`),
                # not a protocol error. Pins the one RtspErrorKind
                # member Arc 2 added.
                from tstrans.exceptions import RtspError, RtspErrorKind

                with pytest.raises(RtspError) as double_take:
                    session.into_demux_receiver()
                assert double_take.value.kind == RtspErrorKind.CLOSED

                def consumer() -> None:
                    try:
                        for ev in demux:
                            consumer_events.append(ev)
                            if isinstance(ev, DemuxEvent.Video):
                                saw_video.set()
                                break
                    except BaseException as exc:  # noqa: BLE001
                        consumer_err.append(exc)

                consumer_thread = threading.Thread(
                    target=consumer, daemon=True
                )
                consumer_thread.start()

                # Wait up to 5 s for the bridge round-trip to deliver
                # at least one Video event end-to-end.
                got_video = saw_video.wait(timeout=5.0)

                # Signal the producer to stop and let the consumer
                # exit naturally on the next iteration (or via close()).
                stop_producer.set()
                # Close the receiver so the consumer's `for ev in demux`
                # iterator wakes on CANCELLED.
                demux.close()
                consumer_thread.join(timeout=1.0)
        finally:
            # Producer might still be running if the `with RtspClient`
            # body raised before stop_producer was set.
            stop_producer.set()
            producer_thread.join(timeout=1.0)

    # Producer surfacing an error is the most actionable failure mode —
    # surface it before the assertion on saw_video.
    if producer_err:
        pytest.fail(f"mount.push_video raised: {producer_err[0]!r}")

    if not got_video:
        # Categorize the failure: did the consumer raise, or did the
        # bridge just never deliver bytes?
        if consumer_err:
            pytest.fail(
                "RtspSession.into_demux_receiver bridge did not deliver "
                f"a Video event within 5s; consumer raised: "
                f"{consumer_err[0]!r}; events seen: "
                f"{[type(e).__name__ for e in consumer_events]}"
            )
        pytest.fail(
            "RtspSession.into_demux_receiver bridge did not deliver a "
            f"Video event within 5s; events seen: "
            f"{[type(e).__name__ for e in consumer_events]}"
        )

    assert saw_video.is_set()


# --------------------------------------------------------------------------- #
# 3. rtsps:// — TLS end-to-end with a custom trust anchor                     #
# --------------------------------------------------------------------------- #


def test_rtsps_client_connect_with_custom_ca() -> None:
    """TLS end-to-end over rtsps://: the server binds with the fixture
    cert + key (`RtspServerConfig.tls_cert` / `tls_key`), the client
    trusts that same self-signed cert via
    `RtspClientConfig.tls_root_certs_pem`, and the full
    OPTIONS/DESCRIBE/SETUP/PLAY handshake runs over the encrypted
    control connection (rtsps forces TCP-interleaved data).

    Regressions pinned: rtsps:// used to be unreachable from Python
    (tst-rtp built without its tls feature), and tls_root_certs_pem used
    to be an accepted-but-unread field.
    """
    import pathlib

    d = pathlib.Path(__file__).parent / "fixtures" / "tls"
    cert, key = str(d / "cert.pem"), str(d / "key.pem")

    server_cfg = RtspServerConfig(
        bind_addr="rtsps://127.0.0.1:0",
        tls_cert=cert,
        tls_key=key,
        graceful_shutdown_drain_ms=50,
    )
    program = _build_test_program()

    with RtspServer.start(server_cfg) as server:
        local = server.local_addr()
        assert local is not None
        host, _, port_str = local.rpartition(":")
        port = int(port_str)

        server.add_unicast_mount("/live", program)

        client_cfg = RtspClientConfig(
            url=f"rtsps://{host}:{port}/live",
            tls_root_certs_pem=pathlib.Path(cert).read_bytes(),
            keepalive=False,
        )
        with RtspClient.connect(client_cfg) as session:
            assert not session.is_torn_down()
        # __exit__ tears down over the same TLS connection.


def test_rtsps_client_untrusted_cert_fails_closed() -> None:
    """Without a custom trust anchor the self-signed server cert fails
    native-root verification — the client raises RtspError and never
    reaches the RTSP layer. Proves certificate verification is on."""
    import pathlib

    import pytest

    from tstrans.exceptions import RtspError

    d = pathlib.Path(__file__).parent / "fixtures" / "tls"
    cert, key = str(d / "cert.pem"), str(d / "key.pem")

    server_cfg = RtspServerConfig(
        bind_addr="rtsps://127.0.0.1:0",
        tls_cert=cert,
        tls_key=key,
        graceful_shutdown_drain_ms=50,
    )
    with RtspServer.start(server_cfg) as server:
        local = server.local_addr()
        assert local is not None
        host, _, port_str = local.rpartition(":")
        port = int(port_str)
        server.add_unicast_mount("/live", _build_test_program())

        cfg = RtspClientConfig(url=f"rtsps://{host}:{port}/live", keepalive=False)
        with pytest.raises(RtspError) as exc_info:
            RtspClient.connect(cfg)
        # The feature-off build had a distinct static message; a live
        # verification failure must be anything but that.
        assert "requires the 'tls' cargo feature" not in str(exc_info.value)


def test_rtsp_server_start_rejects_tls_paths_on_plaintext_bind() -> None:
    """TLS cert/key configured on a plaintext rtsp:// bind must fail
    start() instead of silently coming up unencrypted with the certs
    ignored (the pre-fix behavior). The fixture files are VALID and
    readable — the scheme, not the paths, is the failure cause (Python's
    own guard checks readability, never the scheme; the refusal comes
    from the Rust layer)."""
    import pathlib

    from tstrans.exceptions import RtspError

    d = pathlib.Path(__file__).parent / "fixtures" / "tls"
    server_cfg = RtspServerConfig(
        bind_addr="rtsp://127.0.0.1:0",
        tls_cert=str(d / "cert.pem"),
        tls_key=str(d / "key.pem"),
    )
    with pytest.raises(RtspError, match="rtsps"):
        RtspServer.start(server_cfg)
