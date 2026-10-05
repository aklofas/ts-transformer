package org.tstrans.klv;

import static org.junit.jupiter.api.Assertions.*;

import java.util.Collections;
import java.util.HexFormat;
import java.util.List;
import java.nio.ByteBuffer;
import org.junit.jupiter.api.Test;
import org.tstrans.KlvDecodeException;

/**
 * Tests for {@link Klv#decodeCoreId}, {@link Klv#encodeCoreId},
 * {@link Klv#coreIdText}, and {@link Klv#validateMismms}.
 *
 * <p>Reference vectors are drawn from the Rust unit tests in
 * {@code crates/tst-core/src/klv/st1204/mod.rs}:
 * <ul>
 *   <li>TABLE7 — ST 1204.3 Table 7 reference vector (34 bytes)</li>
 *   <li>Expected display — {@code "0170:F592-…-B2DA/16B7-…-3645:D3"}</li>
 * </ul>
 */
class St1204Test {

    /**
     * ST 1204.3 Table 7 reference vector.
     * version=0x01, usage=0x70 (b6-5=11 Physical sensor, b4-3=10 Virtual platform).
     * Matches the Rust TABLE7 constant byte-for-byte.
     */
    private static final byte[] TABLE7 = HexFormat.of().parseHex(
            "01" + "70"
            + "F592F0237336" + "4A" + "F8AA9162C00F2EB2DA"   // sensor UUID (physical)
            + "16B74341000841A0BE365B5AB96A3645"              // platform UUID (virtual)
    );

    /**
     * Expected ST 1204.3 §7.4.2 textual form for TABLE7.
     * Pinned from the Rust {@code display_matches_spec_example} test.
     */
    private static final String TABLE7_TEXT =
            "0170:F592-F023-7336-4AF8-AA91-62C0-0F2E-B2DA/16B7-4341-0008-41A0-BE36-5B5A-B96A-3645:D3";

    // -----------------------------------------------------------------------
    // decodeCoreId
    // -----------------------------------------------------------------------

    @Test
    void decodeTable7FieldsCorrect() throws KlvDecodeException {
        CoreId id = Klv.decodeCoreId(TABLE7);
        assertNotNull(id);
        assertEquals(1, id.version());
        // usage=0x70: b6-5=11 → Physical sensor; b4-3=10 → Virtual platform
        assertEquals(IdType.PHYSICAL, id.sensorType());
        assertNotNull(id.sensorId());
        assertEquals(16, id.sensorId().length);
        assertEquals(IdType.VIRTUAL, id.platformType());
        assertNotNull(id.platformId());
        assertEquals(16, id.platformId().length);
        assertNull(id.windowId());
        assertNull(id.minorId());
    }

    @Test
    void decodeTable7SensorUuidBytes() throws KlvDecodeException {
        CoreId id = Klv.decodeCoreId(TABLE7);
        // First 16 bytes after the 2-byte header are the sensor UUID.
        byte[] expected = HexFormat.of().parseHex("F592F0237336" + "4A" + "F8AA9162C00F2EB2DA");
        assertArrayEquals(expected, id.sensorId());
    }

    @Test
    void decodeTable7PlatformUuidBytes() throws KlvDecodeException {
        CoreId id = Klv.decodeCoreId(TABLE7);
        byte[] expected = HexFormat.of().parseHex("16B74341000841A0BE365B5AB96A3645");
        assertArrayEquals(expected, id.platformId());
    }

    // -----------------------------------------------------------------------
    // encodeCoreId — round-trip
    // -----------------------------------------------------------------------

    @Test
    void encodeRoundTripByteIdentical() throws KlvDecodeException {
        CoreId id = Klv.decodeCoreId(TABLE7);
        byte[] encoded = Klv.encodeCoreId(id);
        assertArrayEquals(TABLE7, encoded, "encode(decode(TABLE7)) must be byte-identical to TABLE7");
    }

