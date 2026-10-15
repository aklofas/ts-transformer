package org.tstrans.klv;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.beBytes;
import static org.tstrans.TestSupport.cat;

import java.nio.ByteBuffer;
import java.util.HexFormat;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.tstrans.KlvDecodeException;
import org.tstrans.KlvEncodeException;

/**
 * ST 0806.4 RVT (Remote Video Terminal) Local Set decode/encode tests.
 *
 * <p>Fixture byte sequences are ported from the tst-py test suite
 * ({@code test_klv_st0806.py}), which in turn ports the hand-built
 * fixtures in the Rust {@code crates/tst-core/src/klv/st0806/tests.rs}
 * (the spec ships no vectors of its own).
 */
class St0806Test {

    private static final byte[] RVT_LS_UL =
            HexFormat.of().parseHex("060e2b34020b01010e01030102000000");

    // -----------------------------------------------------------------------
    // Fixture helpers (mirroring St0903Test's cat/beBytes idiom)
    // -----------------------------------------------------------------------

    /**
     * Timestamp + true airspeed + one POI (number=7, lat=45.0, lon=-90.0).
     *
     * <p>POI lat 45.0 -&gt; round(45/90 * (2^31-1)) + 1 = 0x4000_0000
     * (symmetric int32 mapping, ST 0806.4 Table 8-2 Tag 2); lon -90.0 -&gt;
     * 0xC000_0000. Ported verbatim from tst-py's {@code _body_with_poi()}.
     */
    private static byte[] bodyWithPoi() {
        byte[] poi = cat(
                new byte[]{0x01, 0x02, 0x00, 0x07},
                new byte[]{0x02, 0x04, 0x40, 0x00, 0x00, 0x00},
                new byte[]{0x03, 0x04, (byte) 0xC0, 0x00, 0x00, 0x00});
        return cat(
                new byte[]{0x02, 0x08},
                beBytes(1_700_000_000_000_000L, 8),
                new byte[]{0x03, 0x02, 0x00, 0x64}, // Tag 3, len 2, 100 m/s
                new byte[]{0x0C, (byte) poi.length},
                poi);
    }

    // -----------------------------------------------------------------------
    // decodeRvt (body form)
    // -----------------------------------------------------------------------

    @Test
    void decodeRvtBodyScalarsAndPoi() throws KlvDecodeException {
        RvtLs ls = Klv.decodeRvt(bodyWithPoi());
        assertNotNull(ls);
        assertEquals(1_700_000_000_000_000L, ls.timestampUs());
        assertEquals(100, ls.platformTrueAirspeed());
        assertEquals(1, ls.pointsOfInterest().size());
        RvtPoi poi = ls.pointsOfInterest().get(0);
        assertEquals(7, poi.number());
        assertEquals(45.0, poi.latDeg(), 1e-6);
        assertEquals(-90.0, poi.lonDeg(), 1e-6);
        assertTrue(ls.fieldErrors().isEmpty());
    }

    @Test
    void decodeRvtRepeatablePoisAccumulate() throws KlvDecodeException {
        byte[] b = cat(bodyWithPoi(), new byte[]{0x0C, 0x04, 0x01, 0x02, 0x00, 0x08});
        RvtLs ls = Klv.decodeRvt(b);
        assertEquals(2, ls.pointsOfInterest().size());
        assertEquals(8, ls.pointsOfInterest().get(1).number());
    }

    @Test
    void decodeRvtPoiErrorSentinelRecorded() throws KlvDecodeException {
        // POI lat = 0x80000000 -> spec "error" sentinel: field null, tag recorded.
        byte[] b = new byte[]{0x0C, 0x06, 0x02, 0x04, (byte) 0x80, 0x00, 0x00, 0x00};
        RvtLs ls = Klv.decodeRvt(b);
        RvtPoi poi = ls.pointsOfInterest().get(0);
        assertNull(poi.latDeg());
        assertEquals(List.of(2L), poi.sentinelTags());
    }

