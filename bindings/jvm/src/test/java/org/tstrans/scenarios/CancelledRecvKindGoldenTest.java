package org.tstrans.scenarios;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.roundtripConfig;

import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.SrtException;
import org.tstrans.srt.Builder;
import org.tstrans.srt.CancelHandle;
import org.tstrans.srt.Listener;
import org.tstrans.srt.MuxSender;
import org.tstrans.srt.Receiver;
import org.tstrans.srt.Socket;

/**
 * JVM leg of the {@code cancelled-recv-kind} cross-binding golden
 * (crates/tst-integration/tests/fixtures/scenarios/cancelled-recv-kind/golden.json):
 * one cancelled plain SRT receive surfaces the golden's kind name and detail.
 * The other legs are the Rust/Python/C scenario adapters, which iterate the
 * manifest; the JVM adapter reads goldens by id, hence this dedicated test.
 */
final class CancelledRecvKindGoldenTest {
    private static final String SCENARIO_ID = "cancelled-recv-kind";
    private static final int LATENCY_MS = 120;

    /** Workspace-relative shared scenario dir; resolved from Gradle's user.dir (bindings/jvm). */
    private static Path goldenPath() {
        return Path.of(System.getProperty("user.dir"), "..", "..",
                "crates/tst-integration/tests/fixtures/scenarios", SCENARIO_ID, "golden.json")
            .normalize();
    }

    /**
     * Read one string-valued key out of the committed golden.
     *
     * <p>String extraction rather than a JSON parser, matching
     * {@link ScenarioReproductionTest}'s {@code extractVideoEvent} /
     * {@code extractKlvEvent}: the JVM binding's test classpath is JUnit only
     * (see {@code build.gradle.kts}), and pulling Jackson or Gson into a
     * PUBLISHED artifact's build to read two fields in one test is not a
     * trade worth making. The brittleness that would matter — a schema that
     * grows a SECOND occurrence of the key, so the first match is silently
     * the wrong one — is asserted away here instead of assumed.
     */
    private static String jsonString(String json, String key) {
        Matcher m = Pattern.compile("\"" + key + "\"\\s*:\\s*\"([^\"]*)\"").matcher(json);
        assertTrue(m.find(), "golden has no \"" + key + "\": " + json);
        String value = m.group(1);
        assertFalse(m.find(),
            "golden has more than one \"" + key + "\" — this test reads the first and would "
                + "silently assert the wrong one; scope the lookup or parse properly: " + json);
        return value;
    }

    @Test
    @Timeout(60)
    void cancelledPlainRecvMatchesCommittedGolden() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        Path golden = goldenPath();
        // Skip-guard: the shared fixtures ARE committed; their absence is a hard
        // failure (the cross-binding contract relies on the single-sourced golden).
        assertTrue(Files.isRegularFile(golden),
            "shared scenario golden missing (expected committed fixture): " + golden);
        String json = Files.readString(golden, StandardCharsets.UTF_8);
        String expectedCode = jsonString(json, "code");
        String expectedDetail = jsonString(json, "detail");

        Listener listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
            .listener().listen();
        int port = listener.localAddr().port();
        CountDownLatch release = new CountDownLatch(1);
        Thread peer = new Thread(() -> {
            try (MuxSender tx = MuxSender.fromUrl(
                    "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS,
                    roundtripConfig())) {
                release.await();
            } catch (Throwable ignored) {
                // teardown races the receiver's close; benign
            }
        }, "idle-peer");
        peer.setDaemon(true);
        peer.start();

        Socket sock = listener.accept(null);
        listener.close();
        try (Receiver rx = sock.intoReceiver()) {
            CancelHandle cancel = rx.cancelHandle(); // obtained BEFORE the reader parks
            CompletableFuture<Throwable> end = new CompletableFuture<>();
            CountDownLatch entered = new CountDownLatch(1);
            Thread reader = new Thread(() -> {
                try {
                    entered.countDown();
                    rx.recvBytes();
                    end.complete(null);
                } catch (Throwable t) {
                    end.complete(t);
                }
            }, "parked-reader");
            reader.setDaemon(true);
            reader.start();
            assertTrue(entered.await(10, TimeUnit.SECONDS), "reader never started");
            Thread.sleep(300); // let recvBytes() park inside the native receive

            cancel.cancel();
            Throwable cause = end.get(10, TimeUnit.SECONDS); // FAILURE bound, never a duration assert
            assertTrue(cause instanceof SrtException, "expected SrtException, got " + cause);
            SrtException se = (SrtException) cause;
            assertEquals(expectedCode, se.kind().name(), "kind name must equal the golden's code");
            assertEquals(expectedDetail, se.getMessage(), "detail must equal the golden's");
        } finally {
            release.countDown();
        }
    }
}
