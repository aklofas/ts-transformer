package org.tstrans.tcp;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;
import static org.tstrans.TestSupport.cancelNow;
import static org.tstrans.TestSupport.scaledMs;
import static org.tstrans.shells.ShellPeers.within;

import java.io.IOException;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.net.ConnectException;
import java.net.Inet4Address;
import java.net.Inet6Address;
import java.net.InetAddress;
import java.net.SocketTimeoutException;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.NativeHandle;
import org.tstrans.TcpException;
import org.tstrans.TestSupport;
import org.tstrans.shells.CancelHandle;
import org.tstrans.shells.ShellPeers.Cleanup;
import org.tstrans.shells.ShellPeers.Parked;
import org.tstrans.shells.ShellPeers.Watchdog;

@Timeout(60)
class ListenerTest {

    private static final String LOOPBACK_V4 = "tcp://127.0.0.1:0?listen=1";

    /** {@code accept()} parked on a daemon thread; it must end by a cancel or a close. */
    private static Parked parkAccept(Listener l) throws InterruptedException {
        return new Parked(l, l.cancelHandle(), "accept returned a connection with no peer", () -> {
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
            try (Watchdog w = new Watchdog(l.cancelHandle(), scaledMs(10_000))) {
                t = c.add(l.accept());
            }
            assertEquals(client.getLocalSocketAddress(), t.peerAddr());
        }
    }

    @Test
    void ipv6PeerAddressIsAnInet6Address() throws Exception {
        TestSupport.assumeIpv6Loopback();
        try (Cleanup c = new Cleanup()) {
            Listener l = c.add(Listener.open("tcp://[::1]:0?listen=1"));
            java.net.Socket client = c.add(new java.net.Socket(InetAddress.getByName("::1"), l.localPort()));
            Transport t;
            try (Watchdog w = new Watchdog(l.cancelHandle(), scaledMs(10_000))) {
                t = c.add(l.accept());
            }
            assertEquals(client.getLocalSocketAddress(), t.peerAddr());
            assertInstanceOf(Inet6Address.class, t.peerAddr().getAddress());
        }
    }

    @Test
    void anIpv4PeerOnADualStackListenerIsAnInet4Address() throws Exception {
        TestSupport.assumeIpv6Loopback();
        try (Cleanup c = new Cleanup()) {
            Listener l = c.add(Listener.open("tcp://[::]:0?listen=1"));
            java.net.Socket client;
            try {
                client = c.add(new java.net.Socket(InetAddress.getByAddress(new byte[] {127, 0, 0, 1}), l.localPort()));
            } catch (ConnectException e) {
                // Linux accepts IPv4 on a [::] socket by default; other platforms may not.
                if (TestSupport.isLinux()) throw e;
                System.out.println("skipping: this platform's [::] listener takes no IPv4: " + e);
                org.junit.jupiter.api.Assumptions.assumeTrue(false, "no dual-stack listener: " + e);
                return;
            }
            Transport t;
            try (Watchdog w = new Watchdog(l.cancelHandle(), scaledMs(10_000))) {
                t = c.add(l.accept());
            }
            assertInstanceOf(Inet4Address.class, t.peerAddr().getAddress());
            assertEquals(client.getLocalSocketAddress(), t.peerAddr());
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
            try (CancelHandle h = l.cancelHandle()) {
                try (Parked p = parkAccept(l)) {
                    h.cancel();
                    assertKind(TcpException.Kind.CLOSED, p.awaitEnd(scaledMs(2000)));
                }
            }
        }
    }

    @Test
    void afterACancelEveryAcceptThrowsClosed() throws Exception {
        try (Listener l = Listener.open(LOOPBACK_V4)) {
            cancelNow(l::cancelHandle);
            for (int i = 0; i < 3; i++) {
                assertKind(TcpException.Kind.CLOSED, assertThrows(TcpException.class, l::accept));
            }
        }
    }

    @Test
    void closeEndsAParkedAcceptAndIsIdempotent() throws Exception {
        Listener l = Listener.open(LOOPBACK_V4);
        try (Parked p = parkAccept(l)) {
            // Bounded: a close that took the listener without cancelling first would wait on
            // the parked accept until Parked's own watchdog cancelled it.
            long t0 = System.nanoTime();
            l.close();
            long closeMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
            assertTrue(closeMs < scaledMs(2000), "close() took " + closeMs + " ms");
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

    /**
     * A {@code close()} that lands between {@code accept()}'s open check and its native finds
     * no listener: that is a closed object, {@code IllegalStateException}, not the
     * {@code CLOSED} of a call that was already parked.
     */
    @Test
    void acceptOnAListenerClosedUnderneathThrowsIllegalState() throws Exception {
        Listener l = Listener.open(LOOPBACK_V4);
        org.tstrans.TestSupport.closeUnderneath(l);
        try {
            IllegalStateException e = assertThrows(IllegalStateException.class, l::accept);
            assertEquals("Listener is closed", e.getMessage());
        } finally {
            org.tstrans.TestSupport.forgetKey(l);
        }
    }

    /**
     * A {@code close()} can land between {@code cancelHandle()}'s open check and its native
     * lookup; the native is driven directly with the closed listener's key, which no other
     * listener reuses, so that window is hit every time.
     */
    @Test
    void cancelHandleOnAListenerClosedUnderneathThrows() throws Exception {
        Listener l = Listener.open(LOOPBACK_V4);
        Method peek = NativeHandle.class.getDeclaredMethod("peekHandle");
        peek.setAccessible(true);
        long key = (long) peek.invoke(l);
        l.close();
        Method native_ = Listener.class.getDeclaredMethod("nCancelHandle", long.class);
        native_.setAccessible(true);
        InvocationTargetException ite = assertThrows(InvocationTargetException.class,
            () -> native_.invoke(null, key), "a missing listener must not yield a handle");
        assertInstanceOf(IllegalStateException.class, ite.getCause());
    }

    @Test
    void localPortAnswersWhileAcceptIsParked() throws Exception {
        try (Listener l = Listener.open(LOOPBACK_V4)) {
            int port = l.localPort();
            try (Parked p = parkAccept(l)) {
                assertEquals(port, (int) within(scaledMs(1000), "localPort()", l::localPort));
                p.assertStillParked();
                cancelNow(l::cancelHandle);
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
            try (Watchdog w = new Watchdog(l.cancelHandle(), scaledMs(10_000))) {
                t = c.add(l.accept());
            }
            l.close();
            assertEquals(client.getLocalSocketAddress(), t.peerAddr());
            // Still connected: a read on the client waits for data instead of seeing EOF or a reset.
            client.setSoTimeout((int) scaledMs(300));
            assertThrows(SocketTimeoutException.class, () -> client.getInputStream().read());
            t.close();
            t.close();
            assertThrows(IllegalStateException.class, t::peerAddr);
        }
    }
}
