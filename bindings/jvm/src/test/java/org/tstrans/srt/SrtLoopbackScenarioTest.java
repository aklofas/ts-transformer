package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.scenarioDir;
import static org.tstrans.TestSupport.sha256OfDemuxedVideoUnits;
import static org.tstrans.TestSupport.sha256Units;

import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.tstrans.GoldenSupport;
import org.tstrans.SrtException;
import org.tstrans.shells.Sender;
import org.tstrans.shells.Receiver;

/**
 * Cross-binding loopback parity test for the JVM SRT surface.
 *
 * <p>Reproduces the {@code h264-st0601-mp} committed scenario over a real SRT
 * socket pair (loopback on an ephemeral port), then demuxes the received bytes
 * and asserts the same {@code video.payload_sha256} the in-memory feed and file
 * paths produce. This is the JVM analogue of the Rust
 * {@code tst-srt/tests/pipeline/pipeline_receiver_live.rs} test.
 *
 * <h2>What this proves</h2>
 * <ul>
 *   <li><b>SRT is byte-faithful.</b> The first {@code input.length} bytes of
 *       the received payload must equal the scenario's {@code input.ts} bytes
 *       exactly.</li>
 *   <li><b>Demux is parity-correct.</b> The scenario input bytes (recovered from
 *       the first {@code input.length} bytes of the SRT payload) produce the
 *       same {@code video.payload_sha256} as the committed golden — the same
 *       hash the Rust and Python cross-binding adapters assert.</li>
 * </ul>
 *
 * <h2>Send payload padding</h2>
 * The scenario {@code input.ts} is exactly 4 × 188 = 752 bytes. The JVM
 * pipeline receiver ({@code tst_pipeline::Receiver}) contains a TS-sync
 * verification window that requires at least {@code 4 × 188 + 1 = 753} bytes
 * before it can lock on sync and emit packets. To satisfy this constraint and
 * also trigger the sender-side framer to emit a full SRT bundle (which requires
 * exactly {@code 7 × 188 = 1316} bytes), the test pads the payload to
 * {@code 7 × 188 = 1316} bytes with three MPEG-TS null/stuffing packets
 * (PID 0x1FFF, header {@code 0x47 0x1F 0xFF 0x10}). The demuxer drops PID
 * 0x1FFF entirely, so these packets never affect the byte-faithful or SHA
 * assertions — both operate on the first {@code input.length} real bytes only.
 *
 * <h2>Platform gate</h2>
 * The test opens real SRT sockets and is gated to Linux only via
 * {@link org.junit.jupiter.api.Assumptions#assumeTrue}. On macOS and Windows
 * the test is skipped (not failed) — identical to the Rust
 * {@code #![cfg(target_os = "linux")]} gate on the Rust live-socket tests.
 *
 * <h2>Accept discipline</h2>
 * The listener uses {@link Listener#accept(Integer) accept(null)} (infinite
 * wait via {@code srt_accept} directly) rather than {@code accept(timeout)}
 * (epoll-based). This avoids a subtle interaction where the epoll subscription
 * set up by {@code accept_timeout} could interfere with the subsequent
 * {@code srt_recv} epoll on the accepted socket.
 */
class SrtLoopbackScenarioTest {

    private static final String SCENARIO_ID = "h264-st0601-mp";

    /** Max SRT payload chunk when sending. One 1316-byte SRT live-mode message = 7 × 188-byte TS packets. */
    private static final int SRT_CHUNK = 1316;

    /** Timeout in seconds for inter-thread signalling and overall test completion. */
    private static final int TIMEOUT_SEC = 15;

    /**
     * SRT latency in milliseconds for both sides of the loopback pair.
     * 120 ms matches the Rust live test's {@code recv_latency} setting and
     * provides sufficient margin above loopback RTT (~0 ms).
     */
    private static final int LATENCY_MS = 120;

