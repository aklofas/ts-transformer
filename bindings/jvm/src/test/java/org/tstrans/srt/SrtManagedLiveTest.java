package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.sha256Units;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.io.ByteArrayOutputStream;
import java.nio.ByteBuffer;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.tstrans.SrtException;
import org.tstrans.TestSupport;
import org.tstrans.mpegts.DataStreamHandle;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.Demuxer;
import org.tstrans.mpegts.Muxer;
import org.tstrans.mpegts.MuxerConfig;

/**
 * Live-socket happy-path + stats-drift parity tests for the four managed
 * (auto-reconnect) SRT shells — {@link ManagedSender}, {@link ManagedReceiver},
 * {@link ManagedMuxSender}, and {@link ManagedDemuxReceiver} (sub-wave C).
 *
 * <p>This is the live-socket companion to the socket-free {@link SrtManagedTest}.
 * It opens real SRT socket pairs over loopback on ephemeral ports and proves two
 * classes of behaviour <em>without</em> exercising reconnect (reconnect is a
 * separate task):
 *
 * <ol>
 *   <li><b>End-to-end happy path.</b> A {@link ManagedMuxSender} muxes synthetic
 *       H.264 IDR access units into MPEG-TS and ships them over SRT to a plain
 *       {@code DemuxReceiver} peer; the recovered video payload's SHA-256 must
 *       equal the OFFLINE {@code Muxer → Demuxer} SHA computed in-test from the
 *       same inputs (self-validating cross-binding parity, no committed golden —
 *       same pattern as {@link SrtMuxDemuxLoopbackTest}).</li>
 *   <li><b>The three documented stats drifts.</b>
 *       <ul>
 *         <li>{@link ManagedSender#srtStats()} ALWAYS throws
 *             {@code SrtException(IO)}; {@link ManagedSender#socketStats()} works.</li>
 *         <li>{@link ManagedReceiver#srtStats()} ALWAYS throws
 *             {@code SrtException(IO)}.</li>
 *         <li>{@link ManagedDemuxReceiver#srtStats()} RETURNS a
 *             {@link SocketStats} (NOT {@code SrtStats}) and does NOT throw —
 *             the same value as {@link ManagedDemuxReceiver#socketStats()}.</li>
 *       </ul></li>
 * </ol>
 *
 * <h2>Platform gate</h2>
 * Every test opens real SRT sockets and is gated to Linux only via
 * {@link org.junit.jupiter.api.Assumptions#assumeTrue} — identical to the Rust
 * {@code #![cfg(target_os = "linux")]} gate and to {@link SrtMuxDemuxLoopbackTest}.
 *
 * <h2>Threading / robustness</h2>
 * Receivers run on daemon threads. Port hand-off and results travel via
 * {@link CompletableFuture}s / {@link CountDownLatch}es with a 15-second ceiling
 * so a hung socket can never wedge the suite. The managed-listener tests use the
 * sanctioned discover-then-reuse ephemeral-port pattern (bind a throwaway plain
 * listener on {@code :0}, read its port, close it, reuse that exact port). Where
 * the managed shell iterates/blocks on its own thread, all stats reads and the
 * drift assertions are captured <em>on that same thread</em> (into
 * {@link AtomicReference}/{@link AtomicLong} + a latch) so we never make racing
 * native calls on a single non-thread-safe handle from two threads.
 *
 * <h2>Delivery determinism (deflake)</h2>
 * Wherever an assertion depends on data ARRIVING (a first demux event, a first
 * {@code recvBytes()}), the sending side streams CONTINUOUSLY until the consuming
 * side signals it has observed what it needs (latch/future), bounded by a round
 * cap — never a fixed batch followed by a drain pause. A fixed-batch sender that
 * closes after a pause races a starved receiver: SRT discards undelivered TSBPD
 * data on close, the receiver sees a clean end-of-stream with zero deliveries,
 * and the test flakes (the historical CI failure of the
 * managed-demux-receiver stats test). Same shape as {@link SrtManagedReconnectTest}.
 */
class SrtManagedLiveTest {

    /** Timeout in seconds for inter-thread signalling and overall test completion. */
    private static final int TIMEOUT_SEC = 15;

    /** SRT latency in milliseconds for both sides (matches SrtMuxDemuxLoopbackTest). */
    private static final int LATENCY_MS = 120;

