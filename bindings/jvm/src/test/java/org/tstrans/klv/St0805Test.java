package org.tstrans.klv;

import static org.junit.jupiter.api.Assertions.*;

import java.nio.ByteBuffer;
import org.junit.jupiter.api.Test;

/**
 * MISB ST 0805.1 KLV -&gt; Cursor-on-Target (CoT) conversion tests.
 *
 * <p>Fixture values are ported verbatim from the Rust {@code crates/tst-core/
 * src/klv/st0805.rs::tests::fixture()} hand-built fixture (also mirrored by
 * {@code test_klv_st0805.py}), so the expected golden values asserted here
 * are already proven correct at the Rust layer — these tests exercise the
 * JVM &lt;-&gt; Rust marshaling ({@link UasDatalinkLs} extraction, {@link CotConfig}
 * translation, error mapping), not the CoT mapping logic itself.
 */
class St0805Test {

    private static final long GENERATED_US = 798_039_895_000_000L;

    // -----------------------------------------------------------------------
    // Fixture helpers
    // -----------------------------------------------------------------------

    /** Mirrors the Rust {@code fixture()} helper in st0805.rs. */
    private static UasDatalinkLs fixture() {
        return new UasDatalinkLs.Builder()
                .universalLabel(ByteBuffer.wrap(Klv.st0601Ul()))
                .timestampUs(798_039_894_000_000L)
                .platformDesignation("PRED01")
                .missionId("M05")
                .imageSourceSensor("EO")
                .sensorLatDeg(34.05)
                .sensorLonDeg(-118.25)
                .sensorEllipsoidHeightM(1524.0)   // HAE-native -> no geoid needed
                .platformHeadingDeg(90.0)
                .sensorRelAzDeg(300.0)             // 90+300 = 390 -> azimuth 30.0
                .sensorHfovDeg(2.5)
                .sensorVfovDeg(1.9)
                .slantRangeM(12_000.0)
                .targetLocationLatDeg(34.10)       // SPI prefers 40/41 over 23/24
                .targetLocationLonDeg(-118.20)
                .targetLocationElevM(250.0)        // MSL, no undulation set -> as-is
                .targetErrorCe90M(425.215152)
                .targetErrorLe90M(608.9231)
                .build();
    }

    /** {@link #fixture()} with missionId (Tag 3) omitted. */
    private static UasDatalinkLs fixtureWithoutMissionId() {
        return new UasDatalinkLs.Builder()
                .universalLabel(ByteBuffer.wrap(Klv.st0601Ul()))
                .timestampUs(798_039_894_000_000L)
                .platformDesignation("PRED01")
                // missionId omitted
                .imageSourceSensor("EO")
                .sensorLatDeg(34.05)
                .sensorLonDeg(-118.25)
                .sensorEllipsoidHeightM(1524.0)
                .platformHeadingDeg(90.0)
                .sensorRelAzDeg(300.0)
                .sensorHfovDeg(2.5)
                .sensorVfovDeg(1.9)
                .slantRangeM(12_000.0)
                .targetLocationLatDeg(34.10)
                .targetLocationLonDeg(-118.20)
                .targetLocationElevM(250.0)
                .targetErrorCe90M(425.215152)
                .targetErrorLe90M(608.9231)
                .build();
    }

    /** {@link #fixture()} with imageSourceSensor (Tag 11) omitted. */
    private static UasDatalinkLs fixtureWithoutImageSourceSensor() {
        return new UasDatalinkLs.Builder()
                .universalLabel(ByteBuffer.wrap(Klv.st0601Ul()))
                .timestampUs(798_039_894_000_000L)
                .platformDesignation("PRED01")
                .missionId("M05")
                // imageSourceSensor omitted
                .sensorLatDeg(34.05)
                .sensorLonDeg(-118.25)
                .sensorEllipsoidHeightM(1524.0)
                .platformHeadingDeg(90.0)
                .sensorRelAzDeg(300.0)
                .sensorHfovDeg(2.5)
                .sensorVfovDeg(1.9)
                .slantRangeM(12_000.0)
                .targetLocationLatDeg(34.10)
                .targetLocationLonDeg(-118.20)
                .targetLocationElevM(250.0)
                .targetErrorCe90M(425.215152)
                .targetErrorLe90M(608.9231)
                .build();
    }

