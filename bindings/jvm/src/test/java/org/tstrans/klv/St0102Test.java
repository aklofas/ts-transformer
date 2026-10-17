package org.tstrans.klv;

import static org.junit.jupiter.api.Assertions.*;

import java.util.Optional;
import org.junit.jupiter.api.Test;
import org.tstrans.KlvDecodeException;
import org.tstrans.KlvEncodeException;

/**
 * Tests for ST 0102 Security Metadata LS decode/encode.
 *
 * <p>Ported from {@code bindings/python/tests/test_klv_st0102.py},
 * {@code test_klv_st0102_enums.py}, and {@code test_klv_encode_st0102_st0605.py}.
 */
class St0102Test {

    // -----------------------------------------------------------------------
    // Enum codepoint tests
    // -----------------------------------------------------------------------

    @Test
    void securityClassificationCodepoints() {
        assertEquals(0x01, SecurityClassification.UNCLASSIFIED.code());
        assertEquals(0x02, SecurityClassification.RESTRICTED.code());
        assertEquals(0x03, SecurityClassification.CONFIDENTIAL.code());
        assertEquals(0x04, SecurityClassification.SECRET.code());
        assertEquals(0x05, SecurityClassification.TOP_SECRET.code());
    }

    @Test
    void securityClassificationFromCode() {
        assertEquals(Optional.of(SecurityClassification.UNCLASSIFIED),
                SecurityClassification.fromCode(0x01));
        assertEquals(Optional.of(SecurityClassification.TOP_SECRET),
                SecurityClassification.fromCode(0x05));
        assertEquals(Optional.empty(), SecurityClassification.fromCode(0xFE));
    }

    @Test
    void classifyingCountryCodepoints() {
        // Verify a few key codepoints to catch off-by-one errors vs Tag 12.
        assertEquals(0x05, ClassifyingCountryCodingMethod.ISO_3166_NUMERIC.code());
        assertEquals(0x03, ClassifyingCountryCodingMethod.FIPS_104_TWO_LETTER.code());
        assertEquals(0x10, ClassifyingCountryCodingMethod.GENC_MIXED.code());
        assertEquals(0x08, ClassifyingCountryCodingMethod.OMITTED_VALUE_08.code());
    }

    @Test
    void objectCountryCodepoints() {
        // ST 0102.12 §6.7 Table 2: Tag 12 ISO-3166 Numeric is 0x03 (vs 0x05 on Tag 2).
        assertEquals(0x03, ObjectCountryCodingMethod.ISO_3166_NUMERIC.code());
        // Tag 12 FIPS-104 Two Letter is 0x04 (vs 0x03 on Tag 2).
        assertEquals(0x04, ObjectCountryCodingMethod.FIPS_104_TWO_LETTER.code());
        // Non-contiguous jump to 0x40.
        assertEquals(0x40, ObjectCountryCodingMethod.GENC_ADMIN_SUB.code());
        // 0x10 is unknown (the gap between 0x0F and 0x40).
        assertEquals(Optional.empty(), ObjectCountryCodingMethod.fromCode(0x10));
    }

    @Test
    void objectCountryFromCodeRoundTrip() {
        assertEquals(Optional.of(ObjectCountryCodingMethod.GENC_ADMIN_SUB),
                ObjectCountryCodingMethod.fromCode(0x40));
        assertEquals(Optional.of(ObjectCountryCodingMethod.ISO_3166_NUMERIC),
                ObjectCountryCodingMethod.fromCode(0x03));
    }

    // -----------------------------------------------------------------------
    // Decode tests
    // -----------------------------------------------------------------------

    @Test
    void decodeMinimalBody() throws KlvDecodeException {
        // Body: Tag 1 (Security Classification) = 0x01 (UNCLASSIFIED), 1-byte value.
        // Wire: tag=0x01, length=0x01, value=0x01.
        SecurityLs s = Klv.decodeSecurity(new byte[]{0x01, 0x01, 0x01});
        assertEquals(Optional.of(SecurityClassification.UNCLASSIFIED),
                s.securityClassification());
        assertEquals(0x01, (int) s.securityClassificationCode());
    }

    @Test
    void decodeSecretClassification() throws KlvDecodeException {
        // Tag 1 = 0x04 (SECRET)
        SecurityLs s = Klv.decodeSecurity(new byte[]{0x01, 0x01, 0x04});
        assertEquals(Optional.of(SecurityClassification.SECRET), s.securityClassification());
    }

