package org.tstrans.klv;

import static org.junit.jupiter.api.Assertions.*;

import java.util.HexFormat;
import org.junit.jupiter.api.Test;
import org.tstrans.KlvDecodeException;
import org.tstrans.KlvEncodeException;

/**
 * ST 0903 VMTI LS + VTargetPack decode/encode tests.
 *
 * <p>Fixture byte sequences are ported from the tst-py test suite:
 * <ul>
 *   <li>{@code test_klv_st0903.py} — minimal VMTI body fixtures and basic decode tests</li>
 *   <li>{@code test_klv_vtarget_pack.py} — VTargetPack structure + vmask pass-through test</li>
 *   <li>{@code test_klv_encode_st0903.py} — encode round-trip + standalone framing tests</li>
 * </ul>
 */
class St0903Test {

    // -----------------------------------------------------------------------
    // Fixture helpers (mirroring tst-py's _ber_short / _tlv / _minimal_vmti_body)
    // -----------------------------------------------------------------------

    /** Encode a BER short-form length (0..=127). */
    private static byte[] berShort(int n) {
        assert n >= 0 && n < 0x80;
        return new byte[]{(byte) n};
    }

    /** Encode a 1-byte tag + BER short length + value TLV. */
    private static byte[] tlv(int tag, byte[] value) {
        assert tag >= 0 && tag < 0x80;
        byte[] out = new byte[1 + 1 + value.length];
        out[0] = (byte) tag;
        out[1] = (byte) value.length;
        System.arraycopy(value, 0, out, 2, value.length);
        return out;
    }

    /** Concat two byte arrays. */
    private static byte[] cat(byte[]... arrays) {
        int total = 0;
        for (byte[] a : arrays) total += a.length;
        byte[] out = new byte[total];
        int pos = 0;
        for (byte[] a : arrays) {
            System.arraycopy(a, 0, out, pos, a.length);
            pos += a.length;
        }
        return out;
    }

    /** Big-endian long to n-byte array. */
    private static byte[] beBytes(long val, int n) {
        byte[] out = new byte[n];
        for (int i = n - 1; i >= 0; i--) {
            out[i] = (byte) (val & 0xFF);
            val >>= 8;
        }
        return out;
    }

    /**
     * Minimal VMTI body with precision_time_stamp + version + frame_width + frame_height.
     * Mirrors tst-py's {@code _minimal_vmti_body()} in {@code test_klv_st0903.py}.
     *
     * <ul>
     *   <li>Tag 2 (precision_time_stamp) = 1_700_000_000_000_000 (8-byte BE)</li>
     *   <li>Tag 4 (vmtiLsVersionNum) = 6 (1 byte)</li>
     *   <li>Tag 8 (frame_width) = 1920 (2 bytes)</li>
     *   <li>Tag 9 (frame_height) = 1080 (2 bytes)</li>
     * </ul>
     */
    private static byte[] minimalVmtiBody() {
        return cat(
                tlv(2, beBytes(1_700_000_000_000_000L, 8)),  // precision_time_stamp
                tlv(4, beBytes(6, 1)),                         // vmtiLsVersionNum = 6
                tlv(8, beBytes(1920, 2)),                      // frame_width
                tlv(9, beBytes(1080, 2))                       // frame_height
        );
    }

    // -----------------------------------------------------------------------
    // Basic decode tests (from test_klv_st0903.py)
    // -----------------------------------------------------------------------

    /** Ported from {@code test_decode_empty_body_lenient} in test_klv_st0903.py. */
    @Test
    void decodeEmptyBodyLenient() throws KlvDecodeException {
        VmtiLs v = Klv.decodeVmti(new byte[0]);
        assertNotNull(v);
        assertNull(v.precisionTimeStamp());
        assertTrue(v.targets().isEmpty());
    }

    /** Ported from {@code test_decode_minimal_vmti} in test_klv_st0903.py. */
    @Test
    void decodeMinimalVmti() throws KlvDecodeException {
        VmtiLs v = Klv.decodeVmti(minimalVmtiBody());
        assertEquals(1_700_000_000_000_000L, v.precisionTimeStamp());
        assertEquals(6, (int) v.versionNumber());
        assertEquals(1920L, (long) v.frameWidth());
        assertEquals(1080L, (long) v.frameHeight());
    }

