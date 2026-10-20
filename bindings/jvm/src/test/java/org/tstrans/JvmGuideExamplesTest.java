package org.tstrans;

import static org.junit.jupiter.api.Assertions.*;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import org.junit.jupiter.api.Test;

/**
 * docs/languages/jvm.md carries the full text of each program under
 * src/examples/java, which the Gradle build compiles; this keeps the two
 * identical so the guide never shows code that no longer builds.
 */
final class JvmGuideExamplesTest {
    /** Paths resolved from Gradle's user.dir (bindings/jvm). */
    private static String read(String relative) throws IOException {
        Path p = Path.of(System.getProperty("user.dir")).resolve(relative).normalize();
        // The Windows checkout converts line endings; compare the text, not the bytes.
        return Files.readString(p).replace("\r\n", "\n");
    }

    @Test
    void serveWhepInTheGuideMatchesTheCompiledExample() throws IOException {
        String source = read("src/examples/java/ServeWhep.java");
        String guide = read("../../docs/languages/jvm.md");
        String block = "```java\n" + source + "```\n";
        assertTrue(guide.contains(block),
            "docs/languages/jvm.md must carry src/examples/java/ServeWhep.java verbatim"
                + " in a ```java block");
    }
}
