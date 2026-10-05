"""H.265 codec surface tests.

Fixture bytes are the same RBSP payloads used by tst-core's Rust H.265
parameter-set tests — extracted from the on-disk binaries at
``crates/tst-core/tests/fixtures/codec/h265/``.
"""

import pytest

from tstrans.codec import (
    ChromaFormat,
    H265ParameterSets,
    H265Pps,
    H265ProfileTierLevel,
    H265SliceHeaderLight,
    H265SliceType,
    H265Sps,
    H265Vps,
    NalUnit,
    parse_h265_parameter_sets,
    parse_h265_pps,
    parse_h265_slice_header_light,
    parse_h265_sps,
    parse_h265_vps,
)
from tstrans.exceptions import CodecError, CodecErrorKind

# ---------------------------------------------------------------------------
# Real fixtures — 1080p Main@Level4.0 8-bit (41 bytes SPS, 4 bytes PPS,
# 21 bytes VPS)
# ---------------------------------------------------------------------------
# Source: crates/tst-core/tests/fixtures/codec/h265/h265_1080p_main40_sps.bin
SPS_MAIN40_BYTES = bytes.fromhex(
    "01016000000300900000030000030078"
    "a003c0801107cb965654a4c2f016808"
    "0000003008000000c84"
)
# Source: crates/tst-core/tests/fixtures/codec/h265/h265_1080p_main40_pps.bin
PPS_MAIN40_BYTES = bytes.fromhex("c073c189")
# Source: crates/tst-core/tests/fixtures/codec/h265/h265_1080p_main40_vps.bin
VPS_MAIN40_BYTES = bytes.fromhex("0c01ffff2408000003009fa8000003000078ba0240")

# 1080p Main10@Level5.0 HDR PQ (BT.2020 / SMPTE ST 2084)
# Source: crates/tst-core/tests/fixtures/codec/h265/h265_1080p_main10_50_pq_sps.bin
SPS_MAIN10_50_PQ_BYTES = bytes.fromhex(
    "01222000000300900000030000030096"
    "a003c0801107cad965654a4c2f016a1"
    "22012080000030008000003019040"
)

# Synthetic IDR (IDR_W_RADL, nal_unit_type=19) slice segment header with
# no SPS context.  Encoding (from slice_header_light.rs §tests):
#   first_slice_segment_in_pic_flag = 1  → u(1): '1'        1 bit
#   no_output_of_prior_pics_flag = 0     → u(1): '0'        1 bit  (IRAP)
#   slice_pic_parameter_set_id = 0       → ue(v): '1'       1 bit
#   slice_type = 2 (I)                   → ue(v): '011'     3 bits
#   Total 6 bits → 0xAC + stop-bit 0x80
SLICE_HEADER_IDR_BYTES = bytes([0xAC, 0x80])


# ---------------------------------------------------------------------------
# SPS parse tests
# ---------------------------------------------------------------------------


def test_parse_h265_sps_1080p_returns_h265_sps():
    """parse_h265_sps returns an H265Sps for well-formed 1080p Main input."""
    sps = parse_h265_sps(SPS_MAIN40_BYTES)
    assert isinstance(sps, H265Sps)


def test_parse_h265_sps_1080p_dimensions():
    """SPS dimensions match the expected 1920×1080 post-crop values."""
    sps = parse_h265_sps(SPS_MAIN40_BYTES)
    assert sps.width == 1920
    assert sps.height == 1080


def test_parse_h265_sps_1080p_profile_level():
    """general_level_idc==120 (Level 4.0) and bit depths correct."""
    sps = parse_h265_sps(SPS_MAIN40_BYTES)
    assert sps.general_level_idc == 120
    assert sps.bit_depth_luma == 8
    assert sps.bit_depth_chroma == 8


def test_parse_h265_sps_1080p_chroma_format():
    """1080p Main fixture uses 4:2:0 chroma format."""
    sps = parse_h265_sps(SPS_MAIN40_BYTES)
    assert sps.chroma_format == ChromaFormat.YUV420


def test_parse_h265_sps_1080p_sps_ids():
    """Both sps_seq_parameter_set_id and sps_video_parameter_set_id are 0."""
    sps = parse_h265_sps(SPS_MAIN40_BYTES)
    assert sps.sps_seq_parameter_set_id == 0
    assert sps.sps_video_parameter_set_id == 0


def test_parse_h265_sps_1080p_coded_dimensions():
    """coded_width/coded_height reflect the CTB-aligned size."""
    sps = parse_h265_sps(SPS_MAIN40_BYTES)
    # 1080p HEVC is coded as 1920×1088; conformance window removes bottom 8.
    assert sps.coded_width() == 1920
    assert sps.coded_height() == 1088