    /** Ported from {@code test_vmti_ls_targets_is_tuple} in test_klv_st0903.py. */
    @Test
    void decodeMinimalVmtiTargetsEmpty() throws KlvDecodeException {
        VmtiLs v = Klv.decodeVmti(minimalVmtiBody());
        assertNotNull(v.targets());
        assertTrue(v.targets().isEmpty());
    }

    /** Ported from {@code test_unknown_tag_preserved} in test_klv_st0903.py. */
    @Test
    void unknownTagPreserved() throws KlvDecodeException {
        // Tag 50 is not in the typed table — should land in unknown.
        byte[] body = cat(minimalVmtiBody(), tlv(50, new byte[]{0x68, 0x65, 0x6c, 0x6c, 0x6f}));
        VmtiLs v = Klv.decodeVmti(body);
        assertTrue(
                v.unknown().stream().anyMatch(u -> u.tag() == 50),
                "Tag 50 should be in unknown list");
    }

    /** Ported from {@code test_vmti_ls_miis_id_is_bytes_not_list} in test_klv_st0903.py. */
    @Test
    void miisIdPreservedAsBytes() throws KlvDecodeException {
        // Tag 13 = MIIS ID
        byte[] body = cat(minimalVmtiBody(), tlv(13, new byte[]{(byte) 0xde, (byte) 0xad, (byte) 0xbe, (byte) 0xef}));
        VmtiLs v = Klv.decodeVmti(body);
        assertNotNull(v.miisId());
        byte[] arr = v.miisId().array();
        assertArrayEquals(new byte[]{(byte) 0xde, (byte) 0xad, (byte) 0xbe, (byte) 0xef}, arr);
    }

    // -----------------------------------------------------------------------
    // Strict decode (from test_klv_st0903.py :: test_decode_strict_rejects_missing_required)
    // -----------------------------------------------------------------------

    /**
     * Ported from {@code test_decode_strict_rejects_missing_required} in
     * test_klv_st0903.py. An empty body in strict mode must throw
     * {@code MISSING_REQUIRED_TAG} (or an equivalent structural error).
     */
    @Test
    void strictDecodesRejectsMissingRequired() {
        KlvDecodeException ex = assertThrows(
                KlvDecodeException.class, () -> Klv.decodeVmti(new byte[0], true));
        // tst-py accepts MISSING_REQUIRED_TAG | MALFORMED_BYTES | TRUNCATED_SET for strict(empty)
        assertTrue(
                ex.kind() == KlvDecodeException.Kind.MISSING_REQUIRED_TAG
                        || ex.kind() == KlvDecodeException.Kind.MALFORMED_BYTES
                        || ex.kind() == KlvDecodeException.Kind.TRUNCATED_SET,
                "Expected structural/missing-required error, got: " + ex.kind());
    }

    // -----------------------------------------------------------------------
    // VTargetPack decode (from test_klv_vtarget_pack.py)
    // -----------------------------------------------------------------------

    /**
     * Ported from {@code test_vtarget_pack_vmask_is_bytes_not_list} in
     * test_klv_vtarget_pack.py. Verifies that a VTargetSeries (VmtiLs Tag 101)
     * carrying one pack with a vmask payload (pack-internal Tag 101) is decoded
     * correctly and vmask is surfaced as a ByteBuffer.
     *
     * <p>Fixture construction:
     * <ul>
     *   <li>minimal_vmti = Tag 2 (PTS=1_700_000_000_000_000) + Tag 4 (version=6)</li>
     *   <li>pack_body = [0x01, 0x65, 0x02, 0xDE, 0xAD] — BER-OID target_id=1, Tag 101 vmask=0xDEAD</li>
     *   <li>series = [len(pack_body)] + pack_body</li>
     *   <li>body = minimal_vmti + [101, len(series)] + series</li>
     * </ul>
     */
    @Test
    void vtargetPackVmaskPreservedAsBytes() throws KlvDecodeException {
        // Minimal VMTI baseline: PTS + version (matches tst-py fixture)
        byte[] minimalVmti = cat(
                new byte[]{2, 8},
                beBytes(1_700_000_000_000_000L, 8),
                new byte[]{4, 1, 6}
        );
        // VTargetPack body: BER-OID target_id=1, then Tag 101 vmask = 0xDEAD
        byte[] packBody = new byte[]{0x01, 0x65, 0x02, (byte) 0xDE, (byte) 0xAD};
        // VTargetSeries (VmtiLs Tag 101): each pack is BER-length-prefixed
        byte[] series = cat(new byte[]{(byte) packBody.length}, packBody);
        byte[] body = cat(minimalVmti, new byte[]{101, (byte) series.length}, series);

        VmtiLs v = Klv.decodeVmti(body);
        assertEquals(1, v.targets().size(), "Expected 1 target");
        VTargetPack t = v.targets().get(0);
        assertEquals(1L, t.targetId());
        assertNotNull(t.vmask(), "vmask should be non-null");
        assertArrayEquals(
                new byte[]{(byte) 0xDE, (byte) 0xAD},
                t.vmask().array(),
                "vmask bytes should match 0xDEAD");
    }

