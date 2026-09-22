package org.tstrans;

import static org.junit.jupiter.api.Assertions.*;
import org.junit.jupiter.api.Test;

class MuxErrorModelTest {
    @Test
    void muxExceptionCarriesKindAndMessage() {
        MuxException e = new MuxException(MuxException.Kind.CONFIG_INVALID, "bad config");
        assertTrue(e instanceof BindingException);
        assertEquals(MuxException.Kind.CONFIG_INVALID, e.kind());
        assertEquals("bad config", e.getMessage());
    }
    /**
     * Task B3.2 ADDED the four precise {@code MuxError} members that used to
     * fold into {@code INPUT_MALFORMED}; nothing retires here, so B3.6 leaves
     * this at nine.
     */
    @Test
    void kindDeclaresEveryMemberTheNativeCanRaise() {
        for (String n : new String[]{
                "INPUT_MALFORMED","CONFIG_INVALID","INVALID_USAGE","BACKPRESSURE","INTERNAL",
                "INVALID_NAL","KLV_TOO_LARGE","INVALID_AV1_OBU","MISP_TIME"}) {
            MuxException.Kind.valueOf(n);
        }
        assertEquals(9, MuxException.Kind.values().length);
    }
}
