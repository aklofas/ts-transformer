"""H.264 codec surface tests.

Fixture bytes are the same RBSP payloads used by tst-core's Rust H.264
parameter-set tests — extracted from the on-disk binaries at
``crates/tst-core/tests/fixtures/codec/h264/``.
"""

import pytest

from tstrans.codec import (
    EntropyCodingMode,
    H264ParameterSets,
    H264Pps,
    H264SliceHeaderLight,
    H264SliceType,
    H264Sps,
    NalUnit,
    parse_h264_parameter_sets,
    parse_h264_pps,
    parse_h264_slice_header_light,
    parse_h264_sps,
)
from tstrans.exceptions import CodecError, CodecErrorKind

# ---------------------------------------------------------------------------
# Real fixtures — 1080p High@4.0 BT.709 (26 bytes SPS, 4 bytes PPS)
# ---------------------------------------------------------------------------
# Source: crates/tst-core/tests/fixtures/codec/h264/h264_1080p_high40_bt709_sps.bin
SPS_1080P_HIGH40_BYTES = bytes.fromhex(
    "641028acb80f0044fcb80b50101014000003000400000300f010"
)
# Source: crates/tst-core/tests/fixtures/codec/h264/h264_1080p_high40_bt709_pps.bin
PPS_1080P_HIGH40_BYTES = bytes.fromhex("ee0f2c8b")

# 720p Main@3.1 (23 bytes SPS, 4 bytes PPS)
# Source: crates/tst-core/tests/fixtures/codec/h264/h264_720p_main31_sps.bin
SPS_720P_MAIN31_BYTES = bytes.fromhex("4d401fdc05005bb0110000030001000003003c0f183380")
# Source: crates/tst-core/tests/fixtures/codec/h264/h264_720p_main31_pps.bin
PPS_720P_MAIN31_BYTES = bytes.fromhex("ee0f2c80")

# Synthetic IDR slice header with no SPS context.
# Encoding (from slice_header_light.rs §tests):
#   first_mb_in_slice = 0  → ue(v) = '1'            (1 bit)
#   slice_type = 7         → ue(v) = '0001000'       (7 bits)
#   pic_parameter_set_id=0 → ue(v) = '1'             (1 bit)
#   Total 9 bits → pad to 2 bytes:
#     byte 0: 1_0001000 = 0x88
#     byte 1: 1_xxxxxxx = 0x80
SLICE_HEADER_IDR_BYTES = bytes([0x88, 0x80])


# ---------------------------------------------------------------------------
# SPS parse tests
# ---------------------------------------------------------------------------


def test_parse_h264_sps_1080p_returns_h264_sps():
    """parse_h264_sps returns an H264Sps for well-formed 1080p High input."""
    sps = parse_h264_sps(SPS_1080P_HIGH40_BYTES)
    assert isinstance(sps, H264Sps)


def test_parse_h264_sps_1080p_dimensions():
    """SPS dimensions match the expected 1920×1080 post-crop values."""
    sps = parse_h264_sps(SPS_1080P_HIGH40_BYTES)
    assert sps.width == 1920
    assert sps.height == 1080


def test_parse_h264_sps_1080p_profile_level():
    """profile_idc=100 (High) and level_idc=40 must be decoded."""
    sps = parse_h264_sps(SPS_1080P_HIGH40_BYTES)
    assert sps.profile_idc == 100
    assert sps.level_idc == 40


def test_parse_h264_sps_1080p_seq_parameter_set_id():
    """seq_parameter_set_id is 0 for this fixture."""
    sps = parse_h264_sps(SPS_1080P_HIGH40_BYTES)
    assert sps.seq_parameter_set_id == 0


def test_parse_h264_sps_1080p_frame_mbs_only():
    """frame_mbs_only is True for progressive encoding."""
    sps = parse_h264_sps(SPS_1080P_HIGH40_BYTES)
    assert sps.frame_mbs_only is True


