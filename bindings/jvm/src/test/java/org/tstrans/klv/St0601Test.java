package org.tstrans.klv;

import static org.junit.jupiter.api.Assertions.*;

import java.util.HexFormat;
import java.util.Optional;
import org.junit.jupiter.api.Test;
import org.tstrans.KlvDecodeException;
import org.tstrans.KlvEncodeException;

/**
 * Tests for {@link Klv#decodeUasDatalink} / {@link Klv#encodeUasDatalink} /
 * {@link Klv#encodeUasDatalinkStrictCompliance} and the composite accessor methods
 * on {@link UasDatalinkLs}.
 *
 * <p>Fixture bytes are the committed {@code crates/tst-core/tests/fixtures/st0601/}
 * synthetic KLV records. Expected field values are pinned from
 * {@code tst_core::klv::st0601::decode} of the same bytes.
 */
class St0601Test {

    /**
     * synthetic_full.klv — ~41 tags populated; used for composite accessor tests
     * and decode→encode→decode round-trip.
     *
     * <p>Expected values (pinned from {@code tst_core::klv::st0601::decode}):
     * <ul>
     *   <li>timestampUs = 1700123456789000</li>
     *   <li>sensorLatDeg ≈ 38.1234560199</li>
     *   <li>sensorLonDeg ≈ -121.6543210026</li>
     *   <li>sensorAltM ≈ 2500.0198</li>
     *   <li>frameCenterLatDeg ≈ 38.0000000158</li>
     *   <li>frameCenterLonDeg ≈ -121.5000000230</li>
     *   <li>frameCenterElevM ≈ 0.0320439</li>
     *   <li>platformHeadingDeg ≈ 123.4498</li>
     *   <li>sensorHfovDeg ≈ 45.0007</li>
     *   <li>sensorVfovDeg ≈ 30.0014</li>
     * </ul>
     */
    private static final byte[] FULL_FIXTURE = HexFormat.of().parseHex(
            "060e2b34020b01010e0103010100000081f4020800060a40d6b75a0803054d2d30303104064e31323334"
                    + "35050257c90602e0000702199908012d0901280a0744524f4e452d410b07454f2d4e4f53450c065747"
                    + "532d38340d04363853a50e04a97d81b90f022bbd1002400011022aab1204800000001304e000000014"
                    + "0400000000150400275254160203d71704360b60b61804a999999a19020b941a0211111b0211111c02"
                    + "11111d02eeef1e02eeef1f02eeef2002eeef210211112f0109300301020332024fff3b064543484f2d"
                    + "314101135204360bbdeb5304a999c8355404360bbdeb5504a9996b005604360b03815704a9996b0058"
                    + "04360b03815904a999c835010239c5");

    /**
     * synthetic_minimal.klv — Tag 02 (timestamp) + Tag 65 (version) + Tag 01
     * (checksum). Crafted to satisfy strict-compliance ordering.
     */
    private static final byte[] MINIMAL_FIXTURE = HexFormat.of().parseHex(
            "060e2b34020b01010e0103010100000011020800060a24181e40004101130102aa0a");

    // -----------------------------------------------------------------------
    // Basic decode tests
    // -----------------------------------------------------------------------

    @Test
    void decodeFullFixtureReturnsUasDatalinkLs() {
        UasDatalinkLs rec = assertDoesNotThrow(() -> Klv.decodeUasDatalink(FULL_FIXTURE));
        assertNotNull(rec);
    }

    @Test
    void decodeFullFixturePopulatesTimestamp() throws KlvDecodeException {
        UasDatalinkLs rec = Klv.decodeUasDatalink(FULL_FIXTURE);
        assertNotNull(rec.timestampUs());
        assertEquals(1700123456789000L, rec.timestampUs());
    }

    @Test
    void decodeFullFixturePopulatesIdentityFields() throws KlvDecodeException {
        UasDatalinkLs rec = Klv.decodeUasDatalink(FULL_FIXTURE);
        assertEquals("M-001", rec.missionId());
        assertEquals("EO-NOSE", rec.imageSourceSensor());
        assertEquals("N12345", rec.platformTailNumber());
        assertEquals("DRONE-A", rec.platformDesignation());
        assertEquals("WGS-84", rec.imageCoordinateSystem());
        assertEquals(19, rec.uasLsVersion());
    }

    @Test
    void decodeFullFixtureNoFieldErrors() throws KlvDecodeException {
        UasDatalinkLs rec = Klv.decodeUasDatalink(FULL_FIXTURE);
        assertTrue(rec.fieldErrors().isEmpty(), "expected no field errors on well-formed input");
    }

    @Test
    void decodeRejectsTruncated() {
        // 4-byte buffer — too short to contain a 16-byte UL
        byte[] truncated = HexFormat.of().parseHex("060e2b34");
        KlvDecodeException ex = assertThrows(KlvDecodeException.class,
                () -> Klv.decodeUasDatalink(truncated));
        assertEquals(KlvDecodeException.Kind.TRUNCATED_SET, ex.kind());
    }

    @Test
    void decodeMinimalFixtureSucceeds() throws KlvDecodeException {
        UasDatalinkLs rec = Klv.decodeUasDatalink(MINIMAL_FIXTURE);
        assertNotNull(rec.timestampUs());
        assertNotNull(rec.uasLsVersion());
    }

    // -----------------------------------------------------------------------
    // Composite accessor tests
    // -----------------------------------------------------------------------

    @Test
    void sensorPositionPopulated() throws KlvDecodeException {
        UasDatalinkLs rec = Klv.decodeUasDatalink(FULL_FIXTURE);
        Optional<GeoPoint> pos = rec.sensorPosition();
        assertTrue(pos.isPresent(), "expected sensorPosition to be present");
        GeoPoint gp = pos.get();
        assertEquals(38.1234560199, gp.latDeg(), 1e-6);
        assertEquals(-121.6543210026, gp.lonDeg(), 1e-6);
        assertEquals(2500.019, gp.altM(), 0.01);
    }

    @Test
    void frameCenterPopulated() throws KlvDecodeException {
        UasDatalinkLs rec = Klv.decodeUasDatalink(FULL_FIXTURE);
        Optional<GeoPoint> fc = rec.frameCenter();
        assertTrue(fc.isPresent(), "expected frameCenter to be present");
        GeoPoint gp = fc.get();
        assertEquals(38.000000015, gp.latDeg(), 1e-6);
        assertEquals(-121.500000023, gp.lonDeg(), 1e-6);
        assertEquals(0.032, gp.altM(), 0.01);
    }

    @Test
    void sensorAttitudePopulated() throws KlvDecodeException {
        UasDatalinkLs rec = Klv.decodeUasDatalink(FULL_FIXTURE);
        Optional<Attitude> att = rec.sensorAttitude();
        assertTrue(att.isPresent(), "expected sensorAttitude to be present");
        Attitude a = att.get();
        assertEquals(180.0, a.headingDeg(), 0.01);
        assertEquals(-45.0, a.pitchDeg(), 0.01);
        assertEquals(0.0, a.rollDeg(), 0.01);
    }

    @Test
    void platformAttitudePopulated() throws KlvDecodeException {
        UasDatalinkLs rec = Klv.decodeUasDatalink(FULL_FIXTURE);
        Optional<Attitude> pa = rec.platformAttitude();
        assertTrue(pa.isPresent(), "expected platformAttitude to be present");
        Attitude a = pa.get();
        assertEquals(123.45, a.headingDeg(), 0.01);
        assertEquals(-5.0, a.pitchDeg(), 0.01);
        assertEquals(10.0, a.rollDeg(), 0.01);
    }

    @Test
    void sensorFovPopulated() throws KlvDecodeException {
        UasDatalinkLs rec = Klv.decodeUasDatalink(FULL_FIXTURE);
        Optional<FieldOfView> fov = rec.sensorFov();
        assertTrue(fov.isPresent(), "expected sensorFov to be present");
        assertEquals(45.0, fov.get().horizontalDeg(), 0.01);
        assertEquals(30.0, fov.get().verticalDeg(), 0.01);
    }

    @Test
    void cornersAbsolutePreferred() throws KlvDecodeException {
        // The full fixture has absolute corner tags (82–89) populated.
        UasDatalinkLs rec = Klv.decodeUasDatalink(FULL_FIXTURE);
        Optional<Corners> c = rec.corners();
        assertTrue(c.isPresent(), "expected corners to be present");
        Corners corners = c.get();
        // p1 should be the absolute cornerLatP1Deg / cornerLonP1Deg value from the fixture
        assertEquals(38.001, corners.p1().latDeg(), 0.001);
        assertEquals(-121.499, corners.p1().lonDeg(), 0.001);
    }

