package org.tstrans;

import static org.junit.jupiter.api.Assertions.*;

import java.util.Arrays;
import java.util.List;
import org.junit.jupiter.api.Test;

class RistErrorModelTest {
    /** Exactly the 13 Rist rows of scripts/ratchets/kind-equivalence.tsv, in table order. */
    @Test
    void kindMembersMatchTheRistDomain() {
        List<String> expected = List.of("URL", "FFI", "TOO_LARGE", "CLOSED", "INVALID_CONFIG", "ENCRYPTION_DISABLED", "CONTEXT_CREATE_FAILED", "PEER_CREATE_FAILED", "BACKPRESSURE", "BROKEN", "INPUT_MALFORMED", "END_OF_STREAM", "CONFIG_INVALID");
        List<String> actual = Arrays.stream(RistException.Kind.values())
            .map(Enum::name).toList();
        assertEquals(expected, actual);
        assertEquals(13, RistException.Kind.values().length);
    }

    @Test
    void carriesKindAndMessage() {
        var e = new RistException(RistException.Kind.URL, "boom");
        assertEquals(RistException.Kind.URL, e.kind());
        assertEquals("boom", e.getMessage());
        assertInstanceOf(BindingException.class, e);
    }
}