def test_parse_h265_sps_1080p_crop_bottom():
    """crop_bottom is 8 luma samples for the 1080p fixture."""
    sps = parse_h265_sps(SPS_MAIN40_BYTES)
    assert sps.crop_top == 0
    assert sps.crop_left == 0
    assert sps.crop_right == 0
    assert sps.crop_bottom == 8


def test_parse_h265_sps_1080p_raw_rbsp():
    """raw_rbsp round-trips the original bytes."""
    sps = parse_h265_sps(SPS_MAIN40_BYTES)
    assert sps.raw_rbsp == SPS_MAIN40_BYTES


def test_parse_h265_sps_main10_50_pq_bit_depth():
    """Main10 HDR PQ fixture decodes to 10-bit and Level 5.0."""
    sps = parse_h265_sps(SPS_MAIN10_50_PQ_BYTES)
    assert sps.bit_depth_luma == 10
    assert sps.bit_depth_chroma == 10
    assert sps.general_level_idc == 150


def test_parse_h265_sps_main10_50_pq_color():
    """Main10 HDR PQ fixture carries BT.2020/SMPTE-ST2084 colour info."""
    from tstrans.codec import ColourPrimaries, MatrixCoefficients, TransferCharacteristics

    sps = parse_h265_sps(SPS_MAIN10_50_PQ_BYTES)
    color = sps.color
    assert color is not None
    assert color.primaries == ColourPrimaries.BT2020
    assert color.transfer == TransferCharacteristics.SMPTE_ST2084
    assert color.matrix == MatrixCoefficients.BT2020_NON_CONSTANT


def test_parse_h265_sps_repr():
    """H265Sps.__repr__ contains profile, level, and dimensions."""
    sps = parse_h265_sps(SPS_MAIN40_BYTES)
    r = repr(sps)
    assert "1920" in r
    assert "1080" in r
    assert "120" in r  # general_level_idc


def test_parse_h265_sps_truncated_empty_raises_codec_error():
    """Empty input raises CodecError with kind=TRUNCATED_RBSP."""
    with pytest.raises(CodecError) as exc_info:
        parse_h265_sps(b"")
    err = exc_info.value
    assert err.kind is CodecErrorKind.TRUNCATED_RBSP
    assert err.codec == "h265"


def test_parse_h265_sps_garbage_raises_codec_error():
    """All-0xFF garbage raises a CodecError."""
    with pytest.raises(CodecError) as exc_info:
        parse_h265_sps(bytes([0xFF] * 16))
    assert exc_info.value.codec == "h265"


def test_parse_h265_sps_profile_tier_level_method():
    """profile_tier_level() returns an H265ProfileTierLevel with correct fields."""
    sps = parse_h265_sps(SPS_MAIN40_BYTES)
    ptl = sps.profile_tier_level()
    assert isinstance(ptl, H265ProfileTierLevel)
    assert ptl.general_level_idc == sps.general_level_idc
    assert ptl.general_tier_flag == sps.general_tier_flag
    assert ptl.general_profile_idc == sps.general_profile_idc


# ---------------------------------------------------------------------------
# PPS parse tests
# ---------------------------------------------------------------------------


def test_parse_h265_pps_returns_h265_pps():
    """parse_h265_pps returns an H265Pps for well-formed input."""
    pps = parse_h265_pps(PPS_MAIN40_BYTES)
    assert isinstance(pps, H265Pps)


def test_parse_h265_pps_ids():
    """pps_pic_parameter_set_id and pps_seq_parameter_set_id are both 0."""
    pps = parse_h265_pps(PPS_MAIN40_BYTES)
    assert pps.pps_pic_parameter_set_id == 0
    assert pps.pps_seq_parameter_set_id == 0


def test_parse_h265_pps_raw_rbsp():
    """raw_rbsp round-trips the original bytes."""
    pps = parse_h265_pps(PPS_MAIN40_BYTES)
    assert pps.raw_rbsp == PPS_MAIN40_BYTES


def test_parse_h265_pps_repr():
    """H265Pps.__repr__ contains pps_id and sps_id."""
    pps = parse_h265_pps(PPS_MAIN40_BYTES)
    r = repr(pps)
    assert "pps_id=0" in r
    assert "sps_id=0" in r


def test_parse_h265_pps_empty_raises_codec_error():
    """Empty input raises CodecError with kind=TRUNCATED_RBSP."""
    with pytest.raises(CodecError) as exc_info:
        parse_h265_pps(b"")
    err = exc_info.value
    assert err.kind is CodecErrorKind.TRUNCATED_RBSP
    assert err.codec == "h265"


# ---------------------------------------------------------------------------
# VPS parse tests
# ---------------------------------------------------------------------------


