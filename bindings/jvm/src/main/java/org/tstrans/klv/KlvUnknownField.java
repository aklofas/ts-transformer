package org.tstrans.klv;

import java.nio.ByteBuffer;

/**
 * A KLV field whose tag is not modelled by the typed decoder. Round-trips
 * through encode unchanged (typed-tag collisions are dropped; typed wins).
 *
 * <p>{@code value} is a writable heap {@code ByteBuffer} copy of the raw TLV
 * value bytes (no tag, no length — value only), owned by the caller of the
 * record and safe to retain past the decode call.
 *
 * @param tag   BER-OID tag value
 * @param value raw value bytes (heap copy, JVM-owned)
 */
public record KlvUnknownField(long tag, ByteBuffer value) {}
