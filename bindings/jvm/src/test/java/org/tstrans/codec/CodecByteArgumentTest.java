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
}