    /**
     * Build a 1316-byte send payload: the scenario {@code input.ts} (752 bytes)
     * followed by three MPEG-TS null/stuffing packets (PID 0x1FFF) to reach
     * exactly {@code 7 × 188 = 1316} bytes.
     *
     * <p>Why 1316 bytes? Two reasons:
     * <ol>
     *   <li>The {@code tst_pipeline::Sender} framer emits SRT bundles when the
     *       internal buffer reaches {@code SRT_TS_BUNDLE_BYTES = 7 × 188 = 1316}
     *       bytes. Sending exactly 1316 bytes triggers emission during
     *       {@code sendBytes} rather than requiring a {@code flush()} call.</li>
     *   <li>The {@code tst_pipeline::Receiver} sync window needs at least
     *       {@code 4 × 188 + 1 = 753} bytes to lock on TS alignment. With a full
     *       1316-byte SRT message the sync window is always satisfied on the
     *       first {@code srt_recv} call.</li>
     * </ol>
     *
     * <p>The three padding packets use the standard MPEG-TS null packet header
     * {@code 0x47, 0x1F, 0xFF, 0x10} (sync byte; PID=0x1FFF the null PID;
     * TSC=00, AFC=01 payload-only, CC=0), with the remaining 184 payload bytes
     * set to zero. The demuxer drops PID 0x1FFF entirely, so these packets
     * never produce any event and do not affect the byte-faithful or SHA
     * assertions (both operate on the first {@code input.length} real bytes only).
     */
    private static byte[] buildSendPayload(byte[] input) {
        // input is 4 × 188 = 752 bytes; pad to 7 × 188 = 1316 with MPEG-TS null packets
        byte[] payload = new byte[SRT_CHUNK];
        System.arraycopy(input, 0, payload, 0, input.length);
        // MPEG-TS null packet header per ISO 13818-1:
        //   0x47        sync byte
        //   0x1F, 0xFF  transport_error=0, payload_unit_start=0, transport_priority=0, PID=0x1FFF (null PID)
        //   0x10        TSC=00 (not scrambled), AFC=01 (payload only), CC=0
        // Remaining 184 bytes are 0x00 (stuffing); the demuxer drops PID 0x1FFF entirely.
        for (int i = input.length; i < SRT_CHUNK; i += 188) {
            payload[i]     = 0x47; // sync byte
            payload[i + 1] = 0x1F; // PID high byte (null PID 0x1FFF)
            payload[i + 2] = (byte) 0xFF; // PID low byte
            payload[i + 3] = 0x10; // AFC=01 (payload only), CC=0
            // bytes [i+4..i+187] = 0x00 (null packet stuffing payload)
        }
        return payload;
    }

    @Test
    void srtLoopbackReproducesH264St0601MpGolden() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket loopback gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");

        Path dir = scenarioDir(SCENARIO_ID);
        Path inputPath  = dir.resolve("input.ts");
        Path goldenPath = dir.resolve("golden.json");

        assertTrue(Files.isRegularFile(inputPath),
            "shared scenario input missing (expected committed fixture): " + inputPath);
        assertTrue(Files.isRegularFile(goldenPath),
            "shared scenario golden missing (expected committed fixture): " + goldenPath);

        byte[] input = Files.readAllBytes(inputPath);
        byte[] sendPayload = buildSendPayload(input);  // 1316 bytes (input + 3 PID-0x1FFF null packets)
        String goldenJson = Files.readString(goldenPath, StandardCharsets.UTF_8);
        String expectedSha = GoldenSupport.stringField(GoldenSupport.videoObject(goldenJson), "payload_sha256");

        // ── receiver thread ────────────────────────────────────────────────
        //
        // Listener side: bind to ephemeral port (:0), publish the assigned
        // port to the sender thread via a CompletableFuture, accept the first
        // incoming SRT connection (one-shot), then drain recvBytes() packets
        // until we have sendPayload.length bytes or the transport closes.
        //
        // Uses accept(null) (= srt_accept directly, infinite wait) rather than
        // accept(timeout) (epoll-based) to avoid an epoll-subscription
        // interaction that prevents srt_recv from waking on TSBPD delivery.
        //
        // The Builder→Listener→accept(null)→intoReceiver path exercises the
        // full low-level Task-3 primitive chain in a single test.
        CompletableFuture<Integer> portFuture     = new CompletableFuture<>();
        CompletableFuture<byte[]>  receivedFuture = new CompletableFuture<>();

