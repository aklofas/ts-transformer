package org.tstrans;

import static org.junit.jupiter.api.Assertions.*;
import org.junit.jupiter.api.Test;

class RtpErrorModelTest {
    @Test
    void rtpExceptionCarriesKindAndMessage() {
        RtpException e = new RtpException(RtpException.Kind.TRANSPORT, "wire broke");
        assertTrue(e instanceof BindingException);
        assertEquals(RtpException.Kind.TRANSPORT, e.kind());
        assertEquals("wire broke", e.getMessage());
    }
    /**
     * Intermediate 0.7.0 state: Task B3.2 ADDED the five {@code TransportError}
     * projections and tst-rtp's six {@code ConnectError} kinds; Task B3.6
     * retires {@code TRANSPORT}, {@code MALFORMED_PACKET}, {@code CANCELLED}
     * and {@code TIMEOUT} (all four lose their producers) and re-pins this at
     * ten.
     */
    @Test
    void kindDeclaresEveryMemberTheNativeCanRaise() {
        for (String n : new String[]{
                "TRANSPORT","MALFORMED_PACKET","CANCELLED","TIMEOUT",
                "BACKPRESSURE","BROKEN","CLOSED","TOO_LARGE",
                "PAYLOAD_TYPE_PARAM","MISSING_PAYLOAD_TYPE_PARAM","URL",
                "HOST_NOT_LITERAL","IO","IFACE_UNSUPPORTED"}) {
            RtpException.Kind.valueOf(n);
        }
        assertEquals(14, RtpException.Kind.values().length);
    }
}
