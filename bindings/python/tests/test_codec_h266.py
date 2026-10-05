"""H.266 codec surface tests.

Fixture bytes are the RBSP payloads used by tst-core's Rust H.266 parameter-set
tests — extracted from the on-disk binaries at
``crates/tst-core/tests/fixtures/codec/h266/``.

Note: H.266 SliceHeaderLight returns SENTINEL values for ``slice_type``
(always ``H266SliceType.I``) and ``pps_id`` (always 0). Only ``idr``,
``first_in_pic``, and ``pic_order_cnt_lsb`` are accurate — the sentinel
constraint is documented on ``H266SliceHeaderLight`` and is tested here.
"""

import pytest

from tstrans.codec import (
    ChromaFormat,
    H266ParameterSets,
    H266Pps,
    H266ProfileTierLevel,
    H266SliceHeaderLight,
    H266SliceType,
    H266Sps,
    H266Vps,
    NalUnit,
    parse_h266_parameter_sets,
    parse_h266_pps,
    parse_h266_slice_header_light,
    parse_h266_sps,
    parse_h266_vps,
)
from tstrans.exceptions import CodecError, CodecErrorKind

# ---------------------------------------------------------------------------
# Real fixtures — 320×240 Main10@Level4.0 (20 bytes SPS, 2 bytes PPS, 2 bytes VPS)
# ---------------------------------------------------------------------------
# Source: crates/tst-core/tests/fixtures/codec/h266/h266_320x240_main10_sps.bin
SPS_MAIN10_BYTES = bytes.fromhex("0009023f00000028203c48005db0f80602080002")
# Source: crates/tst-core/tests/fixtures/codec/h266/h266_320x240_main10_pps.bin
PPS_MAIN10_BYTES = bytes.fromhex("0020")
# Source: crates/tst-core/tests/fixtures/codec/h266/h266_320x240_main10_vps.bin
VPS_MAIN10_BYTES = bytes.fromhex("0002")
# Source: crates/tst-core/tests/fixtures/codec/h266/h266_320x240_main10_real_sps.bin
# Real VVenC-encoded 320×240 @ 30fps with timing_hrd parameters.
REAL_SPS_BYTES = bytes.fromhex(
    "00ab02208000008028203c46a00737ffac213656304082700131010410420610842c442164842d4217a3d5a92f"
    "249a92c9116a22f1126a22452444992224d46588842c9085a842f0849a8424524212648425d49084848884245"
    "110849888425d44421228c8424c64212ea3210a0416108203110819220d4804cc104105840082c40202102010"
    "150810080d21020101620402022080405904020242010320202210101642020244040d02024810382062010b20"
    "4088408164204091020d040920838419022d082484388684b91ca8105840082c402021020101508100810cc11"
    "8f14000000300400000078620"
)

# Synthetic IDR slice header (IDR_W_RADL, nal_unit_type=7).
# picture_header_in_slice_header_flag = 1 → byte 0x80.
# The H.266 slice_header_light only reads this single flag bit.
SLICE_HEADER_IDR_BYTES = bytes([0x80])


# ---------------------------------------------------------------------------
# SPS parse tests
# ---------------------------------------------------------------------------


def test_parse_h266_sps_returns_h266_sps():
    """parse_h266_sps returns an H266Sps for well-formed input."""
    sps = parse_h266_sps(SPS_MAIN10_BYTES)
    assert isinstance(sps, H266Sps)


def test_parse_h266_sps_dimensions():
    """SPS dimensions match the expected 320×240 post-crop values."""
    sps = parse_h266_sps(SPS_MAIN10_BYTES)
    assert sps.width == 320
    assert sps.height == 240


def test_parse_h266_sps_chroma_format():
    """320×240 Main10 fixture uses 4:2:0 chroma format."""
    sps = parse_h266_sps(SPS_MAIN10_BYTES)
    assert sps.chroma_format == ChromaFormat.YUV420


def test_parse_h266_sps_ids():
    """sps_id and vps_id are both 0."""
    sps = parse_h266_sps(SPS_MAIN10_BYTES)
    assert sps.sps_id == 0
    assert sps.vps_id == 0