def test_parse_h264_sps_1080p_coded_dimensions():
    """coded_width/coded_height reflect the macroblock-aligned size."""
    sps = parse_h264_sps(SPS_1080P_HIGH40_BYTES)
    # 1080p is coded as 1920×1088 (68 MB rows); frame_crop removes bottom 8.
    assert sps.coded_width() == 1920
    assert sps.coded_height() == 1088


def test_parse_h264_sps_1080p_crop_bottom():
    """crop_bottom is 8 luma samples for the 1080p fixture."""
    sps = parse_h264_sps(SPS_1080P_HIGH40_BYTES)
    assert sps.crop_top == 0
    assert sps.crop_left == 0
    assert sps.crop_right == 0
    assert sps.crop_bottom == 8


def test_parse_h264_sps_1080p_raw_rbsp():
    """raw_rbsp round-trips the original bytes."""
    sps = parse_h264_sps(SPS_1080P_HIGH40_BYTES)
    assert sps.raw_rbsp == SPS_1080P_HIGH40_BYTES


def test_parse_h264_sps_720p_dimensions():
    """720p Main SPS is decoded to 1280×720."""
    sps = parse_h264_sps(SPS_720P_MAIN31_BYTES)
    assert sps.width == 1280
    assert sps.height == 720
    assert sps.profile_idc == 77  # Main profile
    assert sps.level_idc == 31


def test_parse_h264_sps_720p_no_crop():
    """720p fixture has no frame_cropping — all four offsets are zero."""
    sps = parse_h264_sps(SPS_720P_MAIN31_BYTES)
    assert sps.crop_left == 0
    assert sps.crop_right == 0
    assert sps.crop_top == 0
    assert sps.crop_bottom == 0
    assert sps.coded_width() == sps.width
    assert sps.coded_height() == sps.height


def test_parse_h264_sps_repr():
    """H264Sps.__repr__ contains profile, level, and dimensions."""
    sps = parse_h264_sps(SPS_1080P_HIGH40_BYTES)
    r = repr(sps)
    assert "100" in r  # profile_idc
    assert "40" in r   # level_idc
    assert "1920" in r
    assert "1080" in r


def test_parse_h264_sps_truncated_empty_raises_codec_error():
    """Empty input raises CodecError with kind=TRUNCATED_RBSP."""
    with pytest.raises(CodecError) as exc_info:
        parse_h264_sps(b"")
    err = exc_info.value
    assert err.kind is CodecErrorKind.TRUNCATED_RBSP
    assert err.codec == "h264"


def test_parse_h264_sps_garbage_raises_codec_error():
    """All-0xFF garbage raises a CodecError (EngineError)."""
    with pytest.raises(CodecError) as exc_info:
        parse_h264_sps(bytes([0xFF] * 8))
    assert exc_info.value.codec == "h264"


# ---------------------------------------------------------------------------
# PPS parse tests
# ---------------------------------------------------------------------------


def test_parse_h264_pps_returns_h264_pps():
    """parse_h264_pps returns an H264Pps for well-formed input."""
    pps = parse_h264_pps(PPS_1080P_HIGH40_BYTES)
    assert isinstance(pps, H264Pps)


def test_parse_h264_pps_ids():
    """pic_parameter_set_id and seq_parameter_set_id are both 0."""
    pps = parse_h264_pps(PPS_1080P_HIGH40_BYTES)
    assert pps.pic_parameter_set_id == 0
    assert pps.seq_parameter_set_id == 0


def test_parse_h264_pps_entropy_mode():
    """1080p High fixture uses CABAC entropy coding."""
    pps = parse_h264_pps(PPS_1080P_HIGH40_BYTES)
    # High profile with CABAC. PyO3 eq_int enums compare by value, not identity.
    assert pps.entropy_coding_mode == EntropyCodingMode.CABAC


def test_parse_h264_pps_raw_rbsp():
    """raw_rbsp round-trips the original bytes."""
    pps = parse_h264_pps(PPS_1080P_HIGH40_BYTES)
    assert pps.raw_rbsp == PPS_1080P_HIGH40_BYTES


