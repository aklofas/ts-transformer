"""Minimal `MuxerProgramConfig` builders shared by mux/demux tests.

These build just enough program shape to exercise a single stream kind;
shapes used by more than one test file live here.
"""

from __future__ import annotations

from tstrans.mpegts import (
    AudioCodec,
    KlvStreamType,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    VideoCodec,
)


def video_only_program(pid_video: int = 0x101) -> object:
    """Minimal single-video-stream `MuxerProgramConfig` (program 1, PMT
    PID 0x100)."""
    return (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(pid_video, VideoCodec.H264)
        .build()
    )


def video_klv_program() -> object:
    """Video PID 0x101 + synchronous KLV PID 0x102 (program 1, PMT 0x100)."""
    return (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )


def video_data_program() -> object:
    """Video + one private data stream (bare PES-private stream_type 0x06)."""
    return (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_data(0x1F0, 0x06, carries_pts=True)
        .build()
    )


def video_private_data_program() -> object:
    """Video + one private data stream (user-private stream_type 0xF0)."""
    return (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_data(0x1F0, 0xF0, carries_pts=True)
        .build()
    )


def video_only_config() -> object:
    """`MuxerConfig` holding `video_only_program()`."""
    return MuxerConfigBuilder().add_program(video_only_program()).build()


def video_audio_klv_config() -> object:
    """`MuxerConfig`: video 0x101 + AAC 0x102 + synchronous KLV 0x103."""
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_audio(0x102, AudioCodec.AAC)
        .add_klv(0x103, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )
    return MuxerConfigBuilder().add_program(prog).build()
