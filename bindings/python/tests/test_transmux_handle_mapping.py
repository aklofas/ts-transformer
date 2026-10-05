"""_build_handles completeness guards in Transmuxer.

Covers:
- Normal transmux (no drop=) copies all streams: regression baseline.
- drop=[kind] skips that kind cleanly; retained kinds still mapped and copied.
- Per-kind length mismatch → _build_handles raises RuntimeError naming kind +
  counts (defensive guard; the latent path is unreachable via the normal API).
- Retained PID absent from _handles and not in _dropped_pids → _handle_for /
  write raises RuntimeError instead of silently skipping (defensive guard).

The two "guard" tests (length mismatch + retained-PID-without-mapping) exercise
a latent code path not reachable through the normal API — both sides walk the
same ProgramMap order today, so zip() never truncates.  They use minimal
monkeypatching to manufacture the divergence.
"""
from __future__ import annotations

import dataclasses
import types
from pathlib import Path
from typing import List
from unittest.mock import MagicMock

import pytest

import tstrans.io as tio
from tstrans.mpegts import (
    AudioCodec,
    DemuxEvent,
    KlvStreamType,
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    StreamKindTag,
    VideoCodec,
)

# ---------------------------------------------------------------------------
# Shared fixture builders
# ---------------------------------------------------------------------------

_ORIG_AUS = [
    b"\x00\x00\x00\x01\x65\x88\x84\x00\x10\xaa\xbb",
    b"\x00\x00\x00\x01\x41\x9a\x00\x34\xcc",
    b"\x00\x00\x00\x01\x41\x9a\x01\x35\xdd\xee",
]
_KEY_FRAMES = [True, False, False]
_PTS0 = 900_000
_PTS_STEP = 3_000

# Smallest valid parseable MPEG-2 AAC-LC 44100 Hz stereo ADTS frame.
_ADTS = bytes.fromhex("fff95080021ffc000000000000000000")


def _write_video_audio_src(path: Path) -> None:
    """Synthetic single-program H.264 + AAC source."""
    cfg = (
        MuxerConfigBuilder()
        .add_program(
            MuxerProgramConfigBuilder(1, 0x100)
            .add_video(0x101, VideoCodec.H264)
            .add_audio(0x103, AudioCodec.AAC)
            .build()
        )
        .build()
    )
    mux = Muxer(cfg)
    with mux.write_file(path) as proxy:
        for i, (au, key) in enumerate(zip(_ORIG_AUS, _KEY_FRAMES)):
            pts = Pts90khz.from_raw(_PTS0 + i * _PTS_STEP)
            proxy.push_video(au, pts=pts, key_frame=key)
            proxy.push_audio(_ADTS, pts=pts)


def _write_video_klv_src(path: Path) -> None:
    """Synthetic single-program H.264 + sync-KLV source."""
    from tstrans.klv import ST_0601_UL, UasDatalinkLs, encode_uas_datalink

    rec = UasDatalinkLs(
        universal_label=ST_0601_UL,
        declared_version=19,
        timestamp_us=1_700_000_000_000_000,
        frame_center_lat_deg=47.6097,
        frame_center_lon_deg=-122.3321,
        sensor_lat_deg=47.6200,
        sensor_lon_deg=-122.3000,
        sensor_alt_m=500.0,
    )
    klv_bytes = encode_uas_datalink(rec)
    cfg = (
        MuxerConfigBuilder()
        .add_program(
            MuxerProgramConfigBuilder(1, 0x100)
            .add_video(0x101, VideoCodec.H264)
            .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
            .build()
        )
        .build()
    )
    mux = Muxer(cfg)
    with mux.write_file(path) as proxy:
        for i, (au, key) in enumerate(zip(_ORIG_AUS, _KEY_FRAMES)):
            pts = Pts90khz.from_raw(_PTS0 + i * _PTS_STEP)
            proxy.push_video(au, pts=pts, key_frame=key)
            proxy.push_klv(klv_bytes, pts=pts)


def _collect_video(path: Path) -> list[bytes]:
    return [bytes(ev.raw) for ev in tio.parse_file(path) if isinstance(ev, DemuxEvent.Video)]


def _collect_audio(path: Path) -> list[bytes]:
    return [bytes(ev.raw) for ev in tio.parse_file(path) if isinstance(ev, DemuxEvent.Audio)]


# ---------------------------------------------------------------------------
# Regression: normal transmux copies all streams (no drop=)
# ---------------------------------------------------------------------------

def test_normal_transmux_copies_all_streams(tmp_path: Path) -> None:
    """Normal transmux with no drop= must map every stream and copy it
    byte-faithfully.  This is the regression baseline: if _build_handles
    ever truncates via zip() the output stream count would drop silently."""
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_audio_src(src)

    with tio.transmux(src, dst) as tx:
        for ev in tx:
            tx.write(ev)

    out_videos = _collect_video(dst)
    out_audio = _collect_audio(dst)
    assert out_videos == _ORIG_AUS, "all video AUs must survive transmux byte-faithfully"
    assert len(out_audio) == len(_ORIG_AUS), "all audio PES must survive transmux"


# ---------------------------------------------------------------------------
# drop= skips the dropped kind cleanly; retained kinds still copied
# ---------------------------------------------------------------------------