    @Test
    void decodeRvtMgrsUint24AndComposite() throws KlvDecodeException {
        // Zone 18 / band+grid "TWL" / easting 80400 (0x013A10) / northing 12000 (0x002EE0).
        byte[] b = new byte[]{
                0x0E, 0x01, 18,
                0x0F, 0x03, 'T', 'W', 'L',
                0x10, 0x03, 0x01, 0x3A, 0x10,
                0x11, 0x03, 0x00, 0x2E, (byte) 0xE0,
        };
        RvtLs ls = Klv.decodeRvt(b);
        assertEquals(18, ls.aircraftMgrsZone());
        assertEquals("TWL", ls.aircraftMgrsBandGrid());
        assertEquals(80_400L, ls.aircraftMgrsEastingM());
        assertEquals(12_000L, ls.aircraftMgrsNorthingM());
        assertEquals("18TWL8040012000", ls.aircraftMgrs());
        assertNull(ls.frameCenterMgrs());
    }

    @Test
    void decodeRvtUserDefinedLsBitfield() throws KlvDecodeException {
        // User Defined LS (RVT Tag 11): tag1 = 0b10_000101 (UINT, id 5), tag2 = 2 bytes.
        byte[] b = new byte[]{
                0x0B, 0x07, 0x01, 0x01, (byte) 0x85, 0x02, 0x02, (byte) 0xBE, (byte) 0xEF,
        };
        RvtLs ls = Klv.decodeRvt(b);
        RvtUserData ud = ls.userDefined().get(0);
        assertEquals(RvtUserDataType.UINT, ud.dataType());
        assertEquals(5, ud.numericId());
        assertArrayEquals(new byte[]{(byte) 0xBE, (byte) 0xEF}, ud.data().array());
    }

    @Test
    void decodeRvtAoiTypeThreeIsReservedPoiTypeThreeIsTarget() throws KlvDecodeException {
        byte[] poiBuf = new byte[]{0x0C, 0x03, 0x05, 0x01, 0x03};
        byte[] aoiBuf = new byte[]{0x0D, 0x03, 0x06, 0x01, 0x03};
        assertEquals(RvtPoiType.TARGET, Klv.decodeRvt(poiBuf).pointsOfInterest().get(0).poiType());
        assertEquals(RvtAoiType.RESERVED, Klv.decodeRvt(aoiBuf).areasOfInterest().get(0).aoiType());
    }

    @Test
    void decodeRvtPoiTypeWireUnknownIsRawCodeWithNullTyped() throws KlvDecodeException {
        // Code 9 is outside 1..=4 -- wire-unknown: poiTypeCode preserves the raw
        // byte, but the typed poiType() accessor is null (same asymmetry as
        // IcingDetected's icingDetectedCode/icingDetected() pair).
        byte[] b = new byte[]{0x0C, 0x03, 0x05, 0x01, 0x09};
        RvtPoi poi = Klv.decodeRvt(b).pointsOfInterest().get(0);
        assertEquals(9, poi.poiTypeCode());
        assertNull(poi.poiType());
    }

    @Test
    void decodeRvtUnknownTagPreserved() throws KlvDecodeException, KlvEncodeException {
        // Tag 200 is outside the top-level 1..=21 table -- round-trip it through
        // encodeRvt (which BER-OID-encodes the tag id) rather than hand-building
        // wire bytes: a raw byte 200 (0xC8) has the BER-OID continuation bit set,
        // so it isn't a valid single-byte tag id.
        RvtLs ls = new RvtLs.Builder()
                .timestampUs(1L)
                .unknown(List.of(new KlvUnknownField(200L, ByteBuffer.wrap(new byte[]{(byte) 0xAA, (byte) 0xBB}))))
                .build();
        RvtLs back = Klv.decodeRvt(Klv.encodeRvt(ls));
        assertTrue(back.unknown().stream().anyMatch(f -> f.tag() == 200));
    }

    @Test
    void decodeRvtEmptyBodyLenient() throws KlvDecodeException {
        RvtLs ls = Klv.decodeRvt(new byte[0]);
        assertNull(ls.timestampUs());
        assertTrue(ls.pointsOfInterest().isEmpty());
        assertTrue(ls.areasOfInterest().isEmpty());
        assertTrue(ls.userDefined().isEmpty());
        assertTrue(ls.unknown().isEmpty());
        assertTrue(ls.fieldErrors().isEmpty());
    }

