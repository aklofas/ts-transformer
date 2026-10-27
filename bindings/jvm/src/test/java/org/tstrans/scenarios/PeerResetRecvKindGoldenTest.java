package org.tstrans.scenarios;

import static org.junit.jupiter.api.Assertions.*;

import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.TcpException;
import org.tstrans.shells.CancelHandle;
import org.tstrans.shells.Receiver;

/**
 * JVM leg of the {@code peer-reset-recv-kind} cross-binding golden
 * (crates/tst-integration/tests/fixtures/scenarios/peer-reset-recv-kind/golden.json):
 * a plain TCP receive whose peer writes one packet and then resets the
 * connection surfaces the golden's kind name. The other legs are the
 * Rust/Python/C scenario adapters, which iterate the manifest; the JVM adapter
 * reads goldens by id, hence this dedicated test.
 */
final class PeerResetRecvKindGoldenTest {
    private static final String SCENARIO_ID = "peer-reset-recv-kind";

    /** Workspace-relative shared scenario file; resolved from Gradle's user.dir (bindings/jvm). */
    private static Path scenarioFile(String name) {
        return Path.of(System.getProperty("user.dir"), "..", "..",
                "crates/tst-integration/tests/fixtures/scenarios", SCENARIO_ID, name)
            .normalize();
    }

    /** The one {@code "code"} of the committed golden; see {@code CancelledRecvKindGoldenTest.jsonString}. */
    private static String goldenCode(String json) {
        Matcher m = Pattern.compile("\"code\"\\s*:\\s*\"([^\"]*)\"").matcher(json);
        assertTrue(m.find(), "golden has no \"code\": " + json);
        String value = m.group(1);
        assertFalse(m.find(), "golden has more than one \"code\": " + json);
        return value;
    }

    /** 10 s stretched by {@code TST_TEST_TIMEOUT_SCALE} (a whole number, default 1). */
    private static long deadlineSeconds() {
        long scale = 1;
        String v = System.getenv("TST_TEST_TIMEOUT_SCALE");
        if (v != null) {
            try {
                scale = Math.max(1, Long.parseLong(v.trim()));
            } catch (NumberFormatException ignored) {
                // unset semantics, as the Rust helper
            }
        }
        return 10 * scale;
    }

    @Test
    @Timeout(120)
    void peerResetPlainRecvMatchesCommittedGolden() throws Exception {
        Path golden = scenarioFile("golden.json");
        Path input = scenarioFile("input.bin");
        // The shared fixtures ARE committed; their absence is a hard failure.
        assertTrue(Files.isRegularFile(golden), "shared scenario golden missing: " + golden);
        assertTrue(Files.isRegularFile(input), "shared scenario input missing: " + input);
        String expectedCode = goldenCode(Files.readString(golden, StandardCharsets.UTF_8));
        byte[] packet = Files.readAllBytes(input);

        try (ServerSocket server = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
                Receiver r = Receiver.open("tcp://127.0.0.1:" + server.getLocalPort())) {
            try (Socket conn = server.accept()) {
                OutputStream out = conn.getOutputStream();
                out.write(packet);
                out.flush();
                conn.setSoLinger(true, 0); // the close below sends an RST
            }

            CancelHandle cancel = r.cancelHandle(); // obtained BEFORE the reader parks
            CompletableFuture<Throwable> end = new CompletableFuture<>();
            Thread reader = new Thread(() -> {
                try {
                    while (true) {
                        r.recvBytes();
                    }
                } catch (Throwable t) {
                    end.complete(t);
                }
            }, "peer-reset-reader");
            reader.setDaemon(true);
            reader.start();

            Throwable cause;
            try {
                cause = end.get(deadlineSeconds(), TimeUnit.SECONDS); // FAILURE bound
            } catch (TimeoutException e) {
                cancel.cancel();
                throw new AssertionError("the receive did not fail within the deadline after the reset", e);
            }
            assertTrue(cause instanceof TcpException, "expected TcpException, got " + cause);
            assertEquals(expectedCode, ((TcpException) cause).kind().name(),
                "kind name must equal the golden's code");
        }
    }
}
