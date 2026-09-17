"""Audit #11 — verify heavy Rust work releases the GIL.

These tests verify that the hot-path PyO3 methods (`Demuxer.feed`,
`Muxer.push_*`, codec eager-collect iterators) wrap their Rust work
in `py.allow_threads` so other Python threads can run concurrently.

KLV decode entry points are intentionally NOT GIL-released — the
typical record size keeps per-call Rust work below the GIL transition
breakeven point (~50us), and wrapping produces lock-contention
pathology under hot batch loops. See `klv.rs` decision comments and
`reference_pyo3_allow_threads_pattern.md` for the empirical analysis.

## Technique — a structural probe, not a throughput ratio

Each test runs its workload through `_GilProbe.call`, which brackets
every Rust call with `perf_counter()` stamps `(t0, t1)`, while ONE
background "probe" thread waits to execute a single line of Python
bytecode and records WHEN it managed to. The assertion is simply:

    the probe's stamp lies strictly inside one of the call windows.

Why that is a proof and not a measurement:

* Executing bytecode needs the GIL. While the main thread is inside a
  C call that did NOT release the GIL, no other thread can execute a
  single bytecode until the call returns — the interpreter's
  switch-interval check never runs inside a C call.
* The GIL can still change hands *between* bytecodes on the main
  thread (the stamps and the call itself are a handful of bytecodes
  apart), but only when a waiting thread has requested it, and a
  waiting thread only requests it after `sys.getswitchinterval()`
  seconds of waiting. The probe context sets the switch interval to
  `_PINNED_SWITCH_INTERVAL_S` (one hour) for the duration of the
  workload, so that request can never fire. From then on the ONLY way
  the main thread gives up the GIL is voluntarily — i.e. exactly the
  `allow_threads` calls under test (nothing else in the window blocks,
  sleeps or does I/O).
* Therefore, without `allow_threads`, the probe cannot run before the
  main thread finishes the whole workload and blocks in `join()` —
  its stamp lands after the last `t1` and the test fails
  deterministically. With `allow_threads`, the GIL is free for the
  whole duration of every Rust call and the probe's stamp lands inside
  the first call during which the OS scheduled it.

There is no throughput ratio, no "≥ N iterations" and no wall-clock
duration assertion (beyond the setup-sanity guard below). Host load
cannot produce a false failure unless the OS starves the probe thread
for the ENTIRE combined duration of the workload's Rust calls, which is
why every workload is sized to spend well over `_MIN_WORKLOAD_MS`
inside Rust — the longer the window, the more robust the proof. The
earlier ratio form (probe iterations during the workload / solo
iterations ≥ 60 %) flaked twice in CI under host load
(`push_video_to_with_dts` on 2026-09-14, the AAC iterator on
2026-09-15) because a starved probe thread simply iterates less; the
structural form only asks whether it ran at all.

The probe stamps exactly once and then blocks on an `Event` (which
releases the GIL) so that it never competes with the main thread for
the GIL after the call returns; with the switch interval pinned, a
spinning probe would otherwise stall the main thread's re-acquire for
the whole interval.

## Workload sizing

Each workload is sized to spend ≥ `_MIN_WORKLOAD_MS` wall-clock inside
Rust calls. A much shorter workload would still be a valid proof, but
it leaves the OS less time to schedule the probe thread on a loaded
host — the test asserts its own setup is broken in that case (pointing
at the input-size constant to scale up).
"""

from __future__ import annotations

import sys
import threading
from time import perf_counter
from typing import Any, Callable, TypeVar

import pytest

from tstrans.codec import iter_aac_frames_with_resync, iter_mpeg2_audio_frames_with_resync
from tstrans.mpegts import (
    Demuxer,
    Muxer,
    MuxerConfig,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
    WebVttInTsConfig,
)

_R = TypeVar("_R")


# ---------------------------------------------------------------------------
# Structural GIL-release probe
# ---------------------------------------------------------------------------


# Combined time the workload must spend inside Rust calls. Not a
# discriminator (any window length would do for the proof) — a
# scheduling-latency cushion: the OS has to run the probe thread at least
# once while the GIL is free, and a loaded CI host can take tens of ms to
# schedule a woken thread.
_MIN_WORKLOAD_MS = 100.0

