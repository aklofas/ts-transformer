package org.tstrans.klv;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.HexFormat;
import java.util.List;
import java.util.Optional;

/**
 * Static facade for MISB typed-KLV decode/encode (ST 0601 / 0102 / 0605 / 0805 /
 * 0806 / 0903 / 1010 / 1204). Mirrors tst-py's {@code tstrans.klv} free functions.
 *
 * <p>Decode/encode methods for each set are added in Tasks 1–4. The UL accessors
 * and {@link #isSt0601Family} are available immediately.
 */
public final class Klv {
    private Klv() {}

    // Backing arrays are private so the public surface cannot be mutated through
    // a shared reference; accessors below return defensive clones. Internal
    // callers use the backing arrays directly (no clone).
    private static final byte[] ST_0601_UL =
            HexFormat.of().parseHex("060e2b34020b01010e01030101000000");
    private static final byte[] SECURITY_LS_UL =
            HexFormat.of().parseHex("060e2b34020301010e01030302000000");
    private static final byte[] PRECISION_TIMESTAMP_PACK_UL =
            HexFormat.of().parseHex("060e2b34020501010e01010311000000");
    private static final byte[] VMTI_LS_UL =
            HexFormat.of().parseHex("060e2b34020b01010e01030306000000");

    // ST 0601 family prefix (bytes 0–12 must match). Held as a static so
    // isSt0601Family does not re-parse + re-allocate on every call.
    private static final byte[] ST_0601_FAMILY_PREFIX =
            HexFormat.of().parseHex("060e2b34020b01010e01030101");

    /** @return a defensive copy of the 16-byte ST 0601 UAS Datalink LS Universal Label. */
    public static byte[] st0601Ul() {
        return ST_0601_UL.clone();
    }

    /** @return a defensive copy of the 16-byte ST 0102 Security Metadata LS Universal Label. */
    public static byte[] securityLsUl() {
        return SECURITY_LS_UL.clone();
    }

    /** @return a defensive copy of the 16-byte ST 0605 Precision Time Stamp Pack Universal Label. */
    public static byte[] precisionTimestampPackUl() {
        return PRECISION_TIMESTAMP_PACK_UL.clone();
    }

    /** @return a defensive copy of the 16-byte ST 0903 VMTI LS Universal Label. */
    public static byte[] vmtiLsUl() {
        return VMTI_LS_UL.clone();
    }

    /**
     * Return {@code true} if {@code buf} has the ST 0601 family UL prefix.
     * Mirrors Rust {@code UniversalLabel::is_st0601_family}: bytes 0–12 match
     * the ST 0601 canonical prefix and byte 15 is {@code 0x00}.
     */
    public static boolean isSt0601Family(byte[] buf) {
        if (buf.length < 16) return false;
        for (int i = 0; i < 13; i++) {
            if (buf[i] != ST_0601_FAMILY_PREFIX[i]) return false;
        }
        return buf[15] == 0x00;
    }

    // -----------------------------------------------------------------------
    // ST 0605 — Precision Time Stamp Pack
    // -----------------------------------------------------------------------

    /**
     * Decode a full 26-byte ST 0605 Precision Time Stamp Pack (16-byte UL +
     * 1-byte BER length + 1-byte TimeStatus + 8-byte big-endian microsecond
     * timestamp).
     *
     * @param buf the 26-byte wire-format pack
     * @return the decoded {@link PrecisionTimeStampPack}
     * @throws org.tstrans.KlvDecodeException if the buffer is malformed or has the wrong UL
     */
    public static PrecisionTimeStampPack decodePrecisionTimestamp(byte[] buf)
            throws org.tstrans.KlvDecodeException {
        return nDecodePrecisionTimestamp(buf);
    }

    /**
     * Encode a {@link PrecisionTimeStampPack} to the 26-byte wire format.
     * Mirrors tst-py's {@code encode_precision_timestamp}; encoding is infallible.
     *
     * @param pack the pack to encode
     * @return the 26-byte wire-format buffer
     */
    public static byte[] encodePrecisionTimestamp(PrecisionTimeStampPack pack) {
        return nEncodePrecisionTimestamp(pack);
    }

    private static native PrecisionTimeStampPack nDecodePrecisionTimestamp(byte[] buf)
            throws org.tstrans.KlvDecodeException;

    private static native byte[] nEncodePrecisionTimestamp(PrecisionTimeStampPack pack);

    // -----------------------------------------------------------------------
    // ST 0102 — Security Metadata LS
    // -----------------------------------------------------------------------

    /**
     * Decode an ST 0102 Security Metadata LS body (lenient mode).
     *
     * <p>{@code buf} is body-only — no Universal Label or outer BER length wrapper.
     * Lenient mode tolerates missing required tags, unknown enum codepoints
     * (surfaced as null typed accessors with raw code preserved), and malformed
     * Tag 13 UTF-16 (surfaced in {@link SecurityLs#fieldErrors()}).
     * Mirrors tst-py's {@code decode_security(buf)}.
     *
     * @param buf ST 0102 body bytes (no UL / outer BER length)
     * @return the decoded {@link SecurityLs}
     * @throws org.tstrans.KlvDecodeException if the buffer is structurally malformed
     */
    public static SecurityLs decodeSecurity(byte[] buf)
            throws org.tstrans.KlvDecodeException {
        return nDecodeSecurity(buf, false);
    }

    /**
     * Decode an ST 0102 Security Metadata LS body with optional strict mode.
     *
     * <p>{@code buf} is body-only — no Universal Label or outer BER length wrapper.
     * When {@code strict = true}, rejects missing required tags (1, 2, 3, 12, 13, 22),
     * unknown enum codepoints, omitted-value codepoints, non-canonical BER, and
     * malformed UTF-16. Mirrors tst-py's {@code decode_security(buf, strict=True)}.
     *
     * @param buf    ST 0102 body bytes (no UL / outer BER length)
     * @param strict {@code true} for strict validation; {@code false} for lenient
     * @return the decoded {@link SecurityLs}
     * @throws org.tstrans.KlvDecodeException if the buffer is rejected (including missing
     *                                        required tags in strict mode)
     */
    public static SecurityLs decodeSecurity(byte[] buf, boolean strict)
            throws org.tstrans.KlvDecodeException {
        return nDecodeSecurity(buf, strict);
    }

