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

class H266Test {
    // Hand-crafted minimal H.266 fixtures — the exact bytes the tst_core Rust
    // unit tests + gen-h266-fixtures feed (crates/tst-core/tests/fixtures/codec/
    // h266/h266_320x240_main10_{sps,vps,pps}.bin). 320x240 Main10 profile,
    // level 4.0 (63), 8-bit 4:2:0, no VUI / no timing_hrd. Asserted values
    // reused verbatim from codec/h266/tests/sps.rs and vps.rs / pps.rs.
    private static final byte[] SPS_320X240_MAIN10 = unsigned(
            0x00, 0x09, 0x02, 0x3f, 0x00, 0x00, 0x00, 0x28, 0x20, 0x3c, 0x48, 0x00,
            0x5d, 0xb0, 0xf8, 0x06, 0x02, 0x08, 0x00, 0x02);

    // Minimal VPS — vps_id=0, max_layers=1, max_sub_layers=1.
    private static final byte[] VPS_320X240 = unsigned(0x00, 0x02);

    // Minimal PPS — pps_id=0, sps_id=0.
    private static final byte[] PPS_320X240 = unsigned(0x00, 0x20);

    // Synthetic IDR slice RBSP — picture_header_in_slice_header_flag=1
    // (0x80 = 0b1000_0000). The light parser reads only this first bit.
    private static final byte[] IDR_SLICE = unsigned(0x80);

    @Test
    void parseSps320x240Dimensions() throws CodecParseException {
        H266Sps sps = Codec.parseH266Sps(SPS_320X240_MAIN10);
        assertEquals(0, sps.spsId());
        assertEquals(0, sps.vpsId());
        assertEquals(320L, sps.width());
        assertEquals(240L, sps.height());
        assertEquals(8, sps.bitDepthLuma());
        assertEquals(8, sps.bitDepthChroma());
        assertSame(ChromaFormat.YUV420, sps.chromaFormat());
        assertEquals(0L, sps.cropLeft());
        assertEquals(0L, sps.cropRight());
        assertEquals(0L, sps.cropTop());
        assertEquals(0L, sps.cropBottom());
        assertEquals(320L, sps.codedWidth());
        assertEquals(240L, sps.codedHeight());
        // VVenC fixture at this profile emits no VUI / no timing_hrd.
        assertNull(sps.color());
        assertNull(sps.frameRate());
        // raw_rbsp preserved byte-for-byte.
        assertEquals(SPS_320X240_MAIN10.length, sps.rawRbsp().remaining());
    }

    @Test
    void parseSpsNestedProfileTierLevel() throws CodecParseException {
        H266Sps sps = Codec.parseH266Sps(SPS_320X240_MAIN10);
        // PTL is a real nested sub-record on the SPS (not a reconstruction).
        H266ProfileTierLevel ptl = sps.profileTierLevel();
        assertNotNull(ptl);
        assertEquals(1, ptl.generalProfileIdc()); // Main 10
        assertFalse(ptl.generalTierFlag()); // Main tier
        assertEquals(63, ptl.generalLevelIdc()); // Level 4.0
    }

    @Test
    void parseSpsEmptyThrowsTruncated() {
        CodecParseException ex = assertThrows(
                CodecParseException.class, () -> Codec.parseH266Sps(new byte[0]));
        assertSame(CodecParseException.Kind.TRUNCATED_RBSP, ex.kind());
        assertEquals("h266", ex.codec());
    }

    @Test
    void parseVps320x240Basics() throws CodecParseException {
        H266Vps vps = Codec.parseH266Vps(VPS_320X240);
        assertEquals(0, vps.vpsId());
        assertEquals(1, vps.maxLayers());
        assertEquals(1, vps.maxSubLayers());
        assertEquals(VPS_320X240.length, vps.rawRbsp().remaining());
    }

    @Test
    void parseVpsEmptyThrows() {
        assertThrows(CodecParseException.class, () -> Codec.parseH266Vps(new byte[0]));
    }

