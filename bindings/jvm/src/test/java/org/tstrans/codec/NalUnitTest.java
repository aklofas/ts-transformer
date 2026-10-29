package org.tstrans.codec;

import static org.junit.jupiter.api.Assertions.assertEquals;

import org.junit.jupiter.api.Test;

class NalUnitTest {
    @Test
    void h264FactoryCopiesThePayload() {
        byte[] p = {1, 2, 3};
        NalUnit n = NalUnit.h264(5, 3, p);
        p[0] = 9;
        assertEquals(1, n.payload().get(0));
    }

    @Test
    void h265FactoryCopiesThePayload() {
        byte[] p = {1, 2, 3};
        NalUnit n = NalUnit.h265(19, 0, 1, p);
        p[0] = 9;
        assertEquals(1, n.payload().get(0));
    }

    @Test
    void h266FactoryCopiesThePayload() {
        byte[] p = {1, 2, 3};
        NalUnit n = NalUnit.h266(7, 0, 1, p);
        p[0] = 9;
        assertEquals(1, n.payload().get(0));
    }
}