    /**
     * Encode a {@link SecurityLs} to ST 0102 body bytes.
     *
     * <p>Returns body-only bytes — no Universal Label or outer BER length wrapper.
     * Encoding is lenient (emits only populated fields; no mandatory-tag enforcement).
     * A typed/reserved tag placed in {@code unknown} is silently filtered before
     * encoding (the typed field wins). Mirrors tst-py's {@code encode_security(record)}.
     *
     * @param record the Security LS to encode
     * @return ST 0102 body bytes
     * @throws org.tstrans.KlvEncodeException if any field value is out of range
     */
    public static byte[] encodeSecurity(SecurityLs record)
            throws org.tstrans.KlvEncodeException {
        return nEncodeSecurity(record);
    }

    /**
     * Encode a {@link SecurityLs} with strict ST 0102 compliance validation.
     *
     * <p>Enforces mandatory-tag presence (Tags 1, 2, 3, 12, 13, 22 per ST 0102.12
     * §6.7 Table 2) before encoding. Mirrors tst-py's
     * {@code encode_security_strict_compliance(record)}.
     *
     * @param record the Security LS to encode
     * @return ST 0102 body bytes
     * @throws org.tstrans.KlvEncodeException with {@code kind = MISSING_MANDATORY_ITEM}
     *                                        if a required tag is absent
     */
    public static byte[] encodeSecurityStrictCompliance(SecurityLs record)
            throws org.tstrans.KlvEncodeException {
        return nEncodeSecurityStrictCompliance(record);
    }

    private static native SecurityLs nDecodeSecurity(byte[] buf, boolean strict)
            throws org.tstrans.KlvDecodeException;

    private static native byte[] nEncodeSecurity(SecurityLs record)
            throws org.tstrans.KlvEncodeException;

    private static native byte[] nEncodeSecurityStrictCompliance(SecurityLs record)
            throws org.tstrans.KlvEncodeException;

    // -----------------------------------------------------------------------
    // ST 0903 — VMTI LS + VTargetPack
    // -----------------------------------------------------------------------

    /**
     * Decode an ST 0903 VMTI LS body (lenient mode).
     *
     * <p>{@code buf} is body-only — no Universal Label or outer BER length wrapper.
     * Lenient mode tolerates missing required tags and surfaces per-field decode
     * failures in {@link VmtiLs#fieldErrors()}. Mirrors tst-py's
     * {@code decode_vmti(buf)}.
     *
     * @param buf ST 0903 body bytes (no UL / outer BER length)
     * @return the decoded {@link VmtiLs}
     * @throws org.tstrans.KlvDecodeException if the buffer is structurally malformed
     */
    public static VmtiLs decodeVmti(byte[] buf) throws org.tstrans.KlvDecodeException {
        return nDecodeVmti(buf, false);
    }

    /**
     * Decode an ST 0903 VMTI LS body with optional strict mode.
     *
     * <p>{@code buf} is body-only — no Universal Label or outer BER length wrapper.
     * When {@code strict = true}, rejects missing required tags per ST 0903.6 §6
     * Table 1, duplicate tags, and malformed UTF-8. Mirrors tst-py's
     * {@code decode_vmti(buf, strict=True)}.
     *
     * @param buf    ST 0903 body bytes (no UL / outer BER length)
     * @param strict {@code true} for strict validation; {@code false} for lenient
     * @return the decoded {@link VmtiLs}
     * @throws org.tstrans.KlvDecodeException if the buffer is rejected
     */
    public static VmtiLs decodeVmti(byte[] buf, boolean strict)
            throws org.tstrans.KlvDecodeException {
        return nDecodeVmti(buf, strict);
    }

    /**
     * Encode a {@link VmtiLs} to ST 0903 body bytes (embedded form).
     *
     * <p>Returns body-only bytes — no Universal Label, no outer BER length, and
     * no Tag 1 checksum (per ST 0903.6-120). For standalone carriage on a
     * dedicated KLV PID, use {@link #encodeVmtiStandalone(VmtiLs)}. A typed/
     * reserved tag placed in {@code unknown} is silently filtered before
     * encoding (the typed field wins). Mirrors tst-py's {@code encode_vmti(record)}.
     *
     * @param record the VMTI LS to encode
     * @return ST 0903 embedded body bytes
     * @throws org.tstrans.KlvEncodeException if any field value is out of range
     */
    public static byte[] encodeVmti(VmtiLs record) throws org.tstrans.KlvEncodeException {
        return nEncodeVmti(record);
    }

    /**
     * Encode a {@link VmtiLs} as a standalone VMTI wire record.
     *
     * <p>Returns the full framing: {@code [VMTI_LS_UL:16][outer BER length][body][Tag1 checksum]}
     * per ST 0903.4-17 / ST 0903.6-119. The Tag 1 checksum is computed from the
     * assembled framing; any value in {@link VmtiLs#checksum()} is ignored. A typed/
     * reserved tag placed in {@code unknown} is silently filtered before encoding
     * (the typed field wins). Mirrors tst-py's {@code encode_vmti_standalone(record)}.
     *
     * @param record the VMTI LS to encode
     * @return the full standalone VMTI wire record
     * @throws org.tstrans.KlvEncodeException if any field value is out of range
     */
    public static byte[] encodeVmtiStandalone(VmtiLs record) throws org.tstrans.KlvEncodeException {
        return nEncodeVmtiStandalone(record);
    }