    /**
     * Number of synthetic IDR access units in the OFFLINE reference mux
     * ({@link #muxToBytes()}: the offline-SHA path and the pre-muxed chunk the
     * plain-sender tests send). Chosen to exceed the receiver's TS-sync window
     * (≥ 4×188+1 bytes) AND fill at least one full SRT bundle (7×188 = 1316
     * bytes). The live paths no longer push a fixed batch — they stream until
     * observed (see "Delivery determinism" above).
     */
    private static final int PUSH_COUNT = 24;

    // ── Test 1 — ManagedMuxSender → (plain) DemuxReceiver, byte-faithful ──────

    @Test
    void managedMuxSenderToDemuxReceiverByteFaithful() throws Exception {
        assumeTrue(isLinux(),
            "SRT live-socket loopback gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");

        // Expected SHA via the OFFLINE Muxer→Demuxer path (self-validating).
        String offlineSha = offlineMuxDemuxSha();

        CompletableFuture<Integer> portFuture = new CompletableFuture<>();
        CompletableFuture<String> shaFuture = new CompletableFuture<>();
        CompletableFuture<DemuxEvent.UnknownSample> dataFuture = new CompletableFuture<>();
        // Counted down by main once it has STOPPED pushing and closed the sender;
        // the receiver holds its teardown on it so a final in-flight mini-batch can
        // never land on a closing peer (which would surface spurious sender-side
        // Broken errors on main).
        CountDownLatch senderDone = new CountDownLatch(1);

        // Receiver peer is a PLAIN DemuxReceiver (the test subject is the
        // ManagedMuxSender send path). Plain listener on :0 → publish port.
        Thread receiverThread = new Thread(() -> {
            Listener listener = null;
            Socket sock = null;
            DemuxReceiver rx = null;
            try {
                listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
                    .listener()
                    .listen();
                portFuture.complete(listener.localAddr().port());

                sock = listener.accept(null);
                rx = sock.intoDemuxReceiver();

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
                    if (!isCleanEndOfStream(re)) throw re;
                    // else: fall through; sha/data may still be null → fail below
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
                // Hold teardown until main has stopped pushing and closed its sender
                // (bounded, so a failed main can never park this daemon forever).
                try {
                    senderDone.await(TIMEOUT_SEC, TimeUnit.SECONDS);
                } catch (InterruptedException ignored) {
                    Thread.currentThread().interrupt();
                }
                if (rx != null) rx.close();
                if (sock != null) sock.close();
                if (listener != null) listener.close();
            }
        });
        receiverThread.setDaemon(true);
        receiverThread.start();

        int port;
        try {
            port = portFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
        } catch (Exception e) {
            receiverThread.interrupt();
            throw new AssertionError("receiver thread failed to publish port", e);
        }

        // The managed shell is on the MAIN thread here (peer is plain), so the
        // main thread can read its stats/counters directly. Read them BEFORE the
        // try-with-resources closes the handle.
        try (ManagedMuxSender s = ManagedMuxSender.fromUrl(
                "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS,
                roundtripConfig())) {
            // Stream mini-batches until the receiver has captured its SHA (bounded).
            // Continuous streaming (not a fixed batch + 1s drain pause) closes the
            // starved-receiver window: the connection stays live and flowing until
            // the receiver has deterministically seen its first Video event — the
            // same deflake shape as the managed-demux-receiver stats test below.
            // The SHA only covers the FIRST Video event (one access unit, identical
            // bytes every push), so the live stream length does not affect it.
            // 200 rounds × 50ms ≈ 10s — a MAIN-thread loop is deliberately bounded
            // INSIDE TIMEOUT_SEC so every coordination wait (the receiver's
            // senderDone await included) resolves within the test's 15s budget even
            // on the failure path. (Daemon-side runaway caps may use 400; main-side
            // loops must fit the budget.)
            long pts = 0;
            for (int round = 0;
                    round < 200 && (!shaFuture.isDone() || !dataFuture.isDone());
                    round++) {
                for (int i = 0; i < 6; i++, pts += 3000L) {
                    s.sendVideo(syntheticH264Idr(), pts, true);
                }
                // One distinctive private-data record per round (lone-data-stream
                // sendData shorthand) so it deterministically flows under continuous
                // streaming — identical bytes each send; the receiver captures the
                // first. The following round's video sends flush its PES.
                s.sendData(DATA_PAYLOAD, pts);
                Thread.sleep(50);
            }
            // Convenience accessor: the config declares one data stream, so the
            // handle must surface (exercises the native + sentinel path on a live
            // managed sender).
            assertTrue(s.dataHandle().isPresent(),
                "config declares one data stream → dataHandle() must surface it");
            // Strict handle decode: a forged/negative handle is rejected with
            // SrtException(CONFIG_INVALID) in the JNI shim before reaching the
            // muxer (DIFFERS from Muxer.sendDataTo, which maps it to MuxException).
            // Thrown before any send, so the live sender's state is untouched.
            SrtException forged = assertThrows(SrtException.class,
                () -> s.sendDataTo(DataStreamHandle.fromRaw(-1L), DATA_PAYLOAD, 0L));
            assertEquals(SrtException.Kind.CONFIG_INVALID, forged.kind(),
                "forged DataStreamHandle must raise SrtException(CONFIG_INVALID)");
            long attempts = s.reconnectAttempts();
            TransportStats st = s.stats();
            assertEquals(0L, attempts, "no reconnect should have occurred on the happy path");
            assertNotNull(st, "stats() must return a combined snapshot");
            assertNotNull(st.socketStats(), "combined stats must carry a SocketStats");
        } finally {
            senderDone.countDown(); // sender closed — release the receiver's teardown
        }

        String liveSha;
        try {
            liveSha = shaFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
        } catch (Exception e) {
            throw new AssertionError("receiver thread failed to complete", e);
        }

        assertEquals(offlineSha, liveSha,
            "live ManagedMuxSender→SRT→DemuxReceiver path must demux to the same video "
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

        receiverThread.join(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC));
    }

