"""A stream of one or two TS packets, end to end.

`shells.Sender` sends a stream aligned since its first byte on `flush()` and
on `finish()`, whatever its length, and `finish()` reports what it could not
send as TS (`INPUT_MALFORMED`) after sending everything it could. The
receiving shells deliver the whole packets they still hold when the
transport ends, before the end.

The far side of each shell is a plain socket, never one of this library's
receivers or senders: what a plain peer reads or writes is what crossed the
wire. A TCP peer ends on the connection's close, so no timing is involved.
"""

from __future__ import annotations

import socket
import threading

import pytest

from tstrans import shells
from tstrans.exceptions import TcpError, TcpErrorKind
from tstrans.mpegts import DemuxEvent, Muxer, MuxerConfigBuilder, Pts90khz

from _builders.mux_programs import video_only_program
from _builders.tcp_reader import TcpReader
from _builders.wait import wait_for

WAIT_S = 10.0
# Two distinguishable packets: a null-PID packet, then one on PID 0x101.
PACKET_A = bytes([0x47, 0x1F, 0xFF, 0x10]) + bytes(range(184))
PACKET_B = bytes([0x47, 0x01, 0x01, 0x10]) + bytes(reversed(range(184)))


def _serve_once(payload: bytes) -> tuple[socket.socket, int]:
    """A plain TCP listener that writes `payload` to the one connection it
    accepts, then closes it."""
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)
    srv.settimeout(WAIT_S)

    def run() -> None:
        try:
            conn, _ = srv.accept()
        except OSError:
            return
        with conn:
            conn.sendall(payload)

    threading.Thread(target=run, daemon=True).start()
    return srv, srv.getsockname()[1]


def test_finish_sends_a_one_packet_stream() -> None:
    peer = TcpReader(WAIT_S)
    try:
        tx = shells.Sender.open(peer.url)
        tx.send_bytes(PACKET_A)
        assert tx.finish() is None
        assert peer.to_end() == PACKET_A
        tx.close()
    finally:
        peer.close()


def test_flush_sends_a_two_packet_stream() -> None:
    peer = TcpReader(WAIT_S)
    try:
        tx = shells.Sender.open(peer.url)
        tx.send_bytes(PACKET_A + PACKET_B)
        tx.flush()
        # Read while the sender is still open: `close()` cancels first and
        # sends nothing buffered, so these bytes left with the flush.
        wait_for("the peer reading both packets", lambda: len(peer.data) >= 2 * 188, WAIT_S)
        tx.close()
        assert peer.to_end() == PACKET_A + PACKET_B
    finally:
        peer.close()


def test_finish_reports_a_partial_last_packet() -> None:
    peer = TcpReader(WAIT_S)
    try:
        tx = shells.Sender.open(peer.url)
        # One packet, then the start of a second that never completes.
        tx.send_bytes(PACKET_A + PACKET_B[:100])
        with pytest.raises(TcpError) as ei:
            tx.finish()
        assert ei.value.kind == TcpErrorKind.INPUT_MALFORMED, ei.value
        # Everything it could send went out first; the sender is closed.
        assert peer.to_end() == PACKET_A
        assert tx.is_alive() is False
        assert tx.finish() is None
        tx.close()
    finally:
        peer.close()


def _psi_packets() -> bytes:
    """The PAT and the PMT a muxer writes first: a two-packet stream whose
    demux yields one `ProgramMap`."""
    mux = Muxer(MuxerConfigBuilder().add_program(video_only_program()).build())
    mux.push_video(b"\x00\x00\x00\x01\x09\xf0", pts=Pts90khz.from_raw(0), key_frame=True)
    scratch = bytearray(188 * 32)
    out = bytearray()
    while (n := mux.pull(scratch)) > 0:
        out.extend(scratch[:n])
    psi = bytes(out[: 2 * 188])
    pids = [((psi[i + 1] & 0x1F) << 8) | psi[i + 2] for i in (0, 188)]
    assert pids == [0x0000, 0x0100], pids
    return psi


def test_a_two_packet_stream_is_received() -> None:
    stream = _psi_packets()

    srv, port = _serve_once(stream)
    got: list[bytes] = []
    try:
        with shells.Receiver.open(f"tcp://127.0.0.1:{port}") as rx:
            with pytest.raises(TcpError) as ei:
                while True:
                    got.append(rx.recv_bytes())
            assert ei.value.kind == TcpErrorKind.END_OF_STREAM, ei.value
            # The end stays the answer.
            with pytest.raises(TcpError) as ei:
                rx.recv_bytes()
            assert ei.value.kind == TcpErrorKind.END_OF_STREAM, ei.value
    finally:
        srv.close()
    assert b"".join(got) == stream

    srv, port = _serve_once(stream)
    try:
        with shells.DemuxReceiver.open(f"tcp://127.0.0.1:{port}") as drx:
            events = list(drx)
            assert drx.ts_bytes_demuxed() == len(stream)
    finally:
        srv.close()
    maps = [e for e in events if isinstance(e, DemuxEvent.ProgramMap)]
    assert len(maps) == 1, [type(e).__name__ for e in events]
    assert [p.pmt_pid for p in maps[0].programs] == [0x0100]