        Thread receiverThread = new Thread(() -> {
            Listener listener = null;
            Receiver receiver = null;
            try {
                // Bind to kernel-assigned port (":0"); latency matches the
                // Rust live test's recv_latency setting.
                listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
                    .listener()
                    .listen();

                // Publish the actual port to the sender thread before blocking
                // on accept — the sender must not call Sender.fromUrl until after
                // this completes, or the handshake would occur before we're ready.
                int port = listener.localAddr().port();
                portFuture.complete(port);

                // Accept the first (only) incoming connection. Use null (infinite
                // wait = srt_accept directly) so the accepted socket's recv epoll
                // is not perturbed by the accept_timeout epoll set.
                Socket sock = listener.accept(null);
                receiver = sock.intoReceiver();

                // Drain until we have sendPayload.length bytes or the transport
                // closes (BROKEN / CLOSED — peer hangup after the sender's
                // 1-second drain pause + close). Each recvBytes() returns one
                // 188-byte TS packet; we need sendPayload.length / 188 = 7
                // packets to reconstruct the full send payload.
                ByteArrayOutputStream buf = new ByteArrayOutputStream(sendPayload.length);
                while (buf.size() < sendPayload.length) {
                    byte[] pkt;
                    try {
                        pkt = receiver.recvBytes();
                    } catch (SrtException e) {
                        // Peer closed or broken — treat as end-of-stream.
                        // Same dual-path logic as pipeline_receiver_live.rs:
                        // either Closed (graceful) or Broken (peer hangup via
                        // srt_close) is acceptable.
                        if (e.kind() == SrtException.Kind.CLOSED
                                || e.kind() == SrtException.Kind.BROKEN) {
                            break;
                        }
                        receivedFuture.completeExceptionally(e);
                        return;
                    }
                    buf.write(pkt);
                }
                receivedFuture.complete(buf.toByteArray());

            } catch (Exception ex) {
                portFuture.completeExceptionally(ex);
                receivedFuture.completeExceptionally(ex);
            } finally {
                if (receiver != null) receiver.close();
                if (listener != null) listener.close();
            }
        });
        receiverThread.setDaemon(true);
        receiverThread.start();

        // ── sender (main thread) ───────────────────────────────────────────
        //
        // Wait for the receiver to publish its ephemeral port, then connect
        // in caller mode and send sendPayload in ≤1316-byte chunks.

        int port;
        try {
            port = portFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
        } catch (Exception e) {
            receiverThread.interrupt();
            throw new AssertionError("receiver thread failed to publish port", e);
        }

        try (Sender sender = Sender.open(
                "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS)) {
            // sendPayload is exactly SRT_CHUNK = 1316 bytes (one full SRT live-mode
            // bundle). The TS framer emits this as a single srt_send call during
            // sendBytes (no flush needed), and the Syncer on the receive side sees
            // enough data to lock sync immediately.
            sender.sendBytes(sendPayload);
            sender.flush(); // flush any remaining partial bundle (none in this case)
            // SRT's TSBPD latency window buffers data before delivering it to
            // the receiver. Pause before close so the receiver's drain loop has
            // time to receive all packets at LATENCY_MS TSBPD latency.
            // Mirrors the 1-second drain pause in the Rust live test.
            Thread.sleep(1_000);
        }
        // Sender closed; the receiver's drain loop will see BROKEN/CLOSED.

        // ── collect + assert ───────────────────────────────────────────────

        byte[] received;
        try {
            received = receivedFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
        } catch (Exception e) {
            throw new AssertionError("receiver thread failed to complete", e);
        }

        // SRT must be byte-faithful — the first input.length bytes of the received
        // payload must equal the scenario's input.ts exactly.
        // (The received payload may be longer if padding TS packets arrived too;
        // we compare only the meaningful prefix.)
        assertTrue(received.length >= input.length,
            "SRT loopback: received " + received.length + " bytes but expected at least "
                + input.length + " (the input.ts length); transport appears to have lost data");
        assertArrayEquals(input, Arrays.copyOf(received, input.length),
            "SRT loopback must be byte-faithful: first " + input.length
                + " received bytes must equal input.ts exactly");

        // Demux the first input.length bytes (the actual scenario content, without
        // the null-TS padding) and assert the same golden payload_sha256 the
        // in-memory feed path and the file path produce. This is the
        // cross-binding parity proof: the JVM SRT path produces the same hash
        // as Rust and Python for the same scenario.
        byte[] scenarioReceived = Arrays.copyOf(received, input.length);
        String actualSha = sha256OfDemuxedVideoUnits(scenarioReceived);
        assertEquals(expectedSha, actualSha,
            "SRT-received scenario bytes must demux to the same payload_sha256 as the committed "
                + "golden (cross-binding parity proof: JVM SRT path ≡ Rust/Python in-memory path)");

        receiverThread.join(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC));
    }
}