    /**
     * Encode a {@link VmtiLs} to ST 0903 body bytes with strict compliance validation.
     *
     * <p>Returns embedded body bytes (no UL, no outer BER length, no Tag 1 checksum).
     * Enforces mandatory items (Tags 4 and 6), non-empty VTargetPacks, and unique
     * target IDs before encoding. Mirrors tst-py's
     * {@code encode_vmti_strict_compliance(record)}.
     *
     * @param record the VMTI LS to encode
     * @return ST 0903 embedded body bytes
     * @throws org.tstrans.KlvEncodeException with {@code kind = MISSING_MANDATORY_ITEM} if
     *                                        a required item is absent, or
     *                                        {@code kind = V_TARGET_PACK_EMPTY} if a pack has
     *                                        no TLV items, or
     *                                        {@code kind = DUPLICATE_TARGET_ID} if target IDs
     *                                        are not unique
     */
    public static byte[] encodeVmtiStrictCompliance(VmtiLs record)
            throws org.tstrans.KlvEncodeException {
        return nEncodeVmtiStrictCompliance(record);
    }

    /**
     * Encode a {@link VmtiLs} as a standalone VMTI wire record with strict compliance validation.
     *
     * <p>Returns the full framing: {@code [VMTI_LS_UL:16][outer BER length][body][Tag1 checksum]}.
     * In addition to the embedded-mode checks, enforces standalone-required items (Tags 2, 11,
     * 12, 13) and rejects offset tags 10/11/13/14/15/16 on any VTargetPack
     * (ST 0903.6-116 forbidden). Mirrors tst-py's
     * {@code encode_vmti_standalone_strict_compliance(record)}.
     *
     * @param record the VMTI LS to encode
     * @return the full standalone VMTI wire record
     * @throws org.tstrans.KlvEncodeException with {@code kind = MISSING_MANDATORY_ITEM} if a
     *                                        required item is absent, or
     *                                        {@code kind = FORBIDDEN_STANDALONE_OFFSET} if any
     *                                        pack carries a forbidden offset tag
     */
    public static byte[] encodeVmtiStandaloneStrictCompliance(VmtiLs record)
            throws org.tstrans.KlvEncodeException {
        return nEncodeVmtiStandaloneStrictCompliance(record);
    }

    private static native VmtiLs nDecodeVmti(byte[] buf, boolean strict)
            throws org.tstrans.KlvDecodeException;

    private static native byte[] nEncodeVmti(VmtiLs record) throws org.tstrans.KlvEncodeException;

    private static native byte[] nEncodeVmtiStandalone(VmtiLs record)
            throws org.tstrans.KlvEncodeException;

    private static native byte[] nEncodeVmtiStrictCompliance(VmtiLs record)
            throws org.tstrans.KlvEncodeException;

    private static native byte[] nEncodeVmtiStandaloneStrictCompliance(VmtiLs record)
            throws org.tstrans.KlvEncodeException;

    // -----------------------------------------------------------------------
    // ST 0601 — UAS Datalink Local Set
    // -----------------------------------------------------------------------

    /**
     * Decode an ST 0601 UAS Datalink LS (lenient mode).
     *
     * <p>{@code buf} is the full wire-format payload starting with the 16-byte
     * Universal Label. Lenient mode accepts any 16-byte UL, verifies the checksum,
     * and collects per-field decode failures in {@link UasDatalinkLs#fieldErrors()}.
     * Mirrors tst-py's {@code decode_uas_datalink(buf)}.
     *
     * @param buf full ST 0601 wire bytes (UL + BER length + body)
     * @return the decoded {@link UasDatalinkLs}
     * @throws org.tstrans.KlvDecodeException if the buffer is structurally malformed
     */
    public static UasDatalinkLs decodeUasDatalink(byte[] buf)
            throws org.tstrans.KlvDecodeException {
        return nDecodeUasDatalink(buf, false, false);
    }

    /**
     * Decode an ST 0601 UAS Datalink LS with optional strict / compliance mode.
     *
     * <p>{@code buf} is the full wire-format payload starting with the 16-byte UL.
     * When {@code compliance = true}, also enforces Tag 2 first / Tag 1 last /
     * Tag 65 present / no duplicate tags / canonical BER (implies {@code strict}).
     * When {@code strict = true} only, requires the ST 0601 family UL pattern.
     * Mirrors tst-py's {@code decode_uas_datalink(buf, strict=True, compliance=True)}.
     *
     * @param buf        full ST 0601 wire bytes (UL + BER length + body)
     * @param strict     {@code true} for strict UL validation
     * @param compliance {@code true} for full compliance validation (implies strict)
     * @return the decoded {@link UasDatalinkLs}
     * @throws org.tstrans.KlvDecodeException if the buffer is rejected
     */
    public static UasDatalinkLs decodeUasDatalink(byte[] buf, boolean strict, boolean compliance)
            throws org.tstrans.KlvDecodeException {
        return nDecodeUasDatalink(buf, strict, compliance);
    }

    /**
     * Encode a {@link UasDatalinkLs} to the full ST 0601 wire format using the
     * default {@link OutOfRangePolicy#ERROR} policy.
     *
     * <p>Returns the full framing: {@code [UL:16][BER length][body][Tag1 checksum]}.
     * Encoding is lenient (emits only populated fields; no mandatory-tag enforcement).
     * A typed/reserved tag placed in {@code unknown} is silently filtered before
     * encoding (the typed field wins). Mirrors tst-py's {@code encode_uas_datalink(record)}.
     *
     * @param record the UAS Datalink LS to encode
     * @return ST 0601 wire bytes
     * @throws org.tstrans.KlvEncodeException if any field value is out of range
     */
    public static byte[] encodeUasDatalink(UasDatalinkLs record)
            throws org.tstrans.KlvEncodeException {
        return encodeUasDatalink(record, OutOfRangePolicy.ERROR);
    }

