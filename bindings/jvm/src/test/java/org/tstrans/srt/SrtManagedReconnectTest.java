package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.tstrans.SrtException;
import org.tstrans.mpegts.DemuxEvent;

/**
 * Live-socket RECONNECT parity test for the managed (auto-reconnect) SRT shells.
 * Proves that a {@link ManagedDemuxReceiver} survives a peer
 * drop+restore: it surfaces a {@link DemuxEvent.ReconnectDiscontinuity} after the
 * inner SRT transport is rebuilt AND reports {@code reconnectAttempts() > 0}.
 *
 * <p>This is the live-socket companion to {@link SrtManagedLiveTest} (which proves
 * the happy path + the three stats drifts WITHOUT exercising reconnect). It is the
 * JVM mirror of the tst-py managed-reconnect behaviour and of the Rust
 * {@code tst_pipeline} managed-receive reconnect tests.
 *
 * <h2>Race-free topology (caller-on-main, the proven-safe shape)</h2>
 * The {@link ManagedDemuxReceiver} runs as the active connector (CALLER mode) on
 * the MAIN thread — it accepts caller mode (it dials the peer), so it owns its
 * native handle on the main thread and reads its own reconnect counter there with
 * NO cross-thread handle race and NO discover-then-reuse port dance. The peer is a
 * plain {@code Builder} listener on a daemon thread that:
 * <ol>
 *   <li>binds on {@code :0} and publishes its ephemeral port,</li>
 *   <li>accepts connection #1, streams a batch of IDRs, then DROPS it (closes the
 *       accepted {@code MuxSender}/socket) — the managed caller sees the transport
 *       go Broken and re-dials under its {@link ReconnectPolicy},</li>
 *   <li>accepts connection #2 (blocks until the caller re-dials), streams another
 *       batch, then closes.</li>
 * </ol>
 * The listener stays bound for the whole test, so there is NO port-rebind race.
 *
 * <h2>Platform gate</h2>
 * Gated to Linux only via {@link org.junit.jupiter.api.Assumptions#assumeTrue}
 * (same as {@link SrtManagedLiveTest} and the Rust {@code #![cfg(target_os =
 * "linux")]} gate).
 *
 * <h2>Robustness</h2>
 * Daemon peer thread; {@code portFuture.get(15s)} + {@code peer.join(15s)}
 * ceilings; ephemeral port via the peer's own {@code localAddr().port()};
 * CLOSED/BROKEN (and the {@code RuntimeException}-wrapped iterator form) treated as
 * clean end-of-stream. The {@code EVENT_CEILING} bounds the iteration so the loop
 * can never spin forever if a reconnect never surfaces.
 */
class SrtManagedReconnectTest {

    /** Timeout in seconds for inter-thread signalling and overall test completion. */
    private static final int TIMEOUT_SEC = 15;

    /** SRT latency in milliseconds for both sides (matches SrtManagedLiveTest). */
    private static final int LATENCY_MS = 120;

    /**
     * IDRs streamed per connection. Matches {@code SrtManagedLiveTest.PUSH_COUNT} —
     * enough to lock TS sync and emit at least one Video event per connection.
     */
    private static final int PUSH_BATCH = 24;

    /**
     * Safety bound on the iteration: normally we break the instant a
     * {@link DemuxEvent.ReconnectDiscontinuity} arrives, but this caps the loop so
     * it can never spin forever if no reconnect ever surfaces.
     */
    private static final int EVENT_CEILING = 2000;

