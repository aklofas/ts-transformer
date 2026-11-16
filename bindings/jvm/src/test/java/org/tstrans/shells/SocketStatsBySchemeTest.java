package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.tstrans.TestSupport.scaledMs;

import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RistException;
import org.tstrans.tcp.Listener;

/** What {@code socketStats()} carries on a udp, tcp and rist shell: the mapping the Javadoc states. */
@Timeout(90)
class SocketStatsBySchemeTest {
    private static final int BUNDLES = 20;
    private static final long WANT = (long) BUNDLES * ShellPeers.TS_BUNDLE.length;

    /** Receives 188-byte packets until {@code want} bytes came back; RIST ticks BACKPRESSURE while idle. */
    private static long receiveBytes(Receiver rx, long want) throws Exception {
        long got = 0;
        try (ShellPeers.Watchdog w = new ShellPeers.Watchdog(rx.cancelHandle(), scaledMs(20_000))) {
            while (got < want) {
                try {
                    got += rx.recvBytes().length;
                } catch (RistException e) {
                    if (e.kind() != RistException.Kind.BACKPRESSURE) throw e;
                }
            }
        }
        return got;
    }

    private static long sendBundles(Sender tx, int n) throws Exception {
        for (int i = 0; i < n; i++) tx.sendBytes(ShellPeers.TS_BUNDLE);
        tx.flush();
        return (long) n * ShellPeers.TS_BUNDLE.length;
    }

    private static void assertSrtStatsRefused(Receiver rx, Sender tx) {
        assertThrows(UnsupportedOperationException.class, rx::srtStats);
        assertThrows(UnsupportedOperationException.class, tx::srtStats);
    }

    @Test
    void udpCountsBytesAndDatagrams() throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            Receiver rx = c.add(Receiver.open("udp://127.0.0.1:0"));
            Sender tx = c.add(Sender.open("udp://127.0.0.1:" + rx.localPort().getAsInt()));
            long handed = sendBundles(tx, BUNDLES);
            long got = receiveBytes(rx, WANT);

            SocketStats r = rx.socketStats();
            assertEquals(got, r.bytesReceived());
            assertTrue(r.packetsReceived() > 0);
            assertEquals(0, r.packetsDroppedRecv());
            SocketStats s = tx.socketStats();
            assertEquals(handed, s.bytesSent());
            assertTrue(s.packetsSent() > 0);
            assertEquals(0, s.packetsDroppedSend());
            assertSrtStatsRefused(rx, tx);
        }
    }

    @Test
    void tcpCountsBytesAndCalls() throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            Listener l = c.add(Listener.open("tcp://127.0.0.1:0?listen=1"));
            int port = l.localPort();
            Sender tx = c.add(Sender.open("tcp://127.0.0.1:" + port));
            Receiver rx = c.add(ShellPeers.within(scaledMs(10_000), "accept",
                () -> l.accept().intoReceiver()));
            long handed = sendBundles(tx, BUNDLES);
            long got = receiveBytes(rx, WANT);

            SocketStats r = rx.socketStats();
            assertEquals(got, r.bytesReceived());
            assertTrue(r.packetsReceived() > 0);
            assertEquals(0, r.packetsDroppedRecv());
            SocketStats s = tx.socketStats();
            assertEquals(handed, s.bytesSent());
            assertTrue(s.packetsSent() > 0);
            assertEquals(0, s.packetsDroppedSend());
            assertSrtStatsRefused(rx, tx);
        }
    }

    @Test
    void ristCountsBytesAndPacketsAndLagsOnTheRest() throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            Receiver rx = c.add(Receiver.open("rist://@127.0.0.1:0"));
            Sender tx = c.add(Sender.open("rist://127.0.0.1:" + rx.localPort().getAsInt()));
            AtomicBoolean stop = new AtomicBoolean();
            AtomicLong handed = new AtomicLong();
            // The first packets can be lost before the session is up, so keep sending.
            Thread sender = new Thread(() -> {
                try {
                    while (!stop.get()) {
                        tx.sendBytes(ShellPeers.TS_BUNDLE);
                        handed.addAndGet(ShellPeers.TS_BUNDLE.length);
                        Thread.sleep(20);
                    }
                } catch (Exception ignored) {
                    // the shell was closed under it at teardown
                }
            }, "rist-stats-sender");
            sender.setDaemon(true);
            sender.start();
            long got;
            SocketStats lag;
            try {
                got = receiveBytes(rx, WANT);
                // librist reports these once a second, so they lag the counters; poll while
                // traffic still flows, since the report needs a live link.
                long deadline = System.nanoTime() + scaledMs(5_000) * 1_000_000L;
                lag = tx.socketStats();
                while (lag.linkBandwidthBps() == 0 && lag.rttUs() == 0
                        && System.nanoTime() < deadline) {
                    Thread.sleep(100);
                    lag = tx.socketStats();
                }
            } finally {
                stop.set(true);
            }
            sender.join(scaledMs(5_000));
            assertFalse(sender.isAlive(), "the sender thread is still running");
            tx.flush();
            assertTrue(lag.linkBandwidthBps() > 0 || lag.rttUs() > 0, "no librist report: " + lag);

            SocketStats r = rx.socketStats();
            // The transport counts whole 1316-byte blocks as they arrive; the shell may still hold
            // the tail of the last block, which recvBytes() has not returned yet. More blocks can
            // also have arrived while the poll ran, so only the lower bound is exact.
            assertTrue(r.bytesReceived() >= got, r.toString());
            assertTrue(r.packetsReceived() > 0);
            SocketStats s = tx.socketStats();
            assertEquals(handed.get(), s.bytesSent());
            assertTrue(s.packetsSent() > 0);
            assertEquals(0, s.packetsDroppedSend());
            assertSrtStatsRefused(rx, tx);
        }
    }
}