    @Test
    void encodeMinorCoreIdRoundTrip() throws KlvDecodeException {
        // usage 0x02: b1=1 minor, all others 0 → Minor Core Id
        byte[] minorBytes = HexFormat.of().parseHex("DEDEDEDEDEDEDEDEDEDEDEDEDEDEDEDE");
        byte[] buf = new byte[18];
        buf[0] = 0x01;
        buf[1] = 0x02;
        System.arraycopy(minorBytes, 0, buf, 2, 16);
        CoreId id = Klv.decodeCoreId(buf);
        assertNull(id.sensorType());
        assertNull(id.sensorId());
        assertNull(id.platformType());
        assertNull(id.platformId());
        assertNull(id.windowId());
        assertNotNull(id.minorId());
        assertArrayEquals(minorBytes, id.minorId());
        assertArrayEquals(buf, Klv.encodeCoreId(id));
    }

    // -----------------------------------------------------------------------
    // coreIdText
    // -----------------------------------------------------------------------

    @Test
    void coreIdTextMatchesSpecExample() throws KlvDecodeException {
        CoreId id = Klv.decodeCoreId(TABLE7);
        assertEquals(TABLE7_TEXT, Klv.coreIdText(id));
    }

    @Test
    void coreIdTextEndsWithCheckDigits() throws KlvDecodeException {
        // The spec example ends with ":D3".
        CoreId id = Klv.decodeCoreId(TABLE7);
        String text = Klv.coreIdText(id);
        assertTrue(text.endsWith(":D3"),
                "TABLE7 textual form should end with ':D3'; got: " + text);
    }

    // -----------------------------------------------------------------------
    // decodeCoreId — error cases
    // -----------------------------------------------------------------------

    @Test
    void decodeMalformedThrows() {
        // Empty buffer → truncated
        assertThrows(KlvDecodeException.class, () -> Klv.decodeCoreId(new byte[0]));
    }

    @Test
    void decodeUnsupportedVersionThrows() {
        // Version byte 0x02 is unsupported (only version 1 is accepted).
        byte[] bad = new byte[]{0x02, 0x70};
        KlvDecodeException ex = assertThrows(KlvDecodeException.class,
                () -> Klv.decodeCoreId(bad));
        assertEquals(KlvDecodeException.Kind.MALFORMED_BYTES, ex.kind());
    }

    @Test
    void decodeAllNoneUsageThrows() {
        // usage=0x00 is invalid per ST 1204.3 §7.3.1.
        byte[] bad = new byte[]{0x01, 0x00};
        assertThrows(KlvDecodeException.class, () -> Klv.decodeCoreId(bad));
    }

    @Test
    void decodeTrailingBytesThrows() {
        byte[] extra = new byte[TABLE7.length + 1];
        System.arraycopy(TABLE7, 0, extra, 0, TABLE7.length);
        // extra[TABLE7.length] = 0 — a trailing zero byte
        assertThrows(KlvDecodeException.class, () -> Klv.decodeCoreId(extra));
    }

    // -----------------------------------------------------------------------
    // validateMismms — full record (no violations)
    // -----------------------------------------------------------------------

    @Test
    void validateMismmsFullRecordNoViolations() {
        UasDatalinkLs rec = fullMismmsRecord();
        List<MismmsViolation> violations = Klv.validateMismms(rec);
        assertTrue(violations.isEmpty(),
                "full MISMMS record should produce no violations; got: " + violations);
    }

    // -----------------------------------------------------------------------
    // validateMismms — missing item
    // -----------------------------------------------------------------------

