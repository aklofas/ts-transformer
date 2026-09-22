package org.tstrans;
import static org.junit.jupiter.api.Assertions.*;
import org.junit.jupiter.api.Test;

class SrtErrorModelTest {
    /**
     * Intermediate 0.7.0 state: Task B3.2 ADDED {@code BACKPRESSURE},
     * {@code TOO_LARGE} and {@code INPUT_MALFORMED}; Task B3.6 retires
     * {@code WOULD_BLOCK} (no producer once {@code TransportError::Backpressure}
     * maps to {@code BACKPRESSURE}) and re-pins this at ten.
     */
    @Test void kindDeclaresEveryMemberTheNativeCanRaise() {
        assertEquals(11, SrtException.Kind.values().length);
        assertNotNull(SrtException.Kind.valueOf("CONFIG_INVALID"));
        assertNotNull(SrtException.Kind.valueOf("BROKEN"));
        assertNotNull(SrtException.Kind.valueOf("WOULD_BLOCK"));
        assertNotNull(SrtException.Kind.valueOf("BACKPRESSURE"));
        assertNotNull(SrtException.Kind.valueOf("TOO_LARGE"));
        assertNotNull(SrtException.Kind.valueOf("INPUT_MALFORMED"));
    }
    @Test void carriesKindAndMessage() {
        var e = new SrtException(SrtException.Kind.TIMEOUT, "boom");
        assertEquals(SrtException.Kind.TIMEOUT, e.kind());
        assertEquals("boom", e.getMessage());
    }
}