    @Test
    void cornersEmptyWhenFieldsAbsent() {
        // Build a minimal record with only frame center — no corners
        java.nio.ByteBuffer ul = java.nio.ByteBuffer.wrap(
                HexFormat.of().parseHex("060e2b34020b01010e01030101000000"));
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ul)
                .frameCenterLatDeg(38.0)
                .frameCenterLonDeg(-121.5)
                .frameCenterElevM(100.0)
                .build();
        assertTrue(rec.corners().isEmpty(), "expected empty corners when only frame center set");
    }

    @Test
    void cornersOffsetFallback() {
        // Record has frame center + offset fields but NOT absolute corner fields
        java.nio.ByteBuffer ul = java.nio.ByteBuffer.wrap(
                HexFormat.of().parseHex("060e2b34020b01010e01030101000000"));
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ul)
                .frameCenterLatDeg(38.0)
                .frameCenterLonDeg(-121.5)
                .frameCenterElevM(100.0)
                .cornerLatOffsetP1Deg(0.001)
                .cornerLonOffsetP1Deg(-0.001)
                .cornerLatOffsetP2Deg(0.001)
                .cornerLonOffsetP2Deg(0.001)
                .cornerLatOffsetP3Deg(-0.001)
                .cornerLonOffsetP3Deg(0.001)
                .cornerLatOffsetP4Deg(-0.001)
                .cornerLonOffsetP4Deg(-0.001)
                .build();
        Optional<Corners> c = rec.corners();
        assertTrue(c.isPresent(), "expected corners via offset fallback");
        Corners corners = c.get();
        assertEquals(38.001, corners.p1().latDeg(), 1e-9);
        assertEquals(-121.501, corners.p1().lonDeg(), 1e-9);
    }

    @Test
    void sensorPositionEmptyWhenFieldsAbsent() {
        java.nio.ByteBuffer ul = java.nio.ByteBuffer.wrap(
                HexFormat.of().parseHex("060e2b34020b01010e01030101000000"));
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ul)
                .sensorLatDeg(38.0)
                // no lon or alt → should be empty
                .build();
        assertTrue(rec.sensorPosition().isEmpty());
    }

    // -----------------------------------------------------------------------
    // Decode strict / compliance paths
    // -----------------------------------------------------------------------

    @Test
    void complianceModeAcceptsMinimalFixture() throws KlvDecodeException {
        // Minimal fixture was crafted to satisfy compliance (Tag 2 first / Tag 65 / Tag 1 last)
        UasDatalinkLs rec = Klv.decodeUasDatalink(MINIMAL_FIXTURE, false, true);
        assertNotNull(rec.timestampUs());
        assertNotNull(rec.uasLsVersion());
    }

    @Test
    void strictModeAcceptsFullFixture() throws KlvDecodeException {
        // Full fixture has a valid ST 0601 family UL
        UasDatalinkLs rec = Klv.decodeUasDatalink(FULL_FIXTURE, true, false);
        assertNotNull(rec.timestampUs());
    }

    @Test
    void strictModeRejectsWrongUl() {
        // Take the valid full fixture and overwrite the leading 16-byte UL with
        // the VMTI LS UL (not an ST 0601 family UL). Strict mode requires the
        // ST 0601 family UL → decode_strict returns UnexpectedUniversalLabel →
        // mapped to BAD_UNIVERSAL_LABEL.
        byte[] buf = FULL_FIXTURE.clone();
        byte[] vmtiUl = HexFormat.of().parseHex("060e2b34020b01010e01030306000000");
        System.arraycopy(vmtiUl, 0, buf, 0, 16);
        KlvDecodeException ex = assertThrows(KlvDecodeException.class,
                () -> Klv.decodeUasDatalink(buf, true, false));
        assertEquals(KlvDecodeException.Kind.BAD_UNIVERSAL_LABEL, ex.kind());
    }

    // -----------------------------------------------------------------------
    // Encode tests
    // -----------------------------------------------------------------------

    @Test
    void encodeRoundTripManyFields() throws KlvDecodeException, KlvEncodeException {
        // decode → encode → decode, assert many fields survive
        UasDatalinkLs original = Klv.decodeUasDatalink(FULL_FIXTURE);
        byte[] wire = Klv.encodeUasDatalink(original);
        UasDatalinkLs decoded = Klv.decodeUasDatalink(wire);

        // Timestamp
        assertEquals(original.timestampUs(), decoded.timestampUs());
        // Identity strings
        assertEquals(original.missionId(), decoded.missionId());
        assertEquals(original.imageSourceSensor(), decoded.imageSourceSensor());
        assertEquals(original.platformTailNumber(), decoded.platformTailNumber());
        assertEquals(original.platformDesignation(), decoded.platformDesignation());
        assertEquals(original.imageCoordinateSystem(), decoded.imageCoordinateSystem());
        // Platform state
        assertEquals(original.platformHeadingDeg(), decoded.platformHeadingDeg(), 1e-9);
        assertEquals(original.platformPitchDeg(), decoded.platformPitchDeg(), 1e-9);
        assertEquals(original.platformRollDeg(), decoded.platformRollDeg(), 1e-9);
        // Sensor position
        assertEquals(original.sensorLatDeg(), decoded.sensorLatDeg(), 1e-9);
        assertEquals(original.sensorLonDeg(), decoded.sensorLonDeg(), 1e-9);
        assertEquals(original.sensorAltM(), decoded.sensorAltM(), 1e-9);
        // Sensor FOV
        assertEquals(original.sensorHfovDeg(), decoded.sensorHfovDeg(), 1e-9);
        assertEquals(original.sensorVfovDeg(), decoded.sensorVfovDeg(), 1e-9);
        // Frame center
        assertEquals(original.frameCenterLatDeg(), decoded.frameCenterLatDeg(), 1e-9);
        assertEquals(original.frameCenterLonDeg(), decoded.frameCenterLonDeg(), 1e-9);
        assertEquals(original.frameCenterElevM(), decoded.frameCenterElevM(), 1e-9);
        // Slant range
        assertEquals(original.slantRangeM(), decoded.slantRangeM(), 1e-9);
        // Absolute corners
        assertEquals(original.cornerLatP1Deg(), decoded.cornerLatP1Deg(), 1e-9);
        assertEquals(original.cornerLonP1Deg(), decoded.cornerLonP1Deg(), 1e-9);
        // UAS LS version
        assertEquals(original.uasLsVersion(), decoded.uasLsVersion());
        // Composite views survive
        assertEquals(original.sensorPosition(), decoded.sensorPosition());
        assertEquals(original.frameCenter(), decoded.frameCenter());
        assertEquals(original.sensorFov(), decoded.sensorFov());
    }

    @Test
    void encodeStrictComplianceMissingMandatoryTagThrows() {
        // Default record has no timestamp (Tag 2) — strict compliance requires it.
        // The Rust KlvEncodeError::MissingMandatoryItem { tag: 2, .. } is thrown with tag=2.
        java.nio.ByteBuffer ul = java.nio.ByteBuffer.wrap(
                HexFormat.of().parseHex("060e2b34020b01010e01030101000000"));
        UasDatalinkLs rec = new UasDatalinkLs.Builder().universalLabel(ul).build();
        KlvEncodeException ex = assertThrows(KlvEncodeException.class,
                () -> Klv.encodeUasDatalinkStrictCompliance(rec));
        assertEquals(KlvEncodeException.Kind.MISSING_MANDATORY_ITEM, ex.kind());
        // tag-bearing: MISSING_MANDATORY_ITEM carries the offending tag (Tag 2 = timestamp)
        assertTrue(ex.tag().isPresent(), "MISSING_MANDATORY_ITEM must carry a tag");
        assertEquals(2L, ex.tag().get().longValue());
    }

    @Test
    void encodeDeclaredVersionOutOfRangeThrowsIllegalArgument() {
        // declaredVersion is stored as Java int, so values > 255 fit in the field
        // but must be rejected on encode (Rust field is u8).
        java.nio.ByteBuffer ul = java.nio.ByteBuffer.wrap(
                HexFormat.of().parseHex("060e2b34020b01010e01030101000000"));
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ul)
                .declaredVersion(300) // out of u8 range
                .build();
        assertThrows(IllegalArgumentException.class, () -> Klv.encodeUasDatalink(rec));
    }

    @Test
    void encodeSlicedSecurityLocalSetRoundTrips() throws KlvDecodeException, KlvEncodeException {
        // Fix 3: encode a record with a sliced (position/limit-constrained) ByteBuffer
        // as securityLocalSet; the encode path must honour the slice, not read the full
        // backing array.
        //
        // Use a minimal ST 0102 body: Tag 1 = UNCLASSIFIED (0x01, 0x01, 0x01).
        byte[] padding = {(byte) 0xDE, (byte) 0xAD};
        byte[] klvBody = {0x01, 0x01, 0x01};
        byte[] backing = new byte[padding.length + klvBody.length + padding.length];
        System.arraycopy(padding, 0, backing, 0, padding.length);
        System.arraycopy(klvBody, 0, backing, padding.length, klvBody.length);
        System.arraycopy(padding, 0, backing, padding.length + klvBody.length, padding.length);

        // Slice to expose only the 3 klvBody bytes (skip leading + trailing padding).
        java.nio.ByteBuffer sliced = java.nio.ByteBuffer.wrap(
                backing, padding.length, klvBody.length).slice();
        assertEquals(3, sliced.remaining(), "slice should have exactly 3 remaining bytes");

        java.nio.ByteBuffer ul = java.nio.ByteBuffer.wrap(
                HexFormat.of().parseHex("060e2b34020b01010e01030101000000"));
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ul)
                .securityLocalSet(sliced)
                .build();

        byte[] wire = Klv.encodeUasDatalink(rec);
        UasDatalinkLs decoded = Klv.decodeUasDatalink(wire);

        assertNotNull(decoded.securityLocalSet(), "securityLocalSet should survive encode/decode");
        // The decoded securityLocalSet must equal exactly the 3-byte klvBody window.
        byte[] secBytes = new byte[decoded.securityLocalSet().remaining()];
        decoded.securityLocalSet().duplicate().get(secBytes);
        assertArrayEquals(klvBody, secBytes,
                "encoded securityLocalSet must match the sliced window, not the full backing array");
    }

    @Test
    void universalLabelWrongSizeThrowsAtConstruction() {
        assertThrows(IllegalArgumentException.class, () -> {
            java.nio.ByteBuffer ul3 = java.nio.ByteBuffer.wrap(new byte[]{0x06, 0x0e, 0x2b});
            new UasDatalinkLs.Builder().universalLabel(ul3).build();
        });
    }

    @Test
    void encodeMinimalFixtureRoundTrip() throws KlvDecodeException, KlvEncodeException {
        UasDatalinkLs rec = Klv.decodeUasDatalink(MINIMAL_FIXTURE);
        byte[] wire = Klv.encodeUasDatalink(rec);
        UasDatalinkLs decoded = Klv.decodeUasDatalink(wire);
        assertEquals(rec.timestampUs(), decoded.timestampUs());
        assertEquals(rec.uasLsVersion(), decoded.uasLsVersion());
    }

    // -----------------------------------------------------------------------
    // Sentinel round-trip tests
    // -----------------------------------------------------------------------

    /**
     * Tag 6 (Platform Pitch) INT_MIN must decode with a null {@code platformPitchDeg},
     * tag 6 in {@code sentinelTags}, and no field errors.
     *
     * <p>Encodes a record with {@code sentinelTags=(6)} and no typed pitch field, which
     * causes the encoder to emit {@code 0x8000} (i16::MIN) per ST 0601.19 §8.6.
     * Decoding the result must record the sentinel without reporting an error.
     */
    @Test
    void sentinelDecodePopulatesSentinelTagsNotFieldError() throws KlvDecodeException, KlvEncodeException {
        java.nio.ByteBuffer ul = java.nio.ByteBuffer.wrap(
                HexFormat.of().parseHex("060e2b34020b01010e01030101000000"));
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ul)
                .sentinelTags(java.util.List.of(6L))
                .build();
        byte[] wire = Klv.encodeUasDatalink(rec);
        UasDatalinkLs rec2 = Klv.decodeUasDatalink(wire);
        assertNull(rec2.platformPitchDeg(), "INT_MIN sentinel must leave typed field null");
        assertTrue(rec2.sentinelTags().contains(6L), "tag 6 must appear in sentinelTags");
        assertTrue(rec2.fieldErrors().isEmpty(), "sentinel must not produce a field error");
    }

    /**
     * A sentinel record survives two encode/decode cycles with the sentinel preserved.
     */
    @Test
    void sentinelRoundTripsThroughEncode() throws KlvDecodeException, KlvEncodeException {
        java.nio.ByteBuffer ul = java.nio.ByteBuffer.wrap(
                HexFormat.of().parseHex("060e2b34020b01010e01030101000000"));
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ul)
                .sentinelTags(java.util.List.of(6L))
                .build();
        byte[] wire1 = Klv.encodeUasDatalink(rec);
        UasDatalinkLs rec2 = Klv.decodeUasDatalink(wire1);
        byte[] wire2 = Klv.encodeUasDatalink(rec2);
        UasDatalinkLs rec3 = Klv.decodeUasDatalink(wire2);
        assertNull(rec3.platformPitchDeg(), "sentinel field must remain null after re-encode");
        assertTrue(rec3.sentinelTags().contains(6L), "tag 6 must still be a sentinel after re-encode");
        assertTrue(rec3.fieldErrors().isEmpty(), "no field errors after re-encode");
    }

    /**
     * Value-wins: if a typed field is set AND its tag also appears in
     * {@code sentinelTags}, the value must be encoded — not INT_MIN.
     * Re-decoded record must carry the real roll value, not a sentinel.
     */
    @Test
    void sentinelValueWinsOverSentinelTagsEntry() throws KlvDecodeException, KlvEncodeException {
        java.nio.ByteBuffer ul = java.nio.ByteBuffer.wrap(
                HexFormat.of().parseHex("060e2b34020b01010e01030101000000"));
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ul)
                .platformRollDeg(25.0)
                .sentinelTags(java.util.List.of(7L))
                .build();
        byte[] encoded = Klv.encodeUasDatalink(rec);
        UasDatalinkLs rec2 = Klv.decodeUasDatalink(encoded);
        assertNotNull(rec2.platformRollDeg(), "value must survive (not replaced by sentinel)");
        assertEquals(25.0, rec2.platformRollDeg(), 0.5, "value must be close to 25.0");
        assertFalse(rec2.sentinelTags().contains(7L),
                "tag 7 must NOT be a sentinel after value-wins encoding");
    }

    /**
     * A {@code sentinelTags} entry above the u32 range must throw
     * {@link IllegalArgumentException} instead of being silently dropped
     * (regression test for the silent-drop path in the JNI translator).
     */
    @Test
    void sentinelTagsOutOfU32RangeThrows() {
        java.nio.ByteBuffer ul = java.nio.ByteBuffer.wrap(
                HexFormat.of().parseHex("060e2b34020b01010e01030101000000"));
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ul)
                .sentinelTags(java.util.List.of(0x1_0000_0000L))
                .build();
        assertThrows(IllegalArgumentException.class, () -> Klv.encodeUasDatalink(rec));
    }

    // -----------------------------------------------------------------------
    // OutOfRangePolicy overload tests
    // -----------------------------------------------------------------------

    /**
     * Tag 6 (Platform Pitch, ±20°) with value 25.0 is out of range.
     *
     * <p>The default 1-arg {@link Klv#encodeUasDatalink(UasDatalinkLs)} still
     * throws {@link KlvEncodeException} (policy = ERROR). The 2-arg overload
     * with {@link OutOfRangePolicy#INDICATOR} succeeds and the decoded record
     * carries a null {@code platformPitchDeg} with tag 6 in {@code sentinelTags}.
     */
    @Test
    void encodeOutOfRangeIndicatorPolicy() throws KlvDecodeException, KlvEncodeException {
        java.nio.ByteBuffer ul = java.nio.ByteBuffer.wrap(
                HexFormat.of().parseHex("060e2b34020b01010e01030101000000"));
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ul)
                .platformPitchDeg(25.0)
                .build();

        // Default (ERROR) policy still throws for an out-of-range value.
        assertThrows(KlvEncodeException.class, () -> Klv.encodeUasDatalink(rec));

        // INDICATOR policy emits the Out-of-Range special instead of erroring.
        byte[] raw = Klv.encodeUasDatalink(rec, OutOfRangePolicy.INDICATOR);
        UasDatalinkLs back = Klv.decodeUasDatalink(raw);
        assertNull(back.platformPitchDeg(),
                "Out-of-Range indicator must leave platformPitchDeg null");
        assertTrue(back.sentinelTags().contains(6L),
                "tag 6 must appear in sentinelTags after INDICATOR encode");
        assertTrue(back.fieldErrors().isEmpty(),
                "sentinel must not produce a field error on decode");
    }

    /**
     * The 2-arg overload with {@link OutOfRangePolicy#ERROR} behaves identically
     * to the 1-arg overload: it throws for an out-of-range value.
     */
    @Test
    void encodeErrorPolicyThrowsLikeDefault() {
        java.nio.ByteBuffer ul = java.nio.ByteBuffer.wrap(
                HexFormat.of().parseHex("060e2b34020b01010e01030101000000"));
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ul)
                .platformPitchDeg(25.0)
                .build();

        assertThrows(KlvEncodeException.class,
                () -> Klv.encodeUasDatalink(rec, OutOfRangePolicy.ERROR));
    }

    // -----------------------------------------------------------------------
    // New field round-trip tests (mirrors tst-py's test suite;
    // same fixture values as test_klv_encode_st0601.py's _F64_FIELDS /
    // _RAW_FIELDS / enum cases).
    // -----------------------------------------------------------------------

    private static final String BARE_UL_HEX = "060e2b34020b01010e01030101000000";

    private static java.nio.ByteBuffer bareUl() {
        return java.nio.ByteBuffer.wrap(HexFormat.of().parseHex(BARE_UL_HEX));
    }

    private static byte[] bufToArray(java.nio.ByteBuffer buf) {
        byte[] out = new byte[buf.remaining()];
        buf.duplicate().get(out);
        return out;
    }

    @Test
    void wpaNewFieldsDefaultToNull() {
        UasDatalinkLs rec = new UasDatalinkLs.Builder().universalLabel(bareUl()).build();
        assertNull(rec.targetLocationLatDeg());
        assertNull(rec.targetErrorCe90M());
        assertNull(rec.windDirectionDeg());
        assertNull(rec.relativeHumidityPct());
        assertNull(rec.platformVerticalSpeed());
        assertNull(rec.platformSideslipFullDeg());
        assertNull(rec.alternatePlatformLatDeg());
        assertNull(rec.sensorNorthVelocity());
        assertNull(rec.outsideAirTempC());
        assertNull(rec.weaponLoad());
        assertNull(rec.eventStartTimeUs());
        assertNull(rec.alternatePlatformName());
        assertNull(rec.communicationsMethod());
        assertNull(rec.rvt());
        assertNull(rec.sarMiLocalSet());
        assertNull(rec.amendLocalSet());
        assertNull(rec.icingDetectedCode());
        assertNull(rec.icingDetected());
        assertNull(rec.sensorFovNameCode());
        assertNull(rec.sensorFovName());
        assertNull(rec.operationalModeCode());
        assertNull(rec.operationalMode());
    }

    /**
     * Every ranged f64 field survives encode -&gt; decode within its
     * fixed-point quantization step. Values and tolerances are pinned from
     * tst-py's {@code _F64_FIELDS} (same underlying Rust encoder/decoder).
     */
    @Test
    void wpaTableA1RangedFieldsRoundTrip() throws KlvDecodeException, KlvEncodeException {
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .windDirectionDeg(235.924010)
                .windSpeed(69.8039216)
                .staticPressureMbar(3725.18502)
                .densityAltitudeM(14818.6770)
                .targetLocationLatDeg(-79.163850051892850)
                .targetLocationLonDeg(166.40081296041646)
                .targetLocationElevM(18389.0471)
                .targetTrackGateWidthPx(6.0)
                .targetTrackGateHeightPx(30.0)
                .targetErrorCe90M(425.215152)
                .targetErrorLe90M(608.9231)
                .differentialPressureMbar(1191.95850)
                .platformVerticalSpeed(-61.8878750)
                .platformSideslipDeg(-5.08255257)
                .airfieldBarometricPressureMbar(2088.96010)
                .airfieldElevationM(8306.80552)
                .relativeHumidityPct(50.5882353)
                .platformGroundSpeed(140.0)
                .groundRangeM(3506979.0316063400)
                .platformFuelRemainingKg(6420.53864)
                .platformMagneticHeadingDeg(311.868162)
                .alternatePlatformLatDeg(-86.041207348947040)
                .alternatePlatformLonDeg(0.15552755452484243)
                .alternatePlatformAltM(9.44533455)
                .alternatePlatformHeadingDeg(32.6024262)
                .alternatePlatformEllipsoidHeightM(9.44533455)
                .sensorNorthVelocity(25.4977569)
                .sensorEastVelocity(12.1)
                .platformAngleOfAttackFullDeg(-8.6701769841230370)
                .platformSideslipFullDeg(-47.683)
                .build();

        byte[] wire = Klv.encodeUasDatalink(rec);
        UasDatalinkLs decoded = Klv.decodeUasDatalink(wire);

        assertEquals(235.924010, decoded.windDirectionDeg(), 0.01);
        assertEquals(69.8039216, decoded.windSpeed(), 0.6);
        assertEquals(3725.18502, decoded.staticPressureMbar(), 0.12);
        assertEquals(14818.6770, decoded.densityAltitudeM(), 0.46);
        assertEquals(-79.163850051892850, decoded.targetLocationLatDeg(), 1e-6);
        assertEquals(166.40081296041646, decoded.targetLocationLonDeg(), 1e-6);
        assertEquals(18389.0471, decoded.targetLocationElevM(), 0.46);
        assertEquals(6.0, decoded.targetTrackGateWidthPx(), 3.0);
        assertEquals(30.0, decoded.targetTrackGateHeightPx(), 3.0);
        assertEquals(425.215152, decoded.targetErrorCe90M(), 0.1);
        assertEquals(608.9231, decoded.targetErrorLe90M(), 0.1);
        assertEquals(1191.95850, decoded.differentialPressureMbar(), 0.12);
        assertEquals(-61.8878750, decoded.platformVerticalSpeed(), 0.01);
        assertEquals(-5.08255257, decoded.platformSideslipDeg(), 0.001);
        assertEquals(2088.96010, decoded.airfieldBarometricPressureMbar(), 0.12);
        assertEquals(8306.80552, decoded.airfieldElevationM(), 0.46);
        assertEquals(50.5882353, decoded.relativeHumidityPct(), 0.6);
        assertEquals(140.0, decoded.platformGroundSpeed(), 1.5);
        assertEquals(3506979.0316063400, decoded.groundRangeM(), 0.01);
        assertEquals(6420.53864, decoded.platformFuelRemainingKg(), 0.23);
        assertEquals(311.868162, decoded.platformMagneticHeadingDeg(), 0.01);
        assertEquals(-86.041207348947040, decoded.alternatePlatformLatDeg(), 1e-6);
        assertEquals(0.15552755452484243, decoded.alternatePlatformLonDeg(), 1e-6);
        assertEquals(9.44533455, decoded.alternatePlatformAltM(), 0.46);
        assertEquals(32.6024262, decoded.alternatePlatformHeadingDeg(), 0.01);
        assertEquals(9.44533455, decoded.alternatePlatformEllipsoidHeightM(), 0.46);
        assertEquals(25.4977569, decoded.sensorNorthVelocity(), 0.02);
        assertEquals(12.1, decoded.sensorEastVelocity(), 0.02);
        assertEquals(-8.6701769841230370, decoded.platformAngleOfAttackFullDeg(), 1e-6);
        assertEquals(-47.683, decoded.platformSideslipFullDeg(), 1e-6);
        assertTrue(decoded.fieldErrors().isEmpty());
    }

    /**
     * Raw int/string fields are not fixed-point quantized — they
     * round-trip byte-exact. Values pinned from tst-py's {@code _RAW_FIELDS}.
     */
    @Test
    void wpaTableA2RawFieldsRoundTripExact() throws KlvDecodeException, KlvEncodeException {
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .outsideAirTempC(84)
                .weaponLoad(45016)
                .weaponFired(186)
                .laserPrfCode(1743)
                .alternatePlatformName("APACHE")
                .eventStartTimeUs(798039894000000L)
                .streamDesignator("BLUE")
                .operationalBase("BASE01")
                .broadcastSource("HOME")
                .targetId("A123")
                .communicationsMethod("Frequency Modulation")
                .build();

        byte[] wire = Klv.encodeUasDatalink(rec);
        UasDatalinkLs decoded = Klv.decodeUasDatalink(wire);

        assertEquals(84, decoded.outsideAirTempC());
        assertEquals(45016, decoded.weaponLoad());
        assertEquals(186, decoded.weaponFired());
        assertEquals(1743, decoded.laserPrfCode());
        assertEquals("APACHE", decoded.alternatePlatformName());
        assertEquals(798039894000000L, decoded.eventStartTimeUs());
        assertEquals("BLUE", decoded.streamDesignator());
        assertEquals("BASE01", decoded.operationalBase());
        assertEquals("HOME", decoded.broadcastSource());
        assertEquals("A123", decoded.targetId());
        assertEquals("Frequency Modulation", decoded.communicationsMethod());
        assertTrue(decoded.fieldErrors().isEmpty());
    }

    /** Named nested-set raw byte fields round-trip byte-exact. */
    @Test
    void wpaTableA4BytesFieldsRoundTripExact() throws KlvDecodeException, KlvEncodeException {
        byte[] rvtBytes = {(byte) 0xDE, (byte) 0xAD, (byte) 0xBE, (byte) 0xEF, 1};
        byte[] sarMiBytes = {(byte) 0xDE, (byte) 0xAD, (byte) 0xBE, (byte) 0xEF, 2};
        byte[] rangeImageBytes = {(byte) 0xDE, (byte) 0xAD, (byte) 0xBE, (byte) 0xEF, 3};
        byte[] geoRegBytes = {(byte) 0xDE, (byte) 0xAD, (byte) 0xBE, (byte) 0xEF, 4};
        byte[] compositeBytes = {(byte) 0xDE, (byte) 0xAD, (byte) 0xBE, (byte) 0xEF, 5};
        byte[] segmentBytes = {(byte) 0xDE, (byte) 0xAD, (byte) 0xBE, (byte) 0xEF, 6};
        byte[] amendBytes = {(byte) 0xDE, (byte) 0xAD, (byte) 0xBE, (byte) 0xEF, 7};

        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .rvt(java.nio.ByteBuffer.wrap(rvtBytes))
                .sarMiLocalSet(java.nio.ByteBuffer.wrap(sarMiBytes))
                .rangeImageLocalSet(java.nio.ByteBuffer.wrap(rangeImageBytes))
                .geoRegistrationLocalSet(java.nio.ByteBuffer.wrap(geoRegBytes))
                .compositeImagingLocalSet(java.nio.ByteBuffer.wrap(compositeBytes))
                .segmentLocalSet(java.nio.ByteBuffer.wrap(segmentBytes))
                .amendLocalSet(java.nio.ByteBuffer.wrap(amendBytes))
                .build();

        byte[] wire = Klv.encodeUasDatalink(rec);
        UasDatalinkLs decoded = Klv.decodeUasDatalink(wire);

        assertArrayEquals(rvtBytes, bufToArray(decoded.rvt()));
        assertArrayEquals(sarMiBytes, bufToArray(decoded.sarMiLocalSet()));
        assertArrayEquals(rangeImageBytes, bufToArray(decoded.rangeImageLocalSet()));
        assertArrayEquals(geoRegBytes, bufToArray(decoded.geoRegistrationLocalSet()));
        assertArrayEquals(compositeBytes, bufToArray(decoded.compositeImagingLocalSet()));
        assertArrayEquals(segmentBytes, bufToArray(decoded.segmentLocalSet()));
        assertArrayEquals(amendBytes, bufToArray(decoded.amendLocalSet()));
    }

    /**
     * Regression test for the new {@code checked_i8} JNI helper: an
     * {@code outsideAirTempC} value outside the i8 range (-128..=127) must
     * throw, not silently truncate.
     */
    @Test
    void outsideAirTempCOutOfRangeThrowsIllegalArgument() {
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .outsideAirTempC(200) // out of i8 range
                .build();
        assertThrows(IllegalArgumentException.class, () -> Klv.encodeUasDatalink(rec));
    }

    // -----------------------------------------------------------------------
    // Coded enums — known codepoint + wire-unknown round-trip.
    // Enum access goes through the raw-code accessor + <Enum>.fromCode, per
    // the SecurityClassification precedent.
    // -----------------------------------------------------------------------

    @Test
    void icingDetectedKnownCodeRoundTrip() throws KlvDecodeException, KlvEncodeException {
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .icingDetectedCode(IcingDetected.ICING_DETECTED.code())
                .build();
        UasDatalinkLs decoded = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));

        assertEquals(2, decoded.icingDetectedCode());
        assertEquals(IcingDetected.ICING_DETECTED, IcingDetected.fromCode(decoded.icingDetectedCode()));
        assertEquals(IcingDetected.ICING_DETECTED, decoded.icingDetected());
    }

    @Test
    void icingDetectedUnknownCodeRoundTrip() throws KlvDecodeException, KlvEncodeException {
        // A wire-unknown codepoint (not 0/1/2) surfaces as a raw int, not a
        // named enum constant — mirrors the SecurityClassification asymmetry.
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .icingDetectedCode(200)
                .build();
        UasDatalinkLs decoded = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));

        assertEquals(200, decoded.icingDetectedCode());
        assertNull(IcingDetected.fromCode(decoded.icingDetectedCode()), "200 is not a named IcingDetected code");
        assertNull(decoded.icingDetected(), "wire-unknown code must surface as null via the typed accessor");
    }

    @Test
    void sensorFovNameKnownRoundTripIncludingContinuousZoom() throws KlvDecodeException, KlvEncodeException {
        // ContinuousZoom (8) is the spec-discrepancy Table-4 codepoint beyond
        // the item's own [0, 7] definition-table cap.
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .sensorFovNameCode(SensorFovName.CONTINUOUS_ZOOM.code())
                .build();
        UasDatalinkLs decoded = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));

        assertEquals(8, decoded.sensorFovNameCode());
        assertEquals(SensorFovName.CONTINUOUS_ZOOM, SensorFovName.fromCode(decoded.sensorFovNameCode()));
        assertEquals(SensorFovName.CONTINUOUS_ZOOM, decoded.sensorFovName());
    }

    @Test
    void sensorFovNameUnknownCodeRoundTrip() throws KlvDecodeException, KlvEncodeException {
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .sensorFovNameCode(250)
                .build();
        UasDatalinkLs decoded = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));

        assertEquals(250, decoded.sensorFovNameCode());
        assertNull(SensorFovName.fromCode(decoded.sensorFovNameCode()));
        assertNull(decoded.sensorFovName());
    }

    @Test
    void operationalModeOtherModeRoundTrip() throws KlvDecodeException, KlvEncodeException {
        // Spec code 0 ("Other" in Table 5) must round-trip as the named
        // OTHER_MODE constant, NOT as a raw-only value — it's a known
        // codepoint, distinct from a wire-unknown code.
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .operationalModeCode(OperationalMode.OTHER_MODE.code())
                .build();
        UasDatalinkLs decoded = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));

        assertEquals(0, decoded.operationalModeCode());
        assertEquals(OperationalMode.OTHER_MODE, decoded.operationalMode());
    }

    @Test
    void operationalModeKnownRoundTrip() throws KlvDecodeException, KlvEncodeException {
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .operationalModeCode(OperationalMode.TRAINING.code())
                .build();
        UasDatalinkLs decoded = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));

        assertEquals(OperationalMode.TRAINING, decoded.operationalMode());
    }

    @Test
    void operationalModeUnknownCodeRoundTrip() throws KlvDecodeException, KlvEncodeException {
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .operationalModeCode(99)
                .build();
        UasDatalinkLs decoded = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));

        assertEquals(99, decoded.operationalModeCode());
        assertNull(OperationalMode.fromCode(decoded.operationalModeCode()));
        assertNull(decoded.operationalMode());
    }

    // -----------------------------------------------------------------------
    // Spec-vector pins — hand-authored wire bytes, NOT the encoder's
    // own output. Closed-loop round-trips alone can't catch a symmetric
    // transcription error in the JNI layer's locally-duplicated enum
    // wire-code tables (a code swapped identically in both directions
    // round-trips cleanly while emitting the wrong byte on the wire), so
    // each field kind gets at least one decode-from-hand-built-bytes test
    // and one encoded-output-contains-exact-TLV test. Tag/value bytes are
    // pinned from MISB ST 0601.19.
    // -----------------------------------------------------------------------

    /**
     * Assemble a full ST 0601 LS wire buffer from hand-authored TLV hex:
     * {@code UL + short-form BER length + TLVs + trailing Tag 1 checksum}.
     * The checksum is the ST 0601 §6.3 16-bit running sum (even-indexed
     * bytes weighted into the high byte) over every byte from the first UL
     * byte through the checksum item's length byte — the lenient decoder
     * verifies it, so it must be computed, not faked. The helper itself is
     * pinned against the known-good captured fixture by
     * {@link #lsBuilderHelperReproducesMinimalFixture()}.
     */
    private static byte[] buildLs(String tlvHex) {
        byte[] ul = HexFormat.of().parseHex(BARE_UL_HEX);
        byte[] tlvs = HexFormat.of().parseHex(tlvHex);
        int bodyLen = tlvs.length + 4; // + checksum TLV: tag 0x01, len 0x02, 2 value bytes
        if (bodyLen >= 0x80) {
            throw new IllegalArgumentException("test helper only supports short-form BER lengths");
        }
        byte[] out = new byte[16 + 1 + bodyLen];
        System.arraycopy(ul, 0, out, 0, 16);
        out[16] = (byte) bodyLen;
        System.arraycopy(tlvs, 0, out, 17, tlvs.length);
        int csumTag = 17 + tlvs.length;
        out[csumTag] = 0x01;     // Tag 1 (checksum)
        out[csumTag + 1] = 0x02; // length 2
        int bcc = 0;
        for (int i = 0; i < csumTag + 2; i++) { // through the checksum length byte
            bcc += (out[i] & 0xFF) << (8 * ((i + 1) % 2));
        }
        bcc &= 0xFFFF;
        out[csumTag + 2] = (byte) (bcc >>> 8);
        out[csumTag + 3] = (byte) bcc;
        return out;
    }

    /** True when {@code haystack} contains {@code needle} as a contiguous byte run. */
    private static boolean containsBytes(byte[] haystack, byte[] needle) {
        outer:
        for (int i = 0; i + needle.length <= haystack.length; i++) {
            for (int j = 0; j < needle.length; j++) {
                if (haystack[i + j] != needle[j]) continue outer;
            }
            return true;
        }
        return false;
    }

    private static void assertWireContains(byte[] wire, String tlvHex, String what) {
        assertTrue(containsBytes(wire, HexFormat.of().parseHex(tlvHex)),
                "encoded output must contain the exact " + what + " TLV bytes " + tlvHex
                        + "; got " + HexFormat.of().formatHex(wire));
    }

    /**
     * Pins {@link #buildLs} itself (BER framing + §6.3 checksum) against the
     * committed, decoder-accepted MINIMAL_FIXTURE: rebuilding that fixture's
     * two TLVs must reproduce it byte-for-byte, checksum {@code 0xAA0A}
     * included.
     */
    @Test
    void lsBuilderHelperReproducesMinimalFixture() {
        byte[] rebuilt = buildLs("020800060a24181e4000" + "410113");
        assertArrayEquals(MINIMAL_FIXTURE, rebuilt,
                "buildLs must reproduce the captured minimal fixture exactly");
    }

    /** Tag 34 (0x22): spec code 2 = Icing Detected. Decode direction: hand-authored bytes. */
    @Test
    void icingDetectedSpecVectorDecode() throws KlvDecodeException {
        UasDatalinkLs decoded = Klv.decodeUasDatalink(buildLs("220102"));
        assertEquals(2, decoded.icingDetectedCode());
        assertEquals(IcingDetected.ICING_DETECTED, decoded.icingDetected());
        assertTrue(decoded.fieldErrors().isEmpty());
    }

    /** Tag 34 (0x22): the encoded output must carry the exact {@code 22 01 02} TLV. */
    @Test
    void icingDetectedSpecVectorEncode() throws KlvEncodeException {
        byte[] wire = Klv.encodeUasDatalink(new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .icingDetectedCode(IcingDetected.ICING_DETECTED.code())
                .build());
        assertWireContains(wire, "220102", "Tag 34 IcingDetected");
    }

    /**
     * Tag 63 (0x3F): spec code 2 = Medium, and the §8.63.1 Table 4
     * spec-discrepancy codepoint 8 = Continuous Zoom. Decode direction.
     */
    @Test
    void sensorFovNameSpecVectorDecode() throws KlvDecodeException {
        UasDatalinkLs medium = Klv.decodeUasDatalink(buildLs("3f0102"));
        assertEquals(2, medium.sensorFovNameCode());
        assertEquals(SensorFovName.MEDIUM, medium.sensorFovName());
        assertTrue(medium.fieldErrors().isEmpty());

        UasDatalinkLs zoom = Klv.decodeUasDatalink(buildLs("3f0108"));
        assertEquals(8, zoom.sensorFovNameCode());
        assertEquals(SensorFovName.CONTINUOUS_ZOOM, zoom.sensorFovName());
        assertTrue(zoom.fieldErrors().isEmpty());
    }

    /** Tag 63 (0x3F): encoded output carries the exact TLVs for codes 2 and 8. */
    @Test
    void sensorFovNameSpecVectorEncode() throws KlvEncodeException {
        byte[] medium = Klv.encodeUasDatalink(new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .sensorFovNameCode(SensorFovName.MEDIUM.code())
                .build());
        assertWireContains(medium, "3f0102", "Tag 63 SensorFovName=Medium");

        byte[] zoom = Klv.encodeUasDatalink(new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .sensorFovNameCode(SensorFovName.CONTINUOUS_ZOOM.code())
                .build());
        assertWireContains(zoom, "3f0108", "Tag 63 SensorFovName=ContinuousZoom");
    }

    /** Tag 77 (0x4D): spec code 1 = Operational. Decode direction: hand-authored bytes. */
    @Test
    void operationalModeSpecVectorDecode() throws KlvDecodeException {
        UasDatalinkLs decoded = Klv.decodeUasDatalink(buildLs("4d0101"));
        assertEquals(1, decoded.operationalModeCode());
        assertEquals(OperationalMode.OPERATIONAL, decoded.operationalMode());
        assertTrue(decoded.fieldErrors().isEmpty());
    }

    /** Tag 77 (0x4D): the encoded output must carry the exact {@code 4d 01 01} TLV. */
    @Test
    void operationalModeSpecVectorEncode() throws KlvEncodeException {
        byte[] wire = Klv.encodeUasDatalink(new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .operationalModeCode(OperationalMode.OPERATIONAL.code())
                .build());
        assertWireContains(wire, "4d0101", "Tag 77 OperationalMode");
    }

    /**
     * Tag 35 (0x23) Wind Direction: 235.924010 deg
     * maps to value bytes {@code A7 C4} per the ST 0601.19 uint16 [0, 360]
     * linear mapping. Both directions: encode must emit the exact TLV;
     * hand-authored bytes must decode back within the fixed-point step.
     */
    @Test
    void windDirectionSpecVector() throws KlvDecodeException, KlvEncodeException {
        byte[] wire = Klv.encodeUasDatalink(new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .windDirectionDeg(235.924010)
                .build());
        assertWireContains(wire, "2302a7c4", "Tag 35 Wind Direction");

        UasDatalinkLs decoded = Klv.decodeUasDatalink(buildLs("2302a7c4"));
        assertNotNull(decoded.windDirectionDeg());
        assertEquals(235.924010, decoded.windDirectionDeg(), 0.006);
        assertTrue(decoded.fieldErrors().isEmpty());
    }

    /**
     * Tag 39 (0x27) Outside Air Temp: 84 →
     * {@code 54} (raw i8); Tag 60 (0x3C) Weapon Load: 45016 → {@code AF D8}
     * (raw u16). Both directions.
     */
    @Test
    void tableA2SpecVectors() throws KlvDecodeException, KlvEncodeException {
        byte[] tempWire = Klv.encodeUasDatalink(new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .outsideAirTempC(84)
                .build());
        assertWireContains(tempWire, "270154", "Tag 39 Outside Air Temp");

        byte[] weaponWire = Klv.encodeUasDatalink(new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .weaponLoad(45016)
                .build());
        assertWireContains(weaponWire, "3c02afd8", "Tag 60 Weapon Load");

        UasDatalinkLs decoded = Klv.decodeUasDatalink(buildLs("270154" + "3c02afd8"));
        assertEquals(84, decoded.outsideAirTempC());
        assertEquals(45016, decoded.weaponLoad());
        assertTrue(decoded.fieldErrors().isEmpty());
    }

    // -----------------------------------------------------------------------
    // ST 0601.19 §7.5 INT_MIN sentinel-meaning lookup
    // (parity with tst-py's st0601_sentinel_meaning — same four pins).
    // -----------------------------------------------------------------------

    @Test
    void sentinelMeaningLookup() {
        assertEquals("out_of_range", Klv.st0601SentinelMeaning(6L));
        assertEquals("reserved", Klv.st0601SentinelMeaning(13L));
        assertEquals("not_available", Klv.st0601SentinelMeaning(26L));
        assertNull(Klv.st0601SentinelMeaning(5L),
                "tag 5 has no spec-assigned INT_MIN special value");
    }

    // -----------------------------------------------------------------------
    // New field round-trip tests (mirrors tst-py's test suite;
    // same fixture values as test_klv_encode_st0601.py's _IMAPB_F64_FIELDS /
    // _VARINT_FIELDS / imapb_specials cases).
    // -----------------------------------------------------------------------

    @Test
    void wpbNewFieldsDefaultToNull() {
        UasDatalinkLs rec = new UasDatalinkLs.Builder().universalLabel(bareUl()).build();
        assertNull(rec.targetWidthExtendedM());
        assertNull(rec.densityAltitudeExtendedM());
        assertNull(rec.sensorEllipsoidHeightExtendedM());
        assertNull(rec.alternatePlatformEllipsoidHeightExtendedM());
        assertNull(rec.rangeToRecoveryKm());
        assertNull(rec.platformCourseAngleDeg());
        assertNull(rec.altitudeAglM());
        assertNull(rec.radarAltimeterM());
        assertNull(rec.sensorAzimuthRateDps());
        assertNull(rec.sensorElevationRateDps());
        assertNull(rec.sensorRollRateDps());
        assertNull(rec.miStoragePercentFull());
        assertNull(rec.transmissionFrequencyMhz());
        assertNull(rec.zoomPercentage());
        assertNull(rec.timeAirborneS());
        assertNull(rec.propulsionUnitSpeedRpm());
        assertNull(rec.navsatsInView());
        assertNull(rec.positioningMethodSource());
        assertNull(rec.platformStatusCode());
        assertNull(rec.platformStatus());
        assertNull(rec.sensorControlModeCode());
        assertNull(rec.sensorControlMode());
        assertNull(rec.takeOffTimeUs());
        assertNull(rec.miStorageCapacityGb());
        assertNull(rec.leapSeconds());
        assertNull(rec.correctionOffsetUs());
        assertNull(rec.activePayloads());
        assertTrue(rec.imapbSpecials().isEmpty());
    }

    /**
     * Every IMAPB f64 field survives encode -&gt; decode within its
     * fixed-point quantization step. Values and tolerances are pinned from
     * tst-py's {@code _IMAPB_F64_FIELDS} (same underlying Rust encoder/decoder).
     */
    @Test
    void wpbTableB1ImapbFieldsRoundTrip() throws KlvDecodeException, KlvEncodeException {
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .targetWidthExtendedM(13898.5463)
                .densityAltitudeExtendedM(23456.24)
                .sensorEllipsoidHeightExtendedM(23456.24)
                .alternatePlatformEllipsoidHeightExtendedM(23456.24)
                .rangeToRecoveryKm(1.625)
                .platformCourseAngleDeg(125.0)
                .altitudeAglM(2150.0)
                .radarAltimeterM(2154.50)
                .sensorAzimuthRateDps(1.0)
                .sensorElevationRateDps(0.004176)
                .sensorRollRateDps(-50.0)
                .miStoragePercentFull(72.0)
                .transmissionFrequencyMhz(2400.0)
                .zoomPercentage(55.0)
                .build();

        byte[] wire = Klv.encodeUasDatalink(rec);
        UasDatalinkLs decoded = Klv.decodeUasDatalink(wire);

        assertEquals(13898.5463, decoded.targetWidthExtendedM(), 0.5);
        assertEquals(23456.24, decoded.densityAltitudeExtendedM(), 0.015625);
        assertEquals(23456.24, decoded.sensorEllipsoidHeightExtendedM(), 0.015625);
        assertEquals(23456.24, decoded.alternatePlatformEllipsoidHeightExtendedM(), 0.015625);
        assertEquals(1.625, decoded.rangeToRecoveryKm(), 0.0078125);
        assertEquals(125.0, decoded.platformCourseAngleDeg(), 0.03125);
        assertEquals(2150.0, decoded.altitudeAglM(), 0.015625);
        assertEquals(2154.50, decoded.radarAltimeterM(), 0.015625);
        assertEquals(1.0, decoded.sensorAzimuthRateDps(), 0.125);
        assertEquals(0.004176, decoded.sensorElevationRateDps(), 0.00048828125);
        assertEquals(-50.0, decoded.sensorRollRateDps(), 0.125);
        assertEquals(72.0, decoded.miStoragePercentFull(), 0.0078125);
        assertEquals(2400.0, decoded.transmissionFrequencyMhz(), 0.03125);
        assertEquals(55.0, decoded.zoomPercentage(), 0.0078125);
        assertTrue(decoded.fieldErrors().isEmpty());
    }

    /**
     * Var-length int fields are not fixed-point quantized — they
     * round-trip byte-exact. Values pinned from tst-py's {@code _VARINT_FIELDS}.
     */
    @Test
    void wpbTableB2VarIntFieldsRoundTripExact() throws KlvDecodeException, KlvEncodeException {
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .timeAirborneS(19887L)
                .propulsionUnitSpeedRpm(3000L)
                .navsatsInView(7)
                .positioningMethodSource(3)
                .takeOffTimeUs(1_529_588_637_122_999L)
                .miStorageCapacityGb(10000L)
                .leapSeconds(30)
                .correctionOffsetUs(5025678901L)
                .build();

        byte[] wire = Klv.encodeUasDatalink(rec);
        UasDatalinkLs decoded = Klv.decodeUasDatalink(wire);

        assertEquals(19887L, decoded.timeAirborneS());
        assertEquals(3000L, decoded.propulsionUnitSpeedRpm());
        assertEquals(7, decoded.navsatsInView());
        assertEquals(3, decoded.positioningMethodSource());
        assertEquals(1_529_588_637_122_999L, decoded.takeOffTimeUs());
        assertEquals(10000L, decoded.miStorageCapacityGb());
        assertEquals(30, decoded.leapSeconds());
        assertEquals(5025678901L, decoded.correctionOffsetUs());
        assertTrue(decoded.fieldErrors().isEmpty());
    }

    @Test
    void wpbActivePayloadsBytesRoundTripExact() throws KlvDecodeException, KlvEncodeException {
        byte[] payloadBytes = {0x0B};
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .activePayloads(java.nio.ByteBuffer.wrap(payloadBytes))
                .build();
        byte[] wire = Klv.encodeUasDatalink(rec);
        UasDatalinkLs decoded = Klv.decodeUasDatalink(wire);
        assertArrayEquals(payloadBytes, bufToArray(decoded.activePayloads()));
    }

    // -----------------------------------------------------------------------
    // Coded enums — known codepoint + wire-unknown round-trip.
    // -----------------------------------------------------------------------

    @Test
    void platformStatusKnownRoundTrip() throws KlvDecodeException, KlvEncodeException {
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .platformStatusCode(PlatformStatus.EGRESS.code())
                .build();
        UasDatalinkLs decoded = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));

        assertEquals(9, decoded.platformStatusCode());
        assertEquals(PlatformStatus.EGRESS, decoded.platformStatus());
    }

    @Test
    void platformStatusUnknownCodeRoundTrip() throws KlvDecodeException, KlvEncodeException {
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .platformStatusCode(222)
                .build();
        UasDatalinkLs decoded = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));

        assertEquals(222, decoded.platformStatusCode());
        assertNull(PlatformStatus.fromCode(decoded.platformStatusCode()));
        assertNull(decoded.platformStatus());
    }

    @Test
    void sensorControlModeKnownRoundTrip() throws KlvDecodeException, KlvEncodeException {
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .sensorControlModeCode(SensorControlMode.AUTO_TRACKING.code())
                .build();
        UasDatalinkLs decoded = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));

        assertEquals(SensorControlMode.AUTO_TRACKING, decoded.sensorControlMode());
    }

    @Test
    void sensorControlModeUnknownCodeRoundTrip() throws KlvDecodeException, KlvEncodeException {
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .sensorControlModeCode(201)
                .build();
        UasDatalinkLs decoded = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));

        assertEquals(201, decoded.sensorControlModeCode());
        assertNull(decoded.sensorControlMode());
    }

    // -----------------------------------------------------------------------
    // imapb_specials side-channel — both directions.
    // -----------------------------------------------------------------------

    @Test
    void imapbSpecialsPayloadLessCodeRoundTrips() throws KlvDecodeException, KlvEncodeException {
        // tag 113 (altitudeAglM) carrying the BelowMin special: the typed
        // field stays null and the special re-appears in imapbSpecials.
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .imapbSpecials(java.util.List.of(new ImapbSpecialEntry(113, "below_min", 0L)))
                .build();
        UasDatalinkLs back = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));
        assertNull(back.altitudeAglM());
        assertTrue(back.imapbSpecials().contains(new ImapbSpecialEntry(113, "below_min", 0L)));
    }

    @Test
    void imapbSpecialsPayloadCarryingCodeRoundTripsLosslessly() throws KlvDecodeException, KlvEncodeException {
        // tag 96 (targetWidthExtendedM) carrying a positive quiet NaN with a
        // non-zero payload — the payload must survive exactly.
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .imapbSpecials(java.util.List.of(new ImapbSpecialEntry(96, "pos_quiet_nan", 5L)))
                .build();
        UasDatalinkLs back = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));
        assertNull(back.targetWidthExtendedM());
        assertTrue(back.imapbSpecials().contains(new ImapbSpecialEntry(96, "pos_quiet_nan", 5L)));
    }

    @Test
    void imapbSpecialsValueWinsOverSpecialEntry() throws KlvDecodeException, KlvEncodeException {
        // Mirrors sentinelTags: if the typed field is also set, the value
        // wins and the special entry is not re-emitted.
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .altitudeAglM(1234.0)
                .imapbSpecials(java.util.List.of(new ImapbSpecialEntry(113, "above_max", 0L)))
                .build();
        UasDatalinkLs back = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));
        assertNotNull(back.altitudeAglM());
        assertEquals(1234.0, back.altitudeAglM(), 0.02);
        assertTrue(back.imapbSpecials().stream().noneMatch(e -> e.tag() == 113));
    }

    @Test
    void imapbSpecialsAllNineCodesRoundTrip() throws KlvDecodeException, KlvEncodeException {
        // One IMAPB tag per code (a wire record can carry at most one TLV
        // per tag, so distinct tags avoid duplicate-tag ambiguity while
        // still exercising every code string in both crossing directions).
        java.util.List<ImapbSpecialEntry> entries = java.util.List.of(
                new ImapbSpecialEntry(96, "below_min", 0L),
                new ImapbSpecialEntry(103, "above_max", 0L),
                new ImapbSpecialEntry(104, "pos_infinity", 0L),
                new ImapbSpecialEntry(105, "neg_infinity", 0L),
                new ImapbSpecialEntry(109, "pos_quiet_nan", 5L),
                new ImapbSpecialEntry(112, "neg_quiet_nan", 5L),
                new ImapbSpecialEntry(113, "pos_signaling_nan", 5L),
                new ImapbSpecialEntry(114, "neg_signaling_nan", 5L),
                new ImapbSpecialEntry(117, "user_defined", 3L)
        );
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .imapbSpecials(entries)
                .build();
        UasDatalinkLs back = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));
        for (ImapbSpecialEntry e : entries) {
            assertTrue(back.imapbSpecials().contains(e), "missing " + e + " in " + back.imapbSpecials());
        }
    }

    @Test
    void imapbSpecialsInvalidCodeThrowsIllegalArgument() {
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .imapbSpecials(java.util.List.of(new ImapbSpecialEntry(113, "not_a_code", 0L)))
                .build();
        assertThrows(IllegalArgumentException.class, () -> Klv.encodeUasDatalink(rec));
    }

    @Test
    void imapbSpecialsPayloadTooLargeForDefaultLenThrows() {
        // Tag 112 (platformCourseAngleDeg) default_len=2 -> payload bits =
        // 8*2-5 = 11, max valid payload 2047; 99999 doesn't fit.
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .imapbSpecials(java.util.List.of(new ImapbSpecialEntry(112, "pos_quiet_nan", 99999L)))
                .build();
        assertThrows(KlvEncodeException.class, () -> Klv.encodeUasDatalink(rec));
    }

    // -----------------------------------------------------------------------
    // OutOfRangePolicy.INDICATOR for an IMAPB field (tag 113).
    // -----------------------------------------------------------------------

    /**
     * Tag 113 (altitudeAglM, range [-900, 40000]): 50_000.0 is above max.
     * Default policy still throws; INDICATOR emits the AboveMax special
     * (wire bytes {@code E1 00 00} at default_len=3) instead.
     */
    @Test
    void encodeImapbOutOfRangeIndicatorPolicy() throws KlvDecodeException, KlvEncodeException {
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .altitudeAglM(50_000.0)
                .build();
        assertThrows(KlvEncodeException.class, () -> Klv.encodeUasDatalink(rec));

        byte[] raw = Klv.encodeUasDatalink(rec, OutOfRangePolicy.INDICATOR);
        assertWireContains(raw, "7103e10000", "Tag 113 AboveMax IMAPB indicator");
        UasDatalinkLs back = Klv.decodeUasDatalink(raw);
        assertNull(back.altitudeAglM(), "IMAPB special field must be null after decode");
    }

    // -----------------------------------------------------------------------
    // Spec-vector pins — hand-authored wire bytes, same rationale as
    // the spec-vector block above (a symmetric transcription error in
    // the locally-duplicated PlatformStatus/SensorControlMode wire-code
    // tables would round-trip cleanly while emitting the wrong byte).
    // -----------------------------------------------------------------------

    /** Tag 96 (0x60) Target Width Extended. */
    @Test
    void targetWidthExtendedMSpecVector() throws KlvDecodeException, KlvEncodeException {
        byte[] wire = Klv.encodeUasDatalink(new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .targetWidthExtendedM(13898.5463)
                .build());
        assertWireContains(wire, "600300d92a", "Tag 96 Target Width Extended");

        UasDatalinkLs decoded = Klv.decodeUasDatalink(buildLs("600300d92a"));
        assertNotNull(decoded.targetWidthExtendedM());
        assertEquals(13898.5463, decoded.targetWidthExtendedM(), 0.5);
        assertTrue(decoded.fieldErrors().isEmpty());
    }

    /** Tag 110 (0x6E) Time Airborne. */
    @Test
    void timeAirborneSSpecVector() throws KlvDecodeException, KlvEncodeException {
        byte[] wire = Klv.encodeUasDatalink(new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .timeAirborneS(19887L)
                .build());
        assertWireContains(wire, "6e024daf", "Tag 110 Time Airborne");

        UasDatalinkLs decoded = Klv.decodeUasDatalink(buildLs("6e024daf"));
        assertEquals(19887L, decoded.timeAirborneS());
        assertTrue(decoded.fieldErrors().isEmpty());
    }

    /** Tag 125 (0x7D): spec code 9 = Egress. Decode direction: hand-authored bytes. */
    @Test
    void platformStatusSpecVectorDecode() throws KlvDecodeException {
        UasDatalinkLs decoded = Klv.decodeUasDatalink(buildLs("7d0109"));
        assertEquals(9, decoded.platformStatusCode());
        assertEquals(PlatformStatus.EGRESS, decoded.platformStatus());
        assertTrue(decoded.fieldErrors().isEmpty());
    }

    /** Tag 125 (0x7D): the encoded output must carry the exact {@code 7d 01 09} TLV. */
    @Test
    void platformStatusSpecVectorEncode() throws KlvEncodeException {
        byte[] wire = Klv.encodeUasDatalink(new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .platformStatusCode(PlatformStatus.EGRESS.code())
                .build());
        assertWireContains(wire, "7d0109", "Tag 125 PlatformStatus=Egress");
    }

    /** Tag 126 (0x7E): spec code 5 = AutoHoldingPosition. Decode direction. */
    @Test
    void sensorControlModeSpecVectorDecode() throws KlvDecodeException {
        UasDatalinkLs decoded = Klv.decodeUasDatalink(buildLs("7e0105"));
        assertEquals(5, decoded.sensorControlModeCode());
        assertEquals(SensorControlMode.AUTO_HOLDING_POSITION, decoded.sensorControlMode());
        assertTrue(decoded.fieldErrors().isEmpty());
    }

    /** Tag 126 (0x7E): the encoded output must carry the exact {@code 7e 01 05} TLV. */
    @Test
    void sensorControlModeSpecVectorEncode() throws KlvEncodeException {
        byte[] wire = Klv.encodeUasDatalink(new UasDatalinkLs.Builder()
                .universalLabel(bareUl())
                .sensorControlModeCode(SensorControlMode.AUTO_HOLDING_POSITION.code())
                .build());
        assertWireContains(wire, "7e0105", "Tag 126 SensorControlMode=AutoHoldingPosition");
    }
}
