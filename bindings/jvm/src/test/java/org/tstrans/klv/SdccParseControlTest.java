package org.tstrans.klv;

import static org.junit.jupiter.api.Assertions.*;

import java.util.HexFormat;
import org.junit.jupiter.api.Test;
import org.tstrans.KlvDecodeException;
import org.tstrans.KlvEncodeException;

/**
 * ST 1010.3 SDCC-FLP through {@link Klv#decodeSdccFlp} and
 * {@link Klv#encodeSdccFlpMode2}: a Parse Control bit Table 2 reserves in
 * Mode 2, a binary16 value (ST 0107.5 allows binary16, binary32 and
 * binary64) and the largest matrix the decoder reads (256).
 */
class SdccParseControlTest {

    private static byte[] hex(String s) {
        return HexFormat.of().parseHex(s.replaceAll("\\s+", ""));
    }

    @Test
    void aReservedParseControlBitIsMalformed() {
        // N=1, Mode 2 Parse Control C0 04: Byte 1 bit 6 is reserved.
        KlvDecodeException ex = assertThrows(KlvDecodeException.class,
                () -> Klv.decodeSdccFlp(hex("01 C0 04 3F800000")));
        assertEquals(KlvDecodeException.Kind.MALFORMED_BYTES, ex.kind());
    }

    @Test
    void theSamePackWithoutTheReservedBitDecodes() throws KlvDecodeException {
        SdccFlp m = Klv.decodeSdccFlp(hex("01 80 04 3F800000"));
        assertArrayEquals(new double[] {1.0}, m.stdDevs());
    }

    @Test
    void aBinary16StandardDeviationDecodes() throws KlvDecodeException {
        // Mode 2 Parse Control 80 02: Slen 2, the value 3C 00 = 1.0.
        SdccFlp m = Klv.decodeSdccFlp(hex("01 80 02 3C00"));
        assertArrayEquals(new double[] {1.0}, m.stdDevs());
    }

    @Test
    void moreThan256StandardDeviationsAreOutOfRange() {
        int n = 257;
        double[] stdDevs = new double[n];
        java.util.Arrays.fill(stdDevs, 1.0);
        double[] correlations = new double[n * (n - 1) / 2];
        KlvEncodeException ex = assertThrows(KlvEncodeException.class,
                () -> Klv.encodeSdccFlpMode2(stdDevs, correlations, 2));
        assertEquals(KlvEncodeException.Kind.OUT_OF_RANGE, ex.kind());
    }
}