    @Test
    void parsePps320x240Basics() throws CodecParseException {
        H266Pps pps = Codec.parseH266Pps(PPS_320X240);
        assertEquals(0, pps.ppsId());
        assertEquals(0, pps.spsId());
        assertEquals(PPS_320X240.length, pps.rawRbsp().remaining());
    }

    @Test
    void parsePpsEmptyThrows() {
        assertThrows(CodecParseException.class, () -> Codec.parseH266Pps(new byte[0]));
    }

    @Test
    void parseSliceHeaderLightIdrNoSps() throws CodecParseException {
        // nal_unit_type 7 = IDR_W_RADL.
        H266SliceHeaderLight h = Codec.parseH266SliceHeaderLight(IDR_SLICE, null, 7);
        assertTrue(h.firstInPic());
        assertSame(H266SliceType.I, h.sliceType()); // sentinel
        assertEquals(0, h.ppsId()); // sentinel
        assertEquals(Integer.valueOf(0), h.picOrderCntLsb()); // IDR implicit POC=0
        assertTrue(h.idr());
    }

    @Test
    void parseSliceHeaderLightNonIdr() throws CodecParseException {
        // nal_unit_type 0 = TRAIL — not IDR.
        H266SliceHeaderLight h = Codec.parseH266SliceHeaderLight(IDR_SLICE, null, 0);
        assertFalse(h.idr());
        assertTrue(h.firstInPic());
        assertNull(h.picOrderCntLsb()); // non-IDR POC requires SPS context
    }

    @Test
    void parseSliceHeaderLightWithSpsContext() throws CodecParseException {
        // Passing SPS context re-parses its rawRbsp for the POC bit-width;
        // for IDR the value is still implicitly 0.
        H266Sps sps = Codec.parseH266Sps(SPS_320X240_MAIN10);
        H266SliceHeaderLight h = Codec.parseH266SliceHeaderLight(IDR_SLICE, sps, 8);
        assertEquals(Integer.valueOf(0), h.picOrderCntLsb());
        assertTrue(h.idr());
    }

    @Test
    void parseSliceHeaderLightEmptyThrows() {
        assertThrows(
                CodecParseException.class,
                () -> Codec.parseH266SliceHeaderLight(new byte[0], null, 7));
    }

    @Test
    void parseParameterSetsVpsSpsPps() throws CodecParseException {
        // H.266 V4 Table 5: VPS_NUT=14, SPS_NUT=15, PPS_NUT=16.
        List<NalUnit> nals = List.of(
                NalUnit.h266(14, 0, 1, VPS_320X240),
                NalUnit.h266(15, 0, 1, SPS_320X240_MAIN10),
                NalUnit.h266(16, 0, 1, PPS_320X240));
        H266ParameterSets ps = Codec.parseH266ParameterSets(nals);
        // List-backed (Vec), not Map-backed.
        assertEquals(1, ps.vpses().size());
        assertEquals(1, ps.spses().size());
        assertEquals(1, ps.ppses().size());
        assertEquals(0, ps.vpses().get(0).vpsId());
        assertEquals(320L, ps.spses().get(0).width());
        assertEquals(240L, ps.spses().get(0).height());
        assertEquals(0, ps.ppses().get(0).ppsId());
    }

    @Test
    void parseParameterSetsSkipsNonParamNals() throws CodecParseException {
        // A slice NAL (type 0) is silently skipped; only VPS/SPS/PPS land.
        byte[] slice = new byte[32];
        for (int i = 0; i < slice.length; i++) {
            slice[i] = (byte) 0xff;
        }
        List<NalUnit> nals = List.of(
                NalUnit.h266(0, 0, 1, slice),
                NalUnit.h266(15, 0, 1, SPS_320X240_MAIN10));
        H266ParameterSets ps = Codec.parseH266ParameterSets(nals);
        assertEquals(1, ps.spses().size());
        assertEquals(0, ps.vpses().size());
    }
}
