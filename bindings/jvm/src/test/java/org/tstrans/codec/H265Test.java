package org.tstrans.codec;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.tstrans.TestSupport.unsigned;

import java.util.List;
import org.junit.jupiter.api.Test;
import org.tstrans.CodecParseException;

class H265Test {
    // Real x265-emitted 1080p Main@4.0 fixtures — the exact bytes the tst_core
    // Rust unit tests feed (crates/tst-core/tests/fixtures/codec/h265/
    // h265_1080p_main40_{sps,vps,pps}.bin). Asserted
    // dimensions/level/tier/crop reused verbatim from parse_sps.rs / parse_vps.rs.
    private static final byte[] SPS_1080P_MAIN40 = unsigned(
            1, 1, 96, 0, 0, 3, 0, 144, 0, 0, 3, 0, 0, 3, 0, 120, 160, 3, 192, 128,
            17, 7, 203, 150, 86, 84, 164, 194, 240, 22, 128, 128, 0, 0, 3, 0, 128,
            0, 0, 12, 132);

    private static final byte[] VPS_1080P_MAIN40 = unsigned(
            12, 1, 255, 255, 36, 8, 0, 0, 3, 0, 159, 168, 0, 0, 3, 0, 0, 120, 186,
            2, 64);

    private static final byte[] PPS_1080P_MAIN40 = unsigned(192, 115, 193, 137);

    // Real x265-emitted 1080p Main10@5.0 PQ fixtures (HDR BT.2020 / SMPTE ST 2084).
    private static final byte[] SPS_1080P_MAIN10_50 = unsigned(
            1, 34, 32, 0, 0, 3, 0, 144, 0, 0, 3, 0, 0, 3, 0, 150, 160, 3, 192, 128,
            17, 7, 202, 217, 101, 101, 74, 76, 47, 1, 106, 18, 32, 18, 8, 0, 0, 3,
            0, 8, 0, 0, 3, 1, 144, 64);

    private static final byte[] VPS_1080P_MAIN10_50 = unsigned(
            12, 1, 255, 255, 36, 8, 0, 0, 3, 0, 157, 168, 0, 0, 3, 0, 0, 150, 186,
            2, 64);

    // Synthetic minimal IDR slice header, no SPS context — the exact bytes the
    // Rust slice_header_light test `parse_minimal_first_slice_idr` feeds:
    // first_slice_segment_in_pic_flag=1, no_output_of_prior_pics_flag=0 (IRAP),
    // slice_pic_parameter_set_id=0, slice_type=2 (I).
    private static final byte[] IDR_SLICE_NO_SPS = unsigned(0xAC, 0x80);

    @Test
    void parseSps1080pMain40Dimensions() throws CodecParseException {
        H265Sps sps = Codec.parseH265Sps(SPS_1080P_MAIN40);
        assertEquals(1920L, sps.width());
        assertEquals(1080L, sps.height());
        assertEquals(8, sps.bitDepthLuma());
        assertEquals(8, sps.bitDepthChroma());
        assertSame(ChromaFormat.YUV420, sps.chromaFormat());
        assertEquals(0, sps.spsSeqParameterSetId());
        assertEquals(0, sps.spsVideoParameterSetId());
        assertEquals(120, sps.generalLevelIdc());
        // Coded dims: 1920x1088 with an 8-luma-sample bottom crop.
        assertEquals(1920L, sps.codedWidth());
        assertEquals(1088L, sps.codedHeight());
        assertEquals(0L, sps.cropLeft());
        assertEquals(0L, sps.cropRight());
        assertEquals(0L, sps.cropTop());
        assertEquals(8L, sps.cropBottom());
        // raw_rbsp preserved byte-for-byte.
        assertEquals(SPS_1080P_MAIN40.length, sps.rawRbsp().remaining());
    }

