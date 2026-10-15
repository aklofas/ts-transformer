package org.tstrans.klv;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.cat;

import java.util.Arrays;
import java.util.Optional;
import org.junit.jupiter.api.Test;
import org.tstrans.KlvDecodeException;

/**
 * Tests for {@link Klv#parseUniversal(byte[])}, the UL-dispatch façade.
 *
 * <p>Ported from {@code bindings/python/tests/test_parse_klv_universal.py}
 * case-for-case, exercising all four KLV families plus the unknown-UL and
 * short-buffer error paths.
 *
 * <p>Uses {@code instanceof} pattern matching — not {@code switch}-on-sealed —
 * to stay on the JDK 17 baseline.
 */
class ParseUniversalTest {

    // -----------------------------------------------------------------------
    // Shared helpers (mirror tst-py's _ber_short / _ber_long / _tlv / _wrap)
    // -----------------------------------------------------------------------

    /** BER short-form encoding for {@code 0 <= n < 0x80}. */
    private static byte[] berShort(int n) {
        return new byte[]{(byte) n};
    }

    /** BER long-form encoding, choosing the minimal representation. */
    private static byte[] berLong(int n) {
        if (n < 0x80) return new byte[]{(byte) n};
        int nbytes = (n <= 0xFF) ? 1 : (n <= 0xFFFF) ? 2 : (n <= 0xFFFFFF) ? 3 : 4;
        byte[] out = new byte[1 + nbytes];
        out[0] = (byte) (0x80 | nbytes);
        for (int i = nbytes - 1; i >= 0; i--) {
            out[1 + i] = (byte) (n & 0xFF);
            n >>>= 8;
        }
        return out;
    }

    /** 1-byte tag + BER-short length + value TLV. */
    private static byte[] tlv(int tag, byte[] value) {
        byte[] out = new byte[1 + 1 + value.length];
        out[0] = (byte) tag;
        out[1] = (byte) value.length;
        System.arraycopy(value, 0, out, 2, value.length);
        return out;
    }

    /** {@code ul:16 + ber(len(body)) + body}. */
    private static byte[] wrapWithUl(byte[] ul, byte[] body) {
        byte[] ber = berLong(body.length);
        byte[] out = new byte[16 + ber.length + body.length];
        System.arraycopy(ul, 0, out, 0, 16);
        System.arraycopy(ber, 0, out, 16, ber.length);
        System.arraycopy(body, 0, out, 16 + ber.length, body.length);
        return out;
    }

    // -----------------------------------------------------------------------
    // ST 0605 — dispatches to PrecisionTimeStampPack
    // -----------------------------------------------------------------------

    @Test
    void dispatchesSt0605Pack() throws Exception {
        // 16-byte UL + BER 0x09 + 1-byte status + 8-byte BE timestamp.
        byte[] ul = Klv.precisionTimestampPackUl();
        byte[] buf = new byte[26];
        System.arraycopy(ul, 0, buf, 0, 16);
        buf[16] = 0x09;
        buf[17] = 0x1F;  // status byte (locked, all reserved bits valid)
        long ts = 1_700_000_000_000_000L;
        for (int i = 0; i < 8; i++) buf[25 - i] = (byte) (ts >>> (8 * i));

        Optional<KlvSet> result = Klv.parseUniversal(buf);
        assertTrue(result.isPresent(), "expected a PrecisionTimeStampPack result");
        assertTrue(result.get() instanceof PrecisionTimeStampPack,
            "expected PrecisionTimeStampPack, got: " + result.get().getClass().getSimpleName());
        PrecisionTimeStampPack pack = (PrecisionTimeStampPack) result.get();
        assertEquals(ts, pack.timestampUs());
    }

    // -----------------------------------------------------------------------
    // ST 0102 — dispatches to SecurityLs (peel UL + BER then decode body)
    // -----------------------------------------------------------------------

    @Test
    void dispatchesSt0102Standalone() throws Exception {
        // A minimal but structurally valid body: Tag 1 (SecurityClassification) = UNCLASSIFIED.
        byte[] body = tlv(1, new byte[]{(byte) SecurityClassification.UNCLASSIFIED.code()});
        byte[] buf = wrapWithUl(Klv.securityLsUl(), body);

        Optional<KlvSet> result = Klv.parseUniversal(buf);
        assertTrue(result.isPresent(), "expected a SecurityLs result");
        assertTrue(result.get() instanceof SecurityLs,
            "expected SecurityLs, got: " + result.get().getClass().getSimpleName());
        SecurityLs sec = (SecurityLs) result.get();
        assertEquals(Optional.of(SecurityClassification.UNCLASSIFIED),
            sec.securityClassification());
    }

    // -----------------------------------------------------------------------
    // ST 0601 — dispatches to UasDatalinkLs (passes full buffer)
    // -----------------------------------------------------------------------