    @Test
    void decodeClassifyingCountryMethod() throws KlvDecodeException {
        // Tag 2 = 0x05 (ISO_3166_NUMERIC on Tag 2)
        SecurityLs s = Klv.decodeSecurity(new byte[]{0x02, 0x01, 0x05});
        assertEquals(Optional.of(ClassifyingCountryCodingMethod.ISO_3166_NUMERIC),
                s.classifyingCountryCodingMethod());
    }

    @Test
    void decodeObjectCountryMethod() throws KlvDecodeException {
        // Tag 12 = 0x03 (ISO_3166_NUMERIC on Tag 12 — note: 0x03 here, not 0x05)
        SecurityLs s = Klv.decodeSecurity(new byte[]{0x0C, 0x01, 0x03});
        assertEquals(Optional.of(ObjectCountryCodingMethod.ISO_3166_NUMERIC),
                s.objectCountryCodingMethod());
    }

    @Test
    void unknownCodepointSurfacesAsRawCode() throws KlvDecodeException {
        // Tag 1 with out-of-spec codepoint 0xFE → typed accessor empty, raw code preserved.
        SecurityLs s = Klv.decodeSecurity(new byte[]{0x01, 0x01, (byte) 0xFE});
        assertTrue(s.securityClassification().isEmpty());
        assertEquals(254, (int) s.securityClassificationCode());
    }

    @Test
    void omittedValueCodepointRoundTripsViaTypedAccessor() {
        // Forward-compat: the spec-reserved OMITTED_VALUE_08 codepoint (0x08) on
        // Tag 2 is a *known* enum constant (lenient decode tolerates it; only
        // strict mode rejects it). Decoding Tag 2 = 0x08 must surface the typed
        // OMITTED_VALUE_08 constant — proves the codepoint round-trips through
        // the Rust from_wire → to_wire → Java fromCode chain without truncation.
        SecurityLs s = assertDoesNotThrow(() -> Klv.decodeSecurity(new byte[]{0x02, 0x01, 0x08}));
        assertEquals(Optional.of(ClassifyingCountryCodingMethod.OMITTED_VALUE_08),
                s.classifyingCountryCodingMethod());
        assertEquals(0x08, (int) s.classifyingCountryCodingMethodCode());
    }

    @Test
    void decodeEmptyBodySucceeds() throws KlvDecodeException {
        // Lenient mode accepts a record with no tags at all.
        SecurityLs s = Klv.decodeSecurity(new byte[]{});
        assertTrue(s.securityClassification().isEmpty());
        assertNull(s.securityClassificationCode());
        assertTrue(s.unknown().isEmpty());
        assertTrue(s.fieldErrors().isEmpty());
    }

    @Test
    void decodeVersion() throws KlvDecodeException {
        // Tag 22 = 0x000C (version 12 — 2-byte big-endian)
        SecurityLs s = Klv.decodeSecurity(new byte[]{0x16, 0x02, 0x00, 0x0C});
        assertEquals(12, (int) s.version());
    }

    // -----------------------------------------------------------------------
    // Strict-mode tests
    // -----------------------------------------------------------------------

    @Test
    void strictRejectsMissingRequiredTags() {
        // Body with only Tag 1 → strict mode wants 1, 2, 3, 12, 13, 22.
        // The first missing required tag after Tag 1 is Tag 2.
        KlvDecodeException ex = assertThrows(KlvDecodeException.class,
                () -> Klv.decodeSecurity(new byte[]{0x01, 0x01, 0x01}, true));
        assertEquals(KlvDecodeException.Kind.MISSING_REQUIRED_TAG, ex.kind());
    }

    @Test
    void strictAcceptsAllRequiredTags() throws KlvDecodeException {
        // Build a minimal body with all 6 required tags:
        // Tag 1: classification UNCLASSIFIED (0x01)
        // Tag 2: ISO_3166_TWO_LETTER (0x01)
        // Tag 3: "//US" (ASCII)
        // Tag 12: ISO_3166_TWO_LETTER (0x01)
        // Tag 13: "US" (UTF-16 BE with BOM: FE FF 00 55 00 53)
        // Tag 22: version 12 (0x00 0x0C)
        byte[] tag3Value = "//US".getBytes(java.nio.charset.StandardCharsets.US_ASCII);
        // UTF-16BE BOM (FE FF) + "US" as UTF-16BE code units (00 55, 00 53).
        byte[] tag13Value = new byte[]{(byte)0xFE, (byte)0xFF, 0x00, 'U', 0x00, 'S'};
        byte[] body = buildBody(new byte[][]{
                {0x01, 0x01, 0x01},                                  // Tag 1
                {0x02, 0x01, 0x01},                                  // Tag 2
                concat((byte) 0x03, tag3Value),                      // Tag 3
                {0x0C, 0x01, 0x01},                                  // Tag 12
                concat((byte) 0x0D, tag13Value),                     // Tag 13
                {0x16, 0x02, 0x00, 0x0C}                             // Tag 22
        });
        SecurityLs s = Klv.decodeSecurity(body, true);
        assertEquals(Optional.of(SecurityClassification.UNCLASSIFIED), s.securityClassification());
        assertEquals(Optional.of(ClassifyingCountryCodingMethod.ISO_3166_TWO_LETTER),
                s.classifyingCountryCodingMethod());
        assertEquals("//US", s.classifyingCountry());
        assertEquals(12, (int) s.version());
    }