def test_parse_h264_pps_720p_entropy_mode():
    """720p Main fixture PPS entropy mode — CAVLC or CABAC based on fixture."""
    pps = parse_h264_pps(PPS_720P_MAIN31_BYTES)
    assert isinstance(pps, H264Pps)
    # Any valid EntropyCodingMode is acceptable; just check it's one of the two.
    assert pps.entropy_coding_mode in (EntropyCodingMode.CAVLC, EntropyCodingMode.CABAC)


def test_parse_h264_pps_repr():
    """H264Pps.__repr__ contains pps_id and sps_id."""
    pps = parse_h264_pps(PPS_1080P_HIGH40_BYTES)
    r = repr(pps)
    assert "pps_id=0" in r
    assert "sps_id=0" in r


def test_parse_h264_pps_empty_raises_codec_error():
    """Empty input raises CodecError with kind=TRUNCATED_RBSP."""
    with pytest.raises(CodecError) as exc_info:
        parse_h264_pps(b"")
    err = exc_info.value
    assert err.kind is CodecErrorKind.TRUNCATED_RBSP
    assert err.codec == "h264"


# ---------------------------------------------------------------------------
# parse_h264_parameter_sets tests
# ---------------------------------------------------------------------------


def test_parse_h264_parameter_sets_sps_and_pps():
    """parse_h264_parameter_sets populates both maps from two NAL units."""
    sps_nal = NalUnit.h264(nal_type=7, ref_idc=3, payload=SPS_1080P_HIGH40_BYTES)
    pps_nal = NalUnit.h264(nal_type=8, ref_idc=3, payload=PPS_1080P_HIGH40_BYTES)
    ps = parse_h264_parameter_sets([sps_nal, pps_nal])
    assert isinstance(ps, H264ParameterSets)
    assert 0 in ps.sps_by_id
    assert 0 in ps.pps_by_id


def test_parse_h264_parameter_sets_sps_dimensions():
    """SPS in the returned map has correct dimensions."""
    sps_nal = NalUnit.h264(nal_type=7, ref_idc=3, payload=SPS_1080P_HIGH40_BYTES)
    pps_nal = NalUnit.h264(nal_type=8, ref_idc=3, payload=PPS_1080P_HIGH40_BYTES)
    ps = parse_h264_parameter_sets([sps_nal, pps_nal])
    assert ps.sps_by_id[0].width == 1920
    assert ps.sps_by_id[0].height == 1080


def test_parse_h264_parameter_sets_skips_non_h264_nals():
    """H.265 NAL units in the list are silently skipped."""
    sps_nal = NalUnit.h264(nal_type=7, ref_idc=3, payload=SPS_1080P_HIGH40_BYTES)
    h265_nal = NalUnit.h265(nal_type=32, layer_id=0, temporal_id_plus1=1, payload=b"\x00" * 8)
    ps = parse_h264_parameter_sets([sps_nal, h265_nal])
    assert 0 in ps.sps_by_id


def test_parse_h264_parameter_sets_empty_input():
    """Empty list returns an H264ParameterSets with both dicts empty."""
    ps = parse_h264_parameter_sets([])
    assert isinstance(ps, H264ParameterSets)
    assert len(ps.sps_by_id) == 0
    assert len(ps.pps_by_id) == 0


def test_parse_h264_parameter_sets_slice_nals_skipped():
    """Non-SPS/PPS NAL units (e.g. IDR slice, nal_type=5) are ignored."""
    slice_nal = NalUnit.h264(nal_type=5, ref_idc=3, payload=b"\x00" * 16)
    ps = parse_h264_parameter_sets([slice_nal])
    assert len(ps.sps_by_id) == 0
    assert len(ps.pps_by_id) == 0


# ---------------------------------------------------------------------------
# H264SliceHeaderLight tests
# ---------------------------------------------------------------------------


