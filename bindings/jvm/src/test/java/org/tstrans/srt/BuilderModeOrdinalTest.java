package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.assertEquals;

import java.util.Arrays;
import java.util.List;
import org.junit.jupiter.api.Test;

/** {@code Builder.Mode} is package-private, so its order pin lives beside it (native table: srt/lowlevel.rs). */
class BuilderModeOrdinalTest {
    @Test
    void modeOrderMatchesTheNativeDecodeTable() {
        assertEquals(
            List.of("URL_CHOICE", "CALLER", "LISTENER", "RENDEZVOUS"),
            Arrays.stream(Builder.Mode.values()).map(Enum::name).toList());
    }
}