def test_parse_h265_vps_returns_h265_vps():
    """parse_h265_vps returns an H265Vps for well-formed input."""
    vps = parse_h265_vps(VPS_MAIN40_BYTES)
    assert isinstance(vps, H265Vps)


def test_parse_h265_vps_ids_and_level():
    """vps_video_parameter_set_id == 0 and general_level_idc == 120."""
    vps = parse_h265_vps(VPS_MAIN40_BYTES)
    assert vps.vps_video_parameter_set_id == 0
    assert vps.general_level_idc == 120


def test_parse_h265_vps_raw_rbsp():
    """raw_rbsp round-trips the original bytes."""
    vps = parse_h265_vps(VPS_MAIN40_BYTES)
    assert vps.raw_rbsp == VPS_MAIN40_BYTES


def test_parse_h265_vps_repr():
    """H265Vps.__repr__ contains vps_id, profile, and level."""
    vps = parse_h265_vps(VPS_MAIN40_BYTES)
    r = repr(vps)
    assert "vps_id=0" in r


def test_parse_h265_vps_profile_tier_level_method():
    """profile_tier_level() returns an H265ProfileTierLevel with correct fields."""
    vps = parse_h265_vps(VPS_MAIN40_BYTES)
    ptl = vps.profile_tier_level()
    assert isinstance(ptl, H265ProfileTierLevel)
    assert ptl.general_level_idc == vps.general_level_idc
    assert ptl.general_profile_idc == vps.general_profile_idc


def test_parse_h265_vps_empty_raises_codec_error():
    """Empty input raises CodecError with kind=TRUNCATED_RBSP."""
    with pytest.raises(CodecError) as exc_info:
        parse_h265_vps(b"")
    err = exc_info.value
    assert err.kind is CodecErrorKind.TRUNCATED_RBSP
    assert err.codec == "h265"


# ---------------------------------------------------------------------------
# H265ProfileTierLevel tests
# ---------------------------------------------------------------------------


def test_h265_profile_tier_level_repr():
    """H265ProfileTierLevel.__repr__ contains profile_idc, tier, and level."""
    sps = parse_h265_sps(SPS_MAIN40_BYTES)
    ptl = sps.profile_tier_level()
    r = repr(ptl)
    assert "120" in r  # general_level_idc


# ---------------------------------------------------------------------------
# parse_h265_parameter_sets tests
# ---------------------------------------------------------------------------


def test_parse_h265_parameter_sets_vps_sps_pps():
    """parse_h265_parameter_sets populates all three maps from NAL units."""
    vps_nal = NalUnit.h265(nal_type=32, layer_id=0, temporal_id_plus1=1, payload=VPS_MAIN40_BYTES)
    sps_nal = NalUnit.h265(nal_type=33, layer_id=0, temporal_id_plus1=1, payload=SPS_MAIN40_BYTES)
    pps_nal = NalUnit.h265(nal_type=34, layer_id=0, temporal_id_plus1=1, payload=PPS_MAIN40_BYTES)
    ps = parse_h265_parameter_sets([vps_nal, sps_nal, pps_nal])
    assert isinstance(ps, H265ParameterSets)
    assert 0 in ps.vps_by_id
    assert 0 in ps.sps_by_id
    assert 0 in ps.pps_by_id


def test_parse_h265_parameter_sets_sps_dimensions():
    """SPS in the returned map has correct dimensions."""
    sps_nal = NalUnit.h265(nal_type=33, layer_id=0, temporal_id_plus1=1, payload=SPS_MAIN40_BYTES)
    pps_nal = NalUnit.h265(nal_type=34, layer_id=0, temporal_id_plus1=1, payload=PPS_MAIN40_BYTES)
    ps = parse_h265_parameter_sets([sps_nal, pps_nal])
    assert ps.sps_by_id[0].width == 1920
    assert ps.sps_by_id[0].height == 1080


def test_parse_h265_parameter_sets_skips_non_h265_nals():
    """H.264 NAL units in the list are silently skipped."""
    sps_nal = NalUnit.h265(nal_type=33, layer_id=0, temporal_id_plus1=1, payload=SPS_MAIN40_BYTES)
    h264_nal = NalUnit.h264(nal_type=7, ref_idc=3, payload=b"\x00" * 8)
    ps = parse_h265_parameter_sets([sps_nal, h264_nal])
    assert 0 in ps.sps_by_id


def test_parse_h265_parameter_sets_empty_input():
    """Empty list returns an H265ParameterSets with all three dicts empty."""
    ps = parse_h265_parameter_sets([])
    assert isinstance(ps, H265ParameterSets)
    assert len(ps.vps_by_id) == 0
    assert len(ps.sps_by_id) == 0
    assert len(ps.pps_by_id) == 0


