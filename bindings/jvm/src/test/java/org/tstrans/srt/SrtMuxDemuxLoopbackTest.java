package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.sha256Units;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.io.ByteArrayOutputStream;
import java.nio.ByteBuffer;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Test;
import org.tstrans.SrtException;
import org.tstrans.TestSupport;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.Demuxer;
import org.tstrans.mpegts.DemuxerConfig;
import org.tstrans.mpegts.Muxer;
import org.tstrans.mpegts.MuxerConfig;

/**
 * Live cross-binding loopback parity test for the high-level SRT convenience
 * shells {@link MuxSender} and {@link DemuxReceiver}.
 *
 * <p>This test drives a complete {@code MuxSender → SRT → DemuxReceiver} round
 * trip over a real SRT socket pair (loopback on an ephemeral port) and proves
 * four things at once:
 *
 * <ol>
 *   <li><b>SRT is byte-faithful end-to-end through the high-level shells.</b>
 *       A {@code MuxSender} muxes N synthetic H.264 IDR access units into
 *       MPEG-TS and ships them through SRT; the receiving {@code DemuxReceiver}
 *       demuxes the recovered bytes back into typed {@code VideoUnit}s.</li>
 *   <li><b>Cross-binding SHA parity — self-validating, no committed golden.</b>
 *       The expected SHA is computed in the same test by running the
 *       <em>identical</em> {@link MuxerConfig} + N pushes through an OFFLINE
 *       {@link Muxer} → {@link Demuxer} path and hashing the first Video
 *       sample's NAL payloads with the same helper. The live SHA must equal the
 *       offline SHA. This proves the live SRT path is byte-faithful AND
 *       consistent with the offline path — without depending on a hardcoded
 *       golden hash (which would be brittle for a multi-push stream and would
 *       drift if the synthetic IDR or push count changed).</li>
 *   <li><b>The byte-sink fan-out delivers raw 188-byte TS packets before
 *       demux.</b> A sink registered on the {@code DemuxReceiver} before
 *       iteration records per-packet observations; the test asserts at least
 *       one packet was fanned out and at least one was exactly 188 bytes (the
 *       SRT live-mode TS quantum), proving the tee fires on the raw transport
 *       stream ahead of the demuxer.</li>
 *   <li><b>Private-data streams round-trip byte-faithfully.</b> The config
 *       declares a raw-{@code stream_type} 0xF0 data stream; the sender pushes
 *       one distinctive record via the lone-data-stream {@code pushData}
 *       shorthand and the receiver must surface it as a
 *       {@code DemuxEvent.UnknownSample} carrying the configured stream_type
 *       and the verbatim payload (pass-through — no AU-cell framing).</li>
 * </ol>
 *
 * <h2>Why self-validating instead of a committed golden</h2>
 * The sub-wave-A {@link SrtLoopbackScenarioTest} replays a single committed
 * {@code input.ts} and asserts a frozen {@code payload_sha256}. Here the send
 * side is a generative multi-push stream (N synthetic IDRs at increasing PTS),
 * so the resulting hash is a function of N and the IDR shape. Rather than freeze
 * a brittle hash, the test derives the expected value from the OFFLINE
 * Muxer→Demuxer path using the exact same inputs — the live path must match it.
 * This is a stronger statement (live ≡ offline ≡ cross-binding) and survives any
 * future tuning of N.
 *
 * <h2>Platform gate</h2>
 * The test opens real SRT sockets and is gated to Linux only via
 * {@link org.junit.jupiter.api.Assumptions#assumeTrue}. On macOS and Windows
 * the test is skipped (not failed) — identical to the Rust
 * {@code #![cfg(target_os = "linux")]} gate on the live-socket tests, and to the
 * sub-wave-A {@link SrtLoopbackScenarioTest}.
 *
 * <h2>Threading / robustness</h2>
 * The receiver runs on a daemon thread; both the port hand-off and the result
 * are exchanged via {@link CompletableFuture}s with a 15-second ceiling so a
 * hung socket can never wedge the suite. The receiver's drain loop is bounded —
 * it stops once the first typed Video event AND the first private-data
 * {@code UnknownSample} have both arrived, or at clean/transport end-of-stream —
 * and the iterator's checked exceptions (wrapped in {@code RuntimeException} per
 * the {@code Iterator} contract) are unwrapped to treat a CLOSED/BROKEN
 * {@link SrtException} as end-of-stream.
 */
class SrtMuxDemuxLoopbackTest {

    /** Timeout in seconds for inter-thread signalling and overall test completion. */
    private static final int TIMEOUT_SEC = 15;

    /** SRT latency in milliseconds for both sides (matches SrtLoopbackScenarioTest). */
    private static final int LATENCY_MS = 120;

    /**
     * Number of synthetic IDR access units to push. Chosen empirically (tuned via
     * a ×5 isolated stress run) to reliably exceed the receiver's TS-sync window
     * (≥ 4×188+1 bytes) AND flow at least one full SRT bundle (7×188 = 1316 bytes)
     * before the post-push drain pause, so the first Video event reliably arrives.
     * {@code MuxSender} has no {@code flush()} — bytes flush per-push and on close.
     */
    private static final int PUSH_COUNT = 24;