    /**
     * Encode a {@link UasDatalinkLs} to the full ST 0601 wire format with an
     * explicit {@link OutOfRangePolicy}.
     *
     * <p>Returns the full framing: {@code [UL:16][BER length][body][Tag1 checksum]}.
     * Encoding is lenient (emits only populated fields; no mandatory-tag enforcement).
     * Mirrors tst-py's {@code encode_uas_datalink(record, out_of_range_policy=...)}.
     *
     * <p>When {@code policy} is {@link OutOfRangePolicy#INDICATOR}, out-of-range
     * values on tags whose ST 0601 INT_MIN sentinel means "Out of Range"
     * (Tags 6, 7, 50, 51, 52, 79, 80, 90–93 — all of which are encodable
     * typed fields) are replaced by the spec-defined special value rather
     * than throwing. All other tags and non-finite inputs still throw. A
     * typed/reserved tag placed in {@code unknown} is silently filtered
     * before encoding (the typed field wins).
     *
     * @param record the UAS Datalink LS to encode
     * @param policy how to handle out-of-range field values
     * @return ST 0601 wire bytes
     * @throws org.tstrans.KlvEncodeException if any field value is out of range
     *                                        (and not eligible for INDICATOR)
     */
    public static byte[] encodeUasDatalink(UasDatalinkLs record, OutOfRangePolicy policy)
            throws org.tstrans.KlvEncodeException {
        int policyInt = switch (policy) {
            case ERROR -> 0;
            case INDICATOR -> 1;
        };
        return nEncodeUasDatalinkWithPolicy(record, policyInt);
    }

    /**
     * Encode a {@link UasDatalinkLs} with strict ST 0601 compliance validation.
     *
     * <p>Enforces mandatory-tag presence (Tag 2 precision timestamp, Tag 65 LS
     * version, Tag 1 checksum) and structural ordering rules. Mirrors tst-py's
     * {@code encode_uas_datalink_strict_compliance(record)}.
     *
     * @param record the UAS Datalink LS to encode
     * @return ST 0601 wire bytes with checksum
     * @throws org.tstrans.KlvEncodeException with {@code kind = MISSING_MANDATORY_ITEM}
     *                                        if a required tag is absent
     */
    public static byte[] encodeUasDatalinkStrictCompliance(UasDatalinkLs record)
            throws org.tstrans.KlvEncodeException {
        return nEncodeUasDatalinkStrictCompliance(record);
    }

    /**
     * Look up the spec-defined meaning of the INT_MIN sentinel wire value for
     * {@code tag}, per the ST 0601.19 §7.5 special-value assignments.
     *
     * <p>Returns {@code "out_of_range"}, {@code "reserved"},
     * {@code "not_available"}, or {@code null} when the spec assigns no
     * INT_MIN special value for that tag (which does NOT mean the tag is
     * unsigned or that INT_MIN is a valid wire value for it). Tags outside
     * the KLV u32 tag range also return {@code null}.
     *
     * <p>See {@link UasDatalinkLs#sentinelTags()} for where decoded sentinels
     * surface, and {@link OutOfRangePolicy#INDICATOR} for the encode-side
     * counterpart (emitting the sentinel for the Out-of-Range-eligible tags).
     * Mirrors tst-py's {@code st0601_sentinel_meaning(tag)}.
     *
     * @param tag the ST 0601 local-set tag number
     * @return the sentinel meaning string, or {@code null} if none is assigned
     */
    public static String st0601SentinelMeaning(long tag) {
        return nSt0601SentinelMeaning(tag);
    }

    private static native UasDatalinkLs nDecodeUasDatalink(byte[] buf, boolean strict, boolean compliance)
            throws org.tstrans.KlvDecodeException;

    private static native byte[] nEncodeUasDatalinkWithPolicy(UasDatalinkLs record, int policy)
            throws org.tstrans.KlvEncodeException;

    private static native byte[] nEncodeUasDatalinkStrictCompliance(UasDatalinkLs record)
            throws org.tstrans.KlvEncodeException;

    private static native String nSt0601SentinelMeaning(long tag);

    // -----------------------------------------------------------------------
    // ST 1204 — MIIS Core Identifier
    // -----------------------------------------------------------------------

    /**
     * Decode a MIIS Core Identifier from its binary wire form (ST 1204.3 §7.3).
     *
     * <p>Expects exactly the bytes of one Core Identifier — no framing, no BER
     * length wrapper. Rejects trailing bytes, unsupported version bytes,
     * reserved-bit violations, and invalid usage-byte combinations.
     * Mirrors tst-py's {@code decode_core_id(buf)}.
     *
     * @param klv raw Core Identifier bytes (no UL / outer BER length)
     * @return the decoded {@link CoreId}
     * @throws org.tstrans.KlvDecodeException if the buffer is malformed or
     *         contains an unsupported version or invalid usage byte
     */
    public static CoreId decodeCoreId(byte[] klv) throws org.tstrans.KlvDecodeException {
        return decodeCoreIdNative(klv);
    }

    /**
     * Encode a {@link CoreId} to its binary wire form (ST 1204.3 §7.3).
     *
     * <p>Returns the two-byte header (version + usage) followed by the UUIDs in
     * EBNF order: sensor, platform, window, minor. Encoding is infallible;
     * the caller is responsible for maintaining the ST 1204.3 EBNF constraint
     * ({@code minorId} must be {@code null} when any other UUID field is present).
     * Mirrors tst-py's {@code encode_core_id(id)}.
     *
     * @param id the Core Identifier to encode
     * @return binary wire bytes (no UL / outer BER length)
     * @throws NullPointerException if {@code id} is {@code null}
     */
    public static byte[] encodeCoreId(CoreId id) {
        return encodeCoreIdNative(id);
    }

