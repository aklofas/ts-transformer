"""NAL / OBU / audio frame DataFrame adapter tests."""

import pytest

pytestmark = pytest.mark.pandas

# The `pandas` marker filters at run-time, but pytest still imports test
# modules at collection. Skip the whole module when the [pandas] extra
# isn't installed (the python-core CI job runs without it).
pytest.importorskip("pandas")

import pandas as pd  # noqa: E402

from tstrans.codec import NalUnit, Obu, ObuExtension
from tstrans.pandas import nals_to_dataframe, obus_to_dataframe


def test_nals_to_dataframe_h264_basic():
    nals = [
        NalUnit.h264(nal_type=7, ref_idc=3, payload=b"\x00\x01"),
        NalUnit.h264(nal_type=8, ref_idc=3, payload=b"\x02"),
        NalUnit.h264(nal_type=5, ref_idc=3, payload=b"\x03\x04\x05"),
    ]
    df = nals_to_dataframe(nals)
    assert len(df) == 3
    assert df["kind"].tolist() == ["H264", "H264", "H264"]
    assert df["nal_type"].tolist() == [7, 8, 5]
    assert df["nal_type_name"].tolist() == ["SPS", "PPS", "IDR_SLICE"]
    assert df["payload_len"].tolist() == [2, 1, 3]


def test_nals_to_dataframe_h264_has_ref_idc_not_layer():
    nals = [NalUnit.h264(nal_type=5, ref_idc=2, payload=b"")]
    df = nals_to_dataframe(nals)
    assert df["ref_idc"].iloc[0] == 2
    assert pd.isna(df["layer_id"].iloc[0])
    assert pd.isna(df["temporal_id_plus1"].iloc[0])


def test_nals_to_dataframe_h265_has_layer_not_ref_idc():
    nals = [NalUnit.h265(nal_type=19, layer_id=1, temporal_id_plus1=2, payload=b"")]
    df = nals_to_dataframe(nals)
    assert df["layer_id"].iloc[0] == 1
    assert df["temporal_id_plus1"].iloc[0] == 2
    assert pd.isna(df["ref_idc"].iloc[0])


def test_nals_to_dataframe_unknown_nal_type():
    nals = [NalUnit.h264(nal_type=31, ref_idc=0, payload=b"")]
    df = nals_to_dataframe(nals)
    assert df["nal_type_name"].iloc[0] == "unknown_31"


def test_nals_to_dataframe_empty():
    df = nals_to_dataframe([])
    assert isinstance(df, pd.DataFrame)
    assert len(df) == 0


def test_nals_to_dataframe_pts_broadcast():
    nals = [NalUnit.h264(nal_type=5, ref_idc=3, payload=b"\x00")]
    df = nals_to_dataframe(nals, pts=123.0)
    assert "pts_ms" in df.columns
    assert df["pts_ms"].iloc[0] == 123.0


def test_nals_to_dataframe_no_pts_omits_column():
    nals = [NalUnit.h264(nal_type=5, ref_idc=3, payload=b"\x00")]
    df = nals_to_dataframe(nals)
    assert "pts_ms" not in df.columns


def test_obus_to_dataframe_basic():
    obus = [
        Obu(obu_type=1, extension=None, payload=b"\xaa"),
        Obu(obu_type=6, extension=ObuExtension(temporal_id=1, spatial_id=0), payload=b"\xbb\xcc"),
    ]
    df = obus_to_dataframe(obus)
    assert len(df) == 2
    assert df["obu_type_name"].tolist() == ["SEQUENCE_HEADER", "FRAME"]
    assert df["payload_len"].tolist() == [1, 2]
    assert pd.isna(df["temporal_id"].iloc[0])
    assert df["temporal_id"].iloc[1] == 1
    assert df["spatial_id"].iloc[1] == 0


def test_obus_to_dataframe_pts_broadcast():
    obus = [Obu(obu_type=6, extension=None, payload=b"")]
    df = obus_to_dataframe(obus, pts=42.5)
    assert df["pts_ms"].iloc[0] == 42.5


