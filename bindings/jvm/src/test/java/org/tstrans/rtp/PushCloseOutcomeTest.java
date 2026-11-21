package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;

import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import org.junit.jupiter.api.Test;

/**
 * {@link PushCloseOutcome.Kind} is written by hand: it must name every variant of
 * {@code tst_rtp::PushCloseOutcome}, in the same order, and its codes are that order, which
 * the native encoder in bindings/jvm/src/rtp/push.rs writes.
 */
class PushCloseOutcomeTest {
    private static final Path RUST = Path.of(System.getProperty("user.dir"), "..", "..",
        "crates/tst-rtp/src/rtsp/client/push/mod.rs").normalize();

    /** The variant names of {@code pub enum PushCloseOutcome}, in declaration order. */
    private static List<String> rustVariants() throws Exception {
        String src = Files.readString(RUST, StandardCharsets.UTF_8);
        int start = src.indexOf("pub enum PushCloseOutcome {");
        int end = src.indexOf("\n}", start);
        assertFalse(start < 0 || end < 0, "no PushCloseOutcome enum in " + RUST);
        List<String> out = new ArrayList<>();
        // A variant starts at four spaces of indent; its doc lines and fields are deeper or comments.
        Matcher m = Pattern.compile("(?m)^    ([A-Z][A-Za-z0-9]*)\\b").matcher(src.substring(start, end));
        while (m.find()) out.add(m.group(1));
        return out;
    }

    private static String upperSnake(String camel) {
        return camel.replaceAll("([a-z0-9])([A-Z])", "$1_$2").toUpperCase(java.util.Locale.ROOT);
    }

    @Test
    void theKindsAreTheRustVariantsInOrder() throws Exception {
        List<String> rust = rustVariants();
        List<String> expected = rust.stream().map(PushCloseOutcomeTest::upperSnake).toList();
        List<String> java = Arrays.stream(PushCloseOutcome.Kind.values()).map(Enum::name).toList();
        assertEquals(expected, java, "Rust variants " + rust);
        for (PushCloseOutcome.Kind k : PushCloseOutcome.Kind.values()) {
            assertEquals(java.indexOf(k.name()), k.code(), k + "'s code is its Rust position");
        }
    }
}
