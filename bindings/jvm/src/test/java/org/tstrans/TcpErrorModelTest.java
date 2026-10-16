package org.tstrans;

import static org.junit.jupiter.api.Assertions.*;

import java.util.Arrays;
import java.util.List;
import org.junit.jupiter.api.Test;

class TcpErrorModelTest {
    /** Exactly the 13 Tcp rows of scripts/ratchets/kind-equivalence.tsv, in table order. */
    @Test
    void kindMembersMatchTheTcpDomain() {
        List<String> expected = List.of("URL", "IO", "TOO_LARGE", "CLOSED", "CONNECT_TIMEOUT", "INVALID_CONFIG", "TLS", "TLS_DISABLED", "BACKPRESSURE", "BROKEN", "INPUT_MALFORMED", "END_OF_STREAM", "CONFIG_INVALID");
        List<String> actual = Arrays.stream(TcpException.Kind.values())
            .map(Enum::name).toList();
        assertEquals(expected, actual);
        assertEquals(13, TcpException.Kind.values().length);
    }

    @Test
    void carriesKindAndMessage() {
        var e = new TcpException(TcpException.Kind.URL, "boom");
        assertEquals(TcpException.Kind.URL, e.kind());
        assertEquals("boom", e.getMessage());
        assertInstanceOf(BindingException.class, e);
    }
}