def test_obus_to_dataframe_empty():
    df = obus_to_dataframe([])
    assert isinstance(df, pd.DataFrame)
    assert len(df) == 0


# === Audio frame tests ===
#
# Fixture bytes lifted from sibling test files to avoid duplicating the
# ADTS/MPEG-audio bit-layout knowledge — see test_codec_aac.py and
# test_codec_mpeg2_audio.py for the source-of-truth definitions.

SYNTHETIC_AAC_FRAME = bytes.fromhex("fff95080021ffc000000000000000000")
# V1L3 header + zero payload = 417-byte MPEG-1 Layer III joint-stereo frame
# (matches FRAME_V1L3_128K_44100_JS in test_codec_mpeg2_audio.py).
SYNTHETIC_MP2_FRAME = b"\xff\xfb\x90\x44" + bytes(417 - 4)


@pytest.fixture(scope="module")
def aac_frames():
    from tstrans.codec import parse_aac_frames
    return parse_aac_frames(SYNTHETIC_AAC_FRAME + SYNTHETIC_AAC_FRAME)


@pytest.fixture(scope="module")
def mp2_frames():
    from tstrans.codec import parse_mpeg2_audio_frames
    return parse_mpeg2_audio_frames(SYNTHETIC_MP2_FRAME)


def test_audio_frames_to_dataframe_aac(aac_frames):
    from tstrans.pandas import audio_frames_to_dataframe
    df = audio_frames_to_dataframe(aac_frames)
    assert len(df) == 2
    # AdtsFrame schema columns
    expected = {
        "profile", "sample_rate_hz", "channel_configuration",
        "channel_layout", "frame_length_bytes", "samples_per_frame",
        "num_raw_data_blocks", "has_crc", "mpeg_version",
        "raw_header_len", "payload_len", "byte_offset",
    }
    assert expected.issubset(set(df.columns))
    # Lock in bare-variant-name form (matches events.py "VIDEO" not
    # "StreamKind.VIDEO"); _enum_name strips the type prefix.
    assert df["profile"].iloc[0] == "LC"
    assert df["mpeg_version"].iloc[0] == "MPEG2"
    # channel_layout is a struct (parenthesised form) — passes through unchanged.
    assert df["channel_layout"].iloc[0] == "AacChannelLayout(channels=2)"


def test_audio_frames_to_dataframe_byte_offset_accumulates(aac_frames):
    from tstrans.pandas import audio_frames_to_dataframe
    df = audio_frames_to_dataframe(aac_frames)
    assert df["byte_offset"].tolist() == [0, df["frame_length_bytes"].iloc[0]]


def test_audio_frames_to_dataframe_mpeg2(mp2_frames):
    from tstrans.pandas import audio_frames_to_dataframe
    df = audio_frames_to_dataframe(mp2_frames)
    assert "layer" in df.columns
    assert "bitrate_kbps" in df.columns
    assert df["bitrate_kbps"].iloc[0] == 128
    assert df["sample_rate_hz"].iloc[0] == 44100
    assert df["byte_offset"].iloc[0] == 0
    # Lock in bare-variant-name form for enum-stringified columns.
    assert df["layer"].iloc[0] == "III"
    assert df["version"].iloc[0] == "MPEG1"
    assert df["channel_mode"].iloc[0] == "JOINT_STEREO"


def test_audio_frames_to_dataframe_empty():
    from tstrans.pandas import audio_frames_to_dataframe
    df = audio_frames_to_dataframe([])
    assert len(df) == 0


def test_audio_frames_to_dataframe_mixed_raises(aac_frames, mp2_frames):
    from tstrans.pandas import audio_frames_to_dataframe
    mixed = [aac_frames[0], mp2_frames[0]]
    with pytest.raises(TypeError, match="homogeneous"):
        audio_frames_to_dataframe(mixed)
