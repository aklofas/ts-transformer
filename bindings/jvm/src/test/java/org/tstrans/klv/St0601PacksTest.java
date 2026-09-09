package org.tstrans.klv;

import static org.junit.jupiter.api.Assertions.*;

import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;
import java.util.Map;
import org.junit.jupiter.api.Test;
import org.tstrans.KlvDecodeException;
import org.tstrans.KlvEncodeException;

/**
 * WP-C ST 0601 pack & list items (Table C1) + {@code klv::st1010} SDCC-FLP —
 * JVM binding tests.
 *
 * <p>Spec vectors are transcribed from the same MISB ST 0601.19 §8 worked
 * examples the Rust ({@code crates/tst-core/src/klv/st0601/tests.rs},
 * {@code wpc_*}) and Python ({@code test_klv_st0601_packs.py}) suites pin,
 * per the closed-loop-insufficient lesson: a hand-built spec-byte vector
 * catches a wrong wire formula that a decode(encode(x)) round trip cannot.
 * Each vector is also re-encoded and the resulting TLV bytes compared back
 * against the original, exercising the Java-&gt;Rust inverse translator.
 *
 * <p>Hand-built-wire helpers below mirror {@link St0601Test}'s local
 * {@code buildLs} family — each test file keeps its own small copy rather
 * than sharing across test classes (repo convention).
 */
class St0601PacksTest {

    private static final byte[] UL = HexFormat.of().parseHex("060e2b34020b01010e01030101000000");

    // -----------------------------------------------------------------------
    // Hand-built-wire test helpers
    // -----------------------------------------------------------------------

    private static byte[] hex(String s) {
        return HexFormat.of().parseHex(s.replaceAll("\\s+", ""));
    }

    private static byte[] concat(byte[]... parts) {
        int total = 0;
        for (byte[] p : parts) total += p.length;
        byte[] out = new byte[total];
        int off = 0;
        for (byte[] p : parts) {
            System.arraycopy(p, 0, out, off, p.length);
            off += p.length;
        }
        return out;
    }

    /** BER-OID tag encoding (base-128, high-bit-continuation): single byte for
     * tag &lt; 0x80, multi-byte above that. */
    private static byte[] berOidTag(long tag) {
        if (tag < 0x80) {
            return new byte[] {(byte) tag};
        }
        List<Integer> groups = new ArrayList<>();
        long t = tag;
        groups.add((int) (t & 0x7F));
        t >>= 7;
        while (t > 0) {
            groups.add((int) (t & 0x7F));
            t >>= 7;
        }
        java.util.Collections.reverse(groups);
        byte[] out = new byte[groups.size()];
        for (int i = 0; i < groups.size() - 1; i++) {
            out[i] = (byte) (groups.get(i) | 0x80);
        }
        out[groups.size() - 1] = (byte) groups.get(groups.size() - 1).intValue();
        return out;
    }

    /** BER definite-form length: short form under 0x80, else 0x8X header + X big-endian bytes. */
    private static byte[] berLen(int n) {
        if (n < 0x80) {
            return new byte[] {(byte) n};
        }
        List<Byte> payload = new ArrayList<>();
        int t = n;
        while (t > 0) {
            payload.add(0, (byte) (t & 0xFF));
            t >>>= 8;
        }
        byte[] out = new byte[1 + payload.size()];
        out[0] = (byte) (0x80 | payload.size());
        for (int i = 0; i < payload.size(); i++) out[1 + i] = payload.get(i);
        return out;
    }

    /** One {@code [BER-OID tag][BER length][value]} TLV — the ST 0601 LS body shape. */
    private static byte[] tlv(long tag, byte[] value) {
        return concat(berOidTag(tag), berLen(value.length), value);
    }

    /** ST 0601 §6.3 16-bit running-sum checksum over {@code [UL .. start of Tag 1 value]}. */
    private static int checksum(byte[] buf) {
        int bcc = 0;
        for (int i = 0; i < buf.length; i++) {
            int shift = 8 * ((i + 1) % 2);
            bcc = (bcc + ((buf[i] & 0xFF) << shift)) & 0xFFFF;
        }
        return bcc & 0xFFFF;
    }

    /** Wrap an ST 0601 LS body with UL + outer BER length + ... + Tag 1 TLV, computing a
     * valid running-sum checksum so lenient decode accepts it. */
    private static byte[] wrapWithChecksum(byte[] bodyWithoutChecksum) {
        byte[] bodyWithCsumTlv = concat(bodyWithoutChecksum, new byte[] {0x01, 0x02});
        byte[] outerLen = berLen(bodyWithCsumTlv.length + 2);
        byte[] prefix = concat(UL, outerLen, bodyWithCsumTlv);
        int cksum = checksum(prefix);
        return concat(prefix, new byte[] {(byte) (cksum >>> 8), (byte) cksum});
    }

    /** Decode a minimal ST 0601 record containing exactly one TLV. */
    private static UasDatalinkLs decodeSingleTlv(long tag, byte[] value) throws KlvDecodeException {
        return Klv.decodeUasDatalink(wrapWithChecksum(tlv(tag, value)));
    }

    /** Decode a full (already TLV-framed) LS body. */
    private static UasDatalinkLs decodeBody(byte[] body) throws KlvDecodeException {
        return Klv.decodeUasDatalink(wrapWithChecksum(body));
    }

    private static long[] readBerOidTag(byte[] buf, int i) {
        long tag = 0;
        int j = i;
        while (true) {
            int b = buf[j] & 0xFF;
            tag = (tag << 7) | (b & 0x7F);
            j++;
            if ((b & 0x80) == 0) break;
        }
        return new long[] {tag, j - i};
    }

    /** Yield {@code (tag, valueBytes)} for every top-level TLV in an ST 0601 wire record's body. */
    private static List<Map.Entry<Long, byte[]>> iterTlvs(byte[] encoded) {
        List<Map.Entry<Long, byte[]>> out = new ArrayList<>();
        int offset = 16;
        int first = encoded[offset] & 0xFF;
        if (first < 0x80) {
            offset += 1;
        } else {
            offset += 1 + (first & 0x7F);
        }
        while (offset < encoded.length) {
            long[] tagResult = readBerOidTag(encoded, offset);
            long tag = tagResult[0];
            offset += (int) tagResult[1];
            int lengthByte = encoded[offset] & 0xFF;
            int length;
            if (lengthByte < 0x80) {
                length = lengthByte;
                offset += 1;
            } else {
                int nbytes = lengthByte & 0x7F;
                length = 0;
                for (int k = 0; k < nbytes; k++) {
                    length = (length << 8) | (encoded[offset + 1 + k] & 0xFF);
                }
                offset += 1 + nbytes;
            }
            byte[] value = java.util.Arrays.copyOfRange(encoded, offset, offset + length);
            out.add(Map.entry(tag, value));
            offset += length;
        }
        return out;
    }

