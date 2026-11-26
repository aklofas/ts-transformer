package org.tstrans.klv;

import static org.junit.jupiter.api.Assertions.*;

import java.nio.ByteBuffer;
import java.util.HexFormat;
import org.junit.jupiter.api.Test;
import org.tstrans.KlvDecodeException;
import org.tstrans.KlvEncodeException;

/**
 * {@link Klv#decodeVmtiStandalone(byte[], boolean)}: a standalone VMTI packet
 * decoded with its checksum verified, and the strict form. Mirrors
 * {@code test_klv_st0903_standalone.py}.
 */
class St0903StandaloneTest {

    /** The jMISB standalone VMTI test vector: "DSTO_ADSS_VMTI", version 4, checksum 9F97, no Item 6. */
    private static final byte[] JMISB_VECTOR = HexFormat.of().parseHex(
            "060E2B34020B01010E01030306000000"
            + "17030E4453544F5F414453535F564D5449"
            + "040104"
            + "01029F97");

    private static final byte[] CORE_ID = HexFormat.of().parseHex(
            "0170F592F02373364AF8AA9162C00F2EB2DA16B74341000841A0BE365B5AB96A3645");

    @Test
    void theJmisbVectorDecodes() throws KlvDecodeException {
        VmtiLs ls = Klv.decodeVmtiStandalone(JMISB_VECTOR, false);
        assertEquals("DSTO_ADSS_VMTI", ls.vmtiSystemName());
        assertEquals(4, ls.versionNumber());
        KlvDecodeException ex = assertThrows(KlvDecodeException.class,
                () -> Klv.decodeVmtiStandalone(JMISB_VECTOR, true));
        assertEquals(KlvDecodeException.Kind.MISSING_REQUIRED_TAG, ex.kind());
    }

    @Test
    void aFlippedByteIsAChecksumMismatch() {
        byte[] bad = JMISB_VECTOR.clone();
        bad[20] ^= 0x01; // inside the system name
        KlvDecodeException ex = assertThrows(KlvDecodeException.class,
                () -> Klv.decodeVmtiStandalone(bad, false));
        assertEquals(KlvDecodeException.Kind.CHECKSUM_MISMATCH, ex.kind());
    }

    @Test
    void anotherLabelIsRefused() {
        byte[] bad = JMISB_VECTOR.clone();
        bad[12] = 0x01;
        KlvDecodeException ex = assertThrows(KlvDecodeException.class,
                () -> Klv.decodeVmtiStandalone(bad, false));
        assertEquals(KlvDecodeException.Kind.BAD_UNIVERSAL_LABEL, ex.kind());
    }

    @Test
    void ourStrictStandaloneOutputPassesTheStrictDecode() throws KlvDecodeException, KlvEncodeException {
        VmtiLs rec = new VmtiLs.Builder()
                .versionNumber(6)
                .numTargetsReported(0L)
                .precisionTimeStamp(1_700_000_000_000_000L)
                .horizontalFov(45.0)
                .verticalFov(30.0)
                .miisId(ByteBuffer.wrap(CORE_ID))
                .build();
        VmtiLs back = Klv.decodeVmtiStandalone(Klv.encodeVmtiStandaloneStrictCompliance(rec), true);
        assertEquals(rec.precisionTimeStamp(), back.precisionTimeStamp());
        assertEquals(ByteBuffer.wrap(CORE_ID), back.miisId());
    }
}