    // -----------------------------------------------------------------------
    // Encode round-trip (from test_klv_encode_st0903.py)
    // -----------------------------------------------------------------------

    /**
     * Ported from {@code test_encode_vmti_ls_body_round_trip} in
     * test_klv_encode_st0903.py. Decode a body, encode it back, decode again,
     * assert scalar fields are stable.
     */
    @Test
    void encodeVmtiBodyRoundTrip() throws KlvDecodeException, KlvEncodeException {
        // Fixture from tst-py test_encode_vmti_ls_body_round_trip:
        // Tag 2 (PTS) = 1_700_000_000_000_000, Tag 4 (version) = 6, Tag 8 (frameWidth) = 1920
        byte[] body = cat(
                new byte[]{2, 8},
                beBytes(1_700_000_000_000_000L, 8),
                new byte[]{4, 1, 6},
                new byte[]{8, 2},
                beBytes(1920, 2)
        );
        VmtiLs vmti = Klv.decodeVmti(body);
        byte[] out = Klv.encodeVmti(vmti);
        assertNotNull(out);
        assertTrue(out.length > 0);
        VmtiLs vmti2 = Klv.decodeVmti(out);
        assertEquals(vmti.precisionTimeStamp(), vmti2.precisionTimeStamp());
        assertEquals(vmti.versionNumber(), vmti2.versionNumber());
        // frameWidth (Tag 8) is set in the fixture — assert it survives the round-trip.
        assertEquals(1920L, (long) vmti2.frameWidth());
    }

    /**
     * Ported from {@code test_encode_vmti_standalone_has_ul_prefix} in
     * test_klv_encode_st0903.py. The standalone framing must start with the
     * VMTI_LS_UL (first byte = 0x06).
     */
    @Test
    void encodeVmtiStandaloneHasUlPrefix() throws KlvDecodeException, KlvEncodeException {
        VmtiLs vmti = Klv.decodeVmti(new byte[0]);
        byte[] out = Klv.encodeVmtiStandalone(vmti);
        assertTrue(out.length >= 16, "Standalone must be at least 16 bytes (UL)");
        assertEquals((byte) 0x06, out[0], "First byte must be 0x06 (SMPTE designator)");
        // Verify the full UL matches vmtiLsUl()
        byte[] ul = Klv.vmtiLsUl();
        for (int i = 0; i < 16; i++) {
            assertEquals(ul[i], out[i], "VMTI_LS_UL byte " + i + " mismatch");
        }
    }

    /**
     * Ported from {@code test_encode_vmti_with_targets_round_trip} in
     * test_klv_encode_st0903.py. A VMTI with one target (vmask payload)
     * must round-trip with the target and vmask preserved.
     */
    @Test
    void encodeVmtiWithTargetsRoundTrip() throws KlvDecodeException, KlvEncodeException {
        // Fixture from tst-py test_encode_vmti_with_targets_round_trip:
        // pack_body = [0x01, 0x65, 0x02, 0xDE, 0xAD]  # target_id=1, vmask=0xDEAD
        byte[] packBody = new byte[]{0x01, 0x65, 0x02, (byte) 0xDE, (byte) 0xAD};
        byte[] series = cat(new byte[]{(byte) packBody.length}, packBody);
        byte[] body = cat(
                new byte[]{2, 8},
                beBytes(1_700_000_000_000_000L, 8),
                new byte[]{4, 1, 6},
                new byte[]{101, (byte) series.length},
                series
        );
        VmtiLs vmti = Klv.decodeVmti(body);
        assertEquals(1, vmti.targets().size());
        byte[] out = Klv.encodeVmti(vmti);
        VmtiLs vmti2 = Klv.decodeVmti(out);
        assertEquals(1, vmti2.targets().size());
        assertEquals(1L, vmti2.targets().get(0).targetId());
        assertNotNull(vmti2.targets().get(0).vmask());
        assertArrayEquals(
                new byte[]{(byte) 0xDE, (byte) 0xAD},
                vmti2.targets().get(0).vmask().array());
    }