def test_parse_h266_sps_raw_rbsp():
    """raw_rbsp round-trips the original bytes."""
    sps = parse_h266_sps(SPS_MAIN10_BYTES)
    assert sps.raw_rbsp == SPS_MAIN10_BYTES


def test_parse_h266_sps_repr():
    """H266Sps.__repr__ contains dimensions and ids."""
    sps = parse_h266_sps(SPS_MAIN10_BYTES)
    r = repr(sps)
    assert "320" in r
    assert "240" in r


def test_parse_h266_sps_coded_dimensions_no_crop():
    """coded_width/coded_height equal width/height when no conformance window."""
    sps = parse_h266_sps(SPS_MAIN10_BYTES)
    assert sps.coded_width() == sps.width + sps.crop_left + sps.crop_right
    assert sps.coded_height() == sps.height + sps.crop_top + sps.crop_bottom


def test_parse_h266_sps_profile_tier_level():
    """profile_tier_level() returns an H266ProfileTierLevel with correct fields."""
    sps = parse_h266_sps(SPS_MAIN10_BYTES)
    ptl = sps.profile_tier_level()
    assert isinstance(ptl, H266ProfileTierLevel)
    assert ptl.general_level_idc == sps.general_level_idc
    assert ptl.general_tier_flag == sps.general_tier_flag
    assert ptl.general_profile_idc == sps.general_profile_idc


def test_parse_h266_sps_truncated_empty_raises_codec_error():
    """Empty input raises CodecError with kind=TRUNCATED_RBSP."""
    with pytest.raises(CodecError) as exc_info:
        parse_h266_sps(b"")
    err = exc_info.value
    assert err.kind is CodecErrorKind.TRUNCATED_RBSP
    assert err.codec == "h266"


def test_parse_h266_real_sps_frame_rate():
    """Real VVenC SPS recovers frame_rate=30 from timing_hrd parameters."""
    sps = parse_h266_sps(REAL_SPS_BYTES)
    assert sps.width == 320
    assert sps.height == 240
    assert sps.chroma_format == ChromaFormat.YUV420
    fr = sps.frame_rate
    assert fr is not None
    ratio = fr.num / fr.den
    assert abs(ratio - 30.0) < 0.5, f"expected ~30fps, got {fr!r}"


# ---------------------------------------------------------------------------
# PPS parse tests
# ---------------------------------------------------------------------------


def test_parse_h266_pps_returns_h266_pps():
    """parse_h266_pps returns an H266Pps for well-formed input."""
    pps = parse_h266_pps(PPS_MAIN10_BYTES)
    assert isinstance(pps, H266Pps)


def test_parse_h266_pps_ids():
    """pps_id and sps_id are both 0."""
    pps = parse_h266_pps(PPS_MAIN10_BYTES)
    assert pps.pps_id == 0
    assert pps.sps_id == 0


def test_parse_h266_pps_raw_rbsp():
    """raw_rbsp round-trips the original bytes."""
    pps = parse_h266_pps(PPS_MAIN10_BYTES)
    assert pps.raw_rbsp == PPS_MAIN10_BYTES


def test_parse_h266_pps_repr():
    """H266Pps.__repr__ contains pps_id and sps_id."""
    pps = parse_h266_pps(PPS_MAIN10_BYTES)
    r = repr(pps)
    assert "pps_id=0" in r
    assert "sps_id=0" in r


def test_parse_h266_pps_empty_raises_codec_error():
    """Empty input raises CodecError with kind=TRUNCATED_RBSP."""
    with pytest.raises(CodecError) as exc_info:
        parse_h266_pps(b"")
    err = exc_info.value
    assert err.kind is CodecErrorKind.TRUNCATED_RBSP
    assert err.codec == "h266"


# ---------------------------------------------------------------------------
# VPS parse tests
# ---------------------------------------------------------------------------


def test_parse_h266_vps_returns_h266_vps():
    """parse_h266_vps returns an H266Vps for well-formed input."""
    vps = parse_h266_vps(VPS_MAIN10_BYTES)
    assert isinstance(vps, H266Vps)


def test_parse_h266_vps_ids_and_layers():
    """vps_id==0; max_layers and max_sub_layers are both 1 for the minimal fixture."""
    vps = parse_h266_vps(VPS_MAIN10_BYTES)
    assert vps.vps_id == 0
    assert vps.max_layers == 1
    assert vps.max_sub_layers == 1