    private static byte[] findTagValueBytes(byte[] encoded, long tag) {
        for (var e : iterTlvs(encoded)) {
            if (e.getKey() == tag) return e.getValue();
        }
        return null;
    }

    /** Re-encode {@code record} and return {@code tag}'s TLV value bytes. */
    private static byte[] reencodedTagValue(long tag, UasDatalinkLs record) throws KlvEncodeException {
        return findTagValueBytes(Klv.encodeUasDatalink(record), tag);
    }

    // -----------------------------------------------------------------------
    // WP-C Task C2: simple DLP packs (81/115/116/121/127/143)
    // -----------------------------------------------------------------------

    @Test
    void imageHorizonGeoTruncatedVector() throws KlvDecodeException, KlvEncodeException {
        // Tag 81 — (0,36)->(56,0), no optional geo fields (§8.81 example).
        byte[] v = hex("00 24 38 00");
        UasDatalinkLs rec = decodeSingleTlv(81, v);
        ImageHorizonPixels h = rec.imageHorizon();
        assertNotNull(h);
        assertEquals(0, h.x0Pct());
        assertEquals(36, h.y0Pct());
        assertEquals(56, h.x1Pct());
        assertEquals(0, h.y1Pct());
        assertNull(h.startLatDeg());
        assertNull(h.startLonDeg());
        assertNull(h.endLatDeg());
        assertNull(h.endLonDeg());
        assertArrayEquals(v, reencodedTagValue(81, rec));
    }