    // -----------------------------------------------------------------------
    // VTargetPack.TargetColor compact constructor validation
    // -----------------------------------------------------------------------

    @Test
    void targetColorValidChannels() {
        VTargetPack.TargetColor c = new VTargetPack.TargetColor(255, 128, 0);
        assertEquals(255, c.r());
        assertEquals(128, c.g());
        assertEquals(0, c.b());
    }

    @Test
    void targetColorRejectsOutOfRange() {
        assertThrows(IllegalArgumentException.class,
                () -> new VTargetPack.TargetColor(256, 0, 0));
        assertThrows(IllegalArgumentException.class,
                () -> new VTargetPack.TargetColor(0, -1, 0));
    }

    // -----------------------------------------------------------------------
    // VmtiLs.Builder smoke test
    // -----------------------------------------------------------------------

    @Test
    void builderConstructsVmtiLs() {
        VmtiLs v = new VmtiLs.Builder()
                .precisionTimeStamp(1_700_000_000_000_000L)
                .versionNumber(6)
                .frameWidth(1920L)
                .frameHeight(1080L)
                .build();
        assertEquals(1_700_000_000_000_000L, v.precisionTimeStamp());
        assertEquals(6, (int) v.versionNumber());
        assertEquals(1920L, (long) v.frameWidth());
        assertEquals(1080L, (long) v.frameHeight());
        assertTrue(v.targets().isEmpty());
        assertTrue(v.unknown().isEmpty());
        assertTrue(v.fieldErrors().isEmpty());
    }

    // -----------------------------------------------------------------------
    // VTargetPack.Builder smoke test
    // -----------------------------------------------------------------------

    @Test
    void vtargetBuilderConstructsPack() {
        VTargetPack p = new VTargetPack.Builder(42L)
                .priority(5)
                .confidenceLevel(80)
                .targetColor(new VTargetPack.TargetColor(255, 128, 0))
                .build();
        assertEquals(42L, p.targetId());
        assertEquals(5, (int) p.priority());
        assertEquals(80, (int) p.confidenceLevel());
        assertNotNull(p.targetColor());
        assertEquals(255, p.targetColor().r());
        assertNull(p.centroidPixel());
        assertTrue(p.unknown().isEmpty());
    }

    // -----------------------------------------------------------------------
    // encodeVmtiStrictCompliance tests
    // -----------------------------------------------------------------------

    @Test
    void encodeVmtiStrictComplianceMissingVersionNumberThrows() {
        // An empty VmtiLs is missing Tag 4 (version_number) — the first required item.
        VmtiLs rec = new VmtiLs.Builder().build();
        KlvEncodeException ex = assertThrows(KlvEncodeException.class,
                () -> Klv.encodeVmtiStrictCompliance(rec));
        assertEquals(KlvEncodeException.Kind.MISSING_MANDATORY_ITEM, ex.kind());
        assertTrue(ex.tag().isPresent(), "MISSING_MANDATORY_ITEM must carry tag 4");
        assertEquals(4L, ex.tag().get().longValue());
    }

    @Test
    void encodeVmtiStrictComplianceEmptyPackThrows() {
        // A VTargetPack with no TLV items (only target_id) triggers V_TARGET_PACK_EMPTY.
        VmtiLs rec = new VmtiLs.Builder()
                .versionNumber(6)
                .numTargetsReported(1L)
                .targets(java.util.List.of(new VTargetPack.Builder(1L).build()))
                .build();
        KlvEncodeException ex = assertThrows(KlvEncodeException.class,
                () -> Klv.encodeVmtiStrictCompliance(rec));
        assertEquals(KlvEncodeException.Kind.V_TARGET_PACK_EMPTY, ex.kind());
    }

    @Test
    void encodeVmtiStrictComplianceDuplicateTargetIdThrows() {
        // Two VTargetPacks with the same target_id triggers DUPLICATE_TARGET_ID.
        VTargetPack packA = new VTargetPack.Builder(7L).priority(1).build();
        VTargetPack packB = new VTargetPack.Builder(7L).priority(2).build();
        VmtiLs rec = new VmtiLs.Builder()
                .versionNumber(6)
                .numTargetsReported(2L)
                .targets(java.util.List.of(packA, packB))
                .build();
        KlvEncodeException ex = assertThrows(KlvEncodeException.class,
                () -> Klv.encodeVmtiStrictCompliance(rec));
        assertEquals(KlvEncodeException.Kind.DUPLICATE_TARGET_ID, ex.kind());
    }