    // -----------------------------------------------------------------------
    // decodeRvtStandalone (own UL + BER length + CRC-32/MPEG-2 verify)
    // -----------------------------------------------------------------------

    @Test
    void decodeRvtStandaloneRoundTripsAndVerifiesCrc() throws KlvDecodeException, KlvEncodeException {
        RvtLs ls = new RvtLs.Builder()
                .timestampUs(1_700_000_000_000_000L)
                .videoDataRate(2_000_000L)
                .build();
        byte[] good = Klv.encodeRvtStandalone(ls);
        for (int i = 0; i < 16; i++) {
            assertEquals(RVT_LS_UL[i], good[i], "RVT_LS_UL byte " + i + " mismatch");
        }
        RvtLs back = Klv.decodeRvtStandalone(good);
        assertEquals(1_700_000_000_000_000L, back.timestampUs());
        assertEquals(2_000_000L, back.videoDataRate());
    }

    @Test
    void decodeRvtStandaloneCrcMismatchThrows() throws KlvEncodeException {
        // The trailing 4 bytes ARE the declared CRC value itself (Tag 1's value
        // bytes), so flipping only the last byte corrupts the DECLARED value
        // while the RECOMPUTED value (over everything else, unchanged) stays the
        // original correct CRC. Assert the mapper arm's message carries the
        // exact hex the Rust Crc32Mismatch Display impl emits.
        RvtLs ls = new RvtLs.Builder().timestampUs(1L).build();
        byte[] good = Klv.encodeRvtStandalone(ls);
        byte[] bad = good.clone();
        bad[bad.length - 1] ^= (byte) 0xFF;
        long declared = 0;
        long recomputed = 0;
        for (int i = 0; i < 4; i++) {
            declared = (declared << 8) | (bad[bad.length - 4 + i] & 0xFFL);
            recomputed = (recomputed << 8) | (good[good.length - 4 + i] & 0xFFL);
        }
        KlvDecodeException ex = assertThrows(KlvDecodeException.class, () -> Klv.decodeRvtStandalone(bad));
        assertEquals(KlvDecodeException.Kind.CHECKSUM_MISMATCH, ex.kind());
        assertTrue(ex.getMessage().contains(String.format("declared 0x%08x", declared)));
        assertTrue(ex.getMessage().contains(String.format("computed 0x%08x", recomputed)));
    }

    @Test
    void decodeRvtStandaloneBadUniversalLabelThrows() {
        byte[] bad = cat(new byte[16], new byte[]{0x00});
        assertThrows(KlvDecodeException.class, () -> Klv.decodeRvtStandalone(bad));
    }

    // -----------------------------------------------------------------------
    // encodeRvt / encodeRvtStandalone
    // -----------------------------------------------------------------------

    @Test
    void rvtRoundTripBodyForm() throws KlvDecodeException, KlvEncodeException {
        RvtLs ls = new RvtLs.Builder()
                .timestampUs(123L)
                .fragCircleRadiusM(250)
                .pointsOfInterest(List.of(
                        new RvtPoi.Builder().number(7).latDeg(45.0).lonDeg(-90.0).label("ALPHA").build()))
                .build();
        RvtLs back = Klv.decodeRvt(Klv.encodeRvt(ls));
        assertEquals(123L, back.timestampUs());
        assertEquals(250, back.fragCircleRadiusM());
        assertEquals(7, back.pointsOfInterest().get(0).number());
        assertEquals("ALPHA", back.pointsOfInterest().get(0).label());
    }

