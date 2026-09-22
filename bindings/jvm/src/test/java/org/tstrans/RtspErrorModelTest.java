package org.tstrans;
import static org.junit.jupiter.api.Assertions.*;
import java.util.Arrays;
import java.util.Set;
import java.util.stream.Collectors;
import org.junit.jupiter.api.Test;

class RtspErrorModelTest {
    /**
     * The members are exactly the rtsp domain's {@code BindingErrorKind} subset
     * (WP-B3 / spec §3.3). Names are unchanged in 0.7.0; two PRODUCERS moved
     * bucket ({@code AuthUnsupported} → {@code AUTH_REQUIRED}, the four
     * SDP-media errors → {@code NOT_FOUND}).
     */
    @Test void kindMembersMatchTheRtspDomain() {
        Set<String> expected = Set.of("PROTOCOL", "AUTH_FAILED", "AUTH_REQUIRED", "NOT_FOUND",
            "UNSUPPORTED_TRANSPORT", "TLS", "IO", "TIMEOUT", "SERVER", "MOUNT");
        Set<String> actual = Arrays.stream(RtspException.Kind.values())
            .map(Enum::name)
            .collect(Collectors.toSet());
        assertEquals(expected, actual);
        assertEquals(10, RtspException.Kind.values().length);
    }
    @Test void carriesKindAndMessage() {
        var e = new RtspException(RtspException.Kind.TLS, "boom");
        assertEquals(RtspException.Kind.TLS, e.kind());
        assertEquals("boom", e.getMessage());
    }
}