    @Test
    void encodeVmtiStrictComplianceSucceedsWithValidRecord() throws KlvEncodeException, KlvDecodeException {
        // A valid record with required items (Tags 4 + 6) plus a non-empty pack.
        VTargetPack pack = new VTargetPack.Builder(1L).priority(5).build();
        VmtiLs rec = new VmtiLs.Builder()
                .versionNumber(6)
                .numTargetsReported(1L)
                .targets(java.util.List.of(pack))
                .build();
        byte[] wire = assertDoesNotThrow(() -> Klv.encodeVmtiStrictCompliance(rec));
        assertNotNull(wire);
        assertTrue(wire.length > 0);
        VmtiLs decoded = Klv.decodeVmti(wire);
        assertEquals(6, (int) decoded.versionNumber());
        assertEquals(1, decoded.targets().size());
    }

    // -----------------------------------------------------------------------
    // encodeVmtiStandaloneStrictCompliance tests
    // -----------------------------------------------------------------------

    @Test
    void encodeVmtiStandaloneStrictComplianceMissingPtsThrows() {
        // Embedded required items present but Tag 2 (precision_time_stamp) absent
        // — standalone compliance requires it.
        VmtiLs rec = new VmtiLs.Builder()
                .versionNumber(6)
                .numTargetsReported(0L)
                .horizontalFov(30.0)
                .verticalFov(20.0)
                .miisId(java.nio.ByteBuffer.wrap(new byte[]{0x01, 0x02, 0x03, 0x04}))
                .build();
        KlvEncodeException ex = assertThrows(KlvEncodeException.class,
                () -> Klv.encodeVmtiStandaloneStrictCompliance(rec));
        assertEquals(KlvEncodeException.Kind.MISSING_MANDATORY_ITEM, ex.kind());
        assertTrue(ex.tag().isPresent(), "must carry tag 2 (precision_time_stamp)");
        assertEquals(2L, ex.tag().get().longValue());
    }

    @Test
    void encodeVmtiStandaloneStrictComplianceForbiddenOffsetThrows() {
        // A VTargetPack with an offset tag (centroid_lat_offset = Tag 10) triggers
        // FORBIDDEN_STANDALONE_OFFSET.
        VTargetPack packWithOffset = new VTargetPack.Builder(1L)
                .priority(5)
                .centroidLatOffset(0.001)
                .build();
        VmtiLs rec = new VmtiLs.Builder()
                .versionNumber(6)
                .numTargetsReported(1L)
                .precisionTimeStamp(1_700_000_000_000_000L)
                .horizontalFov(30.0)
                .verticalFov(20.0)
                .miisId(java.nio.ByteBuffer.wrap(new byte[]{0x01, 0x02, 0x03, 0x04}))
                .targets(java.util.List.of(packWithOffset))
                .build();
        KlvEncodeException ex = assertThrows(KlvEncodeException.class,
                () -> Klv.encodeVmtiStandaloneStrictCompliance(rec));
        assertEquals(KlvEncodeException.Kind.FORBIDDEN_STANDALONE_OFFSET, ex.kind());
    }

    @Test
    void encodeVmtiStandaloneStrictComplianceSucceedsWithValidRecord() throws KlvDecodeException, KlvEncodeException {
        // Provide all standalone-required items (Tags 2/4/6/11/12/13) with no offset tags.
        VmtiLs rec = new VmtiLs.Builder()
                .versionNumber(6)
                .numTargetsReported(0L)
                .precisionTimeStamp(1_700_000_000_000_000L)
                .horizontalFov(30.0)
                .verticalFov(20.0)
                .miisId(java.nio.ByteBuffer.wrap(new byte[]{0x01, 0x02, 0x03, 0x04}))
                .build();
        byte[] wire = assertDoesNotThrow(() -> Klv.encodeVmtiStandaloneStrictCompliance(rec));
        assertNotNull(wire);
        assertTrue(wire.length >= 16, "Standalone must start with 16-byte UL");
        assertEquals((byte) 0x06, wire[0], "First byte must be 0x06 (SMPTE designator)");
    }