def test_parse_h266_vps_raw_rbsp():
    """raw_rbsp round-trips the original bytes."""
    vps = parse_h266_vps(VPS_MAIN10_BYTES)
    assert vps.raw_rbsp == VPS_MAIN10_BYTES


def test_parse_h266_vps_repr():
    """H266Vps.__repr__ contains vps_id, max_layers."""
    vps = parse_h266_vps(VPS_MAIN10_BYTES)
    r = repr(vps)
    assert "vps_id=0" in r


def test_parse_h266_vps_empty_raises_codec_error():
    """Empty input raises CodecError with kind=TRUNCATED_RBSP."""
    with pytest.raises(CodecError) as exc_info:
        parse_h266_vps(b"")
    err = exc_info.value
    assert err.kind is CodecErrorKind.TRUNCATED_RBSP
    assert err.codec == "h266"


# ---------------------------------------------------------------------------
# H266ProfileTierLevel tests
# ---------------------------------------------------------------------------


def test_h266_profile_tier_level_repr():
    """H266ProfileTierLevel.__repr__ contains profile_idc, tier, and level."""
    sps = parse_h266_sps(SPS_MAIN10_BYTES)
    ptl = sps.profile_tier_level()
    r = repr(ptl)
    assert "profile_idc=" in r
    assert "level_idc=" in r


def test_h266_profile_tier_level_fields():
    """profile_tier_level() fields match the SPS scalar getters."""
    sps = parse_h266_sps(SPS_MAIN10_BYTES)
    ptl = sps.profile_tier_level()
    assert ptl.general_profile_idc == sps.general_profile_idc
    assert ptl.general_tier_flag == sps.general_tier_flag
    assert ptl.general_level_idc == sps.general_level_idc


# ---------------------------------------------------------------------------
# parse_h266_parameter_sets tests
# ---------------------------------------------------------------------------


def test_parse_h266_parameter_sets_vps_sps_pps():
    """parse_h266_parameter_sets populates all three lists from NAL units."""
    # H.266 V4 Table 5: VPS_NUT=14, SPS_NUT=15, PPS_NUT=16
    vps_nal = NalUnit.h266(nal_type=14, layer_id=0, temporal_id_plus1=1, payload=VPS_MAIN10_BYTES)
    sps_nal = NalUnit.h266(nal_type=15, layer_id=0, temporal_id_plus1=1, payload=SPS_MAIN10_BYTES)
    pps_nal = NalUnit.h266(nal_type=16, layer_id=0, temporal_id_plus1=1, payload=PPS_MAIN10_BYTES)
    ps = parse_h266_parameter_sets([vps_nal, sps_nal, pps_nal])
    assert isinstance(ps, H266ParameterSets)
    assert len(ps.vpses) == 1
    assert len(ps.spses) == 1
    assert len(ps.ppses) == 1


def test_parse_h266_parameter_sets_sps_dimensions():
    """SPS in the returned list has correct dimensions."""
    sps_nal = NalUnit.h266(nal_type=15, layer_id=0, temporal_id_plus1=1, payload=SPS_MAIN10_BYTES)
    pps_nal = NalUnit.h266(nal_type=16, layer_id=0, temporal_id_plus1=1, payload=PPS_MAIN10_BYTES)
    ps = parse_h266_parameter_sets([sps_nal, pps_nal])
    assert len(ps.spses) == 1
    assert ps.spses[0].width == 320
    assert ps.spses[0].height == 240


def test_parse_h266_parameter_sets_skips_non_h266_nals():
    """H.264/H.265 NAL units in the list are silently skipped."""
    sps_nal = NalUnit.h266(nal_type=15, layer_id=0, temporal_id_plus1=1, payload=SPS_MAIN10_BYTES)
    h264_nal = NalUnit.h264(nal_type=7, ref_idc=3, payload=b"\x00" * 8)
    ps = parse_h266_parameter_sets([sps_nal, h264_nal])
    assert len(ps.spses) == 1


def test_parse_h266_parameter_sets_empty_input():
    """Empty list returns an H266ParameterSets with all three lists empty."""
    ps = parse_h266_parameter_sets([])
    assert isinstance(ps, H266ParameterSets)
    assert len(ps.vpses) == 0
    assert len(ps.spses) == 0
    assert len(ps.ppses) == 0