    /** {@link #fixture()} with timestampUs (Tag 2) omitted. */
    private static UasDatalinkLs fixtureWithoutTimestamp() {
        return new UasDatalinkLs.Builder()
                .universalLabel(ByteBuffer.wrap(Klv.st0601Ul()))
                // timestampUs omitted
                .platformDesignation("PRED01")
                .missionId("M05")
                .imageSourceSensor("EO")
                .sensorLatDeg(34.05)
                .sensorLonDeg(-118.25)
                .sensorEllipsoidHeightM(1524.0)
                .platformHeadingDeg(90.0)
                .sensorRelAzDeg(300.0)
                .sensorHfovDeg(2.5)
                .sensorVfovDeg(1.9)
                .slantRangeM(12_000.0)
                .targetLocationLatDeg(34.10)
                .targetLocationLonDeg(-118.20)
                .targetLocationElevM(250.0)
                .targetErrorCe90M(425.215152)
                .targetErrorLe90M(608.9231)
                .build();
    }

    // -----------------------------------------------------------------------
    // uid determinism
    // -----------------------------------------------------------------------

    @Test
    void platformUidIsDeterministicConcatenation() {
        assertEquals("PRED01_M05", Klv.platformUid(fixture()));
    }

    @Test
    void spiUidIsDeterministicConcatenation() {
        assertEquals("PRED01_M05_EO", Klv.spiUid(fixture()));
    }

    @Test
    void platformUidMissingMissionIdThrows() {
        IllegalArgumentException ex = assertThrows(IllegalArgumentException.class,
                () -> Klv.platformUid(fixtureWithoutMissionId()));
        assertTrue(ex.getMessage().contains("tag 3"),
                "expected message to mention tag 3; got: " + ex.getMessage());
    }

    @Test
    void spiUidMissingImageSourceSensorThrows() {
        IllegalArgumentException ex = assertThrows(IllegalArgumentException.class,
                () -> Klv.spiUid(fixtureWithoutImageSourceSensor()));
        assertTrue(ex.getMessage().contains("tag 11"),
                "expected message to mention tag 11; got: " + ex.getMessage());
    }

    // -----------------------------------------------------------------------
    // golden XML (defaults)
    // -----------------------------------------------------------------------

    @Test
    void platformPositionGolden() {
        String xml = Klv.platformPositionXml(fixture(), GENERATED_US);
        assertTrue(xml.contains("uid=\"PRED01_M05\""));
        assertTrue(xml.contains("type=\"a-f-A-M-F\""));
        assertTrue(xml.contains("stale=\"1995-04-16T13:44:59.000000Z\""));
        assertTrue(xml.contains("hae=\"1524\""));
        assertTrue(xml.contains("ce=\"9999999\""));
        assertTrue(xml.contains("le=\"9999999\""));
        assertTrue(xml.contains(
                "<sensor azimuth=\"30\" fov=\"2.5\" vfov=\"1.9\" model=\"EO\" range=\"12000\"/>"));
    }

    @Test
    void spiGoldenWithCeLeDivisorsAndLink() {
        String xml = Klv.sensorPointOfInterestXml(fixture(), GENERATED_US);
        assertTrue(xml.contains("type=\"b-m-p-s-p-i\""));
        assertTrue(xml.contains("uid=\"PRED01_M05_EO\""));
        assertTrue(xml.contains("ce=\"198.14312"));
        assertTrue(xml.contains("le=\"370.16601"));
        assertTrue(xml.contains("<link relation=\"p-p\" type=\"a-f-A-M-F\" uid=\"PRED01_M05\"/>"));
    }

    @Test
    void platformPositionMissingTimestampThrows() {
        IllegalArgumentException ex = assertThrows(IllegalArgumentException.class,
                () -> Klv.platformPositionXml(fixtureWithoutTimestamp(), GENERATED_US));
        assertTrue(ex.getMessage().contains("tag 2"),
                "expected message to mention tag 2; got: " + ex.getMessage());
    }