    // ── Test 2 — ManagedSender.srtStats() throws IO; socketStats() works ──────

    @Test
    void managedSenderSrtStatsThrowsIo() throws Exception {
        assumeTrue(isLinux(),
            "SRT live-socket loopback gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");

        CompletableFuture<Integer> portFuture = new CompletableFuture<>();

        // Plain listener peer: accept, drain a few recvBytes() to absorb the
        // sender's data, then stop on clean end-of-stream.
        Thread receiverThread = new Thread(() -> {
            Listener listener = null;
            Socket sock = null;
            Receiver r = null;
            try {
                listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
                    .listener()
                    .listen();
                portFuture.complete(listener.localAddr().port());

                sock = listener.accept(null);
                r = sock.intoReceiver();
                // Drain until the peer closes (CLOSED/BROKEN) — bounded by the
                // main thread closing the sender after the assertions.
                while (true) {
                    try {
                        r.recvBytes();
                    } catch (SrtException e) {
                        if (e.kind() == SrtException.Kind.CLOSED
                                || e.kind() == SrtException.Kind.BROKEN) {
                            break;
                        }
                        throw e;
                    }
                }
            } catch (Exception ex) {
                portFuture.completeExceptionally(ex);
            } finally {
                if (r != null) r.close();
                if (sock != null) sock.close();
                if (listener != null) listener.close();
            }
        });
        receiverThread.setDaemon(true);
        receiverThread.start();

        int port;
        try {
            port = portFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
        } catch (Exception e) {
            receiverThread.interrupt();
            throw new AssertionError("receiver thread failed to publish port", e);
        }

        byte[] preMuxedTs = muxToBytes(); // a valid TS chunk; content is irrelevant to the drift

        try (ManagedSender s = ManagedSender.fromUrl(
                "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS)) {
            s.sendBytes(preMuxedTs);
            // Drift: srtStats() ALWAYS throws SrtException(IO) on a managed sender.
            SrtException ex = assertThrows(SrtException.class, s::srtStats);
            assertEquals(SrtException.Kind.IO, ex.kind(),
                "ManagedSender.srtStats() must throw SrtException(IO) — documented stats drift");
            // socketStats() works (scheme-neutral 16-field view).
            assertNotNull(s.socketStats(), "ManagedSender.socketStats() must return a snapshot");
            Thread.sleep(500);
        }

        receiverThread.join(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC));
    }

    // ── Test 2b — ManagedSender construction + send accepts a BACKGROUND policy ──