    @Test
    void strictDefaultIsFalse() throws KlvDecodeException {
        // decodeSecurity(byte[]) should behave identically to decodeSecurity(byte[], false).
        byte[] body = new byte[]{0x01, 0x01, 0x01};
        SecurityLs lenient1 = Klv.decodeSecurity(body);
        SecurityLs lenient2 = Klv.decodeSecurity(body, false);
        assertEquals(lenient1.securityClassificationCode(), lenient2.securityClassificationCode());
    }

    // -----------------------------------------------------------------------
    // Encode + round-trip tests
    // -----------------------------------------------------------------------

    @Test
    void encodeRoundTrip() throws KlvDecodeException, KlvEncodeException {
        // Decode a single-tag body, re-encode, decode again — typed field must match.
        SecurityLs s = Klv.decodeSecurity(new byte[]{0x01, 0x01, 0x01});
        byte[] wire = Klv.encodeSecurity(s);
        SecurityLs s2 = Klv.decodeSecurity(wire);
        assertEquals(Optional.of(SecurityClassification.UNCLASSIFIED),
                s2.securityClassification());
    }

    @Test
    void encodeFullRoundTrip() throws KlvDecodeException, KlvEncodeException {
        // Build a SecurityLs via Builder with all 6 required fields, encode, decode.
        SecurityLs original = new SecurityLs.Builder()
                .securityClassificationCode(SecurityClassification.TOP_SECRET.code())
                .classifyingCountryCodingMethodCode(ClassifyingCountryCodingMethod.ISO_3166_THREE_LETTER.code())
                .classifyingCountry("//USA")
                .objectCountryCodingMethodCode(ObjectCountryCodingMethod.ISO_3166_THREE_LETTER.code())
                .objectCountryCodes("USA")
                .version(12)
                .build();

        byte[] wire = Klv.encodeSecurity(original);
        SecurityLs decoded = Klv.decodeSecurity(wire);

        assertEquals(Optional.of(SecurityClassification.TOP_SECRET),
                decoded.securityClassification());
        assertEquals(Optional.of(ClassifyingCountryCodingMethod.ISO_3166_THREE_LETTER),
                decoded.classifyingCountryCodingMethod());
        assertEquals("//USA", decoded.classifyingCountry());
        assertEquals(Optional.of(ObjectCountryCodingMethod.ISO_3166_THREE_LETTER),
                decoded.objectCountryCodingMethod());
        assertEquals("USA", decoded.objectCountryCodes());
        assertEquals(12, (int) decoded.version());
    }

    @Test
    void encodeRawCodeRoundTrips() throws KlvDecodeException, KlvEncodeException {
        // Build with a raw (typed) codepoint via int setter, encode, decode:
        // the round-trip should preserve it (encode reads the raw Integer,
        // not the typed accessor, so unknown codepoints also round-trip).
        SecurityLs original = new SecurityLs.Builder()
                .securityClassificationCode(0x01)  // raw int overload
                .build();
        byte[] wire = Klv.encodeSecurity(original);
        SecurityLs decoded = Klv.decodeSecurity(wire);
        assertEquals(Optional.of(SecurityClassification.UNCLASSIFIED),
                decoded.securityClassification());
        assertEquals(0x01, (int) decoded.securityClassificationCode());
    }

    @Test
    void encodeUnknownTagRoundTrips() throws KlvDecodeException, KlvEncodeException {
        // An unknown tag in the decoded `unknown` list should survive decode→encode→decode.
        // Tag 99 is not in the LS table.
        byte[] body = buildBody(new byte[][]{
                {0x01, 0x01, 0x01},                    // Tag 1
                {0x63, 0x03, 'x', 'y', 'z'}            // Tag 99 (0x63) = "xyz"
        });
        SecurityLs s = Klv.decodeSecurity(body);
        assertEquals(1, s.unknown().size());
        assertEquals(99L, s.unknown().get(0).tag());

        byte[] wire = Klv.encodeSecurity(s);
        SecurityLs s2 = Klv.decodeSecurity(wire);
        assertEquals(1, s2.unknown().size());
        assertEquals(99L, s2.unknown().get(0).tag());
    }

