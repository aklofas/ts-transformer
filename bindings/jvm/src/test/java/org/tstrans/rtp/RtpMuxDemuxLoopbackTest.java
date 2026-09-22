package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.freeUdpPort;
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
import org.tstrans.RtpException;
import org.tstrans.TestSupport;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.Demuxer;
import org.tstrans.mpegts.Muxer;
import org.tstrans.mpegts.MuxerConfig;

/**
 * Live cross-binding loopback parity test for the RTP convenience shells
 * {@link MuxSender} and {@link DemuxReceiver}.
 *
 * <p>Drives a complete {@code MuxSender → RTP/UDP → DemuxReceiver} round trip over
 * a real UDP socket pair (loopback on an ephemeral port) and proves four things:
 * <ol>
 *   <li>RTP is byte-faithful end-to-end through the high-level shells.</li>
 *   <li>Cross-binding SHA parity — self-validating, no committed golden: the
 *       expected SHA is the first Video sample's NAL-payload digest from an
 *       OFFLINE {@link Muxer}→{@link Demuxer} run of the identical config + pushes;
 *       the live SHA must equal it.</li>
 *   <li>The byte-sink fan-out delivers raw 188-byte TS packets before demux.</li>
 *   <li>Private-data streams round-trip byte-faithfully: a raw-{@code stream_type}
 *       0xF0 data stream's records (pushed via the lone-data-stream
 *       {@code pushData} shorthand) must surface as
 *       {@code DemuxEvent.UnknownSample}s carrying the configured stream_type
 *       and the verbatim payload (pass-through — no AU-cell framing).</li>
 * </ol>
 *
 * <h2>RTP-specific topology</h2>
 * UDP is connectionless: a sender close does NOT end the receiver's iteration (it
 * parks on the next datagram). So the receiver thread breaks once the first typed
 * Video event and the first private-data {@code UnknownSample} have both
 * arrived, and the {@link DemuxReceiver} is constructed on the MAIN thread so
 * a watchdog can {@link DemuxReceiver#close()} it cross-thread on the failure path
 * (the rtp convenience wrapper exposes no {@code cancelHandle}; {@code close()}
 * cancels the parked recv first, then frees — safe via the inner mutex). The
 * discover-then-rebind free-UDP-port pattern is safe (no SRT-style linger).
 * Linux-gated (real sockets), mirroring the Rust {@code #![cfg(target_os =
 * "linux")]} gate.
 */
class RtpMuxDemuxLoopbackTest {

    private static final int TIMEOUT_SEC = 15;
    private static final int PUSH_COUNT = 24;

    /**
     * Distinctive private-data record pushed every iteration alongside the video
     * stream (identical records — the UDP loss margin; see the sender loop).
     */
    private static final byte[] DATA_PAYLOAD =
        {(byte) 0xD1, 'D', 'A', 'T', 'A', (byte) 0xCA, (byte) 0xFE, 0x02};

    /** Video + one private-data stream — see {@link TestSupport#roundtripConfigWithData()}. */
    private static MuxerConfig roundtripConfig() {
        return TestSupport.roundtripConfigWithData();
    }

    @Test
    void muxSenderToDemuxReceiverIsByteFaithfulAndCrossBindingConsistent() throws Exception {
        assumeTrue(isLinux(),
            "RTP live-socket loopback gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");

        String offlineSha = offlineMuxDemuxSha();

        int port = freeUdpPort();

        // Construct the receiver on MAIN (UDP bind does not block) so the watchdog
        // can close() it cross-thread. Register the byte sink before iterating.
        AtomicInteger sinkCount = new AtomicInteger();
        ConcurrentLinkedQueue<Integer> sinkLens = new ConcurrentLinkedQueue<>();
        DemuxReceiver rx = DemuxReceiver.fromUrl("rtp://127.0.0.1:" + port);
        rx.addByteSink(pkt -> {
            sinkCount.incrementAndGet();
            sinkLens.add(pkt.length);
        });

        CompletableFuture<String> shaFuture = new CompletableFuture<>();
        CompletableFuture<DemuxEvent.UnknownSample> dataFuture = new CompletableFuture<>();
        Thread receiverThread = new Thread(() -> {
            try {
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
                    // The iterator wraps checked RtpException/DemuxException. A
                    // CLOSED RtpException = the watchdog/teardown close() fired.
                    Throwable cause = re.getCause();
                    if (cause instanceof RtpException rex
                            && rex.kind() == RtpException.Kind.CLOSED) {
                        // fall through: sha may still be null → fail below
                    } else {
                        throw re;
                    }
                }
                if (sha == null) {
                    shaFuture.completeExceptionally(
                        new AssertionError("no typed Video event arrived before end-of-stream"));
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
                shaFuture.completeExceptionally(ex);
                dataFuture.completeExceptionally(ex);
            }
        });
        receiverThread.setDaemon(true);
        receiverThread.start();

        // Watchdog: close() the receiver after the ceiling so a parked next() can
        // never wedge the gating runners. close() cancels-first then frees (safe).
        Thread watchdog = new Thread(() -> {
            try {
                Thread.sleep(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC - 3));
                if (!shaFuture.isDone()) {
                    rx.close();
                }
            } catch (InterruptedException ignored) {
                // Happy path: interrupted by the finally before the ceiling.
            }
        });
        watchdog.setDaemon(true);
        watchdog.start();