# Switch interval pinned for the duration of a probed workload. A thread
# waiting for the GIL asks the holder to drop it only after waiting this
# long, so with the interval far longer than any workload the main thread
# can never be made to yield between bytecodes — every GIL hand-off inside
# the window is a voluntary `allow_threads`. Restored on exit.
_PINNED_SWITCH_INTERVAL_S = 3600.0


class _GilProbe:
    """Prove that Rust calls made through `.call()` release the GIL.

    Use as a context manager around the workload; route every Rust call
    under test through `probe.call(fn, *args, **kwargs)`; then call
    `probe.assert_released(op_name)`.
    """

    def __init__(self) -> None:
        self._go = threading.Event()
        self._done = threading.Event()
        self.stamp: float | None = None
        self.windows: list[tuple[float, float]] = []
        self._prev_switch_interval: float = sys.getswitchinterval()
        self._t = threading.Thread(target=self._run, daemon=True)

    def _run(self) -> None:
        # Block (GIL released) until the first probed call is about to be
        # made, then record the first instant this thread executes
        # bytecode — which requires the GIL — and get out of the way.
        self._go.wait()
        self.stamp = perf_counter()
        self._done.wait()

    def __enter__(self) -> _GilProbe:
        self._prev_switch_interval = sys.getswitchinterval()
        sys.setswitchinterval(_PINNED_SWITCH_INTERVAL_S)
        self._t.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self._done.set()
        # Without `allow_threads` this join is the first point at which the
        # probe can run at all — its stamp then lands after every window.
        self._t.join(timeout=10.0)
        sys.setswitchinterval(self._prev_switch_interval)

    def call(self, fn: Callable[..., _R], *args: Any, **kwargs: Any) -> _R:
        """Make one Rust call, recording its `(t0, t1)` window."""
        self._go.set()
        t0 = perf_counter()
        result = fn(*args, **kwargs)
        t1 = perf_counter()
        self.windows.append((t0, t1))
        return result

    @property
    def workload_ms(self) -> float:
        return sum(t1 - t0 for t0, t1 in self.windows) * 1000.0

    def assert_released(self, op_name: str) -> None:
        assert self.windows, f"{op_name}: no calls were routed through the probe"
        assert self.workload_ms >= _MIN_WORKLOAD_MS, (
            f"{op_name}: workload too short ({self.workload_ms:.0f}ms inside Rust "
            f"< {_MIN_WORKLOAD_MS:.0f}ms) — this is a TEST SETUP problem, "
            f"scale up the input or iteration count so the probe thread has a "
            f"comfortable window to be scheduled in"
        )
        assert self.stamp is not None, (
            f"{op_name}: probe thread never ran (join timed out) — test harness bug"
        )
        if any(t0 < self.stamp < t1 for t0, t1 in self.windows):
            return
        first_t0 = self.windows[0][0]
        last_t1 = self.windows[-1][1]
        where = (
            f"{(self.stamp - last_t1) * 1000:.1f}ms AFTER the last call returned"
            if self.stamp >= last_t1
            else f"{(self.stamp - first_t0) * 1000:.1f}ms after the first call "
            f"started, between calls"
        )
        raise AssertionError(
            f"{op_name}: the probe thread first executed Python bytecode {where}, "
            f"never inside any of the {len(self.windows)} call window(s) "
            f"({self.workload_ms:.0f}ms inside Rust in total); the GIL was held "
            f"for the whole of every call — `py.allow_threads` is missing"
        )


# ---------------------------------------------------------------------------
# Muxer fixtures
# ---------------------------------------------------------------------------


def _muxer_config_video_only() -> MuxerConfig:
    """Single-video-stream muxer with large packet buffer."""
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .build()
    )
    return MuxerConfigBuilder().add_program(prog).buffer_packets(1_000_000).build()


def _huge_h264_nal(mb: int = 30) -> bytes:
    """`mb` MB Annex-B NAL — a 30 MB push_video call takes ~50ms.

    Bigger gives the probe a longer window: 5 MB took ~13ms; 30 MB
    lands at ~50ms on fast hardware; 50 MB at ~75ms; 100 MB at ~150ms.
    """
    return b"\x00\x00\x00\x01\x09" + b"\xA0" * (mb * 1024 * 1024)


def _huge_aac_buf() -> bytes:
    """~500 MB of pseudo-ADTS bytes — resync iterator scans for ~180ms.

    The resync scanner walks byte-by-byte; size dominates runtime.
    Below ~200 MB the workload drops under _MIN_WORKLOAD_MS on fast hardware.
    """
    frame = bytes.fromhex("FFF150801FFC") + b"\x00" * (1024 - 6)
    return frame * 500_000