    @Test
    void parseSps1080pMain10ColorAndProfile() throws CodecParseException {
        H265Sps sps = Codec.parseH265Sps(SPS_1080P_MAIN10_50);
        assertEquals(10, sps.bitDepthLuma());
        assertEquals(10, sps.bitDepthChroma());
        assertEquals(150, sps.generalLevelIdc());
        // x265 emits general_profile_idc=2 (Main10) with compat-bit 2 set.
        assertEquals(2, sps.generalProfileIdc());
        assertEquals(0x2000_0000L, sps.generalProfileCompatibilityFlags());
        assertTrue(sps.generalProgressiveSourceFlag());
        assertFalse(sps.generalInterlacedSourceFlag());
        assertTrue(sps.generalFrameOnlyConstraintFlag());
        assertFalse(sps.generalNonPackedConstraintFlag());
        // VUI HDR colour: BT.2020 / SMPTE ST 2084 (PQ) / BT.2020 NCL.
        assertNotNull(sps.color());
        assertSame(ColourPrimaries.BT2020, sps.color().primaries());
        assertSame(TransferCharacteristics.SMPTE_ST2084, sps.color().transfer());
        assertSame(MatrixCoefficients.BT2020_NON_CONSTANT, sps.color().matrix());
    }

    @Test
    void spsProfileTierLevelReconstruction() throws CodecParseException {
        H265Sps sps = Codec.parseH265Sps(SPS_1080P_MAIN10_50);
        H265ProfileTierLevel ptl = sps.profileTierLevel();
        // general_profile_space is always 0 when reconstructed from an SPS.
        assertEquals(0, ptl.generalProfileSpace());
        assertEquals(2, ptl.generalProfileIdc());
        assertEquals(150, ptl.generalLevelIdc());
        assertEquals(0x2000_0000L, ptl.generalProfileCompatibilityFlags());
        assertTrue(ptl.generalProgressiveSourceFlag());
        assertTrue(ptl.generalFrameOnlyConstraintFlag());
    }

    @Test
    void parseSpsEmptyThrowsTruncated() {
        CodecParseException ex = assertThrows(
                CodecParseException.class, () -> Codec.parseH265Sps(new byte[0]));
        assertSame(CodecParseException.Kind.TRUNCATED_RBSP, ex.kind());
        assertEquals("h265", ex.codec());
    }

    @Test
    void parseVps1080pMain40Basics() throws CodecParseException {
        H265Vps vps = Codec.parseH265Vps(VPS_1080P_MAIN40);
        assertEquals(0, vps.vpsVideoParameterSetId());
        assertEquals(120, vps.generalLevelIdc()); // Level 4.0
        assertTrue(vps.generalTierFlag());
    }

    @Test
    void parseVps1080pMain10Basics() throws CodecParseException {
        H265Vps vps = Codec.parseH265Vps(VPS_1080P_MAIN10_50);
        assertEquals(150, vps.generalLevelIdc()); // Level 5.0
        assertTrue(vps.generalTierFlag());
    }

    @Test
    void vpsProfileTierLevelReconstruction() throws CodecParseException {
        H265Vps vps = Codec.parseH265Vps(VPS_1080P_MAIN10_50);
        H265ProfileTierLevel ptl = vps.profileTierLevel();
        assertEquals(0, ptl.generalProfileSpace());
        assertEquals(150, ptl.generalLevelIdc());
        assertTrue(ptl.generalTierFlag());
    }

    @Test
    void parseVpsEmptyThrows() {
        assertThrows(CodecParseException.class, () -> Codec.parseH265Vps(new byte[0]));
    }

    @Test
    void parsePps1080pMain40Basics() throws CodecParseException {
        H265Pps pps = Codec.parseH265Pps(PPS_1080P_MAIN40);
        assertEquals(0, pps.ppsPicParameterSetId());
        assertEquals(0, pps.ppsSeqParameterSetId());
        assertEquals(PPS_1080P_MAIN40.length, pps.rawRbsp().remaining());
    }

    @Test
    void parsePpsEmptyThrows() {
        assertThrows(CodecParseException.class, () -> Codec.parseH265Pps(new byte[0]));
    }

    @Test
    void parseSliceHeaderLightIdrNoSps() throws CodecParseException {
        // nal_unit_type 19 = IDR_W_RADL.
        H265SliceHeaderLight h = Codec.parseH265SliceHeaderLight(IDR_SLICE_NO_SPS, null, 19);
        assertTrue(h.firstInPic());
        assertSame(H265SliceType.I, h.sliceType());
        assertEquals(0, h.ppsId());
        assertNull(h.picOrderCntLsb()); // no SPS supplied
        assertTrue(h.idr());
    }