def test_drop_kind_skips_cleanly_retained_kinds_still_copied(tmp_path: Path) -> None:
    """drop=[KLV_SYNC] must skip KLV events without raising; the retained
    video stream must still be mapped and copied byte-faithfully."""
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_klv_src(src)

    with tio.transmux(src, dst, drop=(StreamKindTag.KLV_SYNC,)) as tx:
        for ev in tx:
            # KLV events for the dropped stream must not raise — they hit the
            # _dropped_pids path in _handle_for, which returns None cleanly.
            tx.write(ev)

    out_videos = _collect_video(dst)
    assert out_videos == _ORIG_AUS, "retained video AUs must survive transmux"
    # KLV absent from output PMT
    out_pm = tio.probe(dst).programs[0]
    assert not tio.probe(dst).has_klv, "dropped KLV stream must not appear in output"
    assert all(
        s.kind not in (StreamKindTag.KLV_SYNC, StreamKindTag.KLV_ASYNC)
        for s in out_pm.streams
    ), "output PMT must not carry a KLV stream when KLV_SYNC was dropped"


def test_drop_audio_skips_cleanly_video_still_copied(tmp_path: Path) -> None:
    """drop=[AUDIO] must skip audio events; video survives byte-faithfully."""
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_audio_src(src)

    with tio.transmux(src, dst, drop=(StreamKindTag.AUDIO,)) as tx:
        for ev in tx:
            tx.write(ev)

    out_videos = _collect_video(dst)
    assert out_videos == _ORIG_AUS, "retained video AUs must survive"
    out_audio = _collect_audio(dst)
    assert out_audio == [], "dropped audio must not appear in output"


# ---------------------------------------------------------------------------
# Guard: per-kind length mismatch → _build_handles raises RuntimeError
# (defensive/latent guard — not reachable through the normal API)
# ---------------------------------------------------------------------------

def test_build_handles_raises_on_per_kind_length_mismatch(tmp_path: Path) -> None:
    """If video_handles() returns fewer handles than there are video PIDs,
    _build_handles must raise RuntimeError naming the kind and both counts,
    rather than silently dropping the unmatched PID via zip() truncation.

    DEFENSIVE / LATENT GUARD: this path is unreachable today because
    from_program_map + the muxer always produce the same per-kind count.
    A minimal monkeypatch manufactures the divergence to verify the guard.
    """
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_klv_src(src)

    # Open a real transmuxer and let it process enough events to build
    # the muxer (first ProgramMap), then trigger _build_handles via
    # _on_program_map with a patched muxer whose video_handles() is short.
    with tio.transmux(src, dst) as tx:
        # Consume the first event so the generator is live.
        for ev in tx:
            if isinstance(ev, DemuxEvent.ProgramMap):
                # Reset state so _on_program_map will re-run _build_handles.
                real_pm = ev.programs[0]

                # Build the real muxer (so we can introspect handle counts).
                from tstrans.mpegts import MuxerConfig
                real_config = MuxerConfig.from_program_map(real_pm)
                real_muxer = Muxer(real_config)

                # Stub: video_handles() returns an empty list (fewer than 1).
                # The stub inherits all other methods from the real muxer.
                class StubMuxerShortVideo:
                    def video_handles(self) -> list:
                        return []  # shorter than the 1 video PID in the PMT

                    def audio_handles(self):
                        return real_muxer.audio_handles()

                    def klv_handles(self):
                        return real_muxer.klv_handles()

                    def subtitle_handles(self):
                        return real_muxer.subtitle_handles()

                    def data_handles(self):
                        return real_muxer.data_handles()

                with pytest.raises(RuntimeError, match="video"):
                    tx._build_handles(real_pm, StubMuxerShortVideo())
                break  # done — don't continue iterating


# ---------------------------------------------------------------------------
# Guard: retained PID without mapping → _handle_for raises instead of skipping
# (defensive/latent guard — not reachable through the normal API)
# ---------------------------------------------------------------------------

def test_handle_for_raises_on_retained_pid_without_mapping(tmp_path: Path) -> None:
    """If a retained PID has no entry in _handles and is not in _dropped_pids,
    _handle_for must raise RuntimeError rather than silently returning None and
    causing write() to drop the stream undetected.

    DEFENSIVE / LATENT GUARD: this path is unreachable today because
    _build_handles always maps every retained PID. A targeted unit test
    manufactures the gap by mutating the Transmuxer's internal state.
    """
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_klv_src(src)

    with tio.transmux(src, dst) as tx:
        video_ev = None
        for ev in tx:
            if isinstance(ev, DemuxEvent.Video) and video_ev is None:
                video_ev = ev
                break  # stop after the first Video event

        assert video_ev is not None, "need at least one Video event"
        vid_pid = video_ev.stream.pid

        # Confirm the PID is currently mapped (pre-condition).
        assert vid_pid in tx._handles, "video PID must be in _handles before the test"

        # Manufacture the gap: remove the video PID from _handles while keeping
        # it out of _dropped_pids (it was never intentionally dropped).
        del tx._handles[vid_pid]
        # _dropped_pids must exist (post-implementation); but the PID must NOT
        # be in it (it was retained, not dropped).
        assert vid_pid not in tx._dropped_pids, (
            "video PID must not be in _dropped_pids (it was retained)"
        )

        with pytest.raises(RuntimeError, match="retained PID"):
            tx._handle_for(video_ev)