        // Sender on the main thread: push PUSH_COUNT IDRs, pause for delivery,
        // then close. (The receiver is already bound, so kernel UDP buffers hold
        // datagrams until the daemon drains them.)
        try (MuxSender s = MuxSender.fromUrl("rtp://127.0.0.1:" + port, roundtripConfig())) {
            for (int i = 0; i < PUSH_COUNT; i++) {
                s.sendVideo(syntheticH264Idr(), i * 3000L, true);
                // A private-data record EVERY iteration (the lone-data-stream
                // sendData shorthand): RTP/UDP may drop datagrams, so a single
                // record would make the UnknownSample assertion flaky on loss.
                // Repeating an identical record gives the data stream the same
                // loss margin as the video sends — the first captured sample
                // still equals DATA_PAYLOAD. Explicit PES length means the
                // demuxer emits each record without waiting for a flush.
                s.sendData(DATA_PAYLOAD, i * 3000L);
            }
            Thread.sleep(1_000);
        }

        String liveSha;
        try {
            liveSha = shaFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
        } catch (Exception e) {
            throw new AssertionError("receiver thread failed to complete", e);
        } finally {
            // The watchdog is no longer needed; interrupt it so it doesn't wake
            // at the ceiling and close a receiver we're about to free.
            watchdog.interrupt();
            // Trigger termination of any still-parked recv (the error/timeout path)
            // so the daemon thread can exit, then join it BEFORE the final close so
            // there is provably no concurrent native call (nNext) on the receiver
            // when we free it. close() cancels the parked recv first, then frees —
            // it's the only safe cross-thread stop here (no cancelHandle on the rtp
            // convenience wrapper) and is idempotent, so this is a no-op on the
            // happy path (daemon already broke + finished) and on the watchdog path
            // (already closed). Bounded join so a wedged recv can't hang the test.
            if (!shaFuture.isDone()) {
                rx.close();
            }
            receiverThread.join(TimeUnit.SECONDS.toMillis(5));
            // Only free the receiver once its thread has provably stopped. If a
            // wedged recv somehow survived the close+join, closing here would free
            // the native handle under an in-flight nNext (UAF). Leak it instead —
            // the test is already failing on that path. close() is idempotent, so a
            // second close after a provably-stopped daemon is harmless.
            if (receiverThread.isAlive()) {
                System.err.println(
                    "WARN: receiver thread still alive after close+join; skipping close to avoid UAF");
            } else {
                rx.close();
            }
        }

        assertEquals(offlineSha, liveSha,
            "live MuxSender→RTP→DemuxReceiver path must demux to the same video "
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

        assertTrue(sinkCount.get() >= 1,
            "byte sink must have observed at least one TS packet before demux");
        assertTrue(sinkLens.stream().anyMatch(len -> len == 188),
            "byte sink must have observed at least one raw 188-byte TS packet ahead of the demuxer");
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    private static byte[] muxToBytes() throws Exception {
        ByteArrayOutputStream acc = new ByteArrayOutputStream();
        byte[] out = new byte[8192];
        try (Muxer m = new Muxer(roundtripConfig())) {
            for (int i = 0; i < PUSH_COUNT; i++) {
                m.pushVideo(syntheticH264Idr(), i * 3000L, true);
                // Mirror the live sender's push sequence exactly (incl. the
                // per-iteration private-data record) so live ≡ offline holds.
                m.pushData(DATA_PAYLOAD, i * 3000L);
            }
            int n;
            while ((n = m.pull(out)) > 0) acc.write(out, 0, n);
        }
        return acc.toByteArray();
    }

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