    // -----------------------------------------------------------------------
    // encodeSecurityStrictCompliance tests
    // -----------------------------------------------------------------------

    @Test
    void encodeSecurityStrictComplianceMissingMandatoryTagThrows() {
        // An empty SecurityLs is missing Tag 1 (Security Classification) — the first
        // required tag. encode_strict_compliance checks tags [1,2,3,12,13,22] in order.
        SecurityLs rec = new SecurityLs.Builder().build();
        KlvEncodeException ex = assertThrows(KlvEncodeException.class,
                () -> Klv.encodeSecurityStrictCompliance(rec));
        assertEquals(KlvEncodeException.Kind.MISSING_MANDATORY_ITEM, ex.kind());
        assertTrue(ex.tag().isPresent(), "MISSING_MANDATORY_ITEM must carry the offending tag");
        assertEquals(1L, ex.tag().get().longValue());
    }

    @Test
    void encodeSecurityStrictComplianceSucceedsWithAllRequired() throws KlvEncodeException, KlvDecodeException {
        // Provide all 6 required tags — strict compliance must succeed.
        SecurityLs rec = new SecurityLs.Builder()
                .securityClassificationCode(SecurityClassification.UNCLASSIFIED.code())
                .classifyingCountryCodingMethodCode(ClassifyingCountryCodingMethod.ISO_3166_TWO_LETTER.code())
                .classifyingCountry("//US")
                .objectCountryCodingMethodCode(ObjectCountryCodingMethod.ISO_3166_TWO_LETTER.code())
                .objectCountryCodes("US")
                .version(12)
                .build();
        byte[] wire = assertDoesNotThrow(() -> Klv.encodeSecurityStrictCompliance(rec));
        assertNotNull(wire);
        assertTrue(wire.length > 0);
        // Round-trip: the encoded bytes must decode cleanly.
        SecurityLs decoded = Klv.decodeSecurity(wire);
        assertEquals(Optional.of(SecurityClassification.UNCLASSIFIED), decoded.securityClassification());
        assertEquals("//US", decoded.classifyingCountry());
        assertEquals(12, (int) decoded.version());
    }

    // -----------------------------------------------------------------------
    // Builder tests
    // -----------------------------------------------------------------------

    @Test
    void builderDefaultsAreNullOrEmpty() {
        SecurityLs s = new SecurityLs.Builder().build();
        assertNull(s.securityClassificationCode());
        assertTrue(s.securityClassification().isEmpty());
        assertNull(s.classifyingCountryCodingMethodCode());
        assertNull(s.objectCountryCodingMethodCode());
        assertNull(s.classifyingCountry());
        assertNull(s.version());
        assertTrue(s.unknown().isEmpty());
        assertTrue(s.fieldErrors().isEmpty());
    }

    @Test
    void builderEnumOverloadsStoreCode() {
        SecurityLs s = new SecurityLs.Builder()
                .securityClassificationCode(SecurityClassification.SECRET.code())
                .classifyingCountryCodingMethodCode(ClassifyingCountryCodingMethod.GENC_NUMERIC.code())
                .objectCountryCodingMethodCode(ObjectCountryCodingMethod.GENC_ADMIN_SUB.code())
                .build();
        assertEquals(0x04, (int) s.securityClassificationCode());
        assertEquals(0x0F, (int) s.classifyingCountryCodingMethodCode());
        assertEquals(0x40, (int) s.objectCountryCodingMethodCode());
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /** Concatenate TLV chunks into a body byte array. */
    private static byte[] buildBody(byte[][] chunks) {
        int total = 0;
        for (byte[] c : chunks) total += c.length;
        byte[] out = new byte[total];
        int pos = 0;
        for (byte[] c : chunks) {
            System.arraycopy(c, 0, out, pos, c.length);
            pos += c.length;
        }
        return out;
    }

    /** Build a TLV triple: tag (1 byte), BER-short length, value. */
    private static byte[] concat(byte tag, byte[] value) {
        byte[] out = new byte[2 + value.length];
        out[0] = tag;
        out[1] = (byte) value.length;
        System.arraycopy(value, 0, out, 2, value.length);
        return out;
    }
}