    @Test
    void rvtRoundTripAllTopLevelScalarFields() throws KlvDecodeException, KlvEncodeException {
        RvtLs ls = new RvtLs.Builder()
                .timestampUs(1_700_000_000_000_000L)
                .platformTrueAirspeed(100)
                .platformIndicatedAirspeed(95)
                .telemetryAccuracyIndicator(3)
                .fragCircleRadiusM(250)
                .frameCode(60L)
                .rvtLsVersion(4)
                .videoDataRate(2_000_000L)
                .digitalVideoFileFormat("MPEG-2")
                .aircraftMgrsZone(18)
                .aircraftMgrsBandGrid("TWL")
                .aircraftMgrsEastingM(80_400L)
                .aircraftMgrsNorthingM(12_000L)
                .frameCenterMgrsZone(19)
                .frameCenterMgrsBandGrid("ABC")
                .frameCenterMgrsEastingM(1L)
                .frameCenterMgrsNorthingM(2L)
                .build();
        RvtLs back = Klv.decodeRvt(Klv.encodeRvt(ls));
        assertEquals(ls.timestampUs(), back.timestampUs());
        assertEquals(ls.platformTrueAirspeed(), back.platformTrueAirspeed());
        assertEquals(ls.platformIndicatedAirspeed(), back.platformIndicatedAirspeed());
        assertEquals(ls.telemetryAccuracyIndicator(), back.telemetryAccuracyIndicator());
        assertEquals(ls.fragCircleRadiusM(), back.fragCircleRadiusM());
        assertEquals(ls.frameCode(), back.frameCode());
        assertEquals(ls.rvtLsVersion(), back.rvtLsVersion());
        assertEquals(ls.videoDataRate(), back.videoDataRate());
        assertEquals(ls.digitalVideoFileFormat(), back.digitalVideoFileFormat());
        assertEquals("18TWL8040012000", back.aircraftMgrs());
        assertEquals("19ABC0000100002", back.frameCenterMgrs());
    }

    @Test
    void rvtRoundTripPoiAllFields() throws KlvDecodeException, KlvEncodeException {
        RvtPoi poi = new RvtPoi.Builder()
                .number(7)
                .latDeg(45.0)
                .lonDeg(-90.0)
                .altM(1000.0)
                .poiTypeCode(RvtPoiType.TARGET.code())
                .text("a POI")
                .sourceIcon("icon")
                .sourceId("src")
                .label("ALPHA")
                .operationId("op1")
                .build();
        RvtLs ls = new RvtLs.Builder().timestampUs(1L).pointsOfInterest(List.of(poi)).build();
        RvtPoi back = Klv.decodeRvt(Klv.encodeRvt(ls)).pointsOfInterest().get(0);
        assertEquals(poi.number(), back.number());
        assertEquals(poi.latDeg(), back.latDeg(), 1e-6);
        assertEquals(poi.lonDeg(), back.lonDeg(), 1e-6);
        // altM is a coarser uint16 range ([-900, 19000] m over 65536 counts,
        // ~0.3 m/count) -- not lossless like the int32 lat/lon mapping above.
        assertEquals(poi.altM(), back.altM(), 1.0);
        assertEquals(RvtPoiType.TARGET, back.poiType());
        assertEquals(poi.text(), back.text());
        assertEquals(poi.sourceIcon(), back.sourceIcon());
        assertEquals(poi.sourceId(), back.sourceId());
        assertEquals(poi.label(), back.label());
        assertEquals(poi.operationId(), back.operationId());
    }

    @Test
    void rvtRoundTripAoiAllFields() throws KlvDecodeException, KlvEncodeException {
        RvtAoi aoi = new RvtAoi.Builder()
                .number(2)
                .cornerLatP1Deg(10.0)
                .cornerLonP1Deg(20.0)
                .cornerLatP3Deg(5.0)
                .cornerLonP3Deg(25.0)
                .aoiTypeCode(RvtAoiType.RESERVED.code())
                .text("an AOI")
                .sourceId("src2")
                .label("BRAVO")
                .operationId("op2")
                .build();
        RvtLs ls = new RvtLs.Builder().timestampUs(1L).areasOfInterest(List.of(aoi)).build();
        RvtAoi back = Klv.decodeRvt(Klv.encodeRvt(ls)).areasOfInterest().get(0);
        assertEquals(aoi.number(), back.number());
        assertEquals(aoi.cornerLatP1Deg(), back.cornerLatP1Deg(), 1e-6);
        assertEquals(aoi.cornerLonP1Deg(), back.cornerLonP1Deg(), 1e-6);
        assertEquals(aoi.cornerLatP3Deg(), back.cornerLatP3Deg(), 1e-6);
        assertEquals(aoi.cornerLonP3Deg(), back.cornerLonP3Deg(), 1e-6);
        assertEquals(RvtAoiType.RESERVED, back.aoiType());
        assertEquals(aoi.text(), back.text());
        assertEquals(aoi.sourceId(), back.sourceId());
        assertEquals(aoi.label(), back.label());
        assertEquals(aoi.operationId(), back.operationId());
    }

