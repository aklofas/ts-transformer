package org.tstrans;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;
import java.util.Arrays;
import java.util.Set;
import java.util.stream.Collectors;

class ErrorModelTest {
    @Test
    void demuxExceptionCarriesKindAndMessage() {
        DemuxException e = new DemuxException(DemuxException.Kind.MALFORMED_PSI, "bad psi");
        assertTrue(e instanceof BindingException, "DemuxException must extend BindingException");
        assertEquals(DemuxException.Kind.MALFORMED_PSI, e.kind());
        assertEquals("bad psi", e.getMessage());
    }

    /**
     * The members are exactly the domain's {@code BindingErrorKind} subset
     * (WP-B3 / spec §3.3). Every member except {@code INTERNAL} maps 1:1 to a
     * {@code tst_core::mpegts::demux::DemuxError} variant; {@code INTERNAL} is the
     * JNI-side event-conversion failure. {@code UNEXPECTED_EOF} was parity-only
     * and had no producer at all.
     */
    @Test
    void kindMembersMatchTheDemuxDomain() {
        Set<String> expected = Set.of("UNRECOVERABLE", "MALFORMED_PSI", "MALFORMED_PES", "SYNC_BUF_EXHAUSTED", "STRICT_REJECTION", "INTERNAL");
        Set<String> actual = Arrays.stream(DemuxException.Kind.values())
            .map(Enum::name)
            .collect(Collectors.toSet());
        assertEquals(expected, actual);
        assertEquals(6, DemuxException.Kind.values().length);
        assertThrows(IllegalArgumentException.class, () -> DemuxException.Kind.valueOf("SYNC_LOSS"));
        assertThrows(IllegalArgumentException.class, () -> DemuxException.Kind.valueOf("BAD_PMT"));
        assertThrows(IllegalArgumentException.class, () -> DemuxException.Kind.valueOf("BAD_PES"));
        assertThrows(IllegalArgumentException.class, () -> DemuxException.Kind.valueOf("UNEXPECTED_EOF"));
    }
}
