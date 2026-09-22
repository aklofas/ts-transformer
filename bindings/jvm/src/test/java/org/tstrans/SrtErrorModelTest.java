package org.tstrans;
import static org.junit.jupiter.api.Assertions.*;
import org.junit.jupiter.api.Test;

class SrtErrorModelTest {
    /**
     * Intermediate 0.7.0 state: Tasks B3.2/B3.4 ADDED {@code BACKPRESSURE},
     * {@code TOO_LARGE} and {@code INPUT_MALFORMED}; Task B3.6 retires
     * {@code WOULD_BLOCK} (no producer once {@code TransportError::Backpressure}
     * maps to {@code BACKPRESSURE}) and re-pins this at ELEVEN — the brief said
     * ten, before {@code END_OF_STREAM} was added to make the kind table total
     * with C and Python.
     */
    @Test void kindDeclaresEveryMemberTheNativeCanRaise() {
        assertEquals(12, SrtException.Kind.values().length);
        assertNotNull(SrtException.Kind.valueOf("CONFIG_INVALID"));
        assertNotNull(SrtException.Kind.valueOf("BROKEN"));
        assertNotNull(SrtException.Kind.valueOf("WOULD_BLOCK"));
        assertNotNull(SrtException.Kind.valueOf("BACKPRESSURE"));
        assertNotNull(SrtException.Kind.valueOf("TOO_LARGE"));
        assertNotNull(SrtException.Kind.valueOf("INPUT_MALFORMED"));
        assertNotNull(SrtException.Kind.valueOf("END_OF_STREAM"));
    }
    @Test void carriesKindAndMessage() {
        var e = new SrtException(SrtException.Kind.TIMEOUT, "boom");
        assertEquals(SrtException.Kind.TIMEOUT, e.kind());
        assertEquals("boom", e.getMessage());
    }
}