def _huge_mp2_buf() -> bytes:
    """~500 MB of pseudo MPEG-2 audio frames — ~150ms scan."""
    frame = b"\xFF\xFB\x90\x00" + b"\x00" * 1020
    return frame * 500_000


def _build_ts_stream(target_mb: int = 50) -> bytes:
    """Mux a ~target_mb MB TS stream by repeated push + drain."""
    cfg = _muxer_config_video_only()
    m = Muxer(cfg)
    nal = b"\x00\x00\x00\x01\x09" + b"\xA0" * (target_mb * 1024 * 1024)
    m.push_video(nal, pts=Pts90khz.from_raw(900_000))
    drain = bytearray(188 * 100_000)
    chunks: list[bytes] = []
    while True:
        n = m.pull(drain)
        if n == 0:
            break
        chunks.append(bytes(drain[:n]))
    return b"".join(chunks)


# ---------------------------------------------------------------------------
# Muxer.push_video — one huge NAL → single long Rust call
# ---------------------------------------------------------------------------


@pytest.mark.timeout(20)
def test_push_video_releases_gil() -> None:
    """One push_video of a 100 MB NAL must let other Python threads run."""
    m = Muxer(_muxer_config_video_only())
    # 100 MB (~150ms) keeps the single call comfortably above the 100ms
    # _MIN_WORKLOAD_MS cushion even on the fastest CI runners (a 30 MB call
    # clocked at 49.6ms on one).
    nal = _huge_h264_nal(100)
    pts = Pts90khz.from_raw(900_000)

    with _GilProbe() as probe:
        probe.call(m.push_video, nal, pts=pts)

    probe.assert_released("push_video")


def _muxer_config_video_and_subtitle() -> MuxerConfig:
    """Single-program muxer with one video + one WebVTT subtitle stream.

    Builder validation requires the program to have a PCR-eligible stream
    (default is the first video), so the subtitle stream is paired with a
    video stream the muxer treats as PCR carrier. The video is never
    pushed in the subtitle GIL test.
    """
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_subtitle(0x200, WebVttInTsConfig())
        .build()
    )
    return MuxerConfigBuilder().add_program(prog).buffer_packets(1_000_000).build()


def _max_subtitle_payload() -> bytes:
    """Single subtitle payload near the 65 KB PES limit.

    The Rust contract caps `push_subtitle` payload at 65527 bytes (the
    `PES_packet_length` budget). 60 KB stays comfortably under both
    DVB-sub (-3 envelope bytes) and DVB-teletext (-69) ceilings.
    """
    return b"WEBVTT\n\n" + b"x" * 60_000


@pytest.mark.timeout(20)
def test_push_subtitle_releases_gil() -> None:
    """Repeated push_subtitle calls (each near the 65 KB PES limit) must
    let other Python threads run.

    Each call is bounded — the PES_packet_length budget caps the payload
    at ~65 KB — so a single call spends only ~5 us inside Rust. The proof
    does not need a long call (the probe lands inside whichever call the
    OS schedules it during), but the test loops enough calls for the
    combined in-Rust window to clear the _MIN_WORKLOAD_MS cushion.
    """
    m = Muxer(_muxer_config_video_and_subtitle())
    payload = _max_subtitle_payload()

    # ~5-7 us inside Rust per call on a dev box (the muxer only copies the
    # payload into TS packets), so 25 000 calls put the combined window at
    # ~170 ms — over _MIN_WORKLOAD_MS (100 ms) — for ~0.7 s wall-clock,
    # comfortably under the 20 s @pytest.mark.timeout even on slow runners.
    n_calls = 25_000
    # Each call emits ~330 packets; drain every 1000 calls (~330 K packets)
    # so the 1 M-packet buffer never overflows.
    drain_every = 1000

    with _GilProbe() as probe:
        for i in range(n_calls):
            pts = Pts90khz.from_raw(900_000 + i * 90_000)
            probe.call(m.push_subtitle, payload, pts=pts)
            if i % drain_every == drain_every - 1:
                buf = bytearray(m.pending_packets() * 188)
                m.pull(buf)

    probe.assert_released("push_subtitle")


