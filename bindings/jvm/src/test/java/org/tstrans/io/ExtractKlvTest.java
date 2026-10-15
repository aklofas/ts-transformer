package org.tstrans.io;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.inputTs;

import java.util.List;
import org.junit.jupiter.api.Test;

class ExtractKlvTest {

    @Test
    void rawDefaultYieldsBytesNoPts() throws Exception {
        List<KlvEntry> entries;
        try (var s = Io.extractKlv(inputTs())) {
            entries = s.toList();
        }
        assertFalse(entries.isEmpty(), "fixture carries an ST 0601 KLV stream");
        KlvEntry first = entries.get(0);
        assertNull(first.pts(), "default withPts=false -> null pts");
        assertNotNull(first.raw(), "default parsed=false -> raw bytes present");
        assertNull(first.parsed(), "default parsed=false -> no typed set");
    }

    @Test
    void withPtsAttachesTimestamp() throws Exception {
        List<KlvEntry> entries;
        try (var s = Io.extractKlv(inputTs(),
                ExtractKlvOptions.builder().withPts(true).build())) {
            entries = s.toList();
        }
        assertFalse(entries.isEmpty());
        assertNotNull(entries.get(0).pts(), "withPts=true -> pts present");
        assertNotNull(entries.get(0).raw());
    }

    @Test
    void parsedMalformedPropagatesAsWrappedKlvDecodeException() throws Exception {
        // The shared golden's KLV is minimal_st0601_ls (a recognized ST 0601 UL with
        // an empty body). parse_klv_universal returns an empty Optional only for an
        // UNRECOGNIZED UL; the ST 0601 UL IS recognized but its empty body makes the
        // decoder raise KlvDecodeException(TRUNCATED_SET). With skipMalformed=false
        // (default) that exception must propagate (not be swallowed). Java streams
        // can't throw checked exceptions in map(), so it surfaces wrapped.
        RuntimeException ex = assertThrows(RuntimeException.class, () -> {
            try (var s = Io.extractKlv(inputTs(),
                    ExtractKlvOptions.builder().parsed(true).build())) {
                s.toList();
            }
        });
        assertTrue(ex.getCause() instanceof org.tstrans.KlvDecodeException,
            "malformed KLV must surface as a (wrapped) KlvDecodeException");
    }

    @Test
    void parsedSkipMalformedSkipsRow() throws Exception {
        List<KlvEntry> entries;
        try (var s = Io.extractKlv(inputTs(),
                ExtractKlvOptions.builder().parsed(true).skipMalformed(true).build())) {
            entries = s.toList();
        }
        assertTrue(entries.isEmpty(), "skipMalformed=true must drop the malformed LS rows");
    }
}
