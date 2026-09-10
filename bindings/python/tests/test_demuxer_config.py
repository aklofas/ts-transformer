"""DemuxerConfig — Phase 2 minimal config (strict mode + PES caps).
Advanced knobs (link_klv, treat_as, av1_carriage) deferred."""

import pytest
from tstrans.mpegts import DemuxerConfig, StrictMode
import tstrans


def test_default_construction():
    cfg = DemuxerConfig()
    assert cfg.strict_mode is StrictMode.OFF
    # Defaults track the Rust DemuxerConfig defaults (current values:
    # 4 MB per-PID, 64 MB total — verified in
    # tst-core/src/mpegts/demux/types.rs DEFAULT_PES_CAP_* constants).
    assert cfg.pes_cap_per_pid == 4 * 1024 * 1024
    assert cfg.pes_cap_total == 64 * 1024 * 1024


def test_overrides_via_kwargs():
    cfg = DemuxerConfig(
        strict_mode=StrictMode.FULL,
        pes_cap_per_pid=1024,
        pes_cap_total=8192,
    )
    assert cfg.strict_mode is StrictMode.FULL
    assert cfg.pes_cap_per_pid == 1024
    assert cfg.pes_cap_total == 8192


def test_immutable():
    import dataclasses
    cfg = DemuxerConfig()
    try:
        cfg.strict_mode = StrictMode.FULL
    except dataclasses.FrozenInstanceError:
        pass
    else:
        raise AssertionError("DemuxerConfig should be frozen")


def test_sync_buf_cap_permits_whole_file_feed():
    # 5 MiB of valid TS in one feed: default config raises DemuxError naming
    # the knob; raised cap accepts it.
    pkt = b"\x47\x1f\xff\x10" + b"\xff" * 184
    data = pkt * ((5 * 1024 * 1024) // 188 + 1)
    d = tstrans.mpegts.Demuxer()
    with pytest.raises(tstrans.exceptions.DemuxError, match="sync_buf_cap"):
        d.feed(data)
    cfg = tstrans.mpegts.DemuxerConfig(sync_buf_cap=16 * 1024 * 1024)
    d2 = tstrans.mpegts.Demuxer(cfg)
    d2.feed(data)  # must not raise


def test_unwrap_timestamps_default_false():
    cfg = DemuxerConfig()
    assert cfg.unwrap_timestamps is False


def test_unwrap_timestamps_accepted():
    cfg = DemuxerConfig(unwrap_timestamps=True)
    assert cfg.unwrap_timestamps is True


def test_unwrap_timestamps_non_bool_rejected():
    with pytest.raises(TypeError, match="unwrap_timestamps"):
        DemuxerConfig(unwrap_timestamps=1)