@pytest.mark.timeout(20)
def test_push_video_to_with_dts_releases_gil() -> None:
    """push_video_to_with_dts on a 30 MB NAL must let other threads run.

    Covers the `_to_with_dts` variant which has the most complex
    signature (two PTS args + handle). If this one releases the GIL,
    the structurally simpler `push_video_to` / `push_audio_to` /
    `push_klv_to` variants do too (they use the same wrapper pattern).
    """
    m = Muxer(_muxer_config_video_only())
    # Need a video handle for the _to variant.
    handles = m.video_handles()
    assert len(handles) == 1
    handle = handles[0]
    nal = _huge_h264_nal()

    # Loop 3× so the combined window comfortably exceeds the 100ms
    # _MIN_WORKLOAD_MS cushion on fast hardware — a single
    # push_video_to_with_dts call clocked at ~49ms on a Ryzen 9 7950X3D.
    with _GilProbe() as probe:
        for i in range(3):
            ts = Pts90khz.from_raw(900_000 + i * 90_000)
            probe.call(m.push_video_to_with_dts, handle, nal, pts=ts, dts=ts)

    probe.assert_released("push_video_to_with_dts")


# ---------------------------------------------------------------------------
# Demuxer.feed — repeated feeds of moderate-sized chunks
# ---------------------------------------------------------------------------


@pytest.mark.timeout(30)
def test_demuxer_feed_releases_gil() -> None:
    """Repeated Demuxer.feed calls must let other threads run.

    Each feed call processes 500 KB (well under the 4 MB sync ceiling)
    so the demuxer doesn't error out. Only the `feed` calls are probed —
    `next_event` does not release the GIL (it builds Python objects) and
    so stays outside the windows.
    """
    ts_bytes = _build_ts_stream(target_mb=20)
    assert len(ts_bytes) > 5_000_000, (
        f"test setup: only got {len(ts_bytes)} bytes of TS data"
    )

    chunk_size = 500_000
    n_chunks = (len(ts_bytes) + chunk_size - 1) // chunk_size

    d = Demuxer()

    # The demuxer walks a 20 MB stream in ~3.5 ms of Rust on a dev box
    # (~85 us per 500 KB feed); 60 passes put the combined in-Rust window
    # at ~200 ms, over _MIN_WORKLOAD_MS, for ~0.25 s wall-clock.
    with _GilProbe() as probe:
        for _ in range(60):
            for ci in range(n_chunks):
                start_off = ci * chunk_size
                end_off = min(start_off + chunk_size, len(ts_bytes))
                probe.call(d.feed, ts_bytes[start_off:end_off])
                while d.next_event() is not None:
                    pass

    probe.assert_released("Demuxer.feed")


# ---------------------------------------------------------------------------
# Codec collect-then-iter — one huge eager collect
# ---------------------------------------------------------------------------


@pytest.mark.timeout(20)
def test_iter_aac_frames_with_resync_releases_gil() -> None:
    """iter_aac_frames_with_resync on ~500MB must release the GIL.

    The Rust resync scanner walks the entire buffer byte-by-byte
    looking for sync patterns; on this scale one call takes ~140ms on a
    dev box (a single long Rust call). Two calls clear the
    _MIN_WORKLOAD_MS cushion with margin without doubling the ~1 GB
    peak (buffer + owned frame copies) a larger input would cost.
    """
    buf = _huge_aac_buf()

    with _GilProbe() as probe:
        for _ in range(2):
            probe.call(iter_aac_frames_with_resync, buf)

    probe.assert_released("iter_aac_frames_with_resync")


@pytest.mark.timeout(20)
def test_iter_mpeg2_audio_frames_with_resync_releases_gil() -> None:
    """iter_mpeg2_audio_frames_with_resync on ~500MB must release the GIL.

    Two ~125 ms calls, for the same reason as the AAC twin above.
    """
    buf = _huge_mp2_buf()

    with _GilProbe() as probe:
        for _ in range(2):
            probe.call(iter_mpeg2_audio_frames_with_resync, buf)

    probe.assert_released("iter_mpeg2_audio_frames_with_resync")


# ---------------------------------------------------------------------------
# KLV decode is intentionally NOT GIL-released — see klv.rs decision comments.
# Records are typically 20-200 bytes; per-call Rust work (~5us) is well below
# the GIL-transition breakeven point (~50us). Wrapping the small fast calls
# produced lock-contention pathology under tight batch loops in pre-ship
# benchmarks (30K decodes degraded from 0.5s baseline to 50+ seconds).
# ---------------------------------------------------------------------------