    /**
     * Return the ST 1204.3 §7.4.2 textual representation of a {@link CoreId}.
     *
     * <p>Format: {@code VVUU:XXXX-XXXX-…/XXXX-XXXX-…:CC} where {@code VV} and
     * {@code UU} are the version and usage bytes as uppercase hex, each UUID is
     * 8 groups of 4 hex chars dash-separated, multiple UUIDs are {@code /}-separated,
     * and {@code CC} is the Appendix B check value.
     * Mirrors tst-py's {@code core_id_text(id)}.
     *
     * @param id the Core Identifier to format
     * @return the ST 1204.3 textual representation
     * @throws NullPointerException if {@code id} is {@code null}
     */
    public static String coreIdText(CoreId id) {
        return coreIdTextNative(id);
    }

    /**
     * Validate a {@link UasDatalinkLs} record against the ST 0902.8 Minimum
     * Metadata Set (MISMMS, Table 1).
     *
     * <p>Returns all violations found; an empty list means the record satisfies
     * every MISMMS requirement at the record level. Violations are instances of
     * {@link MismmsViolation} with {@code kind} ∈ {@code {"missing",
     * "missing_security", "zero_length", "alternation_conflict"}}.
     *
     * <p>The Tag 48 Security Local Set sub-item check decodes the security bytes via
     * ST 0102 and verifies the 9 required sub-items (classification, classifying
     * country coding method, classifying country, SCI/SHI info, caveats, releasing
     * instructions, object country coding method, object country codes, version).
     *
     * <p>Mirrors tst-py's {@code validate_mismms(record)}.
     *
     * @param record the UAS Datalink LS to validate
     * @return a list of all MISMMS violations (empty if compliant)
     * @throws NullPointerException if {@code record} is {@code null}
     */
    public static List<MismmsViolation> validateMismms(UasDatalinkLs record) {
        return validateMismmsNative(record);
    }

    private static native CoreId decodeCoreIdNative(byte[] klv)
            throws org.tstrans.KlvDecodeException;

    private static native byte[] encodeCoreIdNative(CoreId id);

    private static native String coreIdTextNative(CoreId id);

    private static native List<MismmsViolation> validateMismmsNative(UasDatalinkLs record);

    // -----------------------------------------------------------------------
    // ST 1010 — SDCC-FLP (general-purpose; carried inside ST 0601 Item 102,
    // but not ST 0601-specific — see the SdccFlp/SdccFlpField Javadoc)
    // -----------------------------------------------------------------------

    /**
     * Decode a MISB ST 1010.3 SDCC-FLP pack (Mode 1 and Mode 2, §6-§7).
     *
     * <p>{@code buf} is the pack bytes starting at Element 1 (Matrix Size) —
     * no outer TLV framing and no leading Universal Label. General-purpose:
     * not ST 0601-specific. Mirrors tst-py's {@code decode_sdcc_flp(buf)}.
     *
     * @param buf raw SDCC-FLP pack bytes
     * @return the decoded {@link SdccFlp}
     * @throws org.tstrans.KlvDecodeException if the buffer is truncated or malformed
     */
    public static SdccFlp decodeSdccFlp(byte[] buf) throws org.tstrans.KlvDecodeException {
        return decodeSdccFlpNative(buf);
    }

    /**
     * Encode a Mode-2 MISB ST 1010.3 SDCC-FLP: standard deviations as IEEE
     * binary32, correlations as ST 1201 IMAPB(-1, 1, {@code clen}). Sparse
     * mode + Bit Vector are chosen automatically when zero-correlations make
     * it pay (Appendix A cost model).
     *
     * <p>{@code correlations.length} must equal
     * {@code stdDevs.length * (stdDevs.length - 1) / 2} (the upper-triangle
     * slot count for a matrix of size {@code stdDevs.length}), in row-major
     * ({@code i < j}) order. Mirrors tst-py's
     * {@code encode_sdcc_flp_mode2(std_devs, correlations, clen)}.
     *
     * @param stdDevs      diagonal &sigma; values
     * @param correlations upper-triangle &rho; values, row-major (i&lt;j)
     * @param clen         IMAPB byte length for the correlation values, {@code 1..=8}
     * @return the encoded pack bytes
     * @throws org.tstrans.KlvEncodeException if {@code correlations.length}
     *         doesn't match the matrix size implied by {@code stdDevs}, if
     *         {@code clen} is outside {@code 1..=8}, or if any correlation
     *         is outside {@code [-1.0, 1.0]}
     */
    public static byte[] encodeSdccFlpMode2(double[] stdDevs, double[] correlations, int clen)
            throws org.tstrans.KlvEncodeException {
        return encodeSdccFlpMode2Native(stdDevs, correlations, clen);
    }

    private static native SdccFlp decodeSdccFlpNative(byte[] buf)
            throws org.tstrans.KlvDecodeException;

    private static native byte[] encodeSdccFlpMode2Native(double[] stdDevs, double[] correlations, int clen)
            throws org.tstrans.KlvEncodeException;

    // -----------------------------------------------------------------------
    // ST 0806 — RVT (Remote Video Terminal) Local Set
    // -----------------------------------------------------------------------

    /**
     * Decode an RVT Local Set body (ST 0806.4 Table 8-1) — the form carried
     * as the value of ST 0601 Tag 73.
     *
     * <p>{@code buf} is body-only — no Universal Label or outer BER length
     * wrapper. Lenient mode: unknown tags are preserved verbatim in
     * {@link RvtLs#unknown()}, per-field validation failures are collected
     * in {@link RvtLs#fieldErrors()} instead of aborting the whole record.
     * Mirrors tst-py's {@code decode_rvt(buf)}.
     *
     * @param buf ST 0806.4 RVT body bytes (no UL / outer BER length)
     * @return the decoded {@link RvtLs}
     * @throws org.tstrans.KlvDecodeException if the buffer is structurally malformed
     */
    public static RvtLs decodeRvt(byte[] buf) throws org.tstrans.KlvDecodeException {
        return nDecodeRvt(buf);
    }

