package org.tstrans.klv;

import static org.junit.jupiter.api.Assertions.*;

import java.util.HexFormat;
import org.junit.jupiter.api.Test;
import org.tstrans.KlvDecodeException;

/**
 * Unit tests for the ST 0605 Precision Time Stamp Pack decode/encode surface.
 *
 * <p>Mirrors {@code bindings/python/tests/test_klv_st0605.py} case-for-case
 * plus the encode round-trip and error-kind checks.
 */
class St0605Test {

    /**
     * Build a full 26-byte wire-format ST 0605 pack:
     * 16-byte UL + BER {@code 0x09} + 1-byte status + 8-byte BE timestamp.
     */
    private static byte[] pack(long us, int status) {
        byte[] ul = Klv.precisionTimestampPackUl();
        byte[] out = new byte[26];
        System.arraycopy(ul, 0, out, 0, 16);
        out[16] = 0x09;
        out[17] = (byte) status;
        for (int i = 0; i < 8; i++) {
            out[25 - i] = (byte) (us >>> (8 * i));
        }
        return out;
    }

    // -----------------------------------------------------------------------
    // TimeStatus bit-accessor tests (no JNI, pure Java)
    // -----------------------------------------------------------------------

    @Test
    void timeStatusLockedNormal() {
        TimeStatus s = new TimeStatus(0x1F); // 0b0001_1111
        assertTrue(s.isLocked());
        assertFalse(s.hasDiscontinuity());
        assertFalse(s.isReverseJump());
        assertTrue(s.reservedBitsValid());
    }

    @Test
    void timeStatusLockUnknownNormal() {
        TimeStatus s = new TimeStatus(0x9F); // 0b1001_1111 — lock unknown
        assertFalse(s.isLocked());
        assertFalse(s.hasDiscontinuity());
        assertTrue(s.reservedBitsValid());
    }

    @Test
    void timeStatusDiscontinuityReverse() {
        TimeStatus s = new TimeStatus(0xFF);
        assertFalse(s.isLocked());
        assertTrue(s.hasDiscontinuity());
        assertTrue(s.isReverseJump());
    }

    @Test
    void timeStatusInvalidReservedBits() {
        TimeStatus s = new TimeStatus(0x10); // bits 4-0 are not all 1
        assertFalse(s.reservedBitsValid());
    }

    @Test
    void timeStatusRejectsOutOfRange() {
        assertThrows(IllegalArgumentException.class, () -> new TimeStatus(-1));
        assertThrows(IllegalArgumentException.class, () -> new TimeStatus(256));
    }

    // -----------------------------------------------------------------------
    // Decode tests (JNI path)
    // -----------------------------------------------------------------------

    @Test
    void decodeLocked() throws KlvDecodeException {
        PrecisionTimeStampPack p = Klv.decodePrecisionTimestamp(
                pack(1_753_983_356_565_441L, 0x1F));
        assertTrue(p.timeStatus().isLocked());
        assertTrue(p.timeStatus().reservedBitsValid());
        assertEquals(1_753_983_356_565_441L, p.timestampUs());
    }

    @Test
    void decodeRejectsWrongUl() {
        // Replace the ST 0605 UL with the ST 0601 UL → BAD_UNIVERSAL_LABEL
        byte[] b = pack(0L, 0x1F);
        byte[] st0601ul = HexFormat.of().parseHex("060e2b34020b01010e01030101000000");
        System.arraycopy(st0601ul, 0, b, 0, 16);
        KlvDecodeException ex = assertThrows(KlvDecodeException.class,
                () -> Klv.decodePrecisionTimestamp(b));
        assertEquals(KlvDecodeException.Kind.BAD_UNIVERSAL_LABEL, ex.kind());
    }

    @Test
    void decodeRejectsTruncated() {
        // Buffer too short to parse
        KlvDecodeException ex = assertThrows(KlvDecodeException.class,
                () -> Klv.decodePrecisionTimestamp(new byte[8]));
        assertEquals(KlvDecodeException.Kind.TRUNCATED_SET, ex.kind());
    }

    @Test
    void decodeRejectsWrongBodyLength() {
        // BER length 0x05 instead of 0x09 → MALFORMED_BYTES
        byte[] ul = Klv.precisionTimestampPackUl();
        byte[] buf = new byte[ul.length + 6]; // 16 + 1 (BER) + 5 = 22
        System.arraycopy(ul, 0, buf, 0, 16);
        buf[16] = 0x05;
        KlvDecodeException ex = assertThrows(KlvDecodeException.class,
                () -> Klv.decodePrecisionTimestamp(buf));
        assertEquals(KlvDecodeException.Kind.MALFORMED_BYTES, ex.kind());
    }

    // -----------------------------------------------------------------------
    // Encode tests
    // -----------------------------------------------------------------------

    @Test
    void encodeRoundTrip() throws KlvDecodeException {
        PrecisionTimeStampPack in = new PrecisionTimeStampPack(
                new TimeStatus(0x1F), 1_700_000_000_123_456L);
        byte[] wire = Klv.encodePrecisionTimestamp(in);
        assertEquals(26, wire.length);
        // Decoded value must equal the original
        assertEquals(in, Klv.decodePrecisionTimestamp(wire));
    }

    @Test
    void encodeProducesCorrectUl() {
        PrecisionTimeStampPack p = new PrecisionTimeStampPack(
                new TimeStatus(0x1F), 0L);
        byte[] wire = Klv.encodePrecisionTimestamp(p);
        byte[] expectedUl = Klv.precisionTimestampPackUl();
        assertArrayEquals(expectedUl, java.util.Arrays.copyOf(wire, 16));
    }

    @Test
    void negativeTimestampIsAnIllegalArgument() {
        // Java has no unsigned long: a negative value is a caller bug, not a
        // wrapped microsecond count.
        PrecisionTimeStampPack negative = new PrecisionTimeStampPack(
                new TimeStatus(0x1F), -1L);
        IllegalArgumentException ex = assertThrows(IllegalArgumentException.class,
                () -> Klv.encodePrecisionTimestamp(negative));
        assertTrue(ex.getMessage().contains("PrecisionTimeStampPack.timestampUs"),
                "expected message to name the component; got: " + ex.getMessage());
    }

    @Test
    void packEquality() throws KlvDecodeException {
        PrecisionTimeStampPack a = Klv.decodePrecisionTimestamp(pack(100L, 0x1F));
        PrecisionTimeStampPack b = Klv.decodePrecisionTimestamp(pack(100L, 0x1F));
        PrecisionTimeStampPack c = Klv.decodePrecisionTimestamp(pack(101L, 0x1F));
        assertEquals(a, b);
        assertNotEquals(a, c);
    }
}
