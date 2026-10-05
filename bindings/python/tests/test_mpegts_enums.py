"""Enum variants mirror the Rust definitions in
`tst_core::mpegts::demux::event` + `mpegts::demux::strict`."""

import enum

import pytest

from tstrans.mpegts import (
    VideoCodec,
    AudioCodec,
    SubtitleCodec,
    StreamKindTag,
    MetadataKindTag,
    DiscontinuityKindTag,
    NonConformantKind,
    StrictMode,
    LinkSource,
)


@pytest.mark.parametrize("cls,names", [
    (VideoCodec,         ["H264", "H265", "H266", "AV1"]),
    (AudioCodec,         ["MP2", "AAC", "AAC_LATM", "AC3"]),
    (SubtitleCodec,      ["DVB_SUBTITLING", "DVB_TELETEXT", "CEA708_STANDALONE", "WEBVTT_IN_TS"]),
    (StreamKindTag,      ["VIDEO", "AUDIO", "SUBTITLE", "KLV_SYNC", "KLV_ASYNC", "UNKNOWN"]),
    (MetadataKindTag,    ["KLV_SYNC_AU_CELL", "KLV_ASYNC", "UNKNOWN"]),
    (DiscontinuityKindTag, ["CONTINUITY_JUMP", "PES_OVERSIZE", "PES_TOTAL_OVERSIZE", "ADAPTATION_FIELD_FLAG"]),
    (StrictMode,         ["OFF", "TIMING_ONLY", "PSI_ONLY", "FULL"]),
    (LinkSource,         ["DECLARED", "INFERRED", "OVERRIDE"]),
])
def test_enum_variants(cls, names):
    assert issubclass(cls, enum.Enum)
    for name in names:
        assert hasattr(cls, name), f"{cls.__name__} missing variant {name}"


def test_nonconformant_kind_has_minimum_set():
    # NonConformantKind mirrors NonConformantIssue's 30+ variants.
    # The catch-all set ships; per-variant subclasses can grow later.
    required = {
        "PCR_ANOMALY",
        "PSI_CHECKSUM_MISMATCH",
        "MALFORMED_PES",
        "PUSI_MID_PES",
        "TRANSPORT_ERROR_PACKET",
        "STREAM_TYPE_MISMATCH",
        "PID_REUSED_ACROSS_PROGRAMS",
        "NAL_HEADER",
        "AV1_OBU_HEADER",
        "AV1_REGISTRATION_MALFORMED",
        "PES_HEADER_MALFORMED",
        "PTS_ANOMALY",
        "MISSING_REQUIRED_PTS",
        "PCR_MALFORMED",
        "SUBTITLE_MISSING_DESCRIPTOR",
        "SUBTITLE_ALIGNMENT_MISSING",
        "MULTI_CELL_AU",
        "PSI_MULTI_SECTION_UNSUPPORTED",
        "PSI_CC_DISCONTINUITY",
        "PSI_OVERLONG_SECTION",
        "DVB_SUB_DATA_IDENTIFIER",
        "AC3_SYNC_MISSING",
        "LATM_FRAMING",
        "AV1_WRONG_STREAM_ID",
        "AV1_MISSING_TS_OBU_FRAMING",
        "AV1_OBU_MISSING_SIZE_FIELD",
        "AV1_TILE_LIST_NOT_ALLOWED",
        "MISSING_METADATA_DESCRIPTOR",
        "SUBTITLE_DESCRIPTOR_AMBIGUOUS",
        "SUBTITLE_DESCRIPTOR_MALFORMED",
        "OTHER",
    }
    assert required.issubset({v.name for v in NonConformantKind})