    /**
     * Decode a standalone RVT Local Set: 16-byte UL + BER length + body,
     * verifying the CRC-32/MPEG-2 checksum (Tag 1) when present.
     *
     * <p>Absence of Tag 1 is not an error — an embedded RVT LS is not
     * required to carry one, and this method accepts standalone captures
     * that omit it too. Mirrors tst-py's {@code decode_rvt_standalone(buf)}.
     *
     * @param buf the full standalone RVT wire record (UL + BER length + body)
     * @return the decoded {@link RvtLs}
     * @throws org.tstrans.KlvDecodeException with {@code kind = BAD_UNIVERSAL_LABEL} if the
     *         leading 16 bytes are not the RVT LS UL, or
     *         {@code kind = CHECKSUM_MISMATCH} if a declared Tag 1 value does not match
     *         the recomputed CRC-32/MPEG-2
     */
    public static RvtLs decodeRvtStandalone(byte[] buf) throws org.tstrans.KlvDecodeException {
        return nDecodeRvtStandalone(buf);
    }

    /**
     * Encode an {@link RvtLs} to RVT body bytes (embedded form).
     *
     * <p>Returns body-only bytes — no Universal Label, no outer BER length,
     * and no Tag 1 CRC (an embedded RVT LS is not required to carry one).
     * Use for carriage inside ST 0601 Tag 73. A typed/reserved tag placed in
     * an {@code unknown} list (top-level or nested in {@link RvtPoi}/{@link RvtAoi})
     * is silently filtered before encoding (the typed field wins). Mirrors
     * tst-py's {@code encode_rvt(record)}.
     *
     * @param record the RVT LS to encode
     * @return ST 0806.4 embedded body bytes
     * @throws org.tstrans.KlvEncodeException if a nested {@link RvtPoi}/{@link RvtAoi} omits a
     *         mandatory item, a string field exceeds its byte cap, or an MGRS
     *         easting/northing exceeds 99,999
     */
    public static byte[] encodeRvt(RvtLs record) throws org.tstrans.KlvEncodeException {
        return nEncodeRvt(record);
    }

    /**
     * Encode an {@link RvtLs} as a standalone RVT wire record:
     * {@code [RVT_LS_UL:16][outer BER length][Tag 2 timestamp first][body]
     * [Tag 1 CRC-32/MPEG-2 last]} per ST 0806.4-02/-04. Mirrors tst-py's
     * {@code encode_rvt_standalone(record)}.
     *
     * @param record the RVT LS to encode
     * @return the full standalone RVT wire record
     * @throws org.tstrans.KlvEncodeException with {@code kind = MISSING_MANDATORY_ITEM} and
     *         {@code tag = 2} if {@link RvtLs#timestampUs()} is unset, or any other error
     *         {@link #encodeRvt(RvtLs)} can raise from the same body composition
     */
    public static byte[] encodeRvtStandalone(RvtLs record) throws org.tstrans.KlvEncodeException {
        return nEncodeRvtStandalone(record);
    }

    private static native RvtLs nDecodeRvt(byte[] buf) throws org.tstrans.KlvDecodeException;

    private static native RvtLs nDecodeRvtStandalone(byte[] buf) throws org.tstrans.KlvDecodeException;

    private static native byte[] nEncodeRvt(RvtLs record) throws org.tstrans.KlvEncodeException;

    private static native byte[] nEncodeRvtStandalone(RvtLs record) throws org.tstrans.KlvEncodeException;

    // -----------------------------------------------------------------------
    // ST 0805.1 — KLV -> Cursor-on-Target (CoT) conversion
    // -----------------------------------------------------------------------

    /**
     * Serialize a Platform Position CoT event (ST 0805.1 §5 Table 1) from a
     * decoded {@link UasDatalinkLs} record, using {@link CotConfig#defaults()}.
     *
     * @param record      the UAS Datalink LS to convert
     * @param generatedUs POSIX epoch microseconds stamped into
     *                    {@code detail/_flow-tags_}; an explicit argument (not
     *                    sampled internally) so conversion stays deterministic —
     *                    a replayed-file CoT run must be byte-identical to a
     *                    live one (ST 0805.1 §1)
     * @return the serialized CoT event XML
     * @throws IllegalArgumentException naming the missing KLV tag when a
     *         mapping-required field (uid components, timestamp, sensor
     *         position, altitude) is absent from {@code record}
     * @throws NullPointerException if {@code record} is {@code null}
     */
    public static String platformPositionXml(UasDatalinkLs record, long generatedUs) {
        return platformPositionXml(record, CotConfig.defaults(), generatedUs);
    }

    /**
     * Serialize a Platform Position CoT event (ST 0805.1 §5 Table 1) from a
     * decoded {@link UasDatalinkLs} record with an explicit {@link CotConfig}.
     * Mirrors tst-py's {@code platform_position_xml(record, config=..., generated_us=...)}.
     *
     * @param record      the UAS Datalink LS to convert
     * @param config      the CoT conversion configuration
     * @param generatedUs see {@link #platformPositionXml(UasDatalinkLs, long)}
     * @return the serialized CoT event XML
     * @throws IllegalArgumentException naming the missing KLV tag when a
     *         mapping-required field (uid components, timestamp, sensor
     *         position, altitude) is absent from {@code record}
     * @throws NullPointerException if {@code record} or {@code config} is {@code null}
     */
    public static String platformPositionXml(UasDatalinkLs record, CotConfig config, long generatedUs) {
        return platformPositionXmlNative(record, config, generatedUs);
    }

