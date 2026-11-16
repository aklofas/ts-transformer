package org.tstrans.tcp;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;
import static org.tstrans.TestSupport.scaledMs;
import static org.tstrans.shells.ShellPeers.within;

import java.io.IOException;
import java.net.InetAddress;
import java.net.SocketTimeoutException;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.TcpException;
import org.tstrans.TestSupport;
import org.tstrans.shells.CancelHandle;
import org.tstrans.shells.ShellPeers.Cleanup;
import org.tstrans.shells.ShellPeers.Parked;

@Timeout(60)
class ListenerTest {

    private static final String LOOPBACK_V4 = "tcp://127.0.0.1:0?listen=1";

    /**
     * Cancels the listener through a handle taken before the call under test, if that call is
     * still running after {@code ms}: a hung accept on the main thread would wedge the runner.
     */
    private static final class Watchdog implements AutoCloseable {
        private final CountDownLatch finished = new CountDownLatch(1);

        Watchdog(Listener l, long ms) {
            CancelHandle h = l.cancelHandle();
            Thread t = new Thread(() -> {
                try {
                    if (!finished.await(ms, TimeUnit.MILLISECONDS)) h.cancel();
                } catch (InterruptedException ignored) {
                    // the test finished first
                }
            }, "tcp-listener-watchdog");
            t.setDaemon(true);
            t.start();
        }

        @Override public void close() { finished.countDown(); }
    }

    /** {@code accept()} parked on a daemon thread; it must end by a cancel or a close. */
    private static Parked parkAccept(Listener l) throws InterruptedException {
        return new Parked(l.cancelHandle(), "accept returned a connection with no peer", () -> {
            l.accept().close();
            return null;
        });
    }

    private static void assertKind(TcpException.Kind kind, Throwable e) {
        TcpException t = assertInstanceOf(TcpException.class, e);
        assertEquals(kind, t.kind(), t.getMessage());
    }

    @Test
    void openOnPortZeroReportsThePortAClientReaches() throws Exception {
        try (Cleanup c = new Cleanup()) {
            Listener l = c.add(Listener.open(LOOPBACK_V4));
            int port = l.localPort();
            assertTrue(port > 0, "port " + port);
            java.net.Socket client = c.add(new java.net.Socket(InetAddress.getByName("127.0.0.1"), port));
            Transport t;
            try (Watchdog w = new Watchdog(l, scaledMs(10_000))) {
                t = c.add(l.accept());
            }
            assertEquals("127.0.0.1:" + client.getLocalPort(), t.peerAddr());
        }
    }

    @Test
    void ipv6PeerAddressIsBracketed() throws Exception {
        TestSupport.assumeIpv6Loopback();
        try (Cleanup c = new Cleanup()) {
            Listener l = c.add(Listener.open("tcp://[::1]:0?listen=1"));
            java.net.Socket client = c.add(new java.net.Socket(InetAddress.getByName("::1"), l.localPort()));
            Transport t;
            try (Watchdog w = new Watchdog(l, scaledMs(10_000))) {
                t = c.add(l.accept());
            }
            assertEquals("[::1]:" + client.getLocalPort(), t.peerAddr());
        }
    }

    @Test
    void aUrlWithoutListenIsRefused() {
        assertKind(TcpException.Kind.URL,
            assertThrows(TcpException.class, () -> Listener.open("tcp://127.0.0.1:0")));
    }

    @Test
    void aHostNameIsRefused() {
        assertKind(TcpException.Kind.URL,
            assertThrows(TcpException.class, () -> Listener.open("tcp://localhost:0?listen=1")));
    }

    @Test
    void aPortInUseIsAnIoError() throws Exception {
        try (Listener first = Listener.open(LOOPBACK_V4)) {
            String url = "tcp://127.0.0.1:" + first.localPort() + "?listen=1";
            assertKind(TcpException.Kind.IO, assertThrows(TcpException.class, () -> Listener.open(url)));
        }
    }

    @Test
    void cancelEndsAParkedAcceptWithClosed() throws Exception {
        try (Listener l = Listener.open(LOOPBACK_V4)) {
            CancelHandle h = l.cancelHandle();
            try (Parked p = parkAccept(l)) {
                h.cancel();
                assertKind(TcpException.Kind.CLOSED, p.awaitEnd(scaledMs(2000)));
            }
        }
    }

    @Test
    void afterACancelEveryAcceptThrowsClosed() throws Exception {
        try (Listener l = Listener.open(LOOPBACK_V4)) {
            l.cancelHandle().cancel();
            for (int i = 0; i < 3; i++) {
                assertKind(TcpException.Kind.CLOSED, assertThrows(TcpException.class, l::accept));
            }
        }
    }

    @Test
    void closeEndsAParkedAcceptAndIsIdempotent() throws Exception {
        Listener l = Listener.open(LOOPBACK_V4);
        try (Parked p = parkAccept(l)) {
            l.close();
            Throwable e = p.awaitEnd(scaledMs(2000));
            if (!(e instanceof IllegalStateException)) assertKind(TcpException.Kind.CLOSED, e);
        } finally {
            l.close();
        }
        l.close();
    }

    @Test
    void afterCloseEveryCallIsIllegalState() throws Exception {
        Listener l = Listener.open(LOOPBACK_V4);
        l.close();
        assertThrows(IllegalStateException.class, l::accept);
        assertThrows(IllegalStateException.class, l::localPort);
        assertThrows(IllegalStateException.class, l::cancelHandle);
    }

    @Test
    void localPortAnswersWhileAcceptIsParked() throws Exception {
        try (Listener l = Listener.open(LOOPBACK_V4)) {
            int port = l.localPort();
            try (Parked p = parkAccept(l)) {
                assertEquals(port, (int) within(scaledMs(1000), "localPort()", l::localPort));
                l.cancelHandle().cancel();
                assertKind(TcpException.Kind.CLOSED, p.awaitEnd(scaledMs(2000)));
            }
        }
    }

    @Test
    void aQueuedConnectionIsNotHandedOutAfterClose() throws Exception {
        try (Cleanup c = new Cleanup()) {
            Listener l = c.add(Listener.open(LOOPBACK_V4));
            java.net.Socket client = c.add(new java.net.Socket(InetAddress.getByName("127.0.0.1"), l.localPort()));
            l.close();
            client.setSoTimeout((int) scaledMs(2000));
            try {
                assertEquals(-1, client.getInputStream().read(), "the queued client read data");
            } catch (SocketTimeoutException e) {
                fail("the queued connection stayed open after the listener closed");
            } catch (IOException e) {
                // reset by the kernel when the listening socket closed
            }
            assertThrows(IllegalStateException.class, l::accept);
        }
    }

    @Test
    void anAcceptedTransportOutlivesItsListener() throws Exception {
        try (Cleanup c = new Cleanup()) {
            Listener l = c.add(Listener.open(LOOPBACK_V4));
            java.net.Socket client = c.add(new java.net.Socket(InetAddress.getByName("127.0.0.1"), l.localPort()));
            Transport t;
            try (Watchdog w = new Watchdog(l, scaledMs(10_000))) {
                t = c.add(l.accept());
            }
            l.close();
            assertEquals("127.0.0.1:" + client.getLocalPort(), t.peerAddr());
            t.close();
            t.close();
            assertThrows(IllegalStateException.class, t::peerAddr);
        }
    }
}