def test_parse_h266_parameter_sets_repr():
    """H266ParameterSets.__repr__ includes n_vps, n_sps, n_pps counts."""
    ps = parse_h266_parameter_sets([])
    r = repr(ps)
    assert "n_vps=0" in r
    assert "n_sps=0" in r
    assert "n_pps=0" in r


# ---------------------------------------------------------------------------
# H266SliceHeaderLight tests — sentinels documented here
# ---------------------------------------------------------------------------


def test_parse_h266_slice_header_light_no_sps():
    """Synthetic IDR slice header parses correctly without SPS context."""
    sh = parse_h266_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=7)
    assert isinstance(sh, H266SliceHeaderLight)


def test_parse_h266_slice_header_light_first_in_pic():
    """first_in_pic is True for the synthetic IDR header (picture_header_in_slice_header_flag=1)."""
    sh = parse_h266_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=7)
    assert sh.first_in_pic is True


def test_parse_h266_slice_header_light_idr_w_radl():
    """idr is True when nal_unit_type == 7 (IDR_W_RADL per H.266 V4 Table 5)."""
    sh = parse_h266_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=7)
    assert sh.idr is True


def test_parse_h266_slice_header_light_idr_n_lp():
    """idr is True when nal_unit_type == 8 (IDR_N_LP per H.266 V4 Table 5)."""
    sh = parse_h266_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=8)
    assert sh.idr is True


def test_parse_h266_slice_header_light_non_idr():
    """idr is False for a TRAIL (non-IDR) NAL type."""
    # nal_unit_type=0 = TRAIL — not IDR
    sh = parse_h266_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=0)
    assert sh.idr is False


def test_parse_h266_slice_header_light_slice_type_sentinel():
    """slice_type always returns H266SliceType.I (sentinel — accurate value deferred)."""
    sh = parse_h266_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=7)
    # Always I regardless of actual slice type — documented sentinel behaviour.
    assert sh.slice_type == H266SliceType.I


def test_parse_h266_slice_header_light_pps_id_sentinel():
    """pps_id always returns 0 (sentinel — accurate value deferred)."""
    sh = parse_h266_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=7)
    assert sh.pps_id == 0


def test_parse_h266_slice_header_light_pic_order_cnt_idr():
    """pic_order_cnt_lsb is Some(0) for IDR slices (implicit per H.266 spec)."""
    sh = parse_h266_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=7)
    assert sh.pic_order_cnt_lsb == 0


def test_parse_h266_slice_header_light_pic_order_cnt_non_idr():
    """pic_order_cnt_lsb is None for non-IDR slices (SPS context required)."""
    sh = parse_h266_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=0)
    assert sh.pic_order_cnt_lsb is None


def test_parse_h266_slice_header_light_raw_rbsp():
    """raw_rbsp round-trips the input bytes."""
    sh = parse_h266_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=7)
    assert sh.raw_rbsp == SLICE_HEADER_IDR_BYTES


def test_parse_h266_slice_header_light_repr():
    """H266SliceHeaderLight.__repr__ includes first, slice_type, and idr."""
    sh = parse_h266_slice_header_light(SLICE_HEADER_IDR_BYTES, sps=None, nal_unit_type=7)
    r = repr(sh)
    assert "first=true" in r
    assert "idr=true" in r


def test_parse_h266_slice_header_light_truncated_raises_codec_error():
    """Empty RBSP raises CodecError with kind=TRUNCATED_RBSP."""
    with pytest.raises(CodecError) as exc_info:
        parse_h266_slice_header_light(b"", sps=None, nal_unit_type=7)
    err = exc_info.value
    assert err.kind is CodecErrorKind.TRUNCATED_RBSP
    assert err.codec == "h266"


# ---------------------------------------------------------------------------
# H266SliceType enum shape
# ---------------------------------------------------------------------------


def test_h266_slice_type_variants():
    """All expected H266SliceType variants are accessible."""
    assert H266SliceType.I is not None
    assert H266SliceType.P is not None
    assert H266SliceType.B is not None
    assert H266SliceType.Unknown is not None