    @Test
    void dispatchesSt0601Lenient() throws Exception {
        // Reuse the MINIMAL_FIXTURE from St0601Test (Tag 02 timestamp + Tag 65
        // version + Tag 01 checksum), which is the ST 0601 UAS Datalink LS family UL.
        // Hex: the bytes are the synthetic_minimal.klv fixture —
        // 3-tag ST 0601 record (Tag 2 precision timestamp + Tag 65 version + Tag 1
        // checksum), 17-byte body.
        byte[] minimalRecord = java.util.HexFormat.of().parseHex(
            "060e2b34020b01010e0103010100000011020800060a24181e40004101130102aa0a");

        Optional<KlvSet> result = Klv.parseUniversal(minimalRecord);
        assertTrue(result.isPresent(), "expected a UasDatalinkLs result");
        assertTrue(result.get() instanceof UasDatalinkLs,
            "expected UasDatalinkLs, got: " + result.get().getClass().getSimpleName());
    }

    // -----------------------------------------------------------------------
    // ST 0903 — dispatches to VmtiLs (peel UL + BER then decode body)
    // -----------------------------------------------------------------------

    @Test
    void dispatchesSt0903StandaloneEmpty() throws Exception {
        // Empty VMTI body, framed with the VMTI UL.
        byte[] buf = wrapWithUl(Klv.vmtiLsUl(), new byte[0]);

        Optional<KlvSet> result = Klv.parseUniversal(buf);
        assertTrue(result.isPresent(), "expected a VmtiLs result");
        assertTrue(result.get() instanceof VmtiLs,
            "expected VmtiLs, got: " + result.get().getClass().getSimpleName());
        VmtiLs vmti = (VmtiLs) result.get();
        assertEquals(0, vmti.targets().size());
    }

    // -----------------------------------------------------------------------
    // Unknown UL — returns Optional.empty()
    // -----------------------------------------------------------------------

    @Test
    void unknownUlReturnsEmpty() throws Exception {
        // A fake UL starting with the SMPTE designator but with non-matching bytes.
        byte[] fakeUl = new byte[16];
        fakeUl[0] = 0x06;
        fakeUl[1] = 0x0E;
        fakeUl[2] = 0x2B;
        fakeUl[3] = 0x34;
        Arrays.fill(fakeUl, 4, 16, (byte) 0xAA);
        byte[] buf = wrapWithUl(fakeUl, new byte[]{0x01, 0x02, 0x03});

        Optional<KlvSet> result = Klv.parseUniversal(buf);
        assertFalse(result.isPresent(), "expected Optional.empty() for unknown UL");
    }

    // -----------------------------------------------------------------------
    // Short / empty buffers — throws BAD_UNIVERSAL_LABEL
    // -----------------------------------------------------------------------

    @Test
    void shortBufferThrowsBadUniversalLabel() {
        KlvDecodeException ex = assertThrows(KlvDecodeException.class,
            () -> Klv.parseUniversal(new byte[]{0x06, 0x0E, 0x2B, 0x34}));
        assertEquals(KlvDecodeException.Kind.BAD_UNIVERSAL_LABEL, ex.kind());
    }

    @Test
    void emptyBufferThrowsBadUniversalLabel() {
        KlvDecodeException ex = assertThrows(KlvDecodeException.class,
            () -> Klv.parseUniversal(new byte[0]));
        assertEquals(KlvDecodeException.Kind.BAD_UNIVERSAL_LABEL, ex.kind());
    }

    // -----------------------------------------------------------------------
    // ST 0102 BER-peel error: trailing bytes -> MALFORMED_BYTES
    // -----------------------------------------------------------------------

    @Test
    void st0102TrailingBytesThrowsMalformedBytes() {
        // Build a valid framed ST 0102 body then append extra bytes.
        byte[] body = tlv(1, new byte[]{(byte) SecurityClassification.UNCLASSIFIED.code()});
        byte[] framed = wrapWithUl(Klv.securityLsUl(), body);
        byte[] withTrailing = cat(framed, new byte[]{0x00, 0x01});  // 2 extra bytes

        KlvDecodeException ex = assertThrows(KlvDecodeException.class,
            () -> Klv.parseUniversal(withTrailing));
        assertEquals(KlvDecodeException.Kind.MALFORMED_BYTES, ex.kind());
    }

    // -----------------------------------------------------------------------
    // Return type is always a KlvSet implementer
    // -----------------------------------------------------------------------

    @Test
    void returnedTypeImplementsKlvSet() throws Exception {
        byte[] ul = Klv.precisionTimestampPackUl();
        byte[] buf = new byte[26];
        System.arraycopy(ul, 0, buf, 0, 16);
        buf[16] = 0x09;
        buf[17] = 0x1F;
        long ts = 1L;
        for (int i = 0; i < 8; i++) buf[25 - i] = (byte) (ts >>> (8 * i));

        Optional<KlvSet> result = Klv.parseUniversal(buf);
        assertTrue(result.isPresent());
        assertTrue(result.get() instanceof KlvSet,
            "returned type must implement KlvSet");
    }
}