    @Test
    void validateMissmsMissingMissionId() {
        UasDatalinkLs rec = fullMismmsRecord();
        // Remove Tag 3 (missionId) by building a new record without it.
        UasDatalinkLs noMission = new UasDatalinkLs.Builder()
                .universalLabel(rec.universalLabel())
                .declaredVersion(rec.declaredVersion())
                // Skip missionId
                .platformHeadingDeg(rec.platformHeadingDeg())
                .platformPitchDeg(rec.platformPitchDeg())
                .platformRollDeg(rec.platformRollDeg())
                .platformDesignation(rec.platformDesignation())
                .imageSourceSensor(rec.imageSourceSensor())
                .imageCoordinateSystem(rec.imageCoordinateSystem())
                .timestampUs(rec.timestampUs())
                .sensorLatDeg(rec.sensorLatDeg())
                .sensorLonDeg(rec.sensorLonDeg())
                .sensorAltM(rec.sensorAltM())
                .sensorHfovDeg(rec.sensorHfovDeg())
                .sensorVfovDeg(rec.sensorVfovDeg())
                .sensorRelAzDeg(rec.sensorRelAzDeg())
                .sensorRelElDeg(rec.sensorRelElDeg())
                .sensorRelRollDeg(rec.sensorRelRollDeg())
                .slantRangeM(rec.slantRangeM())
                .targetWidthM(rec.targetWidthM())
                .frameCenterLatDeg(rec.frameCenterLatDeg())
                .frameCenterLonDeg(rec.frameCenterLonDeg())
                .frameCenterElevM(rec.frameCenterElevM())
                .securityLocalSet(rec.securityLocalSet())
                .miisCoreId(rec.miisCoreId())
                .build();
        List<MismmsViolation> violations = Klv.validateMismms(noMission);
        boolean hasMissingMissionId = violations.stream()
                .anyMatch(v -> "missing".equals(v.kind()) && v.tag() == 3);
        assertTrue(hasMissingMissionId,
                "missing missionId should yield a 'missing' violation for tag 3; got: " + violations);
    }

    // -----------------------------------------------------------------------
    // validateMismms — missing security sub-item
    // -----------------------------------------------------------------------

    @Test
    void validateMismmsMissingSecurityItem() {
        UasDatalinkLs base = fullMismmsRecord();
        // Rebuild security LS without the caveats item (Tag 5 in Security LS).
        SecurityLs secNoCaveats = new SecurityLs.Builder()
                .securityClassification(SecurityClassification.UNCLASSIFIED)
                .classifyingCountryCodingMethod(ClassifyingCountryCodingMethod.ISO_3166_THREE_LETTER)
                .classifyingCountry("//USA")
                .sciShiInfo("SCI")
                // Omit caveats (Tag 5 in Security LS)
                .releasingInstructions("USA")
                .objectCountryCodingMethod(ObjectCountryCodingMethod.ISO_3166_THREE_LETTER)
                .objectCountryCodes("USA")
                .version(12)
                .build();
        byte[] secBytesNoCaveats;
        try {
            secBytesNoCaveats = Klv.encodeSecurity(secNoCaveats);
        } catch (Exception e) {
            throw new RuntimeException("Failed to encode security LS without caveats for test", e);
        }

        UasDatalinkLs noCaveats = new UasDatalinkLs.Builder()
                .universalLabel(base.universalLabel())
                .declaredVersion(base.declaredVersion())
                .missionId(base.missionId())
                .platformHeadingDeg(base.platformHeadingDeg())
                .platformPitchDeg(base.platformPitchDeg())
                .platformRollDeg(base.platformRollDeg())
                .platformDesignation(base.platformDesignation())
                .imageSourceSensor(base.imageSourceSensor())
                .imageCoordinateSystem(base.imageCoordinateSystem())
                .timestampUs(base.timestampUs())
                .sensorLatDeg(base.sensorLatDeg())
                .sensorLonDeg(base.sensorLonDeg())
                .sensorAltM(base.sensorAltM())
                .sensorHfovDeg(base.sensorHfovDeg())
                .sensorVfovDeg(base.sensorVfovDeg())
                .sensorRelAzDeg(base.sensorRelAzDeg())
                .sensorRelElDeg(base.sensorRelElDeg())
                .sensorRelRollDeg(base.sensorRelRollDeg())
                .slantRangeM(base.slantRangeM())
                .targetWidthM(base.targetWidthM())
                .frameCenterLatDeg(base.frameCenterLatDeg())
                .frameCenterLonDeg(base.frameCenterLonDeg())
                .frameCenterElevM(base.frameCenterElevM())
                .securityLocalSet(ByteBuffer.wrap(secBytesNoCaveats))
                .miisCoreId(base.miisCoreId())
                .build();
        List<MismmsViolation> violations = Klv.validateMismms(noCaveats);
        boolean hasMissingSecurity = violations.stream()
                .anyMatch(v -> "missing_security".equals(v.kind()) && v.tag() == 5);
        assertTrue(hasMissingSecurity,
                "missing caveats in security LS should yield a 'missing_security' violation for tag 5; got: "
                        + violations);
    }