    @Test
    void controlCommandMultiInstanceAndTimeUs() throws KlvDecodeException, KlvEncodeException {
        // Tag 115 — MULTI-INSTANCE: two occurrences append two ControlCommands.
        byte[] v115a = hex("05 11 466C7920746F20576179706F696E742031");
        byte[] v115b = hex("07 03 41 42 43"); // (7, "ABC")
        UasDatalinkLs rec = decodeBody(concat(tlv(115, v115a), tlv(115, v115b)));
        assertEquals(2, rec.controlCommands().size());
        assertEquals(new ControlCommand(5, "Fly to Waypoint 1", null), rec.controlCommands().get(0));
        assertNull(rec.controlCommands().get(0).timeUs());
        assertEquals(new ControlCommand(7, "ABC", null), rec.controlCommands().get(1));

        // Round trip through encode -> decode preserves both instances.
        UasDatalinkLs back = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));
        assertEquals(rec.controlCommands(), back.controlCommands());
    }

    @Test
    void controlCommandWithTimeUsRoundTrips() throws KlvDecodeException, KlvEncodeException {
        // time_us presence isn't one of the Table C1 vectors (only its absence
        // is spec-pinned above) — a closed-loop round trip is a legitimate
        // binding-fidelity check here, not a wire-spec claim.
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ByteBuffer.wrap(UL))
                .controlCommands(List.of(new ControlCommand(200, "abc", 1_700_000_000_000_000L)))
                .build();
        UasDatalinkLs back = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));
        assertEquals(rec.controlCommands(), back.controlCommands());
    }

    @Test
    void controlCommandVerificationAndActiveWavelengthsIdLists() throws KlvDecodeException, KlvEncodeException {
        UasDatalinkLs rec116 = decodeSingleTlv(116, hex("03 07"));
        assertEquals(List.of(3L, 7L), rec116.controlCommandVerification());
        assertArrayEquals(hex("03 07"), reencodedTagValue(116, rec116));

        UasDatalinkLs rec121 = decodeSingleTlv(121, hex("01 03"));
        assertEquals(List.of(1L, 3L), rec121.activeWavelengths());
        assertArrayEquals(hex("01 03"), reencodedTagValue(121, rec121));
    }

    @Test
    void sensorFrameRateVectorAndDenominatorDefault() throws KlvDecodeException, KlvEncodeException {
        byte[] v = hex("83 D4 60 87 69");
        UasDatalinkLs rec = decodeSingleTlv(127, v);
        SensorFrameRate fr = rec.sensorFrameRate();
        assertNotNull(fr);
        assertEquals(60000L, fr.numerator());
        assertEquals(1001L, fr.denominator());
        assertArrayEquals(v, reencodedTagValue(127, rec));

        // Denominator absent from the wire defaults to 1.
        UasDatalinkLs rec2 = decodeSingleTlv(127, hex("1E"));
        SensorFrameRate fr2 = rec2.sensorFrameRate();
        assertEquals(30L, fr2.numerator());
        assertEquals(1L, fr2.denominator());
        assertEquals(30.0, fr2.fps(), 1e-9);
        assertArrayEquals(hex("1E"), reencodedTagValue(127, rec2));
    }

    @Test
    void metadataSubstreamIdVector() throws KlvDecodeException, KlvEncodeException {
        byte[] v = hex("00 8DC4F462 3EA25A85 9C5D0AF0 C95E8C39");
        UasDatalinkLs rec = decodeSingleTlv(143, v);
        MetadataSubstreamId ms = rec.metadataSubstreamId();
        assertNotNull(ms);
        assertEquals(0L, ms.localId());
        assertEquals((byte) 0x8D, ms.uuid()[0]);
        assertEquals(16, ms.uuid().length);
        assertArrayEquals(v, reencodedTagValue(143, rec));
    }

    @Test
    void metadataSubstreamIdUuidWrongLengthRejected() {
        assertThrows(IllegalArgumentException.class, () -> new MetadataSubstreamId(0, new byte[15]));
    }

    @Test
    void strictComplianceAllowsRepeated115And102() throws KlvDecodeException {
        // Multiples Allowed = Yes items must not trip the once-per-packet
        // DuplicateTag check under strict compliance — exercises BOTH
        // carve-out tags (115 and 102).
        byte[] pack = hex("03 84 04 3F800000 40000000 40800000 3F000000 00000000 BF000000");
        byte[] timestampBytes = ByteBuffer.allocate(8).putLong(1_700_000_000_000_000L).array();
        byte[] body = concat(
                tlv(2, timestampBytes),
                tlv(115, hex("05 11 466C7920746F20576179706F696E742031")),
                tlv(115, hex("07 03 41 42 43")),
                tlv(102, pack),
                tlv(102, pack),
                tlv(65, new byte[] {19}));
        UasDatalinkLs rec = Klv.decodeUasDatalink(wrapWithChecksum(body), true, true);
        assertEquals(2, rec.controlCommands().size());
        assertEquals(2, rec.sdccFlps().size());
    }

    // -----------------------------------------------------------------------
    // WP-C Task C3: VLP series packs (122/128/130/138/140/141/142)
    // -----------------------------------------------------------------------

    @Test
    void countryCodesVector() throws KlvDecodeException, KlvEncodeException {
        byte[] v = hex("01 0E 03 43414E 00 03 465241");
        UasDatalinkLs rec = decodeSingleTlv(122, v);
        CountryCodes cc = rec.countryCodes();
        assertEquals(new CountryCodes(14, "CAN", null, "FRA"), cc);
        assertArrayEquals(v, reencodedTagValue(122, rec));
    }

    @Test
    void countryCodesTruncationCases() throws KlvDecodeException, KlvEncodeException {
        // Manufacture explicit-length-0 with Operator present canonicalizes
        // away the now-redundant trailing zero-length pair on re-encode.
        byte[] v = hex("01 0E 03 43414E 03 465241 00");
        UasDatalinkLs rec = decodeSingleTlv(122, v);
        CountryCodes cc = rec.countryCodes();
        assertEquals("FRA", cc.operator());
        assertNull(cc.manufacture());
        assertArrayEquals(hex("01 0E 03 43414E 03 465241"), reencodedTagValue(122, rec));

        // Fully truncated: only codingMethod + overflight on the wire.
        byte[] v2 = hex("01 0E 03 43414E");
        UasDatalinkLs rec2 = decodeSingleTlv(122, v2);
        assertNull(rec2.countryCodes().operator());
        assertNull(rec2.countryCodes().manufacture());
        assertArrayEquals(v2, reencodedTagValue(122, rec2));
    }

    @Test
    void wavelengthsListVector() throws KlvDecodeException, KlvEncodeException {
        byte[] v = hex("0D 15 0000 07D0 0000 0FA0 4E4E 4952");
        UasDatalinkLs rec = decodeSingleTlv(128, v);
        List<WavelengthRecord> wl = rec.wavelengthsList();
        assertEquals(1, wl.size());
        WavelengthRecord w = wl.get(0);
        assertEquals(21L, w.id());
        assertEquals("NNIR", w.name());
        assertArrayEquals(v, reencodedTagValue(128, rec));
    }

    @Test
    void airbaseLocationsVector() throws KlvDecodeException, KlvEncodeException {
        byte[] v = hex("0B406BC20919BDA554070E000B40783CB819A2927407C600");
        UasDatalinkLs rec = decodeSingleTlv(130, v);
        AirbaseLocations al = rec.airbaseLocations();
        Location takeOff = al.takeOff();
        assertEquals(38.841859, takeOff.latDeg(), 1e-4);
        assertEquals(-77.036784, takeOff.lonDeg(), 1e-4);
        assertEquals(3.0, takeOff.haeM(), 0.1);
        Location recovery = al.recovery();
        assertEquals(38.939353, recovery.latDeg(), 1e-4);
        assertEquals(-77.459811, recovery.lonDeg(), 1e-4);
        assertEquals(95.0, recovery.haeM(), 0.1);
        assertArrayEquals(v, reencodedTagValue(130, rec));
    }

    @Test
    void airbaseLocationsRecoveryOmittedDefaultsToTakeOff() throws KlvDecodeException, KlvEncodeException {
        byte[] v = hex("0B406BC20919BDA554070E00"); // take-off only, no recovery pair
        UasDatalinkLs rec = decodeSingleTlv(130, v);
        AirbaseLocations al = rec.airbaseLocations();
        assertEquals(al.takeOff(), al.recovery());
        assertArrayEquals(v, reencodedTagValue(130, rec));
    }

    @Test
    void payloadListVector() throws KlvDecodeException, KlvEncodeException {
        byte[] v = hex(
                "03 12 0000 0F56 4953 204E 6F73 6520 4361 6D65 7261"
                        + "15 01 0012 4143 4D45 2056 4953 204D 6F64 656C 2031 3233"
                        + "14 02 0011 4143 4D45 2049 5220 4D6F 6465 6C20 3435 36");
        assertEquals(63, v.length, "the §8.138 example value is 63 bytes");
        UasDatalinkLs rec = decodeSingleTlv(138, v);
        PayloadList pl = rec.payloadList();
        assertEquals(3L, pl.count());
        assertEquals(3, pl.records().size());
        PayloadRecord r0 = pl.records().get(0);
        assertEquals(0L, r0.id());
        assertEquals(PayloadType.ELECTRO_OPTICAL, r0.payloadType());
        assertEquals("VIS Nose Camera", r0.name());
        assertEquals("ACME VIS Model 123", pl.records().get(1).name());
        assertEquals("ACME IR Model 456", pl.records().get(2).name());
        assertArrayEquals(v, reencodedTagValue(138, rec));
    }

    @Test
    void weaponsStoresVectorAndStatusAccessors() throws KlvDecodeException, KlvEncodeException {
        byte[] r1 = hex("0E 01 01 01 03 82 03 07 48 61 72 70 6F 6F 6E"); // Harpoon
        byte[] r2 = hex("0F 01 01 02 02 9E 04 08 48 65 6C 6C 66 69 72 65"); // Hellfire
        byte[] r3 = hex("0C 01 02 01 01 03 06 47 42 55 2D 31 35"); // GBU-15
        byte[] v = concat(r1, r2, r3);
        assertEquals(44, v.length, "3 records' own length prefixes total 44 bytes");

        UasDatalinkLs rec = decodeSingleTlv(140, v);
        List<WeaponsStore> stores = rec.weaponsStores();
        assertEquals(3, stores.size());

        WeaponsStore harpoon = stores.get(0);
        assertEquals(1L, harpoon.stationId());
        assertEquals(1L, harpoon.hardpointId());
        assertEquals(1L, harpoon.carriageId());
        assertEquals(3L, harpoon.storeId());
        assertEquals(3, harpoon.generalStatus());
        assertTrue(harpoon.fuzeEnabled());
        assertFalse(harpoon.laserEnabled());
        assertFalse(harpoon.targetEnabled());
        assertFalse(harpoon.weaponArmed());
        assertEquals("Harpoon", harpoon.weaponType());

        WeaponsStore hellfire = stores.get(1);
        assertEquals(4, hellfire.generalStatus());
        assertTrue(hellfire.fuzeEnabled());
        assertTrue(hellfire.laserEnabled());
        assertTrue(hellfire.targetEnabled());
        assertTrue(hellfire.weaponArmed());
        assertEquals("Hellfire", hellfire.weaponType());

        WeaponsStore gbu15 = stores.get(2);
        assertEquals(1L, gbu15.stationId());
        assertEquals(2L, gbu15.hardpointId());
        assertEquals(3, gbu15.generalStatus());
        assertFalse(gbu15.fuzeEnabled());
        assertEquals("GBU-15", gbu15.weaponType());

        assertArrayEquals(v, reencodedTagValue(140, rec));
    }

    @Test
    void waypointListVector() throws KlvDecodeException, KlvEncodeException {
        byte[] v = hex(
                "0F 00 0001 03 4071D894 19BDBFE7 089800"
                        + "0F 01 0002 02 4071D388 19BCCE24 08FC00"
                        + "0F 02 7FFF 01 4071E308 19BF2C1B 07D000"
                        + "0F 03 FFFE 00 4071E5AF 19BF5AA7 096000");
        UasDatalinkLs rec = decodeSingleTlv(141, v);
        List<Waypoint> wps = rec.waypointList();
        assertEquals(4, wps.size());
        assertEquals(1, wps.get(0).prosecutionOrder());
        assertEquals(2, wps.get(1).prosecutionOrder());
        assertEquals(0x7FFF, wps.get(2).prosecutionOrder()); // cancelled
        assertEquals(-2, wps.get(3).prosecutionOrder()); // historical
        assertEquals(3L, wps.get(0).info());
        assertEquals(0L, wps.get(3).info());

        Location loc = wps.get(0).location();
        assertEquals(38.889422, loc.latDeg(), 1e-5);
        assertEquals(-77.035162, loc.lonDeg(), 1e-5);
        assertEquals(200.0, loc.haeM(), 0.1);

        Location loc3 = wps.get(3).location();
        assertEquals(38.889822, loc3.latDeg(), 1e-5);
        assertEquals(300.0, loc3.haeM(), 0.1);

        assertArrayEquals(v, reencodedTagValue(141, rec));
    }

    @Test
    void viewDomainTruncatedRoll() throws KlvDecodeException, KlvEncodeException {
        byte[] v = hex("06 348000 4B0000 06 1A4000 0C8000");
        UasDatalinkLs rec = decodeSingleTlv(142, v);
        ViewDomain vd = rec.viewDomain();
        assertEquals(210.0, vd.azimuth().startDeg(), 0.01);
        assertEquals(300.0, vd.azimuth().rangeDeg(), 0.01);
        assertEquals(-75.0, vd.elevation().startDeg(), 0.01);
        assertEquals(50.0, vd.elevation().rangeDeg(), 0.01);
        assertNull(vd.roll());
        assertArrayEquals(v, reencodedTagValue(142, rec));
    }

    @Test
    void viewDomainLeadingUnknownPair() throws KlvDecodeException, KlvEncodeException {
        byte[] v = hex("00 06 1A4000 0C8000 06 578000 050000");
        UasDatalinkLs rec = decodeSingleTlv(142, v);
        ViewDomain vd = rec.viewDomain();
        assertNull(vd.azimuth());
        assertEquals(-75.0, vd.elevation().startDeg(), 0.01);
        assertEquals(350.0, vd.roll().startDeg(), 0.1);
        assertEquals(20.0, vd.roll().rangeDeg(), 0.1);
        assertArrayEquals(v, reencodedTagValue(142, rec));
    }

    // -----------------------------------------------------------------------
    // WP-C Task C4: Tag 102 SDCC-FLP positional capture
    // -----------------------------------------------------------------------

    @Test
    void sdccPositionalCapture() throws KlvDecodeException, KlvEncodeException {
        // Two Tag 102 occurrences over two disjoint preceding-item groups
        // prove per-occurrence capture (not one running list). Preceding
        // items are arbitrary forward-compat tags (150-152, 160-162, all
        // outside the typed 1-143 range) rather than real scalar tags — the
        // positional-capture logic under test doesn't care whether a
        // preceding tag is typed or unknown (see the Rust SdccFlpField
        // rustdoc: "known or unknown, but never another Tag 102").
        byte[] pack = hex("03 84 04 3F800000 40000000 40800000 3F000000 00000000 BF000000");
        byte[] body = concat(
                tlv(150, new byte[] {0x01}),
                tlv(151, new byte[] {0x02}),
                tlv(152, new byte[] {0x03}),
                tlv(102, pack),
                tlv(160, new byte[] {0x04}),
                tlv(161, new byte[] {0x05}),
                tlv(162, new byte[] {0x06}),
                tlv(102, pack));
        UasDatalinkLs rec = decodeBody(body);
        assertEquals(2, rec.sdccFlps().size());
        assertEquals(List.of(150L, 151L, 152L), rec.sdccFlps().get(0).precedingTags());
        assertEquals(List.of(160L, 161L, 162L), rec.sdccFlps().get(1).precedingTags());
        SdccFlp m = Klv.decodeSdccFlp(readByteBuffer(rec.sdccFlps().get(0).bytes()));
        assertArrayEquals(new double[] {1.0, 2.0, 4.0}, m.stdDevs());

        // Byte-fidelity re-encode: both occurrences survive a round trip.
        UasDatalinkLs back = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));
        assertEquals(2, back.sdccFlps().size());
        assertEquals(rec.sdccFlps().get(0).bytes(), back.sdccFlps().get(0).bytes());
    }

    @Test
    void sdccMalformedHeaderIsFieldErrorNotException() throws KlvDecodeException {
        // A truncated BER-OID Matrix Size (empty value) cannot be peeked for
        // N — the occurrence is dropped into fieldErrors, not thrown.
        UasDatalinkLs rec = decodeSingleTlv(102, new byte[0]);
        assertTrue(rec.sdccFlps().isEmpty());
        assertEquals(1, rec.fieldErrors().size());
    }

    @Test
    void sdccTagDroppedFromUnknownOnTheJvmBoundary() throws KlvDecodeException, KlvEncodeException {
        // Tag 102 is now typed (sdccFlps) — is_st0601_typed_tag's "typed
        // wins, silently drop" collision policy filters a caller-supplied
        // `unknown` entry at tag 102 BEFORE it ever reaches the real Rust
        // encoder, so this must NOT throw.
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ByteBuffer.wrap(UL))
                .unknown(List.of(new KlvUnknownField(102L, ByteBuffer.wrap(new byte[] {0x01}))))
                .build();
        byte[] encoded = Klv.encodeUasDatalink(rec); // must NOT throw
        UasDatalinkLs decoded = Klv.decodeUasDatalink(encoded);
        assertTrue(decoded.unknown().stream().noneMatch(f -> f.tag() == 102L));
    }

    @Test
    void sdccFlpFieldLongPrecedingTagsRoundTripsWithoutAborting()
            throws KlvDecodeException, KlvEncodeException {
        // Regression for a JNI local-ref leak: read_sdcc_flp_field's
        // precedingTags loop previously called .get(i) per iteration with
        // NO per-item local frame, while itself running inside the fixed
        // 16-slot with_local_frame that wraps each sdccFlps list item — a
        // precedingTags list with more entries than that frame's spare
        // capacity exhausted the JNI local-ref table and aborted the JVM
        // (not a catchable Java exception). 64 entries is comfortably past
        // the old ~13-entry threshold. Fixed by routing through the shared
        // jutil::read_long_list helper, which applies its own per-item frame.
        //
        // NOTE: `precedingTags` is a DECODE-DERIVED capture, not literal
        // wire data — the Rust SdccFlpField rustdoc's "ascending-order
        // emission caveat" says encode only ever re-emits `bytes`
        // verbatim, never `precedingTags` itself (decode recomputes it
        // fresh from whatever wire-order tags actually precede the
        // occurrence). So a hand-constructed 64-entry list is not expected
        // to survive unchanged; what this test proves is that
        // `Klv.encodeUasDatalink` (which reads it back via
        // `read_sdcc_flp_field`) does not crash the JVM, and that `bytes`
        // (which IS re-emitted verbatim) does survive.
        List<Long> preceding = new ArrayList<>();
        for (long i = 0; i < 64; i++) {
            preceding.add(i);
        }
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ByteBuffer.wrap(UL))
                .sdccFlps(List.of(new SdccFlpField(preceding, ByteBuffer.wrap(hex("038404")))))
                .build();
        byte[] encoded = Klv.encodeUasDatalink(rec); // must not crash the JVM
        UasDatalinkLs back = Klv.decodeUasDatalink(encoded);
        assertEquals(1, back.sdccFlps().size());
        assertArrayEquals(hex("038404"), readByteBuffer(back.sdccFlps().get(0).bytes()));
    }

    @Test
    void controlCommandVerificationLongListRoundTripsWithoutAborting()
            throws KlvDecodeException, KlvEncodeException {
        // Same class of fix as the precedingTags regression above, applied
        // to controlCommandVerification's (and activeWavelengths') own
        // read_long_list call site.
        List<Long> ids = new ArrayList<>();
        for (long i = 0; i < 64; i++) {
            ids.add(i);
        }
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ByteBuffer.wrap(UL))
                .controlCommandVerification(ids)
                .build();
        UasDatalinkLs back = Klv.decodeUasDatalink(Klv.encodeUasDatalink(rec));
        assertEquals(ids, back.controlCommandVerification());
    }

    // -----------------------------------------------------------------------
    // WP-C carry-forward: is_st0601_typed_tag predicate covers every WP-C tag
    // -----------------------------------------------------------------------

    @Test
    void wpcTypedTagsDroppedFromUnknownOnCollision() throws KlvDecodeException, KlvEncodeException {
        // Every WP-C tag must be recognized as typed by is_st0601_typed_tag —
        // a caller-supplied `unknown` entry at that tag is silently dropped
        // (typed wins) rather than surviving into the encoded/decoded
        // record. Before this predicate update, a WP-C tag supplied via
        // `unknown` would have slipped past the filter and hit the real
        // Rust encoder's own (stricter) ReservedTagInUnknown check instead.
        long[] wpcTags = {81, 102, 115, 116, 121, 122, 127, 128, 130, 138, 140, 141, 142, 143};
        for (long tag : wpcTags) {
            UasDatalinkLs rec = new UasDatalinkLs.Builder()
                    .universalLabel(ByteBuffer.wrap(UL))
                    .unknown(List.of(new KlvUnknownField(tag, ByteBuffer.wrap(new byte[] {0x01}))))
                    .build();
            byte[] encoded = Klv.encodeUasDatalink(rec); // must NOT throw
            UasDatalinkLs decoded = Klv.decodeUasDatalink(encoded);
            long tagCopy = tag;
            assertTrue(decoded.unknown().stream().noneMatch(f -> f.tag() == tagCopy),
                    "tag " + tag + " must be dropped from unknown (typed wins)");
        }
    }

    @Test
    void longUnknownListRoundTripsWithPerItemLocalFrame()
            throws KlvDecodeException, KlvEncodeException {
        // Regression for the last per-item JNI loop in jutil without its
        // own local frame: read_unknown_list minted ~5 local refs per
        // KlvUnknownField (List.get, tag(), value(), duplicate(), the byte[]
        // for read_byte_buffer) and freed none until the native returned.
        // Every sibling list reader (read_long_list, the st0601/st0903 list
        // walkers, the codec NAL/OBU list readers) wraps each iteration in
        // with_local_frame; this one was inherited by EVERY typed KLV set's
        // encode path. HotSpot grows the local-ref table silently (measured:
        // 20 000 entries, ~100k live refs, no -Xcheck:jni warning on JDK 17),
        // so the leak is NOT observable from Java — this test instead pins
        // the frame change's own hazard: a ref that escaped the popped
        // per-item frame would corrupt or drop an entry, and 1000 entries
        // checked for ORDER and CONTENT catch that.
        List<KlvUnknownField> many = new ArrayList<>();
        for (int i = 0; i < 1000; i++) {
            // Tags above the ST 0601 typed range (1..=143) and clear of the
            // deprecated-tag stand-ins; each value carries its own index so
            // the round trip proves ORDER and CONTENT, not just the count.
            many.add(new KlvUnknownField(1000L + i,
                    ByteBuffer.wrap(new byte[] {(byte) (i >> 8), (byte) i})));
        }
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ByteBuffer.wrap(UL))
                .unknown(many)
                .build();
        byte[] encoded = Klv.encodeUasDatalink(rec);
        UasDatalinkLs back = Klv.decodeUasDatalink(encoded);
        assertEquals(1000, back.unknown().size());
        for (int i = 0; i < 1000; i++) {
            KlvUnknownField f = back.unknown().get(i);
            assertEquals(1000L + i, f.tag(), "unknown tag order at index " + i);
            byte[] v = readByteBuffer(f.value());
            assertEquals((byte) (i >> 8), v[0], "value high byte at index " + i);
            assertEquals((byte) i, v[1], "value low byte at index " + i);
        }
    }

    @Test
    void deprecatedTag66AndStandInTag200StayUntyped() throws KlvDecodeException, KlvEncodeException {
        // 66 (deprecated-forever) and 200 (out of range) are the durable
        // unknown-tag test stand-ins — encoding must NOT reject them.
        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ByteBuffer.wrap(UL))
                .unknown(List.of(
                        new KlvUnknownField(66L, ByteBuffer.wrap(new byte[] {(byte) 0xDE, (byte) 0xAD})),
                        new KlvUnknownField(200L, ByteBuffer.wrap(new byte[] {(byte) 0xBE, (byte) 0xEF}))))
                .build();
        byte[] encoded = Klv.encodeUasDatalink(rec);
        UasDatalinkLs back = Klv.decodeUasDatalink(encoded);
        assertTrue(back.unknown().stream().anyMatch(
                f -> f.tag() == 66L && readByteBuffer(f.value())[0] == (byte) 0xDE));
        assertTrue(back.unknown().stream().anyMatch(
                f -> f.tag() == 200L && readByteBuffer(f.value())[0] == (byte) 0xBE));
    }

    // -----------------------------------------------------------------------
    // klv::st1010 SDCC-FLP — general-purpose module, standalone entry points
    // -----------------------------------------------------------------------

    @Test
    void decodeSdccFlpMode1Golden() throws KlvDecodeException {
        // Hand-derived Mode-1 golden (ST 1010.1 back-compat): correlations
        // are always ST 1201 in Mode 1; std devs assumed IEEE.
        byte[] v = hex("03 43 3F800000 40000000 40800000 600000 400000 200000");
        SdccFlp m = Klv.decodeSdccFlp(v);
        assertEquals(3L, m.matrixSize());
        assertArrayEquals(new double[] {1.0, 2.0, 4.0}, m.stdDevs());
        assertEquals(0.5, m.correlations()[0], 1e-6);
        assertEquals(0.0, m.correlations()[1], 1e-6);
        assertEquals(-0.5, m.correlations()[2], 1e-6);
    }

    @Test
    void decodeSdccFlpMode2Full3x3IeeeGolden() throws KlvDecodeException {
        byte[] v = hex("03 84 04 3F800000 40000000 40800000 3F000000 00000000 BF000000");
        SdccFlp m = Klv.decodeSdccFlp(v);
        assertEquals(3L, m.matrixSize());
        assertArrayEquals(new double[] {1.0, 2.0, 4.0}, m.stdDevs());
        assertArrayEquals(new double[] {0.5, 0.0, -0.5}, m.correlations());
        assertArrayEquals(new boolean[] {true, true, true}, m.correlationPresent());
        assertEquals(0.0, m.correlation(2, 0)); // symmetry accessor
    }

    @Test
    void sdccFlpCorrelationDiagonalReturnsStdDev() throws KlvDecodeException {
        byte[] v = hex("03 84 04 3F800000 40000000 40800000 3F000000 00000000 BF000000");
        SdccFlp m = Klv.decodeSdccFlp(v);
        assertEquals(1.0, m.correlation(0, 0));
        assertEquals(2.0, m.correlation(1, 1));
        assertEquals(4.0, m.correlation(2, 2));
    }

    @Test
    void sdccFlpCorrelationSlenZeroOffDiagonalStillWorks() throws KlvDecodeException {
        // Mode 2, N=3, Slen=0 (no std-dev data at all — spec-legal), Clen=4 IEEE correlations.
        byte[] v = hex("03 84 00 3F000000 00000000 BF000000");
        SdccFlp m = Klv.decodeSdccFlp(v);
        assertEquals(0, m.stdDevs().length);
        // correlations is always full-triangle-sized regardless of Slen, so
        // off-diagonal access must succeed even with no std devs.
        assertEquals(0.5, m.correlation(0, 1));
        assertEquals(0.0, m.correlation(2, 0));
    }

    @Test
    void sdccFlpCorrelationSlenZeroDiagonalThrows() throws KlvDecodeException {
        byte[] v = hex("03 84 00 3F000000 00000000 BF000000");
        SdccFlp m = Klv.decodeSdccFlp(v);
        // i==0 is well within matrixSize=3 — must be the documented
        // "no std-dev data" message, not a plain out-of-bounds one.
        IndexOutOfBoundsException ex =
                assertThrows(IndexOutOfBoundsException.class, () -> m.correlation(0, 0));
        assertTrue(ex.getMessage().contains("no standard-deviation value"));
    }

    @Test
    void sdccFlpCorrelationOutOfBoundsThrows() throws KlvDecodeException {
        byte[] v = hex("03 84 04 3F800000 40000000 40800000 3F000000 00000000 BF000000");
        SdccFlp m = Klv.decodeSdccFlp(v);
        IndexOutOfBoundsException ex =
                assertThrows(IndexOutOfBoundsException.class, () -> m.correlation(3, 0));
        assertTrue(ex.getMessage().contains("index out of bounds"));
    }

    @Test
    void decodeSdccFlpSparseBitVectorGolden() throws KlvDecodeException {
        // N=3 sparse, only rho13=0.25 present.
        byte[] v = hex("03 A4 04 40 3F800000 40000000 40800000 3E800000");
        SdccFlp m = Klv.decodeSdccFlp(v);
        assertArrayEquals(new double[] {0.0, 0.25, 0.0}, m.correlations());
        assertArrayEquals(new boolean[] {false, true, false}, m.correlationPresent());
    }

    @Test
    void decodeSdccFlpMalformedThrowsKlvDecodeException() {
        assertThrows(KlvDecodeException.class, () -> Klv.decodeSdccFlp(new byte[0]));
    }

    @Test
    void encodeSdccFlpMode2RoundTrips() throws KlvDecodeException, KlvEncodeException {
        byte[] encoded = Klv.encodeSdccFlpMode2(
                new double[] {1.0, 2.0, 4.0}, new double[] {0.5, 0.0, -0.5}, 2);
        SdccFlp m = Klv.decodeSdccFlp(encoded);
        assertArrayEquals(new double[] {1.0, 2.0, 4.0}, m.stdDevs());
        assertEquals(0.5, m.correlations()[0], 1e-3); // IMAPB(-1,1,2) quantization
    }

    @Test
    void encodeSdccFlpMode2MismatchedCorrelationsLengthThrows() {
        assertThrows(KlvEncodeException.class,
                () -> Klv.encodeSdccFlpMode2(new double[] {1.0, 2.0, 4.0}, new double[] {0.5, 0.0}, 2));
    }

    // -----------------------------------------------------------------------
    // Record field sanity (defaults, direct construction)
    // -----------------------------------------------------------------------

    @Test
    void bareUasDatalinkLsWpcFieldsDefaultToAbsent() {
        UasDatalinkLs ls = new UasDatalinkLs.Builder().universalLabel(ByteBuffer.wrap(UL)).build();
        assertNull(ls.imageHorizon());
        assertTrue(ls.controlCommands().isEmpty());
        assertNull(ls.controlCommandVerification());
        assertNull(ls.activeWavelengths());
        assertNull(ls.sensorFrameRate());
        assertNull(ls.metadataSubstreamId());
        assertNull(ls.countryCodes());
        assertNull(ls.wavelengthsList());
        assertNull(ls.airbaseLocations());
        assertNull(ls.payloadList());
        assertNull(ls.weaponsStores());
        assertNull(ls.waypointList());
        assertNull(ls.viewDomain());
        assertTrue(ls.sdccFlps().isEmpty());
    }

    @Test
    void viewDomainPairAndLocationDirectConstruction() {
        ViewDomainPair pair = new ViewDomainPair(10.0, 20.0);
        assertEquals(pair, new ViewDomain(pair, null, null).azimuth());
        Location loc = new Location(1.0, 2.0, 3.0);
        assertEquals(loc, new AirbaseLocations(loc, null).takeOff());
    }

    @Test
    void sdccFlpFieldDirectConstruction() {
        SdccFlpField f = new SdccFlpField(List.of(5L, 6L, 7L), ByteBuffer.wrap(hex("038404")));
        assertEquals(List.of(5L, 6L, 7L), f.precedingTags());
        assertArrayEquals(hex("038404"), readByteBuffer(f.bytes()));
    }

    @Test
    void payloadListAndWeaponsStoreDirectConstruction() {
        PayloadList pl = new PayloadList(1, List.of(new PayloadRecord(0, PayloadType.SAR.code(), "x")));
        assertEquals(PayloadType.SAR, pl.records().get(0).payloadType());

        // WeaponsStore built via its Builder (named setters), not the
        // canonical constructor's four consecutive `long` ids — the
        // Builder exists specifically to avoid transposing them.
        WeaponsStore ws = new WeaponsStore.Builder()
                .stationId(1)
                .hardpointId(1)
                .carriageId(1)
                .storeId(3)
                .statusRaw(0b0000_0001_0000_0011) // fuze bit set
                .weaponType("Harpoon")
                .build();
        assertEquals(1L, ws.stationId());
        assertEquals(1L, ws.hardpointId());
        assertEquals(1L, ws.carriageId());
        assertEquals(3L, ws.storeId());
        assertEquals(0b0000_0011, ws.generalStatus());
        assertTrue(ws.fuzeEnabled());
        assertFalse(ws.laserEnabled());
        assertEquals("Harpoon", ws.weaponType());
    }

    @Test
    void imageHorizonPixelsBuilderConstruction() {
        // Built via named setters, not the canonical constructor's two
        // 4-long same-typed runs — the Builder exists specifically to
        // avoid transposing e.g. x0Pct/y0Pct or start/end lat/lon.
        ImageHorizonPixels h = new ImageHorizonPixels.Builder()
                .x0Pct(10)
                .y0Pct(20)
                .x1Pct(30)
                .y1Pct(40)
                .startLatDeg(1.0)
                .startLonDeg(-2.0)
                .endLatDeg(3.0)
                .endLonDeg(-4.0)
                .build();
        assertEquals(10, h.x0Pct());
        assertEquals(20, h.y0Pct());
        assertEquals(30, h.x1Pct());
        assertEquals(40, h.y1Pct());
        assertEquals(1.0, h.startLatDeg());
        assertEquals(-2.0, h.startLonDeg());
        assertEquals(3.0, h.endLatDeg());
        assertEquals(-4.0, h.endLonDeg());
    }

    // -----------------------------------------------------------------------
    // JNI local-ref capacity — empirical check (review IMPORTANT 2)
    // -----------------------------------------------------------------------

    @Test
    void fullyPopulatedWpcFieldsRoundTrip() throws KlvDecodeException, KlvEncodeException {
        // WP-C's own worst case: every optional WP-C field set simultaneously,
        // with multi-item lists on every Vec-shaped field (not a claim that
        // the record's other 133 pre-existing fields are populated too) —
        // exercises build_uas_datalink's and read_uas_datalink's
        // ensure_local_capacity(320) empirically (a shortfall aborts the JVM
        // outright, not a catchable exception), rather than resting on the
        // module doc's hand-derived tally alone.
        ImageHorizonPixels horizon = new ImageHorizonPixels.Builder()
                .x0Pct(1).y0Pct(2).x1Pct(3).y1Pct(4)
                .startLatDeg(10.0).startLonDeg(-20.0).endLatDeg(30.0).endLonDeg(-40.0)
                .build();
        List<ControlCommand> commands = List.of(
                new ControlCommand(1, "cmd-1", 100L),
                new ControlCommand(2, "cmd-2", null),
                new ControlCommand(3, "cmd-3", 300L));
        List<Long> verification = List.of(1L, 2L, 3L);
        List<Long> wavelengths = List.of(4L, 5L);
        SensorFrameRate frameRate = new SensorFrameRate(30000, 1001);
        MetadataSubstreamId substreamId =
                new MetadataSubstreamId(0, hex("00112233445566778899aabbccddeeff"));
        CountryCodes countryCodes = new CountryCodes(14, "CAN", "USA", "FRA");
        List<WavelengthRecord> wavelengthRecords = List.of(
                new WavelengthRecord(1, 400.0, 700.0, "visible"),
                new WavelengthRecord(2, 8000.0, 14000.0, "LWIR"));
        AirbaseLocations airbase = new AirbaseLocations(
                new Location(38.8, -77.0, 100.0), new Location(39.9, -75.0, 200.0));
        PayloadList payloadList = new PayloadList(2, List.of(
                new PayloadRecord(0, PayloadType.ELECTRO_OPTICAL.code(), "EO Camera"),
                new PayloadRecord(1, PayloadType.LIDAR.code(), "Lidar Sensor")));
        List<WeaponsStore> stores = List.of(
                new WeaponsStore.Builder().stationId(1).hardpointId(1).carriageId(1).storeId(1)
                        .statusRaw(3).weaponType("Harpoon").build(),
                new WeaponsStore.Builder().stationId(2).hardpointId(2).carriageId(2).storeId(2)
                        .statusRaw(4).weaponType("Hellfire").build());
        List<Waypoint> waypoints = List.of(
                new Waypoint(1, 1, 3L, new Location(38.9, -77.0, 200.0)),
                new Waypoint(2, 2, null, null));
        ViewDomain viewDomain = new ViewDomain(
                new ViewDomainPair(210.0, 300.0),
                new ViewDomainPair(-75.0, 50.0),
                new ViewDomainPair(350.0, 20.0));
        List<SdccFlpField> sdccFlps = List.of(
                new SdccFlpField(List.of(1L, 2L, 3L), ByteBuffer.wrap(hex("038404"))),
                new SdccFlpField(List.of(4L, 5L), ByteBuffer.wrap(hex("038404"))));

        UasDatalinkLs rec = new UasDatalinkLs.Builder()
                .universalLabel(ByteBuffer.wrap(UL))
                .imageHorizon(horizon)
                .controlCommands(commands)
                .controlCommandVerification(verification)
                .activeWavelengths(wavelengths)
                .sensorFrameRate(frameRate)
                .metadataSubstreamId(substreamId)
                .countryCodes(countryCodes)
                .wavelengthsList(wavelengthRecords)
                .airbaseLocations(airbase)
                .payloadList(payloadList)
                .weaponsStores(stores)
                .waypointList(waypoints)
                .viewDomain(viewDomain)
                .sdccFlps(sdccFlps)
                .build();

        byte[] encoded = Klv.encodeUasDatalink(rec);
        UasDatalinkLs decoded = Klv.decodeUasDatalink(encoded);

        // Exact fields (BER-OID ids / strings / booleans — no wire
        // quantization involved).
        assertEquals(commands, decoded.controlCommands());
        assertEquals(verification, decoded.controlCommandVerification());
        assertEquals(wavelengths, decoded.activeWavelengths());
        assertEquals(frameRate, decoded.sensorFrameRate());
        // MetadataSubstreamId.uuid is a byte[] record component — the
        // record's auto-generated equals() compares arrays by reference,
        // not content, so compare fields individually rather than via
        // assertEquals on the whole object (same reason SdccFlp's
        // double[]/boolean[] fields are never compared via assertEquals
        // elsewhere in this suite).
        assertEquals(substreamId.localId(), decoded.metadataSubstreamId().localId());
        assertArrayEquals(substreamId.uuid(), decoded.metadataSubstreamId().uuid());
        assertEquals(countryCodes, decoded.countryCodes());
        assertEquals(payloadList, decoded.payloadList());
        assertEquals(stores, decoded.weaponsStores());
        assertEquals(2, decoded.sdccFlps().size());

        // Lossy fields (IMAPB / linear-range int32 wire quantization —
        // compare with an epsilon, same as the Table C1 spec-vector tests
        // above; an exact assertEquals on these fails on quantization
        // noise, not a real bug).
        assertImageHorizonCloseTo(horizon, decoded.imageHorizon());
        assertEquals(wavelengthRecords.size(), decoded.wavelengthsList().size());
        for (int i = 0; i < wavelengthRecords.size(); i++) {
            WavelengthRecord exp = wavelengthRecords.get(i);
            WavelengthRecord act = decoded.wavelengthsList().get(i);
            assertEquals(exp.id(), act.id());
            assertEquals(exp.minNm(), act.minNm(), 1.0);
            assertEquals(exp.maxNm(), act.maxNm(), 1.0);
            assertEquals(exp.name(), act.name());
        }
        assertLocationCloseTo(airbase.takeOff(), decoded.airbaseLocations().takeOff());
        assertLocationCloseTo(airbase.recovery(), decoded.airbaseLocations().recovery());
        assertEquals(waypoints.size(), decoded.waypointList().size());
        for (int i = 0; i < waypoints.size(); i++) {
            Waypoint exp = waypoints.get(i);
            Waypoint act = decoded.waypointList().get(i);
            assertEquals(exp.id(), act.id());
            assertEquals(exp.prosecutionOrder(), act.prosecutionOrder());
            assertEquals(exp.info(), act.info());
            if (exp.location() == null) {
                assertNull(act.location());
            } else {
                assertLocationCloseTo(exp.location(), act.location());
            }
        }
        assertViewDomainPairCloseTo(viewDomain.azimuth(), decoded.viewDomain().azimuth());
        assertViewDomainPairCloseTo(viewDomain.elevation(), decoded.viewDomain().elevation());
        assertViewDomainPairCloseTo(viewDomain.roll(), decoded.viewDomain().roll());

        // Round-trip again through a full decode -> re-encode -> decode
        // cycle to exercise BOTH read_uas_datalink (encode direction) and
        // build_uas_datalink (decode direction) with every field
        // populated — the empirical capacity check itself; the exact
        // fields are sufficient evidence nothing was dropped or crashed.
        UasDatalinkLs again = Klv.decodeUasDatalink(Klv.encodeUasDatalink(decoded));
        assertEquals(decoded.controlCommands(), again.controlCommands());
        assertEquals(decoded.weaponsStores(), again.weaponsStores());
        assertEquals(decoded.waypointList().size(), again.waypointList().size());
        assertEquals(decoded.sdccFlps().size(), again.sdccFlps().size());
    }

    private static void assertImageHorizonCloseTo(ImageHorizonPixels expected, ImageHorizonPixels actual) {
        assertEquals(expected.x0Pct(), actual.x0Pct());
        assertEquals(expected.y0Pct(), actual.y0Pct());
        assertEquals(expected.x1Pct(), actual.x1Pct());
        assertEquals(expected.y1Pct(), actual.y1Pct());
        assertEquals(expected.startLatDeg(), actual.startLatDeg(), 1e-4);
        assertEquals(expected.startLonDeg(), actual.startLonDeg(), 1e-4);
        assertEquals(expected.endLatDeg(), actual.endLatDeg(), 1e-4);
        assertEquals(expected.endLonDeg(), actual.endLonDeg(), 1e-4);
    }

    private static void assertLocationCloseTo(Location expected, Location actual) {
        assertEquals(expected.latDeg(), actual.latDeg(), 1e-4);
        assertEquals(expected.lonDeg(), actual.lonDeg(), 1e-4);
        assertEquals(expected.haeM(), actual.haeM(), 0.1);
    }

    private static void assertViewDomainPairCloseTo(ViewDomainPair expected, ViewDomainPair actual) {
        assertEquals(expected.startDeg(), actual.startDeg(), 0.1);
        assertEquals(expected.rangeDeg(), actual.rangeDeg(), 0.1);
    }

    /** Read a {@link ByteBuffer}'s remaining bytes without mutating its position. */
    private static byte[] readByteBuffer(ByteBuffer buf) {
        ByteBuffer dup = buf.duplicate();
        byte[] out = new byte[dup.remaining()];
        dup.get(out);
        return out;
    }
}
