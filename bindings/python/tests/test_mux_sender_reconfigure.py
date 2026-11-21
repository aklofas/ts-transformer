"""`MuxSender.reconfigure` on the muxing shells (`shells.MuxSender`,
`srt.ManagedMuxSender`): the running sender takes a new program config. A
surviving stream keeps its continuity counter, the PMT version moves, an
added stream is sent and read back, a removed stream's kept handle is
refused, a refused config changes nothing, and the `*_handle()` getters
follow the current configuration without waiting for a call in progress.

The far side of a `tcp://` sender is a plain socket read to its end; what it
read is demuxed with `tstrans.mpegts.Demuxer`. The parked cases use an
`srt://` sender to a peer that never reads, opened with a send timeout, so a
send fails and leaves output the next call must write first: that call
then waits in the transport while it holds the sender.
"""

from __future__ import annotations

import socket
import threading
import time
from typing import Callable, Optional

import pytest

from tstrans import shells
from tstrans.exceptions import MuxError, MuxErrorKind, SrtError, SrtErrorKind, TcpError, TcpErrorKind
from tstrans.mpegts import (
    DemuxEvent,
    KlvStreamType,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)

from _builders.hang_deadline import call_with_hang_deadline
from _builders.parked import slot_held
from _builders.ports import free_udp_port
from _builders.tcp_reader import TcpReader
from _builders.wait import wait_for
from test_cross_thread_close import _stream_handles as _handles
from test_mux_reconfigure import _events

# An IDR slice NAL long enough to span several TS packets, so a continuity
# counter that restarted would show as a jump.
NAL_IDR = b"\x00\x00\x00\x01\x65" + bytes(range(256)) * 4
KLV_LS = (
    b"\x06\x0e\x2b\x34\x02\x0b\x01\x01"
    b"\x0e\x01\x03\x01\x01\x00\x00\x00"
    b"\x03\x02\x01\x02"
)
PMT_PID = 0x100
VIDEO_PID = 0x101
KLV_PID = 0x102

WAIT_S = 10.0
# The parked cases' send timeout: long enough that a check made right after
# the call was seen parked still finds it parked, short enough that filling
# the send buffer (one send must time out first) stays quick.
SEND_TIMEOUT_MS = 3000


def _video_program() -> object:
    return MuxerProgramConfigBuilder(1, PMT_PID).add_video(VIDEO_PID, VideoCodec.H264).build()


def _video_klv_program() -> object:
    return (
        MuxerProgramConfigBuilder(1, PMT_PID)
        .add_video(VIDEO_PID, VideoCodec.H264)
        .add_klv(KLV_PID, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )


def _send_frames(tx: object, first_pts: int, count: int) -> None:
    for i in range(count):
        tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(first_pts + i * 3000), key_frame=i == 0)


def _pmt_versions(ts: bytes) -> list[int]:
    """The version of each PMT section on `PMT_PID` that starts in a packet,
    with repeats of the same version dropped."""
    out: list[int] = []
    for off in range(0, len(ts) - 187, 188):
        pkt = ts[off : off + 188]
        pid = ((pkt[1] & 0x1F) << 8) | pkt[2]
        if pkt[0] != 0x47 or pid != PMT_PID or not pkt[1] & 0x40:
            continue
        at = 4
        if pkt[3] & 0x20:  # adaptation field present
            at += 1 + pkt[4]
        at += 1 + pkt[at]  # pointer field
        assert pkt[at] == 0x02, "a PMT section starts here"
        version = (pkt[at + 5] >> 1) & 0x1F
        if not out or out[-1] != version:
            out.append(version)
    return out