    // -----------------------------------------------------------------------
    // validateMismms — zero-length item
    // -----------------------------------------------------------------------

    @Test
    void validateMismmsZeroLengthItem() {
        UasDatalinkLs base = fullMismmsRecord();
        // Tag 96 (targetWidthExtendedM) is typed: a zero-length wire value
        // for a now-typed tag can no longer be injected through the JVM
        // binding's `unknown` list — the JNI translator's
        // `is_st0601_typed_tag` collision-drop silently eats a tag-96
        // `unknown` entry before it ever reaches the Rust validator (typed
        // field wins per the documented collision policy; same fix as the
        // Python binding's analogous carry-forward). The zero-length
        // scenario itself is still exercised directly at the Rust-core
        // level (bypassing any binding predicate) by
        // `zero_length_unknown_tag_96_and_missing_group` in
        // crates/tst-core/src/klv/st0601/mismms.rs. Omit tag 22 from a full
        // record; the attempted zero-length tag-96 injection below is
        // dropped, so only the "missing tag 22" violation should surface.
        List<KlvUnknownField> unk = java.util.Arrays.asList(
                new KlvUnknownField(96L, ByteBuffer.wrap(new byte[0]))  // Tag 96, zero-length (dropped)
        );
        UasDatalinkLs withZeroLength = new UasDatalinkLs.Builder()
                .universalLabel(base.universalLabel())
                .declaredVersion(base.declaredVersion())
                .missionId(base.missionId())
                .platformHeadingDeg(base.platformHeadingDeg())
                .platformPitchDeg(base.platformPitchDeg())
                .platformRollDeg(base.platformRollDeg())
                .platformDesignation(base.platformDesignation())
                .imageSourceSensor(base.imageSourceSensor())
                .imageCoordinateSystem(base.imageCoordinateSystem())
                .timestampUs(base.timestampUs())
                .sensorLatDeg(base.sensorLatDeg())
                .sensorLonDeg(base.sensorLonDeg())
                .sensorAltM(base.sensorAltM())
                .sensorHfovDeg(base.sensorHfovDeg())
                .sensorVfovDeg(base.sensorVfovDeg())
                .sensorRelAzDeg(base.sensorRelAzDeg())
                .sensorRelElDeg(base.sensorRelElDeg())
                .sensorRelRollDeg(base.sensorRelRollDeg())
                .slantRangeM(base.slantRangeM())
                // Omit targetWidthM (Tag 22); the attempted zero-length tag-96
                // unknown injection above is dropped before it reaches Rust.
                .frameCenterLatDeg(base.frameCenterLatDeg())
                .frameCenterLonDeg(base.frameCenterLonDeg())
                .frameCenterElevM(base.frameCenterElevM())
                .securityLocalSet(base.securityLocalSet())
                .miisCoreId(base.miisCoreId())
                .unknown(unk)
                .build();
        List<MismmsViolation> violations = Klv.validateMismms(withZeroLength);
        boolean hasZeroLength = violations.stream()
                .anyMatch(v -> "zero_length".equals(v.kind()) && v.tag() == 96);
        boolean hasMissing22 = violations.stream()
                .anyMatch(v -> "missing".equals(v.kind()) && v.tag() == 22);
        assertFalse(hasZeroLength,
                "WP-B: tag 96 is now typed, so the unknown-list zero-length injection is silently "
                        + "dropped by the typed-wins collision policy before reaching the Rust "
                        + "validator — no zero_length violation should surface via this binding; got: "
                        + violations);
        assertTrue(hasMissing22,
                "missing tag 22 should yield a 'missing' violation; got: " + violations);
    }

    // -----------------------------------------------------------------------
    // validateMismms — alternation conflict
    // -----------------------------------------------------------------------

