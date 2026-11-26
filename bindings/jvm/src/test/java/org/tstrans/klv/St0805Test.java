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
    // Well-formed XML: refused characters, the producer name, TAB / LF / CR
    // -----------------------------------------------------------------------

    /** {@link #fixture()} with the three uid parts (Tags 10, 3, 11) given. */
    private static UasDatalinkLs fixtureWithUidParts(String tag10, String tag3, String tag11) {
        return new UasDatalinkLs.Builder()
                .universalLabel(ByteBuffer.wrap(Klv.st0601Ul()))
                .timestampUs(798_039_894_000_000L)
                .platformDesignation(tag10)
                .missionId(tag3)
                .imageSourceSensor(tag11)
                .sensorLatDeg(34.05)
                .sensorLonDeg(-118.25)
                .sensorEllipsoidHeightM(1524.0)
                .platformHeadingDeg(90.0)
                .sensorRelAzDeg(300.0)
                .targetLocationLatDeg(34.10)
                .targetLocationLonDeg(-118.20)
                .targetLocationElevM(250.0)
                .build();
    }

    private static void assertBothRefuse(UasDatalinkLs rec, CotConfig cfg, String message) {
        IllegalArgumentException p = assertThrows(IllegalArgumentException.class,
                () -> Klv.platformPositionXml(rec, cfg, GENERATED_US));
        assertEquals(message, p.getMessage());
        IllegalArgumentException s = assertThrows(IllegalArgumentException.class,
                () -> Klv.sensorPointOfInterestXml(rec, cfg, GENERATED_US));
        assertEquals(message, s.getMessage());
    }

    @Test
    void cotRejectsXmlIllegalChars() {
        String[][] cases = {
            {"PRED01", "M\0", "EO", "Mission ID"},
            {"PRED01", "A\u0001", "EO", "Mission ID"},
            {"PRED01", "x\uFFFE", "EO", "Mission ID"},
            {"P\uFFFF", "M05", "EO", "Platform Designation"},
            {"PRED01", "M05", "E\u001BO", "Image Source Sensor"},
        };
        for (String[] c : cases) {
            assertBothRefuse(fixtureWithUidParts(c[0], c[1], c[2]), CotConfig.defaults(),
                    c[3] + " holds a character that XML 1.0 does not allow");
        }
        assertBothRefuse(fixture(), CotConfig.builder().platformType("a-f-\u0002").build(),
                "platform_type holds a character that XML 1.0 does not allow");
    }

    @Test
    void cotRejectsAProducerThatIsNotAnXmlName() {
        String[] bad = {"bad name", "1abc", "", "ns:tag", "x=\"1\"/><evil a", "xmlns", "XMLfoo",
            "a\uD7FFb", "\u2070", "a\u00B7b"};
        for (String producer : bad) {
            assertBothRefuse(fixture(), CotConfig.builder().producer(producer).build(),
                    "the CoT producer name is not an allowed XML name");
        }
        String xml = Klv.platformPositionXml(fixture(),
                CotConfig.builder().producer("_x-1.2").build(), GENERATED_US);
        assertTrue(xml.contains("<_flow-tags_ _x-1.2=\""), xml);
    }

    @Test
    void cotKeepsTabLfCrThroughANamespaceAwareParser() throws Exception {
        UasDatalinkLs rec = fixtureWithUidParts("PRED01", "a\tb\nc\rd", "EO");
        javax.xml.parsers.DocumentBuilderFactory f = javax.xml.parsers.DocumentBuilderFactory.newInstance();
        f.setNamespaceAware(true);
        for (String xml : new String[] {
                Klv.platformPositionXml(rec, GENERATED_US),
                Klv.sensorPointOfInterestXml(rec, GENERATED_US)}) {
            org.w3c.dom.Document doc = f.newDocumentBuilder()
                    .parse(new java.io.ByteArrayInputStream(xml.getBytes(java.nio.charset.StandardCharsets.UTF_8)));
            String uid = doc.getDocumentElement().getAttribute("uid");
            assertTrue(uid.startsWith("PRED01_a\tb\nc\rd"), uid);
            assertEquals(1, doc.getElementsByTagName("_flow-tags_").getLength());
        }
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
    // Negative jlong -> unsigned bit pattern
    // -----------------------------------------------------------------------

    @Test
    void negativeGeneratedUsIsTheBitPatternOfALargeUnsignedTime() {
        // Java has no unsigned long: -1 reads as 2^64 - 1 microseconds, which is
        // year 586524, and the call returns rather than throwing.
        String xml = Klv.platformPositionXml(fixture(), -1L);
        assertTrue(xml.contains("=\"586524-01-19T08:01:49.551615Z\"/>"),
                "expected the flow-tags time to be 2^64-1 microseconds; got: " + xml);
    }

    @Test
    void negativeUpdateIntervalUsIsTheBitPatternOfALargeInterval() {
        // Java has no unsigned long: the interval reads as 2^64 - 5 microseconds,
        // and the stale time saturates rather than the call throwing.
        CotConfig cfg = CotConfig.builder().updateIntervalUs(-5L).build();
        String xml = Klv.platformPositionXml(fixture(), cfg, GENERATED_US);
        assertTrue(xml.contains("stale=\"586524-01-19T08:01:49.551615Z\""),
                "expected the stale time to saturate at 2^64-1 microseconds; got: " + xml);
    }

    @Test
    void nullProducerIsAnIllegalArgumentNamingTheComponent() {
        // A required String component read by the generated reader: null has no
        // Rust value to take its place, so the reader throws naming the component.
        CotConfig cfg = new CotConfig("a-f-A-M-F", 5_000_000L, null, null, "m-p");
        IllegalArgumentException ex = assertThrows(IllegalArgumentException.class,
                () -> Klv.platformPositionXml(fixture(), cfg, GENERATED_US));
        assertEquals("CotConfig.producer must not be null", ex.getMessage());
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