    @Test
    void rvtRoundTripUserDefined() throws KlvDecodeException, KlvEncodeException {
        RvtUserData ud = new RvtUserData(0b10_000101, ByteBuffer.wrap(new byte[]{(byte) 0xBE, (byte) 0xEF}));
        RvtLs ls = new RvtLs.Builder().timestampUs(1L).userDefined(List.of(ud)).build();
        RvtUserData back = Klv.decodeRvt(Klv.encodeRvt(ls)).userDefined().get(0);
        assertEquals(ud.numericIdRaw(), back.numericIdRaw());
        assertArrayEquals(ud.data().array(), back.data().array());
        assertEquals(RvtUserDataType.UINT, back.dataType());
        assertEquals(5, back.numericId());
    }

    @Test
    void rvtStandaloneEmitsUlTimestampFirstCrcLastAndReverifies() throws KlvDecodeException, KlvEncodeException {
        RvtLs ls = new RvtLs.Builder().timestampUs(1L).videoDataRate(2_000_000L).build();
        byte[] encoded = Klv.encodeRvtStandalone(ls);
        for (int i = 0; i < 16; i++) {
            assertEquals(RVT_LS_UL[i], encoded[i], "RVT_LS_UL byte " + i + " mismatch");
        }
        RvtLs reparsed = Klv.decodeRvtStandalone(encoded); // CRC verify is the assertion
        assertEquals(2_000_000L, reparsed.videoDataRate());
        // Tag 1 (CRC), len 4, is the last 6 bytes of the record.
        assertEquals(0x01, encoded[encoded.length - 6]);
        assertEquals(0x04, encoded[encoded.length - 5]);
    }

    @Test
    void encodeRvtStandaloneWithoutTimestampThrows() {
        RvtLs ls = new RvtLs.Builder().build();
        assertThrows(KlvEncodeException.class, () -> Klv.encodeRvtStandalone(ls));
    }

    @Test
    void encodeRvtPoiMissingNumberThrows() {
        RvtLs ls = new RvtLs.Builder()
                .pointsOfInterest(List.of(new RvtPoi.Builder().latDeg(1.0).lonDeg(2.0).build()))
                .build();
        assertThrows(KlvEncodeException.class, () -> Klv.encodeRvt(ls));
    }

    @Test
    void encodeRvtPoiMissingLatitudeThrows() {
        RvtLs ls = new RvtLs.Builder()
                .pointsOfInterest(List.of(new RvtPoi.Builder().number(1).lonDeg(0.0).build()))
                .build();
        assertThrows(KlvEncodeException.class, () -> Klv.encodeRvt(ls));
    }

    @Test
    void encodeRvtAoiMissingTypeThrows() {
        RvtLs ls = new RvtLs.Builder()
                .areasOfInterest(List.of(new RvtAoi.Builder()
                        .number(1)
                        .cornerLatP1Deg(1.0)
                        .cornerLonP1Deg(2.0)
                        .cornerLatP3Deg(3.0)
                        .cornerLonP3Deg(4.0)
                        .build()))
                .build();
        assertThrows(KlvEncodeException.class, () -> Klv.encodeRvt(ls));
    }

    /**
     * Unlike the Rust-layer {@code encode_to_vec} (which raises
     * {@code ReservedTagInUnknown} for this exact collision), the JVM binding's
     * shared {@code read_unknown_list} helper filters typed-tag collisions out
     * of {@code unknown} BEFORE the Rust encoder ever sees them -- "typed wins,
     * drop silently" is this binding's own consistency policy (matches
     * tst-py's {@code py_to_unknown}), so no exception reaches here. Round-trips
     * cleanly with the typed field intact. This is the ACTUAL observed
     * behavior, not the Rust-core behavior a naive reading of the encoder's
     * rustdoc would suggest.
     */
    @Test
    void encodeRvtUnknownTagClobberingTimestampDropped() throws KlvDecodeException, KlvEncodeException {
        RvtLs ls = new RvtLs.Builder()
                .timestampUs(1L)
                .unknown(List.of(new KlvUnknownField(2L, ByteBuffer.wrap(new byte[8]))))
                .build();
        RvtLs back = Klv.decodeRvt(Klv.encodeRvt(ls));
        assertEquals(1L, back.timestampUs());
        assertTrue(back.unknown().isEmpty());
    }

