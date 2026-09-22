package org.tstrans;

import static org.junit.jupiter.api.Assertions.*;
import org.junit.jupiter.api.Test;
import java.util.Arrays;
import java.util.Set;
import java.util.stream.Collectors;

class RtpErrorModelTest {
    @Test
    void rtpExceptionCarriesKindAndMessage() {
        RtpException e = new RtpException(RtpException.Kind.BROKEN, "wire broke");
        assertTrue(e instanceof BindingException);
        assertEquals(RtpException.Kind.BROKEN, e.kind());
        assertEquals("wire broke", e.getMessage());
    }
    /**
     * The members are exactly the domain's {@code BindingErrorKind} subset
     * (WP-B3 / spec §3.3). The five {@code TransportError} projections an rtp shell can raise plus
     * {@code tst_rtp::ConnectError}'s six variants. The four retired members had
     * no producer left after WP-B3 re-pointed the rtp tables.
     */
    @Test
    void kindMembersMatchTheRtpDomain() {
        Set<String> expected = Set.of("BACKPRESSURE", "BROKEN", "CLOSED", "TOO_LARGE", "PAYLOAD_TYPE_PARAM", "MISSING_PAYLOAD_TYPE_PARAM", "URL", "HOST_NOT_LITERAL", "IO", "IFACE_UNSUPPORTED");
        Set<String> actual = Arrays.stream(RtpException.Kind.values())
            .map(Enum::name)
            .collect(Collectors.toSet());
        assertEquals(expected, actual);
        assertEquals(10, RtpException.Kind.values().length);
        assertThrows(IllegalArgumentException.class, () -> RtpException.Kind.valueOf("TRANSPORT"));
        assertThrows(IllegalArgumentException.class, () -> RtpException.Kind.valueOf("CANCELLED"));
        assertThrows(IllegalArgumentException.class, () -> RtpException.Kind.valueOf("MALFORMED_PACKET"));
        assertThrows(IllegalArgumentException.class, () -> RtpException.Kind.valueOf("TIMEOUT"));
    }
}
