package org.tstrans;

import static org.junit.jupiter.api.Assertions.*;
import org.junit.jupiter.api.Test;
import java.util.Arrays;
import java.util.Set;
import java.util.stream.Collectors;

class MuxErrorModelTest {
    @Test
    void muxExceptionCarriesKindAndMessage() {
        MuxException e = new MuxException(MuxException.Kind.CONFIG_INVALID, "bad config");
        assertTrue(e instanceof BindingException);
        assertEquals(MuxException.Kind.CONFIG_INVALID, e.kind());
        assertEquals("bad config", e.getMessage());
    }
    /**
     * The members are exactly the domain's {@code BindingErrorKind} subset
     * (WP-B3 / spec §3.3). The four precise members were added in 0.7.0; nothing retired.
     */
    @Test
    void kindMembersMatchTheMuxDomain() {
        Set<String> expected = Set.of("INPUT_MALFORMED", "CONFIG_INVALID", "INVALID_USAGE", "BACKPRESSURE", "INTERNAL", "INVALID_NAL", "KLV_TOO_LARGE", "INVALID_AV1_OBU", "MISP_TIME");
        Set<String> actual = Arrays.stream(MuxException.Kind.values())
            .map(Enum::name)
            .collect(Collectors.toSet());
        assertEquals(expected, actual);
        assertEquals(9, MuxException.Kind.values().length);

    }
}