    @Test
    void encodeRvtPoiUnknownTagClobberingNumberDropped() throws KlvDecodeException, KlvEncodeException {
        RvtLs ls = new RvtLs.Builder()
                .pointsOfInterest(List.of(new RvtPoi.Builder()
                        .number(7)
                        .latDeg(10.0)
                        .lonDeg(20.0)
                        .unknown(List.of(new KlvUnknownField(1L, ByteBuffer.wrap(new byte[]{0x00, 0x63}))))
                        .build()))
                .build();
        RvtPoi back = Klv.decodeRvt(Klv.encodeRvt(ls)).pointsOfInterest().get(0);
        assertEquals(7, back.number());
        assertTrue(back.unknown().isEmpty());
    }

    @Test
    void encodeRvtAoiUnknownTagClobberingTypeDropped() throws KlvDecodeException, KlvEncodeException {
        RvtLs ls = new RvtLs.Builder()
                .areasOfInterest(List.of(new RvtAoi.Builder()
                        .number(1)
                        .cornerLatP1Deg(1.0)
                        .cornerLonP1Deg(2.0)
                        .cornerLatP3Deg(3.0)
                        .cornerLonP3Deg(4.0)
                        .aoiTypeCode(RvtAoiType.FRIENDLY.code())
                        .unknown(List.of(new KlvUnknownField(6L, ByteBuffer.wrap(new byte[]{0x02}))))
                        .build()))
                .build();
        RvtAoi back = Klv.decodeRvt(Klv.encodeRvt(ls)).areasOfInterest().get(0);
        assertEquals(RvtAoiType.FRIENDLY, back.aoiType());
        assertTrue(back.unknown().isEmpty());
    }

    @Test
    void encodeRvtUnknownFieldsPassThroughWhenNotTyped() throws KlvDecodeException, KlvEncodeException {
        // Tag 200 is outside both the top-level 1..=21 table and the POI/AOI
        // 1..=10 range -- must round-trip verbatim, the clobber guard must not
        // reject tags it doesn't own.
        RvtLs ls = new RvtLs.Builder()
                .timestampUs(1L)
                .unknown(List.of(new KlvUnknownField(200L, ByteBuffer.wrap(new byte[]{(byte) 0xAA, (byte) 0xBB}))))
                .pointsOfInterest(List.of(new RvtPoi.Builder()
                        .number(1)
                        .latDeg(10.0)
                        .lonDeg(20.0)
                        .unknown(List.of(new KlvUnknownField(200L, ByteBuffer.wrap(new byte[]{(byte) 0xCC}))))
                        .build()))
                .areasOfInterest(List.of(new RvtAoi.Builder()
                        .number(2)
                        .cornerLatP1Deg(1.0)
                        .cornerLonP1Deg(2.0)
                        .cornerLatP3Deg(3.0)
                        .cornerLonP3Deg(4.0)
                        .aoiTypeCode(RvtAoiType.FRIENDLY.code())
                        .unknown(List.of(new KlvUnknownField(200L, ByteBuffer.wrap(new byte[]{(byte) 0xDD}))))
                        .build()))
                .build();
        RvtLs back = Klv.decodeRvt(Klv.encodeRvt(ls));
        assertEquals(1, back.unknown().size());
        assertEquals(200L, back.unknown().get(0).tag());
        assertArrayEquals(new byte[]{(byte) 0xAA, (byte) 0xBB}, back.unknown().get(0).value().array());
        assertEquals(1, back.pointsOfInterest().get(0).unknown().size());
        assertArrayEquals(new byte[]{(byte) 0xCC}, back.pointsOfInterest().get(0).unknown().get(0).value().array());
        assertEquals(1, back.areasOfInterest().get(0).unknown().size());
        assertArrayEquals(new byte[]{(byte) 0xDD}, back.areasOfInterest().get(0).unknown().get(0).value().array());
    }
}
