package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.assertEquals;

import java.util.Arrays;
import java.util.Map;
import java.util.stream.Collectors;
import org.junit.jupiter.api.Test;

/** {@code Builder.Mode} is package-private, so its code pin lives beside it (native table: srt/lowlevel.rs). */
class BuilderModeOrdinalTest {
    @Test
    void modeCodesMatchTheNativeDecodeTable() {
        assertEquals(
            Map.of("URL_CHOICE", 0, "CALLER", 1, "LISTENER", 2, "RENDEZVOUS", 3),
            Arrays.stream(Builder.Mode.values()).collect(Collectors.toMap(Enum::name, Builder.Mode::code)));
    }
}