    /**
     * Serialize a Sensor Point of Interest CoT event (ST 0805.1 §5 Table 2)
     * from a decoded {@link UasDatalinkLs} record, using
     * {@link CotConfig#defaults()}. Linked back to the Platform Position
     * event via {@code detail/link}.
     *
     * @param record      the UAS Datalink LS to convert
     * @param generatedUs see {@link #platformPositionXml(UasDatalinkLs, long)}
     * @return the serialized CoT event XML
     * @throws IllegalArgumentException naming the missing KLV tag when a
     *         mapping-required field (uid components, timestamp, an aimpoint
     *         position pair, that pair's elevation) is absent from {@code record}
     * @throws NullPointerException if {@code record} is {@code null}
     */
    public static String sensorPointOfInterestXml(UasDatalinkLs record, long generatedUs) {
        return sensorPointOfInterestXml(record, CotConfig.defaults(), generatedUs);
    }

    /**
     * Serialize a Sensor Point of Interest CoT event (ST 0805.1 §5 Table 2)
     * from a decoded {@link UasDatalinkLs} record with an explicit
     * {@link CotConfig}. See {@link #platformPositionXml(UasDatalinkLs, CotConfig, long)}
     * for the shared {@code config}/{@code generatedUs} contract. Mirrors
     * tst-py's {@code sensor_point_of_interest_xml(record, config=..., generated_us=...)}.
     *
     * @param record      the UAS Datalink LS to convert
     * @param config      the CoT conversion configuration
     * @param generatedUs see {@link #platformPositionXml(UasDatalinkLs, long)}
     * @return the serialized CoT event XML
     * @throws IllegalArgumentException naming the missing KLV tag when a
     *         mapping-required field (uid components, timestamp, an aimpoint
     *         position pair, that pair's elevation) is absent from {@code record}
     * @throws NullPointerException if {@code record} or {@code config} is {@code null}
     */
    public static String sensorPointOfInterestXml(UasDatalinkLs record, CotConfig config, long generatedUs) {
        return sensorPointOfInterestXmlNative(record, config, generatedUs);
    }

    /**
     * Deterministic Platform Position {@code uid}: {@code "{tag10}_{tag3}"}
     * (ST 0805.1 §5 Table 1). Mirrors tst-py's {@code platform_uid(record)}.
     *
     * @param record the UAS Datalink LS to derive the uid from
     * @return the deterministic uid
     * @throws IllegalArgumentException naming the missing tag when Platform
     *         Designation (Tag 10) or Mission ID (Tag 3) is absent
     * @throws NullPointerException if {@code record} is {@code null}
     */
    public static String platformUid(UasDatalinkLs record) {
        return platformUidNative(record);
    }

    /**
     * Deterministic SPI {@code uid}: {@code "{tag10}_{tag3}_{tag11}"} (ST
     * 0805.1 §5 Table 2). Mirrors tst-py's {@code spi_uid(record)}.
     *
     * @param record the UAS Datalink LS to derive the uid from
     * @return the deterministic uid
     * @throws IllegalArgumentException naming the missing tag when Platform
     *         Designation (Tag 10), Mission ID (Tag 3), or Image Source
     *         Sensor (Tag 11) is absent
     * @throws NullPointerException if {@code record} is {@code null}
     */
    public static String spiUid(UasDatalinkLs record) {
        return spiUidNative(record);
    }

    private static native String platformPositionXmlNative(
            UasDatalinkLs record, CotConfig config, long generatedUs);

    private static native String sensorPointOfInterestXmlNative(
            UasDatalinkLs record, CotConfig config, long generatedUs);

    private static native String platformUidNative(UasDatalinkLs record);

    private static native String spiUidNative(UasDatalinkLs record);

    // -----------------------------------------------------------------------
    // UL dispatcher
    // -----------------------------------------------------------------------

    /**
     * Inspect the first 16 bytes of {@code buf} (the SMPTE Universal Label)
     * and route to the matching typed decoder.
     *
     * <p>Returns:
     * <ul>
     *   <li>{@code Optional<UasDatalinkLs>} when the UL is in the ST 0601 family
     *       (first 13 bytes match + byte 15 is {@code 0x00}).</li>
     *   <li>{@code Optional<PrecisionTimeStampPack>} for the ST 0605 UL.</li>
     *   <li>{@code Optional<SecurityLs>} for the ST 0102 UL (peels UL + outer BER
     *       length, then calls {@link #decodeSecurity(byte[])} on the body).</li>
     *   <li>{@code Optional<VmtiLs>} for the ST 0903 UL (same peel + body decode).</li>
     *   <li>{@code Optional.empty()} for an unrecognised UL.</li>
     * </ul>
     *
     * <p>Mirrors {@code tstrans.klv.parse_klv_universal} in tst-py, including the
     * BER-peel error semantics for the body-only sets:
     * <ul>
     *   <li>{@code buf.length < 16}: throws {@code KlvDecodeException(BAD_UNIVERSAL_LABEL)}.</li>
     *   <li>BER-peel failure (truncated / indefinite-length): throws
     *       {@code KlvDecodeException(TRUNCATED_SET)}.</li>
     *   <li>Body overflow ({@code body_end > buf.length}): throws
     *       {@code KlvDecodeException(TRUNCATED_SET)}.</li>
     *   <li>Trailing bytes ({@code body_end < buf.length}): throws
     *       {@code KlvDecodeException(MALFORMED_BYTES)}.</li>
     * </ul>
     *
     * @param buf the full wire-format KLV record starting with the 16-byte UL
     * @return the decoded {@link KlvSet}, or {@code Optional.empty()} for unknown UL
     * @throws org.tstrans.KlvDecodeException if the buffer is too short for a UL,
     *         or if the BER-length peel or per-set decode fails
     */
    public static Optional<KlvSet> parseUniversal(byte[] buf)
            throws org.tstrans.KlvDecodeException {
        if (buf.length < 16) {
            throw new org.tstrans.KlvDecodeException(
                org.tstrans.KlvDecodeException.Kind.BAD_UNIVERSAL_LABEL,
                "buffer too short for 16-byte UL: have " + buf.length + " bytes");
        }
        byte[] ul = Arrays.copyOf(buf, 16);

        if (isSt0601Family(ul)) {
            return Optional.of(decodeUasDatalink(buf));
        }
        if (Arrays.equals(ul, PRECISION_TIMESTAMP_PACK_UL)) {
            return Optional.of(decodePrecisionTimestamp(buf));
        }
        if (Arrays.equals(ul, SECURITY_LS_UL)) {
            long[] berResult = peelBer(buf, 16, "ST 0102");
            long valueLen = berResult[0];
            long berBytes = berResult[1];
            long bodyStart = 16L + berBytes;
            long bodyEnd = bodyStart + valueLen;
            if (bodyEnd > buf.length) {
                throw new org.tstrans.KlvDecodeException(
                    org.tstrans.KlvDecodeException.Kind.TRUNCATED_SET,
                    "ST 0102 declared body length " + valueLen
                        + " exceeds available " + (buf.length - bodyStart));
            }
            if (bodyEnd < buf.length) {
                throw new org.tstrans.KlvDecodeException(
                    org.tstrans.KlvDecodeException.Kind.MALFORMED_BYTES,
                    "ST 0102 universal record has " + (buf.length - bodyEnd)
                        + " trailing bytes after declared body length " + valueLen);
            }
            byte[] body = Arrays.copyOfRange(buf, (int) bodyStart, (int) bodyEnd);
            return Optional.of(decodeSecurity(body));
        }
        if (Arrays.equals(ul, VMTI_LS_UL)) {
            long[] berResult = peelBer(buf, 16, "ST 0903");
            long valueLen = berResult[0];
            long berBytes = berResult[1];
            long bodyStart = 16L + berBytes;
            long bodyEnd = bodyStart + valueLen;
            if (bodyEnd > buf.length) {
                throw new org.tstrans.KlvDecodeException(
                    org.tstrans.KlvDecodeException.Kind.TRUNCATED_SET,
                    "ST 0903 declared body length " + valueLen
                        + " exceeds available " + (buf.length - bodyStart));
            }
            if (bodyEnd < buf.length) {
                throw new org.tstrans.KlvDecodeException(
                    org.tstrans.KlvDecodeException.Kind.MALFORMED_BYTES,
                    "ST 0903 universal record has " + (buf.length - bodyEnd)
                        + " trailing bytes after declared body length " + valueLen);
            }
            byte[] body = Arrays.copyOfRange(buf, (int) bodyStart, (int) bodyEnd);
            return Optional.of(decodeVmti(body));
        }

        return Optional.empty();
    }

