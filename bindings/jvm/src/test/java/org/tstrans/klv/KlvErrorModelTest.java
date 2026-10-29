package org.tstrans.klv;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.EnabledIfSystemProperty;
import org.tstrans.KlvDecodeException;
import org.tstrans.KlvEncodeException;
import org.tstrans.internal.KlvProbe;

/** Verifies that every {@code KlvDecodeException.Kind} and {@code KlvEncodeException.Kind}
 *  constant round-trips through the JNI forced-throw path with the correct kind set. */
@EnabledIfSystemProperty(named = "tst.jniTestHooks", matches = "true")
class KlvErrorModelTest {

    @Test
    void decodeKindsRoundTrip() {
        for (KlvDecodeException.Kind k : KlvDecodeException.Kind.values()) {
            KlvDecodeException ex = assertThrows(
                    KlvDecodeException.class,
                    () -> KlvProbe.nRaiseDecode(k.name()));
            assertEquals(k, ex.kind(), "expected Kind." + k.name());
        }
    }

    @Test
    void encodeKindsRoundTrip() {
        for (KlvEncodeException.Kind k : KlvEncodeException.Kind.values()) {
            KlvEncodeException ex = assertThrows(
                    KlvEncodeException.class,
                    () -> KlvProbe.nRaiseEncode(k.name()));
            assertEquals(k, ex.kind(), "expected Kind." + k.name());
        }
    }
}
