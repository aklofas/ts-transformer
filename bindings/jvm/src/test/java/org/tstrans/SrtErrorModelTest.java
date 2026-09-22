package org.tstrans;
import static org.junit.jupiter.api.Assertions.*;
import org.junit.jupiter.api.Test;
import java.util.Arrays;
import java.util.Set;
import java.util.stream.Collectors;

class SrtErrorModelTest {
    /**
     * The members are exactly the domain's {@code BindingErrorKind} subset
     * (WP-B3 / spec §3.3). Eleven, not the ten of the original plan: {@code END_OF_STREAM} was added
     * so the kind table is total with the C ABI's {@code TST_E_END_OF_STREAM}
     * (-12). {@code WOULD_BLOCK} retired — its only producer
     * ({@code TransportError::Backpressure}) now maps to {@code BACKPRESSURE}.
     */
    @Test
    void kindMembersMatchTheSrtDomain() {
        Set<String> expected = Set.of("CONFIG_INVALID", "CONNECT_FAILED", "ACCEPT_FAILED", "TIMEOUT", "CLOSED", "BROKEN", "IO", "BACKPRESSURE", "TOO_LARGE", "INPUT_MALFORMED", "END_OF_STREAM");
        Set<String> actual = Arrays.stream(SrtException.Kind.values())
            .map(Enum::name)
            .collect(Collectors.toSet());
        assertEquals(expected, actual);
        assertEquals(11, SrtException.Kind.values().length);
        assertThrows(IllegalArgumentException.class, () -> SrtException.Kind.valueOf("WOULD_BLOCK"));
    }

    @Test void carriesKindAndMessage() {
        var e = new SrtException(SrtException.Kind.TIMEOUT, "boom");
        assertEquals(SrtException.Kind.TIMEOUT, e.kind());
        assertEquals("boom", e.getMessage());
    }
}
