package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;

import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.condition.EnabledIfSystemProperty;
import org.tstrans.RtspException;
import org.tstrans.internal.PanicProbe;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

/**
 * Exercises the production {@code with_mount_poisoning}/{@code with_server_poisoning}
 * helpers — the ones the RTSP <em>mutating</em> natives ({@code pushX}/{@code flush}/
 * {@code stop}/{@code addX}) route through — on a REAL leased {@code RtspServer}/
 * {@code MountHandle}: a forced panic through the helper POISONS the handle, so
 * every later real op throws {@link IllegalStateException} instead of reusing torn
 * native state (JNI-01).
 *
 * <p>The generic {@code PanicProbe} (see {@link org.tstrans.PanicIsolationTest})
 * proves {@code with_poisoning} on a stand-in {@code u64} registry; these tests
 * drive the SAME production helper on a REAL RTSP handle. The panic is injected via
 * dedicated {@code jni-test-hooks} probes (not by making a real {@code pushVideo}/
 * {@code stop} panic), so this pins the helper's end-to-end poisoning of a real RTSP
 * handle; that each native is wired to the helper is a code-structure fact verified
 * in {@code rtp/server.rs}.
 *
 * <p>Runs only when the cdylib was built with the {@code jni-test-hooks} cargo
 * feature (Gradle {@code -PjniTestHooks=true}); the shipped JAR never carries the
 * probe symbols. The server binds {@code 127.0.0.1:0} (ephemeral loopback) and no
 * peer ever connects, so there is no network flakiness.
 */
@EnabledIfSystemProperty(named = "tst.jniTestHooks", matches = "true")
class RtspPanicPoisoningTest {

    private static MuxerConfig videoCfg() {
        return MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264).build();
    }

    private static byte[] idr() {
        byte[] b = new byte[20];
        b[0] = 0; b[1] = 0; b[2] = 0; b[3] = 1; b[4] = 0x65;
        for (int i = 0; i < 15; i++) b[5 + i] = (byte) (0xA5 ^ i);
        return b;
    }

    /**
     * A panic driven through the production {@code with_mount_poisoning} helper on a
     * leased {@code MountHandle} poisons the mount: the panic surfaces as a
     * {@link RuntimeException}, and a subsequent REAL mount push then throws
     * {@link IllegalStateException} (handle gone). The mount push family routes
     * through that helper (see {@code rtp/server.rs}).
     */
    @Test @Timeout(15)
    void mountMutatorPanicPoisonsMountHandle() throws Exception {
        try (RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"));
             MountHandle m = s.addUnicastMount("/poison-mount", videoCfg())) {

            // Sanity: a real push works BEFORE the torn mutation.
            m.pushVideo(idr(), 0L, true);

            long raw = m.nativeHandleForTest();
            assertNotEquals(0L, raw, "leased mount handle must be non-zero");

            // (1) Force a panic through the real REGISTRY_MOUNT.with_poisoning on
            //     this exact handle — surfaces as a RuntimeException (jni_catch).
            assertThrows(RuntimeException.class,
                () -> PanicProbe.nForcePanicThroughMount(raw));

            // (2) The mount entry is now poisoned: a REAL mount op leases None and
            //     throws IllegalStateException, NOT a reuse of torn state.
            assertThrows(IllegalStateException.class,
                () -> m.pushVideo(idr(), 90_000L, true));
            // A second mutating native (flush) is equally dead.
            assertThrows(IllegalStateException.class, m::flush);
        }
        // try-with-resources close() on the poisoned MountHandle is a safe no-op
        // (the native entry is already gone) — reaching here without throwing
        // confirms that.
    }

    /**
     * A panic driven through the production {@code with_server_poisoning} helper on
     * a leased {@code RtspServer} poisons the server: the panic surfaces as a
     * {@link RuntimeException}, and a subsequent REAL server op then throws
     * {@link IllegalStateException}. The server mutators ({@code stop}/{@code addX})
     * route through that helper (see {@code rtp/server.rs}).
     */
    @Test @Timeout(15)
    void serverMutatorPanicPoisonsRtspServer() throws Exception {
        RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"));
        try {
            long raw = s.nativeHandleForTest();
            assertNotEquals(0L, raw, "leased server handle must be non-zero");

            // (1) Force a panic through the real REGISTRY_SERVER.with_poisoning.
            assertThrows(RuntimeException.class,
                () -> PanicProbe.nForcePanicThroughServer(raw));

            // (2) The server entry is poisoned: a REAL server op (a mutator —
            //     addUnicastMount) now leases None → IllegalStateException.
            assertThrows(IllegalStateException.class,
                () -> s.addUnicastMount("/after-poison", videoCfg()));
            // A getter is equally dead (the entry is gone, not just the resource).
            assertThrows(IllegalStateException.class, s::stats);
        } finally {
            // close() on a poisoned server is a safe idempotent no-op.
            s.close();
        }
    }

    /**
     * A poisoned server mutator drops only the registry's reference; a {@code
     * nextPublisher(Long.MAX_VALUE)} parked on another thread holds its own. The
     * native wait re-leases the server between bounded slices, so the parked call
     * finds the entry gone and ends with {@link RtspException} {@code SERVER} instead
     * of waiting forever.
     */
    @Test @Timeout(20)
    void poisonedServerEndsAParkedNextPublisher() throws Exception {
        RtspServer s = RtspServer.start(RtspServerConfig.builder()
            .bindAddr("127.0.0.1:0").acceptUnregisteredPublishers(true).build());
        AtomicReference<Throwable> error = new AtomicReference<>();
        AtomicReference<Object> value = new AtomicReference<>();
        Thread parked = new Thread(() -> {
            try {
                value.set(s.nextPublisher(Long.MAX_VALUE));
            } catch (Throwable t) {
                error.set(t);
            }
        });
        parked.setDaemon(true);
        try {
            long raw = s.nativeHandleForTest();
            parked.start();
            // Poison only once the call is inside its native wait.
            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
            while (true) {
                StackTraceElement[] st = parked.getStackTrace();
                if (st.length > 0 && st[0].isNativeMethod()
                        && st[0].getMethodName().equals("nNextPublisher")) {
                    break;
                }
                if (!parked.isAlive()) {
                    fail("nextPublisher returned before it parked: value " + value.get()
                        + ", error " + error.get());
                }
                if (System.nanoTime() > deadline) {
                    fail("the side thread never reached nNextPublisher");
                }
                Thread.sleep(5);
            }

            assertThrows(RuntimeException.class,
                () -> PanicProbe.nForcePanicThroughServer(raw));

            // Bounded join: one wait slice plus slack. Without the slicing the call
            // never returns and this fails.
            parked.join(TimeUnit.SECONDS.toMillis(10));
            assertTrue(!parked.isAlive(),
                "a parked nextPublisher outlived the poisoned server entry");
            Throwable e = error.get();
            assertTrue(e instanceof RtspException,
                () -> "nextPublisher: expected RtspException(SERVER), got "
                    + (e != null ? e : "value " + value.get()));
            assertEquals(RtspException.Kind.SERVER, ((RtspException) e).kind());
        } finally {
            s.close();
        }
    }
}