    // -----------------------------------------------------------------------
    // CotConfig marshaling (Java -> Rust field extraction)
    // -----------------------------------------------------------------------

    @Test
    void customConfigOverridesPlatformTypeAndProducer() {
        CotConfig cfg = CotConfig.builder()
                .platformType("a-f-G-U-C")
                .producer("MyOrg")
                .build();
        String xml = Klv.platformPositionXml(fixture(), cfg, GENERATED_US);
        assertTrue(xml.contains("type=\"a-f-G-U-C\""));
        assertTrue(xml.contains("<_flow-tags_ MyOrg="));
    }

    @Test
    void customConfigGeoidUndulationMarshalsToMslAltitude() {
        // MSL-only record (no HAE-native tag 75/104) + a configured geoid
        // undulation: hae = msl + undulation, marshaled through CotConfig.
        UasDatalinkLs record = new UasDatalinkLs.Builder()
                .universalLabel(ByteBuffer.wrap(Klv.st0601Ul()))
                .timestampUs(798_039_894_000_000L)
                .platformDesignation("PRED01")
                .missionId("M05")
                .imageSourceSensor("EO")
                .sensorLatDeg(34.05)
                .sensorLonDeg(-118.25)
                .sensorAltM(1500.0)
                .build();
        CotConfig cfg = CotConfig.builder().geoidUndulationM(24.5).build();
        String xml = Klv.platformPositionXml(record, cfg, GENERATED_US);
        assertTrue(xml.contains("hae=\"1524.5\""));
    }

    // -----------------------------------------------------------------------
    // Negative jlong -> u64 wrap guards
    // -----------------------------------------------------------------------

    @Test
    void negativeGeneratedUsThrows() {
        IllegalArgumentException ex = assertThrows(IllegalArgumentException.class,
                () -> Klv.platformPositionXml(fixture(), -1L));
        assertTrue(ex.getMessage().contains("generatedUs"),
                "expected message to mention generatedUs; got: " + ex.getMessage());
    }

    @Test
    void negativeUpdateIntervalUsThrows() {
        CotConfig cfg = CotConfig.builder().updateIntervalUs(-5L).build();
        IllegalArgumentException ex = assertThrows(IllegalArgumentException.class,
                () -> Klv.platformPositionXml(fixture(), cfg, GENERATED_US));
        assertTrue(ex.getMessage().contains("updateIntervalUs"),
                "expected message to mention updateIntervalUs; got: " + ex.getMessage());
    }

    // -----------------------------------------------------------------------
    // Null arguments throw NullPointerException (never a silent null return)
    // -----------------------------------------------------------------------

    /**
     * Every CoT entry point must throw on a {@code null} record/config rather
     * than silently returning {@code null}: jni's own null guard surfaces as a
     * Rust-side {@code Err} with NO pending Java exception, so without an
     * explicit up-front check these natives returned null with no diagnostic
     * at all — the behavior this test retires.
     */
    @Test
    void nullRecordThrowsNpe() {
        assertNullNamed("record", assertThrows(NullPointerException.class,
                () -> Klv.platformPositionXml(null, GENERATED_US)));
        assertNullNamed("record", assertThrows(NullPointerException.class,
                () -> Klv.sensorPointOfInterestXml(null, GENERATED_US)));
        assertNullNamed("record", assertThrows(NullPointerException.class,
                () -> Klv.platformUid(null)));
        assertNullNamed("record", assertThrows(NullPointerException.class,
                () -> Klv.spiUid(null)));
    }

    @Test
    void nullConfigThrowsNpe() {
        assertNullNamed("config", assertThrows(NullPointerException.class,
                () -> Klv.platformPositionXml(fixture(), null, GENERATED_US)));
        assertNullNamed("config", assertThrows(NullPointerException.class,
                () -> Klv.sensorPointOfInterestXml(fixture(), null, GENERATED_US)));
    }

    /** The NPE message must name the offending parameter. */
    private static void assertNullNamed(String param, NullPointerException ex) {
        assertNotNull(ex.getMessage(), "NPE must carry a message");
        assertTrue(ex.getMessage().contains(param),
                "expected message to mention " + param + "; got: " + ex.getMessage());
    }
}
