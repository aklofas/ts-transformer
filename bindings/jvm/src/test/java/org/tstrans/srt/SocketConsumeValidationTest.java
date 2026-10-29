package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.roundtripConfig;

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.concurrent.CountDownLatch;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.MuxException;
import org.tstrans.NativeHandle;
import org.tstrans.mpegts.DemuxerConfig;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;
import org.tstrans.shells.MuxSender;

/**
 * A conversion that refuses its arguments leaves the {@link Socket} with the
 * caller: still open, still consumable exactly once. A null config used to
 * throw after the socket was claimed, leaving the native socket with no owner,
 * and a config the native refused closed the socket, so the corrected retry
 * found it closed.
 *
 * <p>Loopback choreography as in {@link CancelHandleObservesCloseTest}, but the
 * accepted raw {@code Socket} is kept rather than converted.
 */
final class SocketConsumeValidationTest {
    private static final int LATENCY_MS = 120;

    /** The accepted socket + the latch that releases its idle peer. */
    private record Live(Socket sock, CountDownLatch release) implements AutoCloseable {
        @Override
        public void close() {
            release.countDown();
            sock.close(); // a no-op once a conversion consumed it
        }
    }

    private static Live connect() throws Exception {
        Listener listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
            .listener().listen();
        int port = listener.localAddr().port();
        CountDownLatch release = new CountDownLatch(1);
        Thread peer = new Thread(() -> {
            try (MuxSender tx = MuxSender.open(
                    "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS,
                    roundtripConfig())) {
                release.await();
            } catch (Throwable ignored) {
                // teardown races the socket's close; benign
            }
        }, "idle-peer");
        peer.setDaemon(true);
        peer.start();
        // Bounded: an unbounded native accept on the @Timeout thread cannot be
        // interrupted, so a failed peer connect would hang the suite.
        Socket sock = listener.accept(5000);
        listener.close();
        return new Live(sock, release);
    }

    /**
     * Video + a 0x06 data stream carrying the KLVA registration descriptor: the
     * Java builder accepts it, but the Rust config build refuses it with
     * {@code CONFIG_INVALID} (the data stream classifies as KLV).
     */
    private static MuxerConfig masqueradeConfig() {
        return MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264)
            .addData(0x01F0, 0x06, true)
            .streamDescriptorsForData(0, new byte[][] {{0x05, 0x04, 'K', 'L', 'V', 'A'}})
            .build();
    }

    @Test
    @Timeout(30)
    void nullDemuxConfigLeavesTheSocketWithTheCaller() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        try (Live live = connect()) {
            assertThrows(NullPointerException.class,
                () -> live.sock.intoDemuxReceiver((DemuxerConfig) null));
            assertDoesNotThrow(() -> live.sock.peerAddr(), "the socket must still be open");
            live.sock.intoDemuxReceiver().close(); // still consumable exactly once
        }
    }

    @Test
    @Timeout(30)
    void nullMuxerConfigLeavesTheSocketWithTheCaller() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        try (Live live = connect()) {
            assertThrows(NullPointerException.class, () -> live.sock.intoMuxSender(null));
            assertDoesNotThrow(() -> live.sock.peerAddr());
        }
    }

    @Test
    @Timeout(30)
    void refusedMuxerConfigLeavesTheSocketWithTheCaller() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        try (Live live = connect()) {
            MuxException ex = assertThrows(MuxException.class,
                () -> live.sock.intoMuxSender(masqueradeConfig()));
            assertEquals(MuxException.Kind.CONFIG_INVALID, ex.kind(), ex.getMessage());
            assertTrue(live.sock.isAlive(), "a refused config must not consume the socket");
            assertDoesNotThrow(() -> live.sock.peerAddr(), "the native socket must still be open");
            live.sock.intoMuxSender(roundtripConfig()).close();
            assertFalse(live.sock.isAlive(), "the corrected conversion consumes the socket");
        }
    }

    /**
     * The demuxer conversion's only native refusal is an unknown
     * {@code Av1CarriageMode} ordinal, which the Java enum cannot produce, so this
     * drives the private native directly: a refusal must leave the native socket
     * registered.
     */
    @Test
    @Timeout(30)
    void refusedDemuxConfigLeavesTheNativeSocketOpen() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        try (Live live = connect()) {
            Method peek = NativeHandle.class.getDeclaredMethod("peekHandle");
            peek.setAccessible(true);
            long key = (long) peek.invoke(live.sock);
            Method native_ = Socket.class.getDeclaredMethod("nIntoDemuxReceiverWithConfig",
                long.class, int.class, long.class, long.class, boolean.class, int.class,
                long.class, boolean.class, long.class, boolean.class);
            native_.setAccessible(true);
            InvocationTargetException ite = assertThrows(InvocationTargetException.class,
                () -> native_.invoke(null, key, 0, 0L, 0L, true, 99, 0L, false, 0L, false));
            // Demux declares no INTERNAL kind, so the refusal surfaces as a plain
            // RuntimeException naming the ordinal.
            assertInstanceOf(RuntimeException.class, ite.getCause());
            assertTrue(String.valueOf(ite.getCause().getMessage()).contains("Av1CarriageMode"),
                String.valueOf(ite.getCause()));
            assertDoesNotThrow(() -> live.sock.peerAddr(), "the native socket must still be open");
            live.sock.intoDemuxReceiver(DemuxerConfig.builder().build()).close();
            assertFalse(live.sock.isAlive());
        }
    }
}
