package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.tstrans.TestSupport.cancelNow;
import static org.tstrans.TestSupport.scaledMs;

import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.util.OptionalInt;
import java.util.concurrent.atomic.AtomicBoolean;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.ValueSource;
import org.tstrans.RistException;
import org.tstrans.TestSupport;

@Timeout(60)
class LocalPortTest {
    @ParameterizedTest
    @ValueSource(strings = {"udp://127.0.0.1:0", "udp://0.0.0.0:0", "udp://[::1]:0"})
    void aUdpReceiverReportsThePortADatagramReaches(String url) throws Exception {
        if (url.contains("[::1]")) TestSupport.assumeIpv6Loopback();
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            Receiver rx = c.add(Receiver.open(url));
            int port = rx.localPort().getAsInt();
            assertTrue(port > 0 && port < 65536, "port " + port);
            InetAddress dest = InetAddress.getByName(url.contains("[::1]") ? "::1" : "127.0.0.1");
            DatagramSocket tx = c.add(new DatagramSocket());
            byte[] ts = new byte[188 * 7];
            for (int i = 0; i < 7; i++) ts[i * 188] = 0x47;
            tx.send(new DatagramPacket(ts, ts.length, dest, port));
            byte[] got = ShellPeers.within(scaledMs(10_000), "recvBytes", rx::recvBytes);
            assertEquals(188, got.length);
        }
    }

    @Test
    void aDemuxReceiverReportsItToo() throws Exception {
        try (DemuxReceiver rx = DemuxReceiver.open("udp://127.0.0.1:0")) {
            assertTrue(rx.localPort().getAsInt() > 0);
        }
    }

    @Test
    void aRistReceiverOnPortZeroReportsThePortASenderReaches() throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            Receiver rx = c.add(Receiver.open("rist://@127.0.0.1:0"));
            int port = rx.localPort().getAsInt();
            assertTrue(port > 0, "port " + port);
            Sender tx = c.add(Sender.open("rist://127.0.0.1:" + port));
            AtomicBoolean done = new AtomicBoolean();
            // The first packets can be lost before the session is up, so keep sending.
            Thread sender = new Thread(() -> {
                try {
                    while (!done.get()) {
                        tx.sendBytes(ShellPeers.TS_BUNDLE);
                        Thread.sleep(20);
                    }
                } catch (Exception ignored) {
                    // the shell was closed under it at teardown
                }
            }, "rist-local-port-sender");
            sender.setDaemon(true);
            sender.start();
            try (ShellPeers.Watchdog watchdog = new ShellPeers.Watchdog(rx.cancelHandle(), scaledMs(10_000))) {
                assertEquals(188, ShellPeers.recvTolerant(rx).length);
            } finally {
                done.set(true);
            }
        }
    }

    @Test
    void aRistSimpleReceiverOnPortZeroIsRefused() {
        RistException e = assertThrows(RistException.class,
            () -> Receiver.open("rist://@127.0.0.1:0?profile=simple"));
        assertEquals(RistException.Kind.INVALID_CONFIG, e.kind());
    }

    @ParameterizedTest
    @ValueSource(strings = {"Receiver", "DemuxReceiver"})
    void localPortAnswersWhileAReceiveIsParked(String shell) throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            ShellPeers.Rx rx = c.add(ShellPeers.openRx(shell, "udp://127.0.0.1:0"));
            try (ShellPeers.Parked parked = new ShellPeers.Parked(rx)) {
                OptionalInt port = ShellPeers.within(scaledMs(1000), "localPort", rx::localPort);
                assertTrue(port.getAsInt() > 0);
                parked.assertStillParked();
                cancelNow(rx::cancelHandle);
                parked.awaitEnd(scaledMs(5_000));
            }
        }
    }

    /** One RTP packet (version 2, payload type 33) carrying seven TS packets. */
    private static byte[] rtpMp2t() {
        byte[] d = new byte[12 + 188 * 7];
        d[0] = (byte) 0x80;
        d[1] = 33;
        for (int i = 0; i < 7; i++) d[12 + i * 188] = 0x47;
        return d;
    }

    @ParameterizedTest
    @ValueSource(strings = {"Receiver", "DemuxReceiver"})
    void localPortOverRtpIsTheBoundPort(String shell) throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            ShellPeers.Rx rx = c.add(ShellPeers.openRx(shell, "rtp://127.0.0.1:0"));
            int port = rx.localPort().getAsInt();
            assertTrue(port > 0 && port < 65536, "port " + port);
            assertEquals(port, rx.localAddr().orElseThrow().getPort());
            if (shell.equals("Receiver")) {
                // A packet sent to that port is the one received.
                DatagramSocket tx = c.add(new DatagramSocket());
                byte[] d = rtpMp2t();
                tx.send(new DatagramPacket(d, d.length, InetAddress.getLoopbackAddress(), port));
                try (ShellPeers.Watchdog w = new ShellPeers.Watchdog(rx.cancelHandle(), scaledMs(10_000))) {
                    rx.recvOnce();
                }
            }
        }
    }

    @ParameterizedTest
    @ValueSource(strings = {"Receiver", "DemuxReceiver"})
    void localPortOverAnSrtListenerAfterTheAccept(String shell) throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            int port = TestSupport.freeUdpPort();
            ShellPeers.Pending<ShellPeers.Rx> listening = ShellPeers.openInBackground(
                () -> ShellPeers.openRx(shell, "srt://127.0.0.1:" + port + "?mode=listener"));
            c.add(ShellPeers.retryCaller(() -> Sender.open("srt://127.0.0.1:" + port + "?mode=caller")));
            ShellPeers.Rx rx = c.add(listening.await());
            // The listener's own port, known only once a caller connected.
            assertEquals(OptionalInt.of(port), rx.localPort());
            assertEquals(port, rx.localAddr().orElseThrow().getPort());
            assertEquals(InetAddress.getByName("127.0.0.1"), rx.localAddr().orElseThrow().getAddress());
        }
    }

    @ParameterizedTest
    @ValueSource(strings = {"Receiver", "DemuxReceiver"})
    void aCallerShellHasNoLocalPort(String shell) throws Exception {
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            ServerSocket server = c.add(new ServerSocket(0, 1, InetAddress.getLoopbackAddress()));
            ShellPeers.Rx rx = c.add(ShellPeers.openRx(shell, "tcp://127.0.0.1:" + server.getLocalPort()));
            assertEquals(OptionalInt.empty(), rx.localPort());
        }
    }

    @Test
    void aFixedPortReadsBack() throws Exception {
        int p = TestSupport.freeUdpPort();
        try (Receiver rx = Receiver.open("udp://127.0.0.1:" + p)) {
            assertEquals(p, rx.localPort().getAsInt());
        }
    }

    @Test
    void aClosedShellThrowsIllegalState() throws Exception {
        Receiver r = Receiver.open("udp://127.0.0.1:0");
        r.close();
        assertThrows(IllegalStateException.class, r::localPort);
        DemuxReceiver d = DemuxReceiver.open("udp://127.0.0.1:0");
        d.close();
        assertThrows(IllegalStateException.class, d::localPort);
    }
}