    /** Distinctive private-data record pushed once alongside the video stream. */
    private static final byte[] DATA_PAYLOAD =
        {(byte) 0xD0, 'D', 'A', 'T', 'A', (byte) 0xBE, (byte) 0xEF, 0x01};

    /** Video + one private-data stream — see {@link TestSupport#roundtripConfigWithData()}. */
    private static MuxerConfig roundtripConfig() {
        return TestSupport.roundtripConfigWithData();
    }

    @Test
    void muxSenderToDemuxReceiverIsByteFaithfulAndCrossBindingConsistent() throws Exception {
        assumeTrue(isLinux(),
            "SRT live-socket loopback gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");

        // ── expected SHA via the OFFLINE Muxer→Demuxer path (self-validating) ──
        //
        // Run the identical config + PUSH_COUNT pushes through an in-memory
        // Muxer, drain to a byte[], feed that to an offline Demuxer, and hash the
        // first Video sample's NAL payloads. This is what the live path must
        // reproduce — no committed golden needed.
        String offlineSha = offlineMuxDemuxSha();

        // ── receiver thread ────────────────────────────────────────────────
        CompletableFuture<Integer> portFuture = new CompletableFuture<>();
        CompletableFuture<String> shaFuture = new CompletableFuture<>();
        CompletableFuture<DemuxEvent.UnknownSample> dataFuture = new CompletableFuture<>();

        // Byte-sink observations (populated on the receiver thread, read on main).
        AtomicInteger sinkCount = new AtomicInteger();
        ConcurrentLinkedQueue<Integer> sinkLens = new ConcurrentLinkedQueue<>();

        Thread receiverThread = new Thread(() -> {
            Listener listener = null;
            Socket sock = null;
            DemuxReceiver rx = null;
            try {
                listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
                    .listener()
                    .listen();
                portFuture.complete(listener.localAddr().port());

                // accept(null) = srt_accept directly (infinite wait); avoids the
                // accept-timeout epoll interaction (see SrtLoopbackScenarioTest).
                sock = listener.accept(null);
                // Take the config overload with an all-defaults config: same
                // behavior as the no-arg form, but it runs the wider
                // `nIntoDemuxReceiverWithConfig` native, which otherwise has no
                // live-socket coverage anywhere in the suite.
                rx = sock.intoDemuxReceiver(DemuxerConfig.builder().build());

                // Register the byte sink BEFORE iterating: it fires per 188-byte
                // TS packet ahead of the demuxer. Keep it cheap (just record).
                rx.addByteSink(pkt -> {
                    sinkCount.incrementAndGet();
                    sinkLens.add(pkt.length);
                });

                // Drain to the first typed Video event with a non-empty payload
                // AND the first private-data UnknownSample.
                String sha = null;
                DemuxEvent.UnknownSample data = null;
                try {
                    for (DemuxEvent e : rx) {
                        if (sha == null && e instanceof DemuxEvent.Video v
                                && !v.parse().isEmpty()) {
                            sha = sha256Units(v.parse());
                        } else if (data == null && e instanceof DemuxEvent.UnknownSample u) {
                            data = u;
                        }
                        if (sha != null && data != null) break;
                    }
                } catch (RuntimeException re) {
                    // The iterator wraps checked SrtException/DemuxException in a
                    // RuntimeException. A CLOSED/BROKEN SrtException = peer hangup
                    // after the sender's drain pause + close → clean end-of-stream.
                    Throwable cause = re.getCause();
                    if (cause instanceof SrtException se
                            && (se.kind() == SrtException.Kind.CLOSED
                                || se.kind() == SrtException.Kind.BROKEN)) {
                        // fall through: sha may still be null (no Video) → fail below
                    } else {
                        throw re;
                    }
                }

                if (sha == null) {
                    shaFuture.completeExceptionally(new AssertionError(
                        "no typed Video event arrived before end-of-stream"));
                } else {
                    shaFuture.complete(sha);
                }
                if (data == null) {
                    dataFuture.completeExceptionally(new AssertionError(
                        "no UnknownSample (private-data) event arrived before end-of-stream"));
                } else {
                    dataFuture.complete(data);
                }
            } catch (Exception ex) {
                portFuture.completeExceptionally(ex);
                shaFuture.completeExceptionally(ex);
                dataFuture.completeExceptionally(ex);
            } finally {
                // sock is consumed by intoDemuxReceiver() on the success path
                // (its handle is zeroed → close() is a no-op there); closing it
                // here frees the accepted socket if an exception fired before/
                // during intoDemuxReceiver().
                if (rx != null) rx.close();
                if (sock != null) sock.close();
                if (listener != null) listener.close();
            }
        });
        receiverThread.setDaemon(true);
        receiverThread.start();

        // ── sender (main thread) ───────────────────────────────────────────
        int port;
        try {
            port = portFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
        } catch (Exception e) {
            receiverThread.interrupt();
            throw new AssertionError("receiver thread failed to publish port", e);
        }

        try (MuxSender s = MuxSender.fromUrl(
                "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS,
                roundtripConfig())) {
            for (int i = 0; i < PUSH_COUNT; i++) {
                // Increasing PTS so the muxer never rejects a duplicate timestamp.
                s.sendVideo(syntheticH264Idr(), i * 3000L, true);
                // One distinctive private-data record early in the stream (the
                // lone-data-stream sendData shorthand) so the remaining video
                // sends flush it through any transport bundling; explicit PES
                // length means the demuxer emits it without waiting for a flush.
                if (i == 0) s.sendData(DATA_PAYLOAD, 0L);
            }
            // Sender handle smoke pin: the config declares one data stream, so
            // the convenience accessor must surface it (exercises the native +
            // sentinel path on a live sender).
            assertTrue(s.dataHandle().isPresent());
            // SRT TSBPD buffers before delivery; pause before close so the
            // receiver drains everything (mirrors the Rust live test's 1s pause).
            Thread.sleep(1_000);
        }

        // ── collect + assert ───────────────────────────────────────────────
        String liveSha;
        try {
            liveSha = shaFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
        } catch (Exception e) {
            throw new AssertionError("receiver thread failed to complete", e);
        }

        assertEquals(offlineSha, liveSha,
            "live MuxSender→SRT→DemuxReceiver path must demux to the same video "
                + "payload SHA as the offline Muxer→Demuxer path (cross-binding parity, "
                + "self-validating)");

        // The pushData record must round-trip byte-faithfully as an UnknownSample
        // on the configured 0xF0 data stream.
        DemuxEvent.UnknownSample dataSample;
        try {
            dataSample = dataFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
        } catch (Exception e) {
            throw new AssertionError("receiver thread did not surface the private-data sample", e);
        }
        assertEquals(0xF0, dataSample.streamType(),
            "private-data sample must carry the configured raw stream_type");
        ByteBuffer dataView = dataSample.payload().duplicate();
        byte[] dataBytes = new byte[dataView.remaining()];
        dataView.get(dataBytes);
        assertArrayEquals(DATA_PAYLOAD, dataBytes,
            "private-data payload must arrive verbatim (pass-through, no framing)");

        // Byte-sink fan-out: at least one packet, at least one exactly 188 bytes.
        assertTrue(sinkCount.get() >= 1,
            "byte sink must have observed at least one TS packet before demux");
        assertTrue(sinkLens.stream().anyMatch(len -> len == 188),
            "byte sink must have observed at least one raw 188-byte TS packet "
                + "(the SRT live-mode quantum) ahead of the demuxer");

        receiverThread.join(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC));
    }

    // NOTE: no separate live "throwing byte sink" test. The fail-loud throw path
    // (a sink exception is captured first-wins and re-raised from the next
    // iteration, stopping it) is wired structurally in DemuxReceiver/addByteSink
    // and is not feasible to assert here without live-timing fragility:
    //   - addByteSink exists only on the SRT DemuxReceiver, not the offline
    //     Demuxer, so the throw path cannot be exercised against a deterministic
    //     offline demuxer; and
    //   - a live throwing-sink test would race the TSBPD delivery window (the
    //     sink must fire before the iterator returns), which is exactly the kind
    //     of flake the ×5 stress gate exists to prevent.
    // A flaky live test is worse than none, so the throw path is left to its
    // structural wiring rather than a fragile live assertion.

    // ── helpers ─────────────────────────────────────────────────────────────

    /** Mux PUSH_COUNT synthetic IDRs at increasing PTS and return the TS bytes. */
    private static byte[] muxToBytes() throws Exception {
        ByteArrayOutputStream acc = new ByteArrayOutputStream();
        byte[] out = new byte[8192];
        try (Muxer m = new Muxer(roundtripConfig())) {
            for (int i = 0; i < PUSH_COUNT; i++) {
                m.pushVideo(syntheticH264Idr(), i * 3000L, true);
                // Mirror the live sender's push sequence exactly (incl. the
                // single private-data record) so live ≡ offline holds.
                if (i == 0) m.pushData(DATA_PAYLOAD, 0L);
            }
            int n;
            while ((n = m.pull(out)) > 0) {
                acc.write(out, 0, n);
            }
        }
        return acc.toByteArray();
    }

    /** Offline Muxer→Demuxer SHA of the first Video sample's NAL payloads. */
    private static String offlineMuxDemuxSha() throws Exception {
        byte[] ts = muxToBytes();
        try (Demuxer d = new Demuxer()) {
            d.feed(ts);
            d.flush();
            for (DemuxEvent e : d) {
                if (e instanceof DemuxEvent.Video v && !v.parse().isEmpty()) {
                    return sha256Units(v.parse());
                }
            }
        }
        throw new AssertionError("offline path produced no typed Video event");
    }

}
