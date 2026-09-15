package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.fail;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.roundtripConfigWithData;

import java.util.Arrays;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import org.tstrans.SrtException;

/**
 * Shared machinery for the sender-side cancel-first tests: park a daemon
 * thread inside ONE native send and report when it is parked.
 *
 * <p>A send parks in two ways, and the tests need both:
 * <ul>
 *   <li>a <b>managed</b> sender whose peer vanished parks inside the Blocking
 *       reconnect's backoff wait — {@link #managedParkPolicy()} makes that wait
 *       10 s: long enough to close into, short enough that a failed verdict
 *       still unparks on its own (two attempts, then {@code BROKEN});</li>
 *   <li>a <b>plain</b> sender whose peer never reads parks in libsrt's blocking
 *       {@code srt_sendmsg} once its ~12 MB send buffer is full. The peer must
 *       be opened with {@link #NON_READING_PEER_KNOBS}: with the peer's default
 *       too-late-packet-drop advertised, the sender drops instead of blocking
 *       (libsrt {@code sndDropTooLate} keys on the PEER flag).</li>
 * </ul>
 *
 * <p>The pump records the wall-clock start of every send; a send still in
 * flight after {@link #PARKED_AFTER_MS} is parked, not slow (a live-link send
 * takes milliseconds). Pump threads are daemons: a JUnit {@code @Timeout}
 * cannot interrupt a blocked native call, so a pump that outlives a failed
 * verdict must not pin the JVM at exit.
 */
final class SrtSenderParkSupport {
    private SrtSenderParkSupport() {}

    /** Appended to a listener URL whose accepted socket will never be read. */
    static final String NON_READING_PEER_KNOBS = "&tlpktdrop=0";
    /** A send in flight this long is parked. */
    static final long PARKED_AFTER_MS = 1_500;
    /** Upper bound on waiting for a park (a plain sender first fills ~24 MB of buffers). */
    static final long PARK_DEADLINE_MS = 30_000;

    /**
     * Blocking reconnect, 10 s constant backoff, two attempts. Pair it with
     * {@code ?conntimeo=300} on the caller URL so each re-dial into the vanished
     * listener fails in 300 ms and the parked time IS the backoff wait.
     */
    static ReconnectPolicy managedParkPolicy() {
        return ReconnectPolicy.builder()
            .backoff(BackoffStrategy.constant(10_000))
            .maxAttempts(2)
            .mode(ReconnectMode.BLOCKING)
            .build();
    }

    /** Bind a plain {@link Receiver} listener on a daemon thread; it never reads. */
    static CompletableFuture<Receiver> peerListener(String listenUrl) {
        CompletableFuture<Receiver> peerFuture = new CompletableFuture<>();
        Thread peer = new Thread(() -> {
            try {
                peerFuture.complete(Receiver.fromUrl(listenUrl)); // blocks until a caller connects
            } catch (Exception ex) {
                peerFuture.completeExceptionally(ex);
            }
        }, "peer-listener");
        peer.setDaemon(true);
        peer.start();
        return peerFuture;
    }

    /** Connect a managed mux-sender caller, retrying while the listener is between binds. */
    static ManagedMuxSender connectManagedMux(String url, long budgetMs) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(budgetMs);
        SrtException last = null;
        while (System.nanoTime() < deadline) {
            try {
                return ManagedMuxSender.fromUrl(url, roundtripConfig(), managedParkPolicy());
            } catch (SrtException e) {
                last = e;
                Thread.sleep(50);
            }
        }
        throw new AssertionError("caller could not connect within " + budgetMs + " ms", last);
    }

    /** Connect a plain mux-sender caller, retrying while the listener is between binds. */
    static MuxSender connectPlainMux(String url, long budgetMs) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(budgetMs);
        SrtException last = null;
        while (System.nanoTime() < deadline) {
            try {
                return MuxSender.fromUrl(url, roundtripConfigWithData());
            } catch (SrtException e) {
                last = e;
                Thread.sleep(50);
            }
        }
        throw new AssertionError("caller could not connect within " + budgetMs + " ms", last);
    }

    /** One send; the pump repeats it until it throws. */
    @FunctionalInterface
    interface Send {
        void run() throws Exception;
    }

    /** A running pump: the daemon thread, its in-flight marker, its terminal throwable. */
    static final class Pump {
        final Thread thread;
        /** {@code System.nanoTime()} at which the current send began; 0 between sends. */
        final AtomicLong inFlightSince = new AtomicLong();
        /** Completed with whatever ended the pump (never null). */
        final CompletableFuture<Throwable> end = new CompletableFuture<>();

        private Pump(String name, Send send, long pauseMs) {
            this.thread = new Thread(() -> {
                try {
                    for (;;) {
                        inFlightSince.set(System.nanoTime());
                        send.run();
                        inFlightSince.set(0);
                        if (pauseMs > 0) Thread.sleep(pauseMs);
                    }
                } catch (Throwable t) {
                    end.complete(t);
                }
            }, name);
            this.thread.setDaemon(true);
        }
    }

    /** Start a pump named {@code name} calling {@code send} every {@code pauseMs} ms until it throws. */
    static Pump pump(String name, Send send, long pauseMs) {
        Pump p = new Pump(name, send, pauseMs);
        p.thread.start();
        return p;
    }

    /**
     * Block until one send has been in flight for {@link #PARKED_AFTER_MS};
     * fail if the pump ends first or {@link #PARK_DEADLINE_MS} passes.
     */
    static void awaitParked(Pump p, String what) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(PARK_DEADLINE_MS);
        while (System.nanoTime() < deadline) {
            if (p.end.isDone()) {
                fail(what + " ended before it parked: " + p.end.get());
            }
            long since = p.inFlightSince.get();
            if (since != 0
                    && TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - since) >= PARKED_AFTER_MS) {
                return;
            }
            Thread.sleep(50);
        }
        fail(what + " did not park within " + PARK_DEADLINE_MS + " ms");
    }

    /** 32 KiB private-data payload (under the 65 527-byte PES ceiling of {@code sendData}). */
    static byte[] dataBlob() {
        byte[] b = new byte[32 * 1024];
        for (int i = 0; i < b.length; i++) b[i] = (byte) i;
        return b;
    }

    /** 350 null TS packets (65 800 bytes = 50 exact 1316-byte bundles) for {@code Sender.sendBytes}. */
    static byte[] nullTsBlock() {
        byte[] b = new byte[188 * 350];
        for (int off = 0; off < b.length; off += 188) {
            b[off] = 0x47;
            b[off + 1] = 0x1F;          // PID 0x1FFF: null packet, no TEI, no PUSI
            b[off + 2] = (byte) 0xFF;
            b[off + 3] = 0x10;          // payload only, continuity 0
            Arrays.fill(b, off + 4, off + 188, (byte) 0xFF);
        }
        return b;
    }
}