    // -----------------------------------------------------------------------
    // DA-JVM-1: regression coverage for many-target local-ref reclamation
    // -----------------------------------------------------------------------

    /**
     * Regression test for DA-JVM-1: {@code nDecodeVmti} leaked one JNI local
     * ref per VTarget into the outer native frame, causing HotSpot to grow the
     * local-ref table (or crash on constrained VMs) when the target count is
     * large. The fix builds and adds each VTargetPack inside its own
     * {@code PushLocalFrame}/{@code PopLocalFrame} pair, keeping the live count
     * O(1) regardless of target count.
     *
     * <p>Uses encode→decode round-trip: encode 500 targets, then decode. The
     * decode path exercises the fixed {@code build_vmti} loop. The test asserts
     * functional correctness; the absence of local-ref-table-overflow is the
     * implicit guarantee of the fix.
     */
    @Test
    void decodeVmtiManyTargetsRoundTrip() throws KlvDecodeException, KlvEncodeException {
        int count = 500;
        java.util.List<VTargetPack> targets = new java.util.ArrayList<>(count);
        for (long id = 1; id <= count; id++) {
            // Each pack must have at least one field; priority is the smallest.
            targets.add(new VTargetPack.Builder(id).priority(1).build());
        }
        VmtiLs vmti = new VmtiLs.Builder()
                .versionNumber(6)
                .targets(targets)
                .build();
        byte[] encoded = Klv.encodeVmti(vmti);
        VmtiLs decoded = Klv.decodeVmti(encoded);

        assertEquals(count, decoded.targets().size(), "All 500 targets must survive the round-trip");
        for (int i = 0; i < count; i++) {
            assertEquals((long) (i + 1), decoded.targets().get(i).targetId(),
                    "Target id " + (i + 1) + " must match");
        }
    }

    // -----------------------------------------------------------------------
    // REF-KLV-04: large-value (>0xFFFF_FFFF) round-trip for target_id + pixels
    // -----------------------------------------------------------------------

    @Test
    void largeTargetIdAndPixelRoundTrip() throws KlvDecodeException, KlvEncodeException {
        // target_id + Tags 1/2/3 (centroidPixel/bboxTopLeft/bboxBottomRight) are u64 (V6,
        // max 6 bytes), so values above u32::MAX are wire-valid and round-trip cleanly.
        // Tags 19/20 (centroidPixRow/Col) are V4 (max 4 bytes per §10.2.2.20/.21), so they
        // are widened to u64 in the model but wire values must stay within u32 range.
        long bigTargetId = 0x1_0000_0001L; // 4294967297 — just above u32::MAX
        long bigPixel    = 0x1_FFFF_FFFFL; // 8589934591 — above u32::MAX, valid V6
        long u32MaxRow   = 0xFFFF_FFFFL;   // u32::MAX — max valid for V4 fields

        VTargetPack pack = new VTargetPack.Builder(bigTargetId)
                .centroidPixel(bigPixel)
                .bboxTopLeftPixel(bigPixel + 1L)
                .bboxBottomRightPixel(bigPixel + 2L)
                .centroidPixRow(u32MaxRow)      // V4: max 4 bytes, keep within u32
                .centroidPixCol(u32MaxRow - 1L) // V4: max 4 bytes, keep within u32
                .build();

        VmtiLs vmti = new VmtiLs.Builder()
                .versionNumber(6)
                .targets(java.util.List.of(pack))
                .build();

        byte[] encoded = Klv.encodeVmti(vmti);
        VmtiLs decoded = Klv.decodeVmti(encoded);

        assertEquals(1, decoded.targets().size());
        VTargetPack p = decoded.targets().get(0);
        assertEquals(bigTargetId, p.targetId(), "targetId round-trips across u64 boundary");
        assertEquals(Long.valueOf(bigPixel),       p.centroidPixel(),        "centroidPixel (V6)");
        assertEquals(Long.valueOf(bigPixel + 1L),  p.bboxTopLeftPixel(),     "bboxTopLeftPixel (V6)");
        assertEquals(Long.valueOf(bigPixel + 2L),  p.bboxBottomRightPixel(), "bboxBottomRightPixel (V6)");
        assertEquals(Long.valueOf(u32MaxRow),      p.centroidPixRow(),       "centroidPixRow (V4 max)");
        assertEquals(Long.valueOf(u32MaxRow - 1L), p.centroidPixCol(),       "centroidPixCol (V4 max-1)");
    }
}