    @Test
    void validateMismmsAlternationConflict75And104() {
        // Build a record with both Tag 75 (sensorEllipsoidHeightM) and Tag 104
        // (sensorEllipsoidHeightExtendedM) present simultaneously — should
        // trigger AlternationConflict. Tag 104 is typed, so it can no
        // longer be injected via `unknown` (the typed-wins collision-drop
        // would eat it, same as the neighboring zero-length test above) —
        // set the typed field directly instead, mirroring the Rust
        // `alternation_conflict_75_and_104` / `wpb_mismms_typed_96_104`
        // tests (crates/tst-core/src/klv/st0601/mismms.rs) and the
        // analogous fix in the Python binding.
        UasDatalinkLs base = fullMismmsRecord();
        UasDatalinkLs withConflict = new UasDatalinkLs.Builder()
                .universalLabel(base.universalLabel())
                .declaredVersion(base.declaredVersion())
                .missionId(base.missionId())
                .platformHeadingDeg(base.platformHeadingDeg())
                .platformPitchDeg(base.platformPitchDeg())
                .platformRollDeg(base.platformRollDeg())
                .platformDesignation(base.platformDesignation())
                .imageSourceSensor(base.imageSourceSensor())
                .imageCoordinateSystem(base.imageCoordinateSystem())
                .timestampUs(base.timestampUs())
                .sensorLatDeg(base.sensorLatDeg())
                .sensorLonDeg(base.sensorLonDeg())
                .sensorEllipsoidHeightM(1500.0)  // Tag 75 typed — present
                .sensorEllipsoidHeightExtendedM(1500.0)  // Tag 104 typed — present
                // Omit sensorAltM (Tag 15) so the 15|75|104 req is met by 75/104 only.
                .sensorHfovDeg(base.sensorHfovDeg())
                .sensorVfovDeg(base.sensorVfovDeg())
                .sensorRelAzDeg(base.sensorRelAzDeg())
                .sensorRelElDeg(base.sensorRelElDeg())
                .sensorRelRollDeg(base.sensorRelRollDeg())
                .slantRangeM(base.slantRangeM())
                .targetWidthM(base.targetWidthM())
                .frameCenterLatDeg(base.frameCenterLatDeg())
                .frameCenterLonDeg(base.frameCenterLonDeg())
                .frameCenterElevM(base.frameCenterElevM())
                .securityLocalSet(base.securityLocalSet())
                .miisCoreId(base.miisCoreId())
                .build();
        List<MismmsViolation> violations = Klv.validateMismms(withConflict);
        boolean hasConflict = violations.stream().anyMatch(v ->
                "alternation_conflict".equals(v.kind()) && v.tag() == 75 && v.tagB() == 104);
        assertTrue(hasConflict,
                "both Tag 75 and Tag 104 present should yield an alternation_conflict; got: "
                        + violations);
    }

    // -----------------------------------------------------------------------
    // miisCoreId wiring through UasDatalinkLs encode/decode round-trip
    // -----------------------------------------------------------------------

