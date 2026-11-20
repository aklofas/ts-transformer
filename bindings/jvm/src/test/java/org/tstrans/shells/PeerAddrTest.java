package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.tstrans.TestSupport.scaledMs;

import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.util.Optional;
import java.util.concurrent.atomic.AtomicBoolean;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.ValueSource;

@Timeout(60)
class PeerAddrTest {
    private static final int TAG_OFFSET = 4;
    private static final byte TAG_A = 'A';
    private static final byte TAG_B = 'B';
    /** Delivered packets the negative claims rest on. */
    private static final int ENOUGH = 200;

    private static InetAddress loopbackV4() throws Exception {
        return InetAddress.getByAddress(new byte[] {127, 0, 0, 1});
    }

    /** {@code packets} TS packets, each carrying {@code tag} at {@link #TAG_OFFSET}. */
    private static byte[] tagged(byte tag, int packets) {
        byte[] ts = new byte[188 * packets];
        for (int i = 0; i < packets; i++) {
            ts[i * 188] = 0x47;
            ts[i * 188 + TAG_OFFSET] = tag;
        }
        return ts;
    }

    private static void send(DatagramSocket s, InetAddress dest, int port, byte[] ts) throws Exception {
        s.send(new DatagramPacket(ts, ts.length, dest, port));
    }

    /**
     * Poll {@code rx.peerAddr()} until it is present or {@code ms} elapses; each read must answer
     * within {@code perReadMs}.
     */
    private static Optional<InetSocketAddress> awaitPeer(ShellPeers.Rx rx, long ms, long perReadMs)
            throws Exception {
        long deadline = System.nanoTime() + ms * 1_000_000L;
        while (true) {
            Optional<InetSocketAddress> p = ShellPeers.within(perReadMs, "peerAddr", rx::peerAddr);
            if (p.isPresent() || System.nanoTime() >= deadline) return p;
            Thread.sleep(10);
        }
    }

    /** Keeps calling {@code send} every few milliseconds on a daemon thread until closed. */
    private static final class Repeater implements AutoCloseable {
        private final AtomicBoolean done = new AtomicBoolean();

        Repeater(String name, Sending send) {
            Thread t = new Thread(() -> {
                try {
                    while (!done.get()) {
                        send.run();
                        Thread.sleep(2);
                    }
                } catch (Exception ignored) {
                    // closed under it at teardown
                }
            }, name);
            t.setDaemon(true);
            t.start();
        }

        @Override public void close() { done.set(true); }
    }

    @FunctionalInterface
    private interface Sending {
        void run() throws Exception;
    }