def test_parse_h265_parameter_sets_repr():
    """H265ParameterSets.__repr__ includes n_vps, n_sps, n_pps counts."""
    ps = parse_h265_parameter_sets([])
    r = repr(ps)
    assert "n_vps=0" in r
    assert "n_sps=0" in r
    assert "n_pps=0" in r


# ---------------------------------------------------------------------------
# H265SliceHeaderLight tests
# ---------------------------------------------------------------------------


def test_parse_h265_slice_header_light_no_sps():
    """Synthetic IDR slice header parses correctly without SPS context."""
    sh = parse_h265_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=19)
    assert isinstance(sh, H265SliceHeaderLight)


def test_parse_h265_slice_header_light_first_in_pic():
    """first_in_pic is True for the synthetic IDR header."""
    sh = parse_h265_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=19)
    assert sh.first_in_pic is True


def test_parse_h265_slice_header_light_slice_type_i():
    """slice_type is H265SliceType.I for the IDR slice fixture."""
    sh = parse_h265_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=19)
    # PyO3 eq_int enums compare by value, not identity — use ==.
    assert sh.slice_type == H265SliceType.I


def test_parse_h265_slice_header_light_pps_id():
    """pps_id == 0 for the minimal synthetic header."""
    sh = parse_h265_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=19)
    assert sh.pps_id == 0


def test_parse_h265_slice_header_light_idr_flag_w_radl():
    """idr is True when nal_unit_type == 19 (IDR_W_RADL)."""
    sh = parse_h265_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=19)
    assert sh.idr is True


def test_parse_h265_slice_header_light_idr_flag_n_lp():
    """idr is True when nal_unit_type == 20 (IDR_N_LP)."""
    sh = parse_h265_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=20)
    assert sh.idr is True


def test_parse_h265_slice_header_light_non_idr():
    """idr is False when nal_unit_type is a non-IDR NAL type."""
    # TRAIL_N (1) is not IRAP — build a matching non-IRAP slice header:
    #   first_slice_segment_in_pic_flag = 1  → '1'    1 bit
    #   slice_pic_parameter_set_id = 0       → '1'    1 bit
    #   slice_type = 2 (I)                   → '011'  3 bits
    #   Total: 5 bits → 1_1_011_xxx → 0xD8
    rbsp = bytes([0xD8])
    sh = parse_h265_slice_header_light(rbsp, sps=None, nal_unit_type=1)
    assert sh.idr is False


def test_parse_h265_slice_header_light_pic_order_cnt_none_without_sps():
    """pic_order_cnt_lsb is None when no SPS is provided."""
    sh = parse_h265_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=19)
    assert sh.pic_order_cnt_lsb is None


def test_parse_h265_slice_header_light_raw_rbsp():
    """raw_rbsp round-trips the input bytes."""
    sh = parse_h265_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=19)
    assert sh.raw_rbsp == SLICE_HEADER_IDR_BYTES


def test_parse_h265_slice_header_light_with_sps_idr_pic_order_cnt():
    """With SPS context and an IDR NAL, pic_order_cnt_lsb is 0 (spec implicit)."""
    sps = parse_h265_sps(SPS_MAIN40_BYTES)
    # IDR_W_RADL (19): pic_order_cnt_lsb is implicitly 0 per H.265 §8.3.1.
    sh = parse_h265_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=sps, nal_unit_type=19)
    assert sh.pic_order_cnt_lsb == 0


def test_parse_h265_slice_header_light_repr():
    """H265SliceHeaderLight.__repr__ includes first, slice_type, and idr."""
    sh = parse_h265_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=19)
    r = repr(sh)
    # Rust Debug format uses lowercase true/false.
    assert "first=true" in r
    assert "idr=true" in r


def test_parse_h265_slice_header_light_truncated_raises_codec_error():
    """Empty RBSP raises CodecError with kind=TRUNCATED_RBSP."""
    with pytest.raises(CodecError) as exc_info:
        parse_h265_slice_header_light(b"", sps=None, nal_unit_type=19)
    err = exc_info.value
    assert err.kind is CodecErrorKind.TRUNCATED_RBSP
    assert err.codec == "h265"


# ---------------------------------------------------------------------------
# H265SliceType enum shape
# ---------------------------------------------------------------------------


def test_h265_slice_type_variants():
    """All expected H265SliceType variants are accessible."""
    # PyO3 eq_int enums are not iterable like Python enum.Enum — check directly.
    assert H265SliceType.I is not None
    assert H265SliceType.P is not None
    assert H265SliceType.B is not None
    assert H265SliceType.Unknown is not None
