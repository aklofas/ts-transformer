package org.tstrans;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

import org.junit.jupiter.api.Test;

class ErrorModelTest {
    @Test
    void demuxExceptionCarriesKindAndMessage() {
        DemuxException e = new DemuxException(DemuxException.Kind.BAD_PMT, "bad pmt");
        assertTrue(e instanceof BindingException, "DemuxException must extend BindingException");
        assertEquals(DemuxException.Kind.BAD_PMT, e.kind());
        assertEquals("bad pmt", e.getMessage());
    }

    @Test
    void kindDeclaresEveryMemberTheNativeCanRaise() {
        // Intermediate 0.7.0 state: Task B3.2 ADDED the four members named for
        // their `tst_core::DemuxError` variants; Task B3.6 retires the four
        // bucket names they replace (SYNC_LOSS / BAD_PMT / BAD_PES) plus the
        // parity-only UNEXPECTED_EOF, and re-pins this at six.
        DemuxException.Kind[] ks = DemuxException.Kind.values();
        assertEquals(10, ks.length);
        // names asserted so a Rust-side rename is caught here.
        for (String n : new String[] {
                "SYNC_LOSS", "BAD_PMT", "BAD_PES", "UNEXPECTED_EOF",
                "STRICT_REJECTION", "INTERNAL",
                "UNRECOVERABLE", "MALFORMED_PSI", "MALFORMED_PES",
                "SYNC_BUF_EXHAUSTED"}) {
            DemuxException.Kind.valueOf(n); // throws if missing
        }
    }
}