def _udp_port_bound(port: int) -> bool:
    """Whether something has bound UDP `port` on loopback (a listener is
    ready for a caller)."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        try:
            s.bind(("127.0.0.1", port))
        except OSError:
            return True
    return False


class _SrtPeer:
    """A listener-mode `shells.Receiver` on its own thread. With `read`, it
    collects every packet until its receive raises; without, it never
    reads after the accept."""

    def __init__(self, port: int, *, read: bool, knobs: str = "") -> None:
        self.packets: list[bytes] = []
        self.rx: Optional[shells.Receiver] = None
        self.error: Optional[BaseException] = None
        self.accepted = threading.Event()
        self.ended = threading.Event()
        self.read = read
        self.url = f"srt://:{port}?mode=listener{knobs}"
        threading.Thread(target=self._serve, daemon=True).start()
        wait_for("the srt listener bound", lambda: _udp_port_bound(port), WAIT_S)

    def _serve(self) -> None:
        try:
            self.rx = shells.Receiver.open(self.url)
            self.accepted.set()
            if self.read:
                while True:
                    self.packets.append(self.rx.recv_bytes())
        except BaseException as exc:  # noqa: BLE001
            self.error = exc
        finally:
            self.accepted.set()
            self.ended.set()

    def received(self, tx: object) -> bytes:
        """What the peer read once it holds every byte `tx` has sent. An
        SRT receiver delivers a packet only at its playout time, so the
        sender is not closed before then: a close that comes first drops
        what is still waiting at the receiver."""
        sent = tx.stats()[1].ts_bytes_emitted
        wait_for("the peer reading every byte sent", lambda: len(self.packets) * 188 >= sent, WAIT_S)
        assert self.error is None, self.error
        return b"".join(self.packets)

    def close(self) -> None:
        if self.rx is not None:
            self.rx.close()


def _check_survivor_and_pmt_version(ts: bytes) -> None:
    events = _events(ts)
    jumps = [e for e in events if isinstance(e, DemuxEvent.Discontinuity) and e.stream.pid == VIDEO_PID]
    assert jumps == []
    videos = [e for e in events if isinstance(e, DemuxEvent.Video) and e.stream.pid == VIDEO_PID]
    assert len(videos) == 10
    assert all(e.raw.endswith(NAL_IDR[5:]) for e in videos)
    assert _pmt_versions(ts) == [0, 1]
    maps = [e for e in events if isinstance(e, DemuxEvent.ProgramMap)]
    assert [s.pid for s in maps[-1].programs[0].streams] == [VIDEO_PID, KLV_PID]


def _check_added_klv(tx: object, finish: Callable[[], bytes]) -> None:
    assert tx.klv_handle() is None
    _send_frames(tx, 90_000, 2)
    tx.reconfigure(_video_klv_program())
    k = tx.klv_handle()
    assert k is not None
    tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(99_000), key_frame=True)
    tx.send_klv(KLV_LS, pts=Pts90khz.from_raw(99_000))
    tx.send_klv_to(k, KLV_LS, pts=Pts90khz.from_raw(102_000))
    tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(105_000))
    metadata = [
        e for e in _events(finish()) if isinstance(e, DemuxEvent.Metadata) and e.stream.pid == KLV_PID
    ]
    assert [e.payload for e in metadata] == [KLV_LS, KLV_LS]


# --------------------------------------------------------------------------- #
# shells.MuxSender over tcp://                                                #
# --------------------------------------------------------------------------- #


def test_reconfigure_keeps_the_survivor_without_a_discontinuity_and_moves_the_pmt_version() -> None:
    peer = TcpReader(WAIT_S)
    try:
        tx = shells.MuxSender.open(peer.url, _video_program())
        v = tx.video_handle()
        _send_frames(tx, 90_000, 5)
        tx.reconfigure(_video_klv_program())
        assert tx.video_handle() == v
        _send_frames(tx, 90_000 + 5 * 3000, 5)
        tx.finish()
        tx.close()
        _check_survivor_and_pmt_version(peer.to_end())
    finally:
        peer.close()


def test_reconfigure_adds_a_klv_stream_and_klv_handle_reads_it() -> None:
    peer = TcpReader(WAIT_S)
    try:
        tx = shells.MuxSender.open(peer.url, _video_program())

        def finish() -> bytes:
            tx.finish()
            tx.close()
            return peer.to_end()

        _check_added_klv(tx, finish)
    finally:
        peer.close()


def test_a_removed_streams_kept_handle_is_refused() -> None:
    peer = TcpReader(WAIT_S)
    try:
        tx = shells.MuxSender.open(peer.url, _video_klv_program())
        k = tx.klv_handle()
        assert k is not None
        tx.reconfigure(_video_program())
        assert tx.klv_handle() is None
        with pytest.raises(MuxError) as ei:
            tx.send_klv_to(k, KLV_LS, pts=Pts90khz.from_raw(90_000))
        assert ei.value.kind == MuxErrorKind.INVALID_USAGE
        # The survivor still sends.
        tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(90_000), key_frame=True)
        tx.close()
    finally:
        peer.close()


def test_a_refused_config_changes_nothing() -> None:
    peer = TcpReader(WAIT_S)
    try:
        tx = shells.MuxSender.open(peer.url, _video_klv_program())
        before = _handles(tx)
        _send_frames(tx, 90_000, 2)
        # The same PID now carries another codec: valid on its own, refused
        # by reconfigure.
        refused = (
            MuxerProgramConfigBuilder(1, PMT_PID)
            .add_video(VIDEO_PID, VideoCodec.H265)
            .add_klv(KLV_PID, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
            .build()
        )
        with pytest.raises(MuxError) as ei:
            tx.reconfigure(refused)
        assert ei.value.kind == MuxErrorKind.CONFIG_INVALID
        assert _handles(tx) == before
        _send_frames(tx, 96_000, 2)
        tx.send_klv(KLV_LS, pts=Pts90khz.from_raw(96_000))
        tx.finish()
        tx.close()
        ts = peer.to_end()
        assert _pmt_versions(ts) == [0]
        events = _events(ts)
        assert len([e for e in events if isinstance(e, DemuxEvent.Video)]) == 4
        assert [e.payload for e in events if isinstance(e, DemuxEvent.Metadata)] == [KLV_LS]
    finally:
        peer.close()


def test_reconfigure_after_close_raises_closed() -> None:
    peer = TcpReader(WAIT_S)
    try:
        tx = shells.MuxSender.open(peer.url, _video_program())
        tx.close()
        with pytest.raises(TcpError) as ei:
            tx.reconfigure(_video_klv_program())
        assert ei.value.kind == TcpErrorKind.CLOSED
    finally:
        peer.close()
    # A cancelled sender refuses it the same way.
    peer = TcpReader(WAIT_S)
    try:
        tx = shells.MuxSender.open(peer.url, _video_program())
        tx.cancel_handle().cancel()
        with pytest.raises(TcpError) as ei:
            tx.reconfigure(_video_klv_program())
        assert ei.value.kind == TcpErrorKind.CLOSED
        assert tx.klv_handle() is None
        tx.close()
    finally:
        peer.close()


# --------------------------------------------------------------------------- #
# A reconfigure parked in the transport                                       #
# --------------------------------------------------------------------------- #


def _srt_sender_with_pending_output() -> tuple[shells.MuxSender, _SrtPeer]:
    """An `srt://` `MuxSender` to a peer that never reads, opened with a send
    timeout, after a send that timed out: its output is retained, so the
    next call on the sender writes it first and waits in the transport."""
    port = free_udp_port()
    peer = _SrtPeer(port, read=False, knobs="&tlpktdrop=0")
    tx = shells.MuxSender.open(
        f"srt://127.0.0.1:{port}?mode=caller&x-sendtimeout={SEND_TIMEOUT_MS}",
        _video_program(),
    )
    assert peer.accepted.wait(WAIT_S), "the srt listener did not accept"
    deadline = time.monotonic() + 60.0
    i = 0
    while True:
        assert time.monotonic() < deadline, "no send timed out against a peer that never reads"
        try:
            tx.send_video(NAL_IDR * 32, pts=Pts90khz.from_raw(i * 3000), key_frame=i == 0)
        except SrtError as exc:
            assert exc.kind in (SrtErrorKind.BACKPRESSURE, SrtErrorKind.TIMEOUT), exc
            return tx, peer
        i += 1


class _Call:
    """`call` on a daemon thread; `done` is set when it returned or raised."""

    def __init__(self, call: Callable[[], object]) -> None:
        self.exc: Optional[BaseException] = None
        self.done = threading.Event()

        def run() -> None:
            try:
                call()
            except BaseException as exc:  # noqa: BLE001
                self.exc = exc
            finally:
                self.done.set()

        threading.Thread(target=run, daemon=True).start()


def test_handle_getters_answer_while_a_reconfigure_is_parked() -> None:
    tx, peer = _srt_sender_with_pending_output()
    try:
        before = _handles(tx)
        assert before[0] is not None
        # Each attempt parks for up to the send timeout; a machine slow
        # enough to let one end before it was read is given another.
        for _ in range(5):
            call = _Call(lambda: tx.reconfigure(_video_klv_program()))
            wait_for("the reconfigure holding the sender", lambda: slot_held(tx) or call.done.is_set(), WAIT_S)
            blocked, got = call_with_hang_deadline(lambda: _handles(tx), 2.0)
            parked_throughout = slot_held(tx) and not call.done.is_set()
            assert not blocked, "the handle getters waited behind the parked reconfigure"
            assert "exc" not in got, got
            assert call.done.wait(WAIT_S), "the reconfigure did not end at its send timeout"
            if parked_throughout:
                break
        else:
            pytest.fail("the reconfigure was never seen parked across the getters' read")
        # Parked in the write of the earlier output, the reconfigure had not
        # taken effect: the getters read the handles of the running config.
        assert got["value"] == before
        assert isinstance(call.exc, SrtError), call.exc
    finally:
        tx.close()
        peer.close()


def test_close_ends_a_parked_reconfigure() -> None:
    tx, peer = _srt_sender_with_pending_output()
    try:
        call = _Call(lambda: tx.reconfigure(_video_klv_program()))
        wait_for("the reconfigure holding the sender", lambda: slot_held(tx), WAIT_S)
        assert not call.done.is_set()
        closed_at = time.monotonic()
        tx.close()
        assert call.done.wait(WAIT_S), "the parked reconfigure did not end"
        # Ended by the close's cancel, well inside the send timeout.
        assert time.monotonic() - closed_at < SEND_TIMEOUT_MS / 1000.0
        assert isinstance(call.exc, SrtError), call.exc
        assert call.exc.kind == SrtErrorKind.CLOSED
        assert _handles(tx) == (None,) * 5
    finally:
        tx.close()
        peer.close()


# --------------------------------------------------------------------------- #
# srt.ManagedMuxSender                                                        #
# --------------------------------------------------------------------------- #


def _managed_pair(program: object):
    from tstrans.srt import ManagedMuxSender

    port = free_udp_port()
    peer = _SrtPeer(port, read=True)
    tx = ManagedMuxSender.from_url(f"srt://127.0.0.1:{port}?mode=caller", program)
    assert peer.accepted.wait(WAIT_S), "the srt listener did not accept"
    return tx, peer


def test_managed_reconfigure_keeps_the_survivor_without_a_discontinuity_and_moves_the_pmt_version() -> None:
    tx, peer = _managed_pair(_video_program())
    try:
        v = tx.video_handle()
        _send_frames(tx, 90_000, 5)
        tx.reconfigure(_video_klv_program())
        assert tx.video_handle() == v
        _send_frames(tx, 90_000 + 5 * 3000, 5)
        _check_survivor_and_pmt_version(peer.received(tx))
    finally:
        tx.close()
        peer.close()


def test_managed_reconfigure_adds_a_klv_stream_and_klv_handle_reads_it() -> None:
    tx, peer = _managed_pair(_video_program())
    try:
        _check_added_klv(tx, lambda: peer.received(tx))
    finally:
        tx.close()
        peer.close()