    @ParameterizedTest
    @ValueSource(strings = {"Receiver", "DemuxReceiver"})
    void udpPeerAddrIsEmptyThenTheSendersAddress(String shell) throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            ShellPeers.Rx rx = c.add(ShellPeers.openRx(shell, "udp://127.0.0.1:0"));
            assertEquals(Optional.empty(), rx.peerAddr());
            DatagramSocket tx = c.add(new DatagramSocket(new InetSocketAddress(loopbackV4(), 0)));
            try (ShellPeers.Parked parked = new ShellPeers.Parked(rx)) {
                send(tx, loopbackV4(), rx.localPort().getAsInt(), tagged(TAG_A, 7));
                Optional<InetSocketAddress> peer = awaitPeer(rx, scaledMs(10_000), scaledMs(10_000));
                assertEquals(Optional.of(new InetSocketAddress(loopbackV4(), tx.getLocalPort())), peer);
                assertInstanceOf(Inet4Address.class, peer.get().getAddress());
                c.add(rx.cancelHandle()).cancel();
                parked.awaitEnd(scaledMs(5_000));
            }
        }
    }

    @Test
    void peerAddrDoesNoNameLookup() throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            Receiver rx = c.add(Receiver.open("udp://127.0.0.1:0"));
            DatagramSocket tx = c.add(new DatagramSocket(new InetSocketAddress(loopbackV4(), 0)));
            send(tx, loopbackV4(), rx.localPort().getAsInt(), tagged(TAG_A, 7));
            ShellPeers.within(scaledMs(10_000), "recvBytes", rx::recvBytes);
            InetSocketAddress peer = rx.peerAddr().get();
            assertFalse(peer.isUnresolved());
            assertEquals("127.0.0.1", peer.getHostString());
            // No host name was attached: toString() prints only "/literal:port".
            assertEquals("/127.0.0.1:" + tx.getLocalPort(), peer.toString());
        }
    }

    /**
     * Start B, then receive until the pin has refused something and {@code ENOUGH} packets came
     * after that; none is B's, and the refusals kept rising across that window, so B was sending
     * while A's packets arrived.
     */
    private static void assertBKeptOut(Receiver rx, Runnable startB) throws Exception {
        long atWindowStart = 0;
        try (ShellPeers.Watchdog w = new ShellPeers.Watchdog(rx.cancelHandle(), scaledMs(30_000))) {
            startB.run();
            int delivered = 0;
            while (delivered < ENOUGH) {
                byte[] p = ShellPeers.recvTolerant(rx);
                assertEquals(TAG_A, p[TAG_OFFSET], "a packet from the second sender was delivered");
                if (atWindowStart == 0) {
                    atWindowStart = rx.sourceRejected();
                } else {
                    delivered++;
                }
            }
        }
        assertTrue(rx.sourceRejected() > atWindowStart,
            "no refusal during the window: " + atWindowStart + " -> " + rx.sourceRejected());
    }

    @Test
    void udpSourceFirstDropsAndCountsTheSecondSender() throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            ShellPeers.TwoSources two = ShellPeers.twoUdpSources(c);
            Receiver rx = c.add(Receiver.open("udp://" + two.receiverHost() + ":0?source=first"));
            int port = rx.localPort().getAsInt();
            Repeater a = c.add(new Repeater("udp-sender-a",
                () -> send(two.a(), two.aDest(), port, tagged(TAG_A, 7))));
            assertEquals(TAG_A, ShellPeers.within(scaledMs(10_000), "recvBytes", rx::recvBytes)[TAG_OFFSET]);
            assertEquals(0, rx.sourceRejected());
            assertBKeptOut(rx, () -> c.add(new Repeater("udp-sender-b",
                () -> send(two.b(), two.bDest(), port, tagged(TAG_B, 7)))));
            assertEquals(two.a().getLocalSocketAddress(), rx.peerAddr().get());
            a.close();
        }
    }

    @Test
    void udpSourceIpDropsAndCountsTheSecondSender() throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            ShellPeers.TwoSources two = ShellPeers.twoUdpSources(c);
            Receiver rx = c.add(Receiver.open("udp://" + two.receiverHost() + ":0?source=127.0.0.1"));
            int port = rx.localPort().getAsInt();
            // B starts first: the pin needs no first datagram.
            c.add(new Repeater("udp-sender-b", () -> send(two.b(), two.bDest(), port, tagged(TAG_B, 7))));
            assertBKeptOut(rx, () -> c.add(new Repeater("udp-sender-a",
                () -> send(two.a(), two.aDest(), port, tagged(TAG_A, 7)))));
            assertEquals(two.a().getLocalSocketAddress(), rx.peerAddr().get());
        }
    }

    /** A RIST sender shell sending {@code tag}-ed TS until closed. */
    private static Repeater ristSender(ShellPeers.Cleanup c, String url, byte tag) throws Exception {
        Sender tx = c.add(Sender.open(url));
        byte[] ts = tagged(tag, 7);
        return c.add(new Repeater("rist-sender-" + (char) tag, () -> tx.sendBytes(ts)));
    }

    @Test
    void ristPeerAddrIsTheSendersAddress() throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            Receiver rx = c.add(Receiver.open("rist://@127.0.0.1:0?buffer=100"));
            assertEquals(Optional.empty(), rx.peerAddr());
            ristSender(c, "rist://127.0.0.1:" + rx.localPort().getAsInt() + "?buffer=100", TAG_A);
            try (ShellPeers.Watchdog w = new ShellPeers.Watchdog(rx.cancelHandle(), scaledMs(10_000))) {
                assertEquals(TAG_A, ShellPeers.recvTolerant(rx)[TAG_OFFSET]);
            }
            InetSocketAddress peer = rx.peerAddr().get();
            assertEquals(loopbackV4(), peer.getAddress());
            assertTrue(peer.getPort() > 0, "port " + peer.getPort());
            assertEquals(0, rx.sourceRejected());
        }
    }

    @Test
    void ristSourceFirstKeepsTheSecondSenderOut() throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            ShellPeers.SecondSource second = ShellPeers.secondSource(c);
            Receiver rx = c.add(Receiver.open(
                "rist://@" + second.receiverHost() + ":0?buffer=100&source=first"));
            int port = rx.localPort().getAsInt();
            ristSender(c, "rist://127.0.0.1:" + port + "?buffer=100", TAG_A);
            try (ShellPeers.Watchdog w = new ShellPeers.Watchdog(rx.cancelHandle(), scaledMs(10_000))) {
                assertEquals(TAG_A, ShellPeers.recvTolerant(rx)[TAG_OFFSET]);
            }
            // A sender shell cannot choose its source address: B goes out through the relay.
            ShellPeers.UdpRelay relay = c.add(new ShellPeers.UdpRelay(
                second.socket(), new InetSocketAddress(second.dest(), port)));
            assertBKeptOut(rx, () -> {
                try {
                    ristSender(c, "rist://127.0.0.1:" + relay.frontPort() + "?buffer=100", TAG_B);
                } catch (Exception e) {
                    throw new AssertionError("the second RIST sender did not open", e);
                }
            });
            assertTrue(relay.forwarded() > 0, "the relay forwarded nothing");
            assertEquals(loopbackV4(), rx.peerAddr().get().getAddress());
        }
    }

    @ParameterizedTest
    @ValueSource(strings = {"Receiver", "DemuxReceiver"})
    void peerAddrAnswersWhileAReceiveIsParked(String shell) throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            ShellPeers.Rx rx = c.add(ShellPeers.openRx(shell, "udp://127.0.0.1:0"));
            DatagramSocket tx = c.add(new DatagramSocket(new InetSocketAddress(loopbackV4(), 0)));
            try (ShellPeers.Parked parked = new ShellPeers.Parked(rx)) {
                assertEquals(Optional.empty(),
                    ShellPeers.within(scaledMs(500), "peerAddr", rx::peerAddr));
                assertEquals(0L, (long) ShellPeers.within(scaledMs(500), "sourceRejected", rx::sourceRejected));
                // One packet: too few for the shell's TS sync to lock, so the receive stays parked.
                send(tx, loopbackV4(), rx.localPort().getAsInt(), tagged(TAG_A, 1));
                Optional<InetSocketAddress> peer = awaitPeer(rx, scaledMs(10_000), scaledMs(500));
                assertEquals(Optional.of(new InetSocketAddress(loopbackV4(), tx.getLocalPort())), peer);
                parked.assertStillParked();
                c.add(rx.cancelHandle()).cancel();
                parked.awaitEnd(scaledMs(5_000));
            }
        }
    }

    @ParameterizedTest
    @ValueSource(strings = {"Receiver", "DemuxReceiver"})
    void aShellOverTcpOrSrtReportsNone(String shell) throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            for (String scheme : new String[] {"tcp", "srt"}) {
                ShellPeers.Rx rx = ShellPeers.receiverWithPeer(shell, scheme, c);
                assertEquals(Optional.empty(), rx.peerAddr(), scheme);
                assertEquals(0, rx.sourceRejected(), scheme);
                if (scheme.equals("tcp")) {
                    assertEquals(Optional.empty(), rx.localAddr(), scheme);
                } else {
                    // An srt:// listener reports the accepted connection's local address.
                    assertEquals(rx.localPort().getAsInt(), rx.localAddr().orElseThrow().getPort(), scheme);
                }
            }
        }
    }

    @Test
    void closedShellThrows() throws Exception {
        Receiver r = Receiver.open("udp://127.0.0.1:0");
        r.close();
        assertThrows(IllegalStateException.class, r::peerAddr);
        assertThrows(IllegalStateException.class, r::sourceRejected);
        assertThrows(IllegalStateException.class, r::localAddr);
        DemuxReceiver d = DemuxReceiver.open("udp://127.0.0.1:0");
        d.close();
        assertThrows(IllegalStateException.class, d::peerAddr);
        assertThrows(IllegalStateException.class, d::sourceRejected);
        assertThrows(IllegalStateException.class, d::localAddr);
    }

    @ParameterizedTest
    @ValueSource(strings = {"Receiver", "DemuxReceiver"})
    void localAddrIsTheBoundAddress(String shell) throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            ShellPeers.Rx rx = c.add(ShellPeers.openRx(shell, "udp://127.0.0.1:0"));
            InetSocketAddress local = rx.localAddr().get();
            assertEquals(rx.localPort().getAsInt(), local.getPort());
            assertEquals(loopbackV4(), local.getAddress());
            ServerSocket server = c.add(new ServerSocket(0, 1, loopbackV4()));
            ShellPeers.Rx caller = c.add(ShellPeers.openRx(shell, "tcp://127.0.0.1:" + server.getLocalPort()));
            assertTrue(caller.localPort().isEmpty());
            assertEquals(Optional.empty(), caller.localAddr());
        }
    }
}
