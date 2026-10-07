package org.tstrans;

import static org.junit.jupiter.api.Assertions.*;

import java.util.Arrays;
import java.util.Set;
import java.util.stream.Collectors;
import org.junit.jupiter.api.Test;
import org.tstrans.internal.HlsProbe;

class HlsErrorModelTest {
    /** Exactly the ten HLS rows of scripts/ratchets/kind-equivalence.tsv. */
    @Test
    void kindMembersMatchTheHlsDomain() {
        Set<String> expected = Set.of("IO", "INVALID_CONFIG", "FINISHED", "TLS", "URL",
            "BIND_FAILED", "UNALIGNED_PUSH_TS", "TLS_DISABLED", "CLOSED", "INTERNAL");
        Set<String> actual = Arrays.stream(HlsException.Kind.values())
            .map(Enum::name).collect(Collectors.toSet());
        assertEquals(expected, actual);
        assertEquals(10, HlsException.Kind.values().length);
    }

    @Test
    void carriesKindAndMessage() {
        var e = new HlsException(HlsException.Kind.URL, "boom");
        assertEquals(HlsException.Kind.URL, e.kind());
        assertEquals("boom", e.getMessage());
    }

    /** Every kind round-trips through the one Rust raise path (jni-test-hooks probe). */
    @Test
    void everyKindRoundTripsFromRust() {
        for (HlsException.Kind k : HlsException.Kind.values()) {
            HlsException e = assertThrows(HlsException.class,
                () -> HlsProbe.nRaise(k.name(), "test " + k.name()));
            assertEquals(k, e.kind());
            assertTrue(e.getMessage().contains(k.name()));
        }
    }
}
