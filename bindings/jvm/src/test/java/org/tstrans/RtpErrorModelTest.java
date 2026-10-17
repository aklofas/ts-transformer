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
     * The members are exactly the domain's {@code BindingErrorKind} subset:
     * the five {@code TransportError} projections an rtp shell can raise plus
     * {@code tst_rtp::RtpConnectError}'s six variants plus {@code END_OF_STREAM}
     * (the peer ended the stream) and the two shell-generic kinds
     * {@code INPUT_MALFORMED} and {@code CONFIG_INVALID}. The four retired members have
     * no producer left now that the rtp tables point at the shared classifier.
     */
    @Test
    void kindMembersMatchTheRtpDomain() {
        Set<String> expected = Set.of("BACKPRESSURE", "BROKEN", "CLOSED", "TOO_LARGE", "PAYLOAD_TYPE_PARAM", "MISSING_PAYLOAD_TYPE_PARAM", "URL", "HOST_NOT_LITERAL", "IO", "IFACE_UNSUPPORTED", "END_OF_STREAM", "INPUT_MALFORMED", "CONFIG_INVALID");
        Set<String> actual = Arrays.stream(RtpException.Kind.values())
            .map(Enum::name)
            .collect(Collectors.toSet());
        assertEquals(expected, actual);
        assertEquals(13, RtpException.Kind.values().length);
        assertThrows(IllegalArgumentException.class, () -> RtpException.Kind.valueOf("TRANSPORT"));
        assertThrows(IllegalArgumentException.class, () -> RtpException.Kind.valueOf("CANCELLED"));
        assertThrows(IllegalArgumentException.class, () -> RtpException.Kind.valueOf("MALFORMED_PACKET"));
        assertThrows(IllegalArgumentException.class, () -> RtpException.Kind.valueOf("TIMEOUT"));
    }
}
