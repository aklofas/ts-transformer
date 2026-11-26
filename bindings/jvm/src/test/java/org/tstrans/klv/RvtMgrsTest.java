package org.tstrans.klv;

import static org.junit.jupiter.api.Assertions.*;

import java.util.Optional;
import org.junit.jupiter.api.Test;
import org.tstrans.KlvDecodeException;
import org.tstrans.KlvEncodeException;

/**
 * RVT MGRS components (ST 0806.4 Table 8-1 Items 14-21): the composite
 * accessors and the encoders follow the standard's zone and band rules.
 * Mirrors {@code test_klv_rvt_mgrs.py}.
 */
class RvtMgrsTest {

    /** Items 14-17: zone, band + grid square, easting 80400, northing 12000. */
    private static byte[] mgrsBody(int zone, String bandGrid) {
        byte[] band = bandGrid.getBytes(java.nio.charset.StandardCharsets.US_ASCII);
        byte[] out = new byte[3 + 2 + band.length + 10];
        int i = 0;
        out[i++] = 0x0E;
        out[i++] = 0x01;
        out[i++] = (byte) zone;
        out[i++] = 0x0F;
        out[i++] = (byte) band.length;
        for (byte b : band) {
            out[i++] = b;
        }
        for (int b : new int[] {0x10, 0x03, 0x01, 0x3A, 0x10, 0x11, 0x03, 0x00, 0x2E, 0xE0}) {
            out[i++] = (byte) b;
        }
        return out;
    }

    @Test
    void aValidMgrsReadsAsTheFifteenCharacterString() throws KlvDecodeException {
        RvtLs ls = Klv.decodeRvt(mgrsBody(18, "TWL"));
        assertEquals("18TWL8040012000", ls.aircraftMgrs());
        assertEquals(15, ls.aircraftMgrs().length());
    }

    @Test
    void aDecodedMgrsComponentTheStandardDoesNotDefineHasNoComposite() throws KlvDecodeException {
        Object[][] cases = {{0, "TWL"}, {61, "TWL"}, {18, "tw1"}, {18, "TIO"}};
        for (Object[] c : cases) {
            int zone = (Integer) c[0];
            String band = (String) c[1];
            RvtLs ls = Klv.decodeRvt(mgrsBody(zone, band));
            assertEquals(zone, ls.aircraftMgrsZone(), band);
            assertNull(ls.aircraftMgrs(), zone + " " + band);
            RvtLs fc = new RvtLs.Builder()
                    .frameCenterMgrsZone(zone)
                    .frameCenterMgrsBandGrid(band)
                    .frameCenterMgrsEastingM(1L)
                    .frameCenterMgrsNorthingM(2L)
                    .build();
            assertNull(fc.frameCenterMgrs(), zone + " " + band);
        }
    }

    @Test
    void theEncoderRefusesAnMgrsComponentTheStandardDoesNotDefine() {
        RvtLs[] records = {
            new RvtLs.Builder().aircraftMgrsZone(0).build(),
            new RvtLs.Builder().aircraftMgrsZone(61).build(),
            new RvtLs.Builder().aircraftMgrsBandGrid("tw1").build(),
            new RvtLs.Builder().aircraftMgrsBandGrid("TIO").build(),
            new RvtLs.Builder().frameCenterMgrsZone(0).build(),
            new RvtLs.Builder().frameCenterMgrsBandGrid("TIO").build(),
        };
        long[] tags = {14, 14, 15, 15, 18, 19};
        for (int i = 0; i < records.length; i++) {
            RvtLs rec = records[i];
            KlvEncodeException ex = assertThrows(KlvEncodeException.class, () -> Klv.encodeRvt(rec));
            assertEquals(KlvEncodeException.Kind.OUT_OF_RANGE, ex.kind(), "case " + i);
            assertEquals(Optional.of(tags[i]), ex.tag(), "case " + i);
        }
    }
}