    /**
     * Read a BER short/long-form length starting at {@code offset}.
     * Returns {@code long[] {value, bytesConsumed}}.
     * Mirrors {@code tstrans.klv._read_ber_length}.
     *
     * @throws org.tstrans.KlvDecodeException with {@code TRUNCATED_SET}
     *         if the BER encoding is truncated or uses indefinite-length form
     */
    private static long[] peelBer(byte[] buf, int offset, String setLabel)
            throws org.tstrans.KlvDecodeException {
        if (offset >= buf.length) {
            throw new org.tstrans.KlvDecodeException(
                org.tstrans.KlvDecodeException.Kind.TRUNCATED_SET,
                setLabel + " outer BER length unreadable: truncated BER length");
        }
        int first = buf[offset] & 0xFF;
        if (first < 0x80) {
            return new long[] {first, 1L};
        }
        int nbytes = first & 0x7F;
        if (nbytes == 0) {
            throw new org.tstrans.KlvDecodeException(
                org.tstrans.KlvDecodeException.Kind.TRUNCATED_SET,
                setLabel + " outer BER length unreadable: indefinite-length BER not permitted in KLV");
        }
        if (nbytes > 8) {
            throw new org.tstrans.KlvDecodeException(
                org.tstrans.KlvDecodeException.Kind.TRUNCATED_SET,
                setLabel + " outer BER length unreadable: long-form length exceeds 8 bytes");
        }
        if (offset + 1 + nbytes > buf.length) {
            throw new org.tstrans.KlvDecodeException(
                org.tstrans.KlvDecodeException.Kind.TRUNCATED_SET,
                setLabel + " outer BER length unreadable: truncated BER long-form length");
        }
        long value = 0;
        for (int i = 0; i < nbytes; i++) {
            value = (value << 8) | (buf[offset + 1 + i] & 0xFF);
        }
        return new long[] {value, 1L + nbytes};
    }

    // -----------------------------------------------------------------------
    // Test-only forced-throw helpers (package-private)
    // -----------------------------------------------------------------------

    // Native entry points for test-only forced-throw paths. Package-private so
    // test classes in the same package can reach them; not part of the public API.
    static native void nRaiseDecodeForTest(String kind);
    static native void nRaiseEncodeForTest(String kind);

    /**
     * Force a {@link org.tstrans.KlvDecodeException} with the given
     * {@code Kind} name. Used by {@code KlvErrorModelTest} to exercise the
     * error-mapping wiring before real decode entry points exist.
     *
     * @param kind the {@code KlvDecodeException.Kind} constant name (e.g. {@code "TRUNCATED_SET"})
     * @throws org.tstrans.KlvDecodeException always
     */
    @SuppressWarnings("RedundantThrows")
    static void raiseDecodeForTest(String kind) throws org.tstrans.KlvDecodeException {
        nRaiseDecodeForTest(kind);
    }

    /**
     * Force a {@link org.tstrans.KlvEncodeException} with the given
     * {@code Kind} name. Used by {@code KlvErrorModelTest} to exercise the
     * error-mapping wiring before real encode entry points exist.
     *
     * @param kind the {@code KlvEncodeException.Kind} constant name (e.g. {@code "OUT_OF_RANGE"})
     * @throws org.tstrans.KlvEncodeException always
     */
    @SuppressWarnings("RedundantThrows")
    static void raiseEncodeForTest(String kind) throws org.tstrans.KlvEncodeException {
        nRaiseEncodeForTest(kind);
    }

    static {
        org.tstrans.NativeLoader.load();
    }
}