def test_parse_h264_slice_header_light_no_sps():
    """Synthetic IDR slice header parses correctly without SPS context."""
    sh = parse_h264_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=5)
    assert isinstance(sh, H264SliceHeaderLight)


def test_parse_h264_slice_header_light_first_in_pic():
    """first_in_pic is True when first_mb_in_slice == 0."""
    sh = parse_h264_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=5)
    assert sh.first_in_pic is True


def test_parse_h264_slice_header_light_slice_type_i():
    """slice_type is H264SliceType.I for the IDR slice fixture."""
    sh = parse_h264_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=5)
    # PyO3 eq_int enums compare by value, not identity — use ==.
    assert sh.slice_type == H264SliceType.I


def test_parse_h264_slice_header_light_pps_id():
    """pps_id == 0 for the minimal synthetic header."""
    sh = parse_h264_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=5)
    assert sh.pps_id == 0


def test_parse_h264_slice_header_light_idr_flag():
    """idr is True when nal_unit_type == 5."""
    sh = parse_h264_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=5)
    assert sh.idr is True


def test_parse_h264_slice_header_light_non_idr():
    """idr is False when nal_unit_type != 5."""
    sh = parse_h264_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=1)
    assert sh.idr is False


def test_parse_h264_slice_header_light_frame_num_none_without_sps():
    """frame_num is None when no SPS is provided."""
    sh = parse_h264_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=5)
    assert sh.frame_num is None


def test_parse_h264_slice_header_light_raw_rbsp():
    """raw_rbsp round-trips the input bytes."""
    sh = parse_h264_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=5)
    assert sh.raw_rbsp == SLICE_HEADER_IDR_BYTES


def test_parse_h264_slice_header_light_with_sps():
    """With SPS context, frame_num is populated (not None)."""
    sps = parse_h264_sps(SPS_1080P_HIGH40_BYTES)
    # The 1080p High SPS has log2_max_frame_num_minus4 = 0 → frame_num is 4 bits.
    # Build a slice header that has at least 4 extra bits after pps_id:
    # first_mb_in_slice=0 (1b) + slice_type=7 (7b) + pps_id=0 (1b) = 9 bits,
    # then frame_num fits into the remaining bits of a 2-byte buffer.
    sh = parse_h264_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=sps, nal_unit_type=5)
    # frame_num is not None — exact value depends on remaining bits in the buffer.
    assert sh.frame_num is not None


def test_parse_h264_slice_header_light_repr():
    """H264SliceHeaderLight.__repr__ includes first, slice_type, and idr."""
    sh = parse_h264_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=5)
    r = repr(sh)
    # Rust Debug format uses lowercase true/false.
    assert "first=true" in r
    assert "idr=true" in r


def test_parse_h264_slice_header_light_truncated_raises_codec_error():
    """Empty RBSP raises CodecError with kind=TRUNCATED_RBSP."""
    with pytest.raises(CodecError) as exc_info:
        parse_h264_slice_header_light(b"", sps=None, nal_unit_type=5)
    err = exc_info.value
    assert err.kind is CodecErrorKind.TRUNCATED_RBSP
    assert err.codec == "h264"


# ---------------------------------------------------------------------------
# H264SliceType enum shape
# ---------------------------------------------------------------------------


def test_h264_slice_type_variants():
    """All expected H264SliceType variants are accessible."""
    # PyO3 eq_int enums are not iterable like Python enum.Enum — check directly.
    assert H264SliceType.I is not None
    assert H264SliceType.P is not None
    assert H264SliceType.B is not None
    assert H264SliceType.Si is not None
    assert H264SliceType.Sp is not None


# ---------------------------------------------------------------------------
# EntropyCodingMode enum shape
# ---------------------------------------------------------------------------


def test_entropy_coding_mode_variants():
    """CAVLC and CABAC variants are accessible."""
    # PyO3 eq_int enums are not iterable like Python enum.Enum — check directly.
    assert EntropyCodingMode.CAVLC is not None
    assert EntropyCodingMode.CABAC is not None
