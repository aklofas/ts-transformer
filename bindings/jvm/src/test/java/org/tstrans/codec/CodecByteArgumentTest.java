package org.tstrans.codec;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;

class CodecByteArgumentTest {
    private static final byte[] H264_IDR = {(byte) 0x88, (byte) 0x80};
    private static final byte[] SLICE = {(byte) 0x88, (byte) 0x80, 0x00, 0x00};

    private static void rejects(org.junit.jupiter.api.function.Executable call) {
        var e = assertThrows(IllegalArgumentException.class, call);
        assertTrue(e.getMessage().contains("nalUnitType"), e.getMessage());
    }

    @Test
    void h264NalUnitTypeOutsideAByteIsRejected() throws Exception {
        for (int bad : new int[] {-1, 256}) {
            rejects(() -> Codec.parseH264SliceHeaderLight(H264_IDR, null, bad));
        }
        assertNotNull(Codec.parseH264SliceHeaderLight(H264_IDR, null, 5));
    }

    @Test
    void h265NalUnitTypeOutsideAByteIsRejected() {
        for (int bad : new int[] {-1, 256}) {
            rejects(() -> Codec.parseH265SliceHeaderLight(SLICE, null, bad));
        }
    }

    @Test
    void h266NalUnitTypeOutsideAByteIsRejected() {
        for (int bad : new int[] {-1, 256}) {
            rejects(() -> Codec.parseH266SliceHeaderLight(SLICE, null, bad));
        }
    }

    private static void refusedAsReserved(org.junit.jupiter.api.function.Executable call) {
        var e = assertThrows(org.tstrans.CodecParseException.class, call);
        assertSame(org.tstrans.CodecParseException.Kind.RESERVED_VALUE, e.kind());
    }

    @Test
    void anImpossibleNalUnitTypeIsRefused() throws Exception {
        // nal_unit_type is u(5) in H.264 and H.266 and u(6) in H.265.
        for (int t = 32; t <= 255; t++) {
            final int bad = t;
            refusedAsReserved(() -> Codec.parseH264SliceHeaderLight(H264_IDR, null, bad));
            refusedAsReserved(() -> Codec.parseH266SliceHeaderLight(SLICE, null, bad));
        }
        for (int t = 64; t <= 255; t++) {
            final int bad = t;
            refusedAsReserved(() -> Codec.parseH265SliceHeaderLight(SLICE, null, bad));
        }
        assertNotNull(Codec.parseH264SliceHeaderLight(H264_IDR, null, 31));
        assertNotNull(Codec.parseH265SliceHeaderLight(SLICE, null, 63));
        assertNotNull(Codec.parseH266SliceHeaderLight(SLICE, null, 31));
    }
}