    @Test
    void miisCoreIdSurvivesEncodeDecodeRoundTrip() throws Exception {
        // Build a minimal-ish UasDatalinkLs that has miisCoreId set,
        // encode it and decode it, then verify miisCoreId comes back.
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ByteBuffer.wrap(
                        HexFormat.of().parseHex("060e2b34020b01010e01030101000000")))
                .declaredVersion(19)
                .uasLsVersion(19)
                .timestampUs(1_700_000_000_000_000L)
                .miisCoreId(TABLE7)
                .build();
        byte[] encoded = Klv.encodeUasDatalink(rec);
        UasDatalinkLs decoded = Klv.decodeUasDatalink(encoded);
        assertNotNull(decoded.miisCoreId(),
                "miisCoreId should survive encode→decode round-trip");
        assertArrayEquals(TABLE7, decoded.miisCoreId());
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /**
     * Build a {@link UasDatalinkLs} that satisfies all 23 MISMMS requirements.
     * Mirrors the Rust {@code full_mismms_record()} helper in st0601/mismms.rs.
     */
    private UasDatalinkLs fullMismmsRecord() {
        // Encode a minimal but complete Security LS (all 9 required sub-items).
        SecurityLs sec = new SecurityLs.Builder()
                .securityClassification(SecurityClassification.UNCLASSIFIED)
                .classifyingCountryCodingMethod(ClassifyingCountryCodingMethod.ISO_3166_THREE_LETTER)
                .classifyingCountry("//USA")
                .sciShiInfo("SCI")
                .caveats("FOUO")
                .releasingInstructions("USA")
                .objectCountryCodingMethod(ObjectCountryCodingMethod.ISO_3166_THREE_LETTER)
                .objectCountryCodes("USA")
                .version(12)
                .build();
        byte[] secBytes;
        try {
            secBytes = Klv.encodeSecurity(sec);
        } catch (Exception e) {
            throw new RuntimeException("Failed to encode security LS for test", e);
        }

        return new UasDatalinkLs.Builder()
                .universalLabel(ByteBuffer.wrap(
                        HexFormat.of().parseHex("060e2b34020b01010e01030101000000")))
                .declaredVersion(19)
                .uasLsVersion(19)
                .timestampUs(1_700_000_000_000_000L)          // Tag 2
                .missionId("MISSION-1")                        // Tag 3
                .platformHeadingDeg(45.0)                      // Tag 5
                .platformPitchDeg(5.0)                         // Tag 6  (6|90)
                .platformRollDeg(2.0)                          // Tag 7  (7|91)
                .platformDesignation("UAV-1")                  // Tag 10
                .imageSourceSensor("EO")                       // Tag 11
                .imageCoordinateSystem("WGS84")                // Tag 12
                .sensorLatDeg(47.0)                            // Tag 13
                .sensorLonDeg(-122.0)                          // Tag 14
                .sensorAltM(1500.0)                            // Tag 15  (15|75|104)
                .sensorHfovDeg(5.0)                            // Tag 16
                .sensorVfovDeg(3.75)                           // Tag 17
                .sensorRelAzDeg(180.0)                         // Tag 18
                .sensorRelElDeg(-30.0)                         // Tag 19
                .sensorRelRollDeg(0.5)                         // Tag 20
                .slantRangeM(5000.0)                           // Tag 21
                .targetWidthM(100.0)                           // Tag 22  (22|96)
                .frameCenterLatDeg(46.9)                       // Tag 23
                .frameCenterLonDeg(-122.1)                     // Tag 24
                .frameCenterElevM(50.0)                        // Tag 25  (25|78)
                .securityLocalSet(ByteBuffer.wrap(secBytes))   // Tag 48
                .miisCoreId(TABLE7)                            // Tag 94
                .build();
    }

    // -----------------------------------------------------------------------
    // Null arguments throw NullPointerException (never a silent null return)
    // -----------------------------------------------------------------------

    /**
     * Same contract as St0805Test's null-argument tests: jni's null guard
     * surfaces as a Rust-side {@code Err} with NO pending Java exception, so
     * without an explicit up-front check these natives silently returned
     * {@code null}.
     */
    @Test
    void nullArgumentsThrowNpe() {
        assertNpeNames("record", assertThrows(NullPointerException.class,
                () -> Klv.validateMismms(null)));
        assertNpeNames("id", assertThrows(NullPointerException.class,
                () -> Klv.encodeCoreId(null)));
        assertNpeNames("id", assertThrows(NullPointerException.class,
                () -> Klv.coreIdText(null)));
    }

    /**
     * The NPE message must name the offending parameter. Guards
     * {@code getMessage()} against null first (legal for NPE) so a
     * message-less regression fails with this assert, not an NPE inside
     * the test itself. Mirrors St0805Test's identical helper.
     */
    private static void assertNpeNames(String param, NullPointerException ex) {
        assertNotNull(ex.getMessage(), "NPE must carry a message");
        assertTrue(ex.getMessage().contains(param),
                "expected message to mention " + param + "; got: " + ex.getMessage());
    }
}