    /**
     * {@link ReconnectMode#BACKGROUND} smoke test (D4): a {@link ManagedSender}
     * constructed with a BACKGROUND-mode {@link ReconnectPolicy} connects and
     * sends successfully on the happy path (no outage, so the per-outage worker
     * never spins up — this proves the flattened {@code mode} arg survives the
     * JNI round trip and is accepted by {@code build_reconnect_policy} without
     * throwing, not the reconnect behavior itself, which is covered on the Rust
     * side). Same peer/receiver shape as {@link #managedSenderSrtStatsThrowsIo}.
     */
    @Test
    void managedSenderBackgroundModeSendSucceeds() throws Exception {
        assumeTrue(isLinux(),
            "SRT live-socket loopback gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");

        CompletableFuture<Integer> portFuture = new CompletableFuture<>();

        Thread receiverThread = new Thread(() -> {
            Listener listener = null;
            Socket sock = null;
            Receiver r = null;
            try {
                listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
                    .listener()
                    .listen();
                portFuture.complete(listener.localAddr().port());

                sock = listener.accept(null);
                r = sock.intoReceiver();
                while (true) {
                    try {
                        r.recvBytes();
                    } catch (SrtException e) {
                        if (e.kind() == SrtException.Kind.CLOSED
                                || e.kind() == SrtException.Kind.BROKEN) {
                            break;
                        }
                        throw e;
                    }
                }
            } catch (Exception ex) {
                portFuture.completeExceptionally(ex);
            } finally {
                if (r != null) r.close();
                if (sock != null) sock.close();
                if (listener != null) listener.close();
            }
        });
        receiverThread.setDaemon(true);
        receiverThread.start();

        int port;
        try {
            port = portFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
        } catch (Exception e) {
            receiverThread.interrupt();
            throw new AssertionError("receiver thread failed to publish port", e);
        }

        byte[] preMuxedTs = muxToBytes();

        ReconnectPolicy backgroundPolicy = ReconnectPolicy.builder()
            .mode(ReconnectMode.BACKGROUND)
            .build();
        try (ManagedSender s = ManagedSender.fromUrl(
                "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS,
                backgroundPolicy)) {
            s.sendBytes(preMuxedTs); // must not throw
            assertTrue(s.isAlive(), "sender must be alive after a successful send");
            Thread.sleep(500);
        }

        receiverThread.join(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC));
    }

    // ── Test 2c — ManagedSender.reconnectStats() healthy-link all-zero (D5) ───

    /**
     * D5: {@link ManagedSender#reconnectStats()} on the happy path (no outage)
     * returns a {@link ManagedTransportStats} snapshot with every counter at
     * zero and {@code reconnecting()==false}. Same peer/receiver shape as
     * {@link #managedSenderSrtStatsThrowsIo}.
     */
    @Test
    void managedSenderReconnectStatsHealthyLinkAllZero() throws Exception {
        assumeTrue(isLinux(),
            "SRT live-socket loopback gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");

        CompletableFuture<Integer> portFuture = new CompletableFuture<>();

        Thread receiverThread = new Thread(() -> {
            Listener listener = null;
            Socket sock = null;
            Receiver r = null;
            try {
                listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
                    .listener()
                    .listen();
                portFuture.complete(listener.localAddr().port());

                sock = listener.accept(null);
                r = sock.intoReceiver();
                while (true) {
                    try {
                        r.recvBytes();
                    } catch (SrtException e) {
                        if (e.kind() == SrtException.Kind.CLOSED
                                || e.kind() == SrtException.Kind.BROKEN) {
                            break;
                        }
                        throw e;
                    }
                }
            } catch (Exception ex) {
                portFuture.completeExceptionally(ex);
            } finally {
                if (r != null) r.close();
                if (sock != null) sock.close();
                if (listener != null) listener.close();
            }
        });
        receiverThread.setDaemon(true);
        receiverThread.start();

        int port;
        try {
            port = portFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
        } catch (Exception e) {
            receiverThread.interrupt();
            throw new AssertionError("receiver thread failed to publish port", e);
        }

        byte[] preMuxedTs = muxToBytes(); // a valid TS chunk; content is irrelevant to the stats read

        try (ManagedSender s = ManagedSender.fromUrl(
                "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS)) {
            s.sendBytes(preMuxedTs);
            ManagedTransportStats stats = s.reconnectStats();
            assertEquals(0L, stats.reconnectAttempts(), "no reconnect should have occurred");
            assertEquals(0L, stats.reconnectSuccesses(), "no reconnect should have occurred");
            assertEquals(0L, stats.gapLen(), "healthy link never queues into the gap buffer");
            assertEquals(0L, stats.gapMessagesDropped(), "healthy link never drops");
            assertEquals(0L, stats.gapBytesDropped(), "healthy link never drops");
            assertFalse(stats.reconnecting(),
                "BLOCKING mode (the default) never reports reconnecting==true");
            Thread.sleep(500);
        }

        receiverThread.join(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC));
    }

    // ── Test 1b — ManagedMuxSender.reconnectStats() healthy-link all-zero (D5) ─

    /**
     * D5 twin of {@link #managedSenderReconnectStatsHealthyLinkAllZero} for
     * {@link ManagedMuxSender}. Peer is a plain {@link Receiver} draining bytes —
     * the test subject is the stats accessor, not the mux/demux round trip
     * already covered by {@link #managedMuxSenderToDemuxReceiverByteFaithful}.
     */
    @Test
    void managedMuxSenderReconnectStatsHealthyLinkAllZero() throws Exception {
        assumeTrue(isLinux(),
            "SRT live-socket loopback gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");

        CompletableFuture<Integer> portFuture = new CompletableFuture<>();

        Thread receiverThread = new Thread(() -> {
            Listener listener = null;
            Socket sock = null;
            Receiver r = null;
            try {
                listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
                    .listener()
                    .listen();
                portFuture.complete(listener.localAddr().port());

                sock = listener.accept(null);
                r = sock.intoReceiver();
                while (true) {
                    try {
                        r.recvBytes();
                    } catch (SrtException e) {
                        if (e.kind() == SrtException.Kind.CLOSED
                                || e.kind() == SrtException.Kind.BROKEN) {
                            break;
                        }
                        throw e;
                    }
                }
            } catch (Exception ex) {
                portFuture.completeExceptionally(ex);
            } finally {
                if (r != null) r.close();
                if (sock != null) sock.close();
                if (listener != null) listener.close();
            }
        });
        receiverThread.setDaemon(true);
        receiverThread.start();

        int port;
        try {
            port = portFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
        } catch (Exception e) {
            receiverThread.interrupt();
            throw new AssertionError("receiver thread failed to publish port", e);
        }

        try (ManagedMuxSender s = ManagedMuxSender.fromUrl(
                "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS,
                roundtripConfig())) {
            s.sendVideo(syntheticH264Idr(), 0L, true);
            ManagedTransportStats stats = s.reconnectStats();
            assertEquals(0L, stats.reconnectAttempts(), "no reconnect should have occurred");
            assertEquals(0L, stats.reconnectSuccesses(), "no reconnect should have occurred");
            assertEquals(0L, stats.gapLen(), "healthy link never queues into the gap buffer");
            assertEquals(0L, stats.gapMessagesDropped(), "healthy link never drops");
            assertEquals(0L, stats.gapBytesDropped(), "healthy link never drops");
            assertFalse(stats.reconnecting(),
                "BLOCKING mode (the default) never reports reconnecting==true");
            Thread.sleep(500);
        }

        receiverThread.join(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC));
    }

    // ── Test 3 — ManagedDemuxReceiver.srtStats() returns SocketStats ──────────

    @Test
    void managedDemuxReceiverSrtStatsReturnsSocketStats() throws Exception {
        assumeTrue(isLinux(),
            "SRT live-socket loopback gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");

        // Topology mirrors SrtManagedReconnectTest (the proven deterministic shape):
        // the managed shell is the active CALLER on the MAIN thread (it owns its
        // handle, reads its own stats — no cross-thread handle race, no
        // discover-then-reuse port dance); the peer is a plain listener+MuxSender on
        // a daemon thread that streams CONTINUOUSLY until main signals `observed`.
        // Continuous streaming (not a fixed batch + drain pause) is what makes the
        // first demux event deterministic: the peer can never close before a starved
        // receiver has derived an event, so there is no window in which a peer close
        // discards undelivered TSBPD data — the exact window that flaked this test
        // on loaded CI runners.
        CompletableFuture<Integer> portFuture = new CompletableFuture<>();
        CountDownLatch observed = new CountDownLatch(1);

        Thread peerThread = new Thread(() -> {
            Listener listener = null;
            Socket sock = null;
            MuxSender ms = null;
            try {
                listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
                    .listener()
                    .listen();
                portFuture.complete(listener.localAddr().port());

                sock = listener.accept(null);
                ms = sock.intoMuxSender(roundtripConfig());
                // Stream until main has observed its first event (bounded so this
                // daemon can never run away if main fails before observing).
                long pts = 0;
                for (int round = 0; round < 400 && observed.getCount() > 0; round++) {
                    for (int i = 0; i < 6; i++, pts += 3000L) {
                        ms.sendVideo(syntheticH264Idr(), pts, true);
                    }
                    Thread.sleep(50);
                }
            } catch (Exception ex) {
                // A push racing main's teardown after `observed` fires is benign
                // noise; completeExceptionally is a no-op once the port has been
                // published, and main fails on its own asserts if the peer died
                // before streaming anything.
                portFuture.completeExceptionally(ex);
            } finally {
                if (ms != null) ms.close();
                if (sock != null) sock.close();
                if (listener != null) listener.close();
            }
        });
        peerThread.setDaemon(true);
        peerThread.start();

        int port;
        try {
            port = portFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
        } catch (Exception e) {
            peerThread.interrupt();
            throw new AssertionError("peer thread failed to publish port", e);
        }

        boolean sawEvent = false;
        try (ManagedDemuxReceiver rx = ManagedDemuxReceiver.fromUrl(
                "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS,
                fastPolicy())) {
            // Hard no-hang safety net (same as SrtManagedReconnectTest): a cancel
            // handle obtained BEFORE iterating so the watchdog is armed before the
            // loop can stall (it may equally be obtained mid-iteration).
            // The watchdog converts any unforeseen stall into a prompt CLOSED
            // end-of-iteration + a clean assertion failure, never a wedged worker.
            CancelHandle cancel = rx.cancelHandle();
            Thread watchdog = new Thread(() -> {
                try {
                    if (!observed.await(TIMEOUT_SEC - 3, TimeUnit.SECONDS)) {
                        cancel.cancel();
                    }
                } catch (InterruptedException ignored) {
                    Thread.currentThread().interrupt();
                }
            });
            watchdog.setDaemon(true);
            watchdog.start();
            try {
                for (DemuxEvent e : rx) {
                    sawEvent = true;
                    // Drift: srtStats() RETURNS a SocketStats (NOT SrtStats) and
                    // does NOT throw — read on the OWNING (main) thread.
                    SocketStats srt = rx.srtStats();
                    assertNotNull(srt,
                        "ManagedDemuxReceiver.srtStats() must return a SocketStats (documented "
                            + "drift), not throw");
                    assertInstanceOf(SocketStats.class, srt,
                        "ManagedDemuxReceiver.srtStats() returns SocketStats, not SrtStats");
                    assertNotNull(rx.socketStats(),
                        "ManagedDemuxReceiver.socketStats() must return a snapshot");
                    assertEquals(0L, rx.reconnectAttempts(),
                        "no reconnect should have occurred on the happy path");
                    break;
                }
            } catch (RuntimeException re) {
                if (!isCleanEndOfStream(re)) throw re;
            } finally {
                observed.countDown(); // always release the peer + watchdog
            }
        }

        assertTrue(sawEvent, "managed demux receiver must have seen at least one demux event");
        peerThread.join(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC));
    }

    // ── Test 4 — ManagedReceiver.srtStats() throws IO ─────────────────────────

    @Test
    void managedReceiverSrtStatsThrowsIo() throws Exception {
        assumeTrue(isLinux(),
            "SRT live-socket loopback gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");

        // Listener-side managed shell → discover-then-reuse ephemeral port.
        int port = discoverFreePort();

        AtomicReference<SrtException.Kind> kindRef = new AtomicReference<>();
        AtomicLong attemptsRef = new AtomicLong(-1);
        AtomicReference<Throwable> rxError = new AtomicReference<>();
        CountDownLatch doneLatch = new CountDownLatch(1);

        Thread receiverThread = new Thread(() -> {
            ManagedReceiver mr = null;
            try {
                mr = ManagedReceiver.fromUrl(
                    "srt://127.0.0.1:" + port + "?mode=listener&latency=" + LATENCY_MS,
                    fastPolicy());
                mr.recvBytes(); // blocks until first data arrives
                // Capture the drift assertion ON THIS THREAD (owns the handle).
                SrtException ex = assertThrows(SrtException.class, mr::srtStats);
                kindRef.set(ex.kind());
                attemptsRef.set(mr.reconnectAttempts());
            } catch (Throwable t) {
                rxError.set(t);
            } finally {
                doneLatch.countDown();
                if (mr != null) mr.close();
            }
        });
        receiverThread.setDaemon(true);
        receiverThread.start();

        byte[] preMuxedTs = muxToBytes();

        // Main: retry-connect a plain Sender caller (bounded).
        Sender sender = null;
        for (int i = 0; i < 60 && sender == null; i++) {
            try {
                sender = Sender.fromUrl(
                    "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS);
            } catch (SrtException e) {
                Thread.sleep(50); // not bound yet — retry
            }
        }
        assertNotNull(sender, "Sender caller failed to connect to the managed listener within budget");
        try {
            // Keep the pre-muxed chunk flowing until the receiver's recvBytes() has
            // returned and its drift asserts have run (doneLatch), bounded. The old
            // fixed two-sends + 500ms pause left a window where a starved receiver
            // missed TSBPD delivery before the sender's close discarded it, so
            // recvBytes() threw BROKEN instead of returning data — the same flake
            // class as the managed-demux-receiver stats test above. 200 rounds ×
            // 50ms ≈ 10s: a MAIN-thread loop bounded INSIDE TIMEOUT_SEC, so the
            // failure path surfaces within the doneLatch ceiling instead of adding
            // a full extra loop budget on top of it.
            for (int round = 0; round < 200 && doneLatch.getCount() > 0; round++) {
                sender.sendBytes(preMuxedTs);
                sender.flush();
                Thread.sleep(50);
            }
        } finally {
            sender.close();
        }

        assertTrue(doneLatch.await(TIMEOUT_SEC, TimeUnit.SECONDS),
            "receiver thread did not finish within the ceiling");
        if (rxError.get() != null) {
            throw new AssertionError("receiver thread failed", rxError.get());
        }

        // Drift: srtStats() ALWAYS throws SrtException(IO) on a managed receiver.
        assertEquals(SrtException.Kind.IO, kindRef.get(),
            "ManagedReceiver.srtStats() must throw SrtException(IO) — documented stats drift");
        assertEquals(0L, attemptsRef.get(), "no reconnect should have occurred on the happy path");

        receiverThread.join(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC));
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    /** A fast reconnect policy for the live tests: constant(0) backoff, ≤3 attempts. */
    private static ReconnectPolicy fastPolicy() {
        return ReconnectPolicy.builder()
            .backoff(BackoffStrategy.constant(0))
            .maxAttempts(3)
            .build();
    }

    /**
     * Discover-then-reuse: bind a throwaway plain listener on {@code :0}, read the
     * kernel-assigned port, close it, and return that port for reuse by a
     * managed-listener shell. The sanctioned ephemeral-port pattern for tests
     * where the listening shell is constructed via {@code fromUrl} (which has no
     * post-bind port accessor) — never a hardcoded fixed port.
     */
    private static int discoverFreePort() throws Exception {
        try (Listener probe = new Builder("srt://127.0.0.1:0?mode=listener")
                .listener()
                .listen()) {
            return probe.localAddr().port();
        }
    }

    /**
     * Unwrap an iterator's {@link RuntimeException} and report whether its cause
     * is a CLOSED/BROKEN {@link SrtException} (clean end-of-stream after the peer
     * hangs up). Any other cause is a real failure.
     */
    private static boolean isCleanEndOfStream(RuntimeException re) {
        Throwable cause = re.getCause();
        return cause instanceof SrtException se
            && (se.kind() == SrtException.Kind.CLOSED || se.kind() == SrtException.Kind.BROKEN);
    }

    /** Distinctive private-data record pushed alongside the video stream (test 1). */
    private static final byte[] DATA_PAYLOAD =
        {(byte) 0xD0, 'D', 'A', 'T', 'A', (byte) 0xBE, (byte) 0xEF, 0x01};

    /** Video + one private-data stream — see {@link TestSupport#roundtripConfigWithData()}. */
    private static MuxerConfig roundtripConfig() {
        return TestSupport.roundtripConfigWithData();
    }

    /** Mux PUSH_COUNT synthetic IDRs at increasing PTS and return the TS bytes. */
    private static byte[] muxToBytes() throws Exception {
        ByteArrayOutputStream acc = new ByteArrayOutputStream();
        byte[] out = new byte[8192];
        try (Muxer m = new Muxer(roundtripConfig())) {
            for (int i = 0; i < PUSH_COUNT; i++) {
                m.pushVideo(syntheticH264Idr(), i * 3000L, true);
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
