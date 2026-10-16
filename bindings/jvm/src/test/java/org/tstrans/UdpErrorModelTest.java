package org.tstrans;

import static org.junit.jupiter.api.Assertions.*;

import java.util.Arrays;
import java.util.List;
import org.junit.jupiter.api.Test;

class UdpErrorModelTest {
    /** Exactly the 10 Udp rows of scripts/ratchets/kind-equivalence.tsv, in table order. */
    @Test
    void kindMembersMatchTheUdpDomain() {
        List<String> expected = List.of("URL", "IO", "TOO_LARGE", "CLOSED", "INVALID_CONFIG", "BACKPRESSURE", "BROKEN", "INPUT_MALFORMED", "END_OF_STREAM", "CONFIG_INVALID");
        List<String> actual = Arrays.stream(UdpException.Kind.values())
            .map(Enum::name).toList();
        assertEquals(expected, actual);
        assertEquals(10, UdpException.Kind.values().length);
    }

    @Test
    void carriesKindAndMessage() {
        var e = new UdpException(UdpException.Kind.URL, "boom");
        assertEquals(UdpException.Kind.URL, e.kind());
        assertEquals("boom", e.getMessage());
        assertInstanceOf(BindingException.class, e);
    }
}
