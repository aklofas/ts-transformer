package org.tstrans.scenarios;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.scaledMs;
import static org.tstrans.TestSupport.scenarioDir;

import java.io.OutputStream;
import java.lang.reflect.Field;
import java.lang.reflect.Modifier;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Map;
import java.util.TreeMap;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import org.tstrans.GoldenSupport;
import org.tstrans.SrtException;
import org.tstrans.shells.CancelHandle;
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.shells.DemuxReceiverStats;
import org.tstrans.srt.Builder;
import org.tstrans.srt.ListenRule;
import org.tstrans.srt.Listener;
import org.tstrans.srt.RejectReason;

/**
 * The JVM legs of the binding_contract scenarios whose golden carries observed values in
 * {@code extensions}. {@link ScenarioReproductionTest} runs each one for its id, so the
 * golden is compared on the JVM, not only declared.
 */
final class ScenarioContractLegs {
    private ScenarioContractLegs() {}

    /** The receive deadline: a failing bound, never a pacing sleep. */
    private static final long DEADLINE_MS = 10_000;

    /**
     * {@code demux-receiver-stats}: the committed input through a TCP {@link DemuxReceiver}
     * whose peer writes it and closes normally; at the end of the stream the six counters
     * equal the golden's {@code extensions.stats}.
     */
    static void demuxReceiverStats() throws Exception {
        Path dir = scenarioDir("demux-receiver-stats");
        byte[] input = Files.readAllBytes(dir.resolve("input.ts"));
        String golden = Files.readString(dir.resolve("golden.json"), StandardCharsets.UTF_8);
        assertEquals("demux_receiver_stats", GoldenSupport.stringField(golden, "contract"));
        assertEquals("[]", GoldenSupport.arrayField(golden, "core").replaceAll("\\s", ""),
            "the receive leg observes no core event");
        String expected = GoldenSupport.objectField(golden, "stats");

        DemuxReceiverStats stats;
        try (ServerSocket server = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
                DemuxReceiver r = DemuxReceiver.open("tcp://127.0.0.1:" + server.getLocalPort());
                CancelHandle cancel = r.cancelHandle()) { // obtained BEFORE the reader parks
            try (Socket conn = server.accept()) {
                OutputStream out = conn.getOutputStream();
                out.write(input);
                out.flush();
            } // a normal close: the receiver reads a FIN

            CompletableFuture<Throwable> end = new CompletableFuture<>();
            Thread reader = new Thread(() -> {
                try {
                    while (r.recvEvent() != null) {
                        // drain every event; the counters are the point
                    }
                    end.complete(null);
                } catch (Throwable t) {
                    end.complete(t);
                }
            }, "demux-receiver-stats-reader");
            reader.setDaemon(true);
            reader.start();

            Throwable failed;
            try {
                failed = end.get(scaledMs(DEADLINE_MS), TimeUnit.MILLISECONDS);
            } catch (TimeoutException e) {
                cancel.cancel();
                throw new AssertionError("the receive did not reach the end of the stream within the deadline", e);
            }
            if (failed != null) {
                throw new AssertionError("the receive ended with an error, not at the end of the stream", failed);
            }
            stats = r.stats();
        }

        Map<String, Long> observed = new TreeMap<>(Map.of(
            "bytes_received", stats.bytesReceived(),
            "packets_received", stats.packetsReceived(),
            "program_maps_seen", stats.programMapsSeen(),
            "pmt_versions_seen", stats.pmtVersionsSeen(),
            "discontinuities", stats.discontinuities(),
            "nonconformant", stats.nonconformant()));
        Map<String, Long> committed = new TreeMap<>();
        Matcher field = Pattern.compile("\"([a-z_]+)\"\\s*:\\s*(-?\\d+)").matcher(expected);
        while (field.find()) committed.put(field.group(1), Long.parseLong(field.group(2)));
        assertEquals(committed, observed, "DemuxReceiver.stats() differs from the golden's extensions.stats");
    }

    /**
     * {@code srt-reject-reason}: every row of the golden's {@code extensions.reject_reasons}
     * names its JVM {@link RejectReason} and the JVM has no constant the table lacks; then a
     * caller refused by a listener rule reads the golden's kind and reason.
     */
    static void srtRejectReason() throws Exception {
        Path dir = scenarioDir("srt-reject-reason");
        String golden = Files.readString(dir.resolve("golden.json"), StandardCharsets.UTF_8);
        String streamId = Files.readString(dir.resolve("input.bin"), StandardCharsets.UTF_8);
        assertEquals("srt_reject_reason", GoldenSupport.stringField(golden, "contract"));

        Map<Integer, String> table = new TreeMap<>();
        Matcher row = Pattern.compile("\\{[^{}]*\\}").matcher(GoldenSupport.arrayField(golden, "reject_reasons"));
        while (row.find()) {
            int code = (int) GoldenSupport.signedNumber(row.group(), "code");
            String name = GoldenSupport.stringField(row.group(), "name");
            assertNull(table.put(code, name), "the golden's table lists code " + code + " twice");
            assertEquals(name, RejectReason.ofCode(code).name(),
                "RejectReason.ofCode(" + code + ") does not carry the golden's name");
        }
        Map<Integer, String> constants = new TreeMap<>();
        for (Field f : RejectReason.class.getFields()) {
            if (Modifier.isStatic(f.getModifiers()) && f.getType() == RejectReason.class) {
                RejectReason r = (RejectReason) f.get(null);
                assertEquals(f.getName(), r.name(), "constant " + f.getName() + " carries another name");
                constants.put(r.code(), r.name());
            }
        }
        assertEquals(table, constants, "the JVM's RejectReason constants differ from the golden's table");

        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        String core = GoldenSupport.eventObject(golden, "error");
        String reason = GoldenSupport.objectField(golden, "reject_reason");
        try (Listener listener = new Builder("srt://127.0.0.1:0?mode=listener").listener().listen()) {
            // The reason is fixed here, as in the other legs: read from the golden, a wrong
            // golden reason would pass.
            listener.rules().set(streamId, ListenRule.reject(RejectReason.FORBIDDEN));
            Builder caller = new Builder("srt://127.0.0.1:" + listener.localAddr().getPort())
                .caller().streamId(streamId).connectTimeoutMs((int) scaledMs(5_000));
            SrtException e = assertThrows(SrtException.class, caller::connect);
            assertEquals(GoldenSupport.stringField(core, "code"), e.kind().name(),
                "the refused connect's kind name must equal the golden's code");
            RejectReason got = e.rejectReason().orElseThrow(
                () -> new AssertionError("the refused connect carries no reject reason: " + e));
            assertEquals(GoldenSupport.signedNumber(reason, "code"), got.code(), "reject code");
            assertEquals(GoldenSupport.stringField(reason, "name"), got.name(), "reject reason name");
        }
    }
}
