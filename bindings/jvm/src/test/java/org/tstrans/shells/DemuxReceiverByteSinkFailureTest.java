package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.net.InetAddress;
import java.net.ServerSocket;
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.TestSupport;

/**
 * Byte-sink callbacks on a live {@link DemuxReceiver}: a callback that throws ends the
 * receive. TCP so the listener binds port 0 and the port is read back.
 */
@Timeout(value = 60, threadMode = Timeout.ThreadMode.SEPARATE_THREAD)
class DemuxReceiverByteSinkFailureTest {

    /** {@code n} null packets (PID 0x1FFF): the demuxer never produces an event from them. */
    static byte[] nullPackets(int n) {
        byte[] out = new byte[n * 188];
        Arrays.fill(out, (byte) 0xFF);
        for (int i = 0; i < n; i++) {
            out[i * 188] = 0x47;
            out[i * 188 + 1] = 0x1F;
            out[i * 188 + 2] = (byte) 0xFF;
            out[i * 188 + 3] = 0x10;
        }
        return out;
    }

    /** A sink failure on a quiet stream must surface without waiting for another event. */
    @Test
    void throwingSinkEndsTheReceiveAndIsRethrown() throws Exception {
        try (ServerSocket server = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
             DemuxReceiver rx = DemuxReceiver.open("tcp://127.0.0.1:" + server.getLocalPort());
             java.net.Socket peer = server.accept()) {
            CancelHandle rescue = rx.cancelHandle();
            RuntimeException boom = new IllegalStateException("sink failed");
            AtomicInteger calls = new AtomicInteger();
            rx.addByteSink(pkt -> { calls.incrementAndGet(); throw boom; });
            AtomicBoolean watchdogFired = new AtomicBoolean();
            Thread watchdog = new Thread(() -> {
                try {
                    Thread.sleep(TestSupport.scaledMs(5_000));
                } catch (InterruptedException e) {
                    return;
                }
                watchdogFired.set(true);
                rescue.cancel();
            }, "sink-watchdog");
            watchdog.setDaemon(true);
            watchdog.start();

            peer.getOutputStream().write(nullPackets(16));
            peer.getOutputStream().flush();

            Throwable thrown = assertThrows(Throwable.class, rx::recvEvent);
            watchdog.interrupt();
            assertSame(boom, thrown);
            assertFalse(watchdogFired.get(), "next() waited for an event instead of ending on the sink failure");
            assertTrue(rescue.isCancelled(), "the failed receive is ended, not resumable");
            assertEquals(1, calls.get(), "the sink is not called again after it threw");
        }
    }
}
