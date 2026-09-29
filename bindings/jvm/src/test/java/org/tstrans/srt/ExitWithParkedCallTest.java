package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.io.TempDir;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.CsvSource;

/**
 * A JVM that exits while a thread is parked inside libsrt must terminate.
 *
 * <p>The listener-mode opens ({@code DemuxReceiver.fromUrl},
 * {@code Receiver.fromUrl}, {@code ManagedDemuxReceiver.fromUrl} with
 * {@code ?mode=listener}) block in their first accept BEFORE they return an
 * object, so a program that decides to leave while one is waiting for a peer
 * has nothing to {@code close()}. The native library runs libsrt's cleanup
 * from a C {@code atexit} handler, and that cleanup used to wait forever on
 * the parked accept: the JVM printed its last line and never exited. The
 * handler now unparks every call still inside libsrt first.
 *
 * <p>The park has to live in a CHILD JVM: a regression is a process that does
 * not exit, which no in-process assertion can observe, and a JUnit
 * {@code @Timeout} cannot interrupt a thread blocked in a native call. The
 * parent launches {@link ExitWithParkedCallChild} with its own class path and
 * gives it a hard deadline.
 */
class ExitWithParkedCallTest {
    /**
     * The exit guard's own ceiling is 2 s; a healthy child is done in about
     * that long including JVM start-up. This only bounds a hang.
     */
    private static final long CHILD_DEADLINE_S = 20;

    @ParameterizedTest(name = "{0} parked in its first accept, leaving by {1}")
    @CsvSource({
        "demux, exit",
        "demux, return",
        "receiver, exit",
        "managed, exit",
        "managed, return",
    })
    @Timeout(60) // safety net above the child deadline
    void jvmExitsWithAListenerOpenParkedInItsFirstAccept(String shape, String how, @TempDir Path tmp)
            throws Exception {
        assumeTrue(isLinux(),
            "SRT live-socket test gated to Linux (same as the Rust/C twins)");

        Path out = tmp.resolve("child.out");
        String java = Path.of(System.getProperty("java.home"), "bin", "java").toString();
        Process child = new ProcessBuilder(List.of(
                java,
                "-cp", System.getProperty("java.class.path"),
                ExitWithParkedCallChild.class.getName(),
                shape, how))
            .redirectErrorStream(true)
            .redirectOutput(out.toFile())
            .redirectInput(new File("/dev/null"))
            .start();

        boolean exited = child.waitFor(CHILD_DEADLINE_S, TimeUnit.SECONDS);
        if (!exited) {
            child.destroyForcibly().waitFor();
        }
        String output = Files.readString(out, StandardCharsets.UTF_8);

        assertTrue(exited,
            "the child JVM did not exit within " + CHILD_DEADLINE_S
                + " s — process exit hung with a call parked in libsrt\n" + output);
        assertTrue(output.contains("PARKED"),
            "the child never proved the park (exit " + child.exitValue() + ")\n" + output);
        assertEquals(0, child.exitValue(), "the child JVM did not exit cleanly\n" + output);
    }
}
