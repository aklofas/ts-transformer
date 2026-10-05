"""MuxerFileSink writes via memoryview slice (no per-chunk bytes copy).

This is mostly a regression test confirming that the `view[:n]` slices
`_drain_muxer_to_file` writes (instead of `bytes(buf[:n])` copies) don't
break any sink that accepts only one of the two buffer-protocol shapes.

`io.BufferedWriter` (the real `open(path, "wb")` handle) accepts both
`bytes` and `memoryview` transparently, so the production path is
unaffected. We exercise it here through a buffer-protocol-only fake
file to make the shape difference explicit and to lock the contract.
"""

import tempfile
from pathlib import Path

import pytest

from tstrans.mpegts import (
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)
import tstrans.mpegts as mpegts_mod


def _cfg():
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .build()
    )
    return MuxerConfigBuilder().add_program(prog).build()


def _nal_aud() -> bytes:
    # Minimal H.264 NAL: 4-byte start code + AUD (type 9) + trailing byte.
    return b"\x00\x00\x00\x01\x09\xF0"


class _BufferOnlyFile:
    """Fake file accepting only buffer-protocol writes (memoryview/bytes/
    bytearray). Rejects str. Records each chunk's type to confirm the
    drain hands over memoryview slices, not freshly-allocated bytes."""

    def __init__(self) -> None:
        self.chunks: list[bytes] = []
        self.chunk_types: list[type] = []

    def write(self, data) -> int:
        # `memoryview`, `bytes`, `bytearray` all support `bytes(data)`.
        # `str` would explode here — that's the point of the test.
        self.chunk_types.append(type(data))
        snapshot = bytes(data)
        self.chunks.append(snapshot)
        return len(snapshot)


def test_drain_uses_memoryview_slices() -> None:
    """The drain hands `memoryview` slices to `fh.write`, not
    freshly-allocated `bytes`. Verify directly by intercepting writes."""
    m = Muxer(_cfg())
    # Push enough video to force at least one drain chunk.
    for i in range(5):
        m.push_video(_nal_aud(), pts=Pts90khz.from_raw(900_000 + i * 3000))
    assert m.pending_packets() > 0

    fake = _BufferOnlyFile()
    mpegts_mod._drain_muxer_to_file(m, fake)

    assert m.pending_packets() == 0
    assert len(fake.chunks) > 0
    # Every chunk that the drain wrote must be a memoryview, not bytes.
    assert all(t is memoryview for t in fake.chunk_types), (
        f"_drain_muxer_to_file should write memoryview slices; "
        f"got chunk types {fake.chunk_types}"
    )
    # And every chunk must be a non-empty multiple of 188 (TS packet size).
    for chunk in fake.chunks:
        assert len(chunk) > 0
        assert len(chunk) % 188 == 0


def test_drain_chunk_size_is_seven_packets() -> None:
    """_DRAIN_CHUNK_PACKETS is 7, aligned with the 1316-byte SRT payload
    size. Locked here so a future change is
    a deliberate decision."""
    assert mpegts_mod._DRAIN_CHUNK_PACKETS == 7
    assert mpegts_mod._DRAIN_CHUNK_BYTES == 7 * 188
    assert mpegts_mod._DRAIN_CHUNK_BYTES == 1316


def test_write_file_still_works_end_to_end() -> None:
    """Regression: the production path (real file handle) still produces
    a valid `.ts` file after the memoryview rewrite. Mirrors
    `test_mpegts_muxer_sink.test_write_file_creates_non_empty_file`."""
    m = Muxer(_cfg())
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "out.ts"
        with m.write_file(path) as proxy:
            for i in range(5):
                proxy.push_video(
                    _nal_aud(), pts=Pts90khz.from_raw(900_000 + i * 3000)
                )
        assert path.stat().st_size > 0
        assert path.stat().st_size % 188 == 0


def test_buffer_only_file_rejects_str_baseline() -> None:
    """Sanity-check our fake: it actually rejects `str` writes so the
    main test above is meaningful (i.e. it would have failed if the
    drain still called `bytes(buf[:n])`-then-`str(...)` or similar)."""
    fake = _BufferOnlyFile()
    with pytest.raises(TypeError):
        fake.write("not bytes")  # type: ignore[arg-type]