    @Test
    void managedDemuxReceiverSurvivesPeerDropAndRestore() throws Exception {
        assumeTrue(isLinux(),
            "SRT live-socket reconnect gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");

        CompletableFuture<Integer> portFuture = new CompletableFuture<>();
        // Main signals the peer + watchdog once it has observed the reconnect, so the
        // peer can stop streaming connection #2 and the watchdog can stand down.
        CountDownLatch observed = new CountDownLatch(1);

        // Peer: one plain listener (stays bound for the whole test) that accepts
        // TWICE. Between the accepts the managed caller observes the transport break
        // and re-dials, landing in accept #2. Connection #2 keeps streaming until
        // main signals `observed`, so post-reconnect bytes are ALWAYS flowing when
        // the receiver re-attaches — that read is what surfaces the
        // ReconnectDiscontinuity. The listener stays open the whole time so a re-dial
        // can never hit a dead port (which would otherwise spin recv_event).
        Thread peer = new Thread(() -> {
            Listener listener = null;
            try {
                listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
                    .listener()
                    .listen();
                portFuture.complete(listener.localAddr().port());

                // connection #1 — stream a batch, give it a delivery window so the
                // receiver locks sync + emits events, then DROP it.
                Socket s1 = listener.accept(null);
                MuxSender ms1 = s1.intoMuxSender(roundtripConfig());
                for (int i = 0; i < PUSH_BATCH; i++) {
                    ms1.sendVideo(syntheticH264Idr(), i * 3000L, true);
                }
                Thread.sleep(1_000);
                // DROP on a side daemon thread: libsrt's srt_close LINGERS, which would
                // block this thread from reaching accept #2. Closing on a throwaway
                // thread lets the peer proceed to accept #2 immediately while the linger
                // drains harmlessly in the background.
                Thread dropper = new Thread(ms1::close);
                dropper.setDaemon(true);
                dropper.start();

                // connection #2 — blocks until the managed receiver re-dials, then
                // streams continuously until main has observed the reconnect (bounded
                // so the daemon can't run away if main never observes). Continuous
                // post-reconnect data makes the ReconnectDiscontinuity deterministic.
                Socket s2 = listener.accept(null);
                MuxSender ms2 = s2.intoMuxSender(roundtripConfig());
                long pts = (long) PUSH_BATCH * 3000L;
                for (int round = 0; round < 400 && observed.getCount() > 0; round++) {
                    for (int i = 0; i < 6; i++, pts += 3000L) {
                        ms2.sendVideo(syntheticH264Idr(), pts, true);
                    }
                    Thread.sleep(50);
                }
                ms2.close();
                s2.close();
            } catch (Exception ex) {
                portFuture.completeExceptionally(ex);
            } finally {
                if (listener != null) listener.close();
            }
        });
        peer.setDaemon(true);
        peer.start();

        int port;
        try {
            port = portFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
        } catch (Exception e) {
            peer.interrupt();
            throw new AssertionError("peer thread failed to publish port", e);
        }

        boolean sawReconnect = false;
        long attempts = 0;

        // Managed shell on the MAIN thread — it owns its handle and reads its own
        // reconnect counter with no cross-thread race.
        try (ManagedDemuxReceiver rx = ManagedDemuxReceiver.fromUrl(
                "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS,
                reconnectPolicy())) {
            // Hard no-hang safety net: a cancel handle obtained BEFORE iterating
            // (cancel() is cross-thread-safe). A watchdog cancels it after a ceiling
            // so a stuck recv_event can never wedge the worker — it converts any
            // unforeseen stall into a prompt CLOSED/BROKEN end-of-iteration + a clean
            // assertion failure, never a hang. In the happy path the loop breaks on
            // the discontinuity well before the watchdog fires.
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
                int events = 0;
                for (DemuxEvent e : rx) {
                    if (e instanceof DemuxEvent.ReconnectDiscontinuity) {
                        sawReconnect = true;
                        attempts = rx.reconnectAttempts(); // read on the owning (main) thread
                        observed.countDown();              // release peer + watchdog
                        break;
                    }
                    if (++events > EVENT_CEILING) break; // bound the loop if no reconnect surfaces
                }
            } catch (RuntimeException re) {
                if (!isCleanEndOfStream(re)) throw re;
            } finally {
                observed.countDown(); // always release the peer + watchdog
            }
        }

        assertTrue(sawReconnect,
            "managed demux receiver must surface a ReconnectDiscontinuity after the peer drop+restore");
        assertTrue(attempts > 0,
            "reconnectAttempts() must be > 0 after a reconnect; got " + attempts);

        peer.join(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC));
    }

    // ── helpers (copied from SrtManagedLiveTest) ──────────────────────────────

    /**
     * A reconnect-friendly policy: constant(0) backoff, BOUNDED attempts. The bound
     * is deliberate — a retry-forever policy would let the managed caller spin in
     * {@code recv_event} on the main thread (which has no per-call ceiling) if a
     * re-dial could never land, turning a failed reconnect into a hang. With a
     * generous bound the peer's second accept is reached on the first re-dial in the
     * happy case, while a genuinely stuck reconnect exhausts the budget near-instantly
     * (constant(0) backoff) and surfaces as a clean iteration end + assertion failure
     * rather than a wedged worker.
     */
    private static ReconnectPolicy reconnectPolicy() {
        return ReconnectPolicy.builder()
            .backoff(BackoffStrategy.constant(0))
            .maxAttempts(50)
            .build();
    }

    /**
     * Unwrap an iterator's {@link RuntimeException} and report whether its cause is
     * a CLOSED/BROKEN {@link SrtException} (clean end-of-stream after the peer hangs
     * up). Any other cause is a real failure.
     */
    private static boolean isCleanEndOfStream(RuntimeException re) {
        Throwable cause = re.getCause();
        return cause instanceof SrtException se
            && (se.kind() == SrtException.Kind.CLOSED || se.kind() == SrtException.Kind.BROKEN);
    }

}
