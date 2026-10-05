"""Shared codec types + NalUnit + Obu typed access."""

import pytest

from tstrans import codec
from tstrans.codec import (
    ChromaFormat,
    ColorInfo,
    ColourPrimaries,
    MatrixCoefficients,
    NalUnit,
    Obu,
    ObuExtension,
    Rational,
    TransferCharacteristics,
)


def test_chroma_format_enum_variants():
    assert ChromaFormat.MONOCHROME
    assert ChromaFormat.YUV420
    assert ChromaFormat.YUV422
    assert ChromaFormat.YUV444


def test_rational_constructs_and_evaluates():
    r = Rational(num=24000, den=1001)
    assert r.num == 24000
    assert r.den == 1001
    assert pytest.approx(r.as_float(), rel=1e-6) == 24000 / 1001


def test_color_info_construct():
    ci = ColorInfo(
        primaries=ColourPrimaries.BT709,
        transfer=TransferCharacteristics.BT709,
        matrix=MatrixCoefficients.BT709,
        full_range=False,
    )
    assert ci.primaries == ColourPrimaries.BT709


def test_nal_unit_h264_construct_and_fields():
    nal = NalUnit.h264(nal_type=5, ref_idc=3, payload=b"\x01\x02\x03")
    assert nal.kind == "H264"
    assert nal.nal_type == 5
    assert nal.ref_idc == 3
    assert nal.payload == b"\x01\x02\x03"


def test_nal_unit_h265_construct():
    nal = NalUnit.h265(nal_type=19, layer_id=0, temporal_id_plus1=1, payload=b"\x42\x01")
    assert nal.kind == "H265"
    assert nal.nal_type == 19
    assert nal.layer_id == 0
    assert nal.temporal_id_plus1 == 1


def test_nal_unit_h266_construct():
    nal = NalUnit.h266(nal_type=7, layer_id=0, temporal_id_plus1=1, payload=b"\x00")
    assert nal.kind == "H266"


def test_obu_construct():
    obu = Obu(obu_type=6, extension=None, payload=b"\xab")
    assert obu.obu_type == 6
    assert obu.extension is None
    assert obu.payload == b"\xab"


def test_obu_with_extension():
    ext = ObuExtension(temporal_id=2, spatial_id=1)
    obu = Obu(obu_type=6, extension=ext, payload=b"")
    assert obu.extension.temporal_id == 2
    assert obu.extension.spatial_id == 1


def test_codec_module_exports():
    assert hasattr(codec, "NalUnit")
    assert hasattr(codec, "Obu")
    assert hasattr(codec, "ObuExtension")


# ---------------------------------------------------------------------------
# MispTimeKind, MispTimestamp, extract_misp_timestamp
# ---------------------------------------------------------------------------


def test_misp_time_kind_variants():
    from tstrans.codec import MispTimeKind

    assert MispTimeKind.MICRO == MispTimeKind.MICRO
    assert MispTimeKind.NANO == MispTimeKind.NANO
    assert MispTimeKind.MICRO != MispTimeKind.NANO


def test_misp_timestamp_micros_staticmethod():
    from tstrans.codec import MispTimestamp, MispTimeKind

    ts = MispTimestamp.micros(12345, 0x1F)
    assert ts.kind == MispTimeKind.MICRO
    assert ts.time_status == 0x1F
    assert ts.value == 12345


def test_misp_timestamp_nanos_staticmethod():
    from tstrans.codec import MispTimestamp, MispTimeKind

    ts = MispTimestamp.nanos(99999, 0x00)
    assert ts.kind == MispTimeKind.NANO
    assert ts.time_status == 0x00
    assert ts.value == 99999


def test_misp_timestamp_new_constructor():
    from tstrans.codec import MispTimestamp, MispTimeKind

    ts = MispTimestamp(MispTimeKind.MICRO, 0x3F, 777)
    assert ts.kind == MispTimeKind.MICRO
    assert ts.time_status == 0x3F
    assert ts.value == 777


def test_misp_timestamp_repr():
    from tstrans.codec import MispTimestamp, MispTimeKind

    ts = MispTimestamp.micros(1, 0)
    r = repr(ts)
    assert "MispTimestamp" in r


def test_misp_timestamp_eq():
    from tstrans.codec import MispTimestamp

    a = MispTimestamp.micros(100, 0x1F)
    b = MispTimestamp.micros(100, 0x1F)
    c = MispTimestamp.micros(101, 0x1F)
    assert a == b
    assert a != c


def test_extract_misp_timestamp_absent_returns_none():
    from tstrans.codec import extract_misp_timestamp
    from tstrans.mpegts import VideoCodec

    # A minimal H.264 AUD NAL with no SEI — extract must return None.
    aud = b"\x00\x00\x00\x01\x09\xF0"
    result = extract_misp_timestamp(aud, VideoCodec.H264)
    assert result is None


def test_extract_misp_timestamp_malformed_raises():
    """A confirmed MISP identifier with a truncated payload must raise ValueError."""
    import pytest
    from tstrans.codec import MispTimestamp, extract_misp_timestamp
    from tstrans.mpegts import VideoCodec

    # Build a well-formed AU, then truncate the SEI payload to trigger TruncatedSei.
    # Reuse the Rust-level golden for H.264 micros: the SEI NAL starts at offset 3
    # (after 3-byte start code), and cutting the last 6 bytes produces a confirmed
    # MISP identifier with a declared payload_size that runs past the end.

    # We can't easily call build_sei_nal from Python, so craft minimal raw bytes:
    # Start code + H.264 SEI header (0x06) + payload_type 5 + size 28 +
    # "MISPmicrosectime" (16 bytes) + status 0x1F + partial value (only 6 bytes,
    # not 11) — declared size=28 exceeds actual remaining bytes.
    misp_id = b"MISPmicrosectime"  # 16 bytes
    nal_body = bytes([0x06, 0x05, 28]) + misp_id + bytes([0x1F, 0x01, 0x02])  # 3+16+3 = 22 bytes, payload incomplete
    au = b"\x00\x00\x01" + nal_body
    with pytest.raises(ValueError):
        extract_misp_timestamp(au, VideoCodec.H264)