    @Test
    void parseSliceHeaderLightIdrWithSpsImplicitPoc() throws CodecParseException {
        // With SPS context, pic_order_cnt_lsb is implicitly 0 for IDR slices
        // (H.265 §8.3.1) — read without consuming bits.
        H265Sps sps = Codec.parseH265Sps(SPS_1080P_MAIN40);
        H265SliceHeaderLight h = Codec.parseH265SliceHeaderLight(IDR_SLICE_NO_SPS, sps, 19);
        assertEquals(Integer.valueOf(0), h.picOrderCntLsb());
    }

    @Test
    void parseSliceHeaderLightNonIdrMarksIdrFalse() throws CodecParseException {
        // nal_unit_type 1 = TRAIL_N (not IRAP).
        byte[] trail = unsigned(0xD8);
        H265SliceHeaderLight h = Codec.parseH265SliceHeaderLight(trail, null, 1);
        assertFalse(h.idr());
        assertTrue(h.firstInPic());
        assertSame(H265SliceType.I, h.sliceType());
    }

    @Test
    void parseSliceHeaderLightEmptyThrows() {
        assertThrows(
                CodecParseException.class,
                () -> Codec.parseH265SliceHeaderLight(new byte[0], null, 1));
    }

    @Test
    void parseParameterSetsVpsSpsPps() throws CodecParseException {
        // VPS=32, SPS=33, PPS=34 (H.265 Table 7-1).
        List<NalUnit> nals = List.of(
                NalUnit.h265(32, 0, 1, VPS_1080P_MAIN40),
                NalUnit.h265(33, 0, 1, SPS_1080P_MAIN40),
                NalUnit.h265(34, 0, 1, PPS_1080P_MAIN40));
        H265ParameterSets ps = Codec.parseH265ParameterSets(nals);
        assertEquals(1, ps.vpsById().size());
        assertEquals(1, ps.spsById().size());
        assertEquals(1, ps.ppsById().size());
        assertEquals(1920L, ps.spsById().get(0).width());
        assertEquals(120, ps.vpsById().get(0).generalLevelIdc());
        assertEquals(0, ps.ppsById().get(0).ppsSeqParameterSetId());
    }

    @Test
    void parseParameterSetsSkipsNonParamNals() throws CodecParseException {
        // A slice NAL (type 19) is silently skipped; only VPS/SPS/PPS land.
        byte[] slice = new byte[32];
        for (int i = 0; i < slice.length; i++) {
            slice[i] = (byte) 0xff;
        }
        List<NalUnit> nals = List.of(
                NalUnit.h265(19, 0, 1, slice),
                NalUnit.h265(33, 0, 1, SPS_1080P_MAIN40));
        H265ParameterSets ps = Codec.parseH265ParameterSets(nals);
        assertEquals(1, ps.spsById().size());
        assertEquals(0, ps.vpsById().size());
    }

    @Test
    void aLonePpsGivesEmptyResults() throws CodecParseException {
        // One call is one snapshot: a PPS whose SPS is not in the call is dropped.
        H265ParameterSets ps =
                Codec.parseH265ParameterSets(List.of(NalUnit.h265(34, 0, 1, PPS_1080P_MAIN40)));
        assertTrue(ps.vpsById().isEmpty());
        assertTrue(ps.spsById().isEmpty());
        assertTrue(ps.ppsById().isEmpty());
    }

    @Test
    void continuationSegmentHasNoPoc() throws CodecParseException {
        // TRAIL_R (1), first_slice_segment_in_pic_flag = 0, pps id 0: the bits
        // that follow start slice_segment_address, whose width needs the PPS,
        // so no POC is read.
        H265Sps sps = Codec.parseH265Sps(SPS_1080P_MAIN40);
        H265SliceHeaderLight h =
                Codec.parseH265SliceHeaderLight(unsigned(0x6A, 0x80), sps, 1);
        assertFalse(h.firstInPic());
        assertFalse(h.idr());
        assertNull(h.picOrderCntLsb());
    }
}
