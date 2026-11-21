package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.freeUdpPort;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.syntheticH264Idr;
import static org.tstrans.TestSupport.waitUntil;

import java.util.concurrent.atomic.AtomicBoolean;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.SrtException;
import org.tstrans.shells.Receiver;

/**
 * The URL key {@code x-silentpeertimeout} through the JVM binding: it arms silent-peer
 * detection on a managed SRT sender opened from a URL, and every other SRT open refuses
 * it as {@code CONFIG_INVALID}, naming the key.
 */
class SrtSilentPeerKeyTest {

    private static final String KEY = "x-silentpeertimeout";

    /** {@code n} TS packets on the null PID, enough for a plain byte receiver to hand out. */
    private static byte[] tsNullPacketsOnPid1fff(int n) {
        byte[] one = {0x47, 0x1F, (byte) 0xFF, 0x10};
        byte[] b = new byte[188 * n];
        for (int i = 0; i < n; i++) {
            System.arraycopy(one, 0, b, i * 188, one.length);
        }
        return b;
    }

    private static void assertRefusedNamingKey(SrtException e) {
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind(), e.getMessage());
        assertTrue(e.getMessage().contains(KEY), "message must name the key: " + e.getMessage());
    }

    /** A plain listener whose first accepted connection is read on a daemon thread. */
    private static final class Peer implements AutoCloseable {
        final Listener listener;
        final AtomicBoolean gotBytes = new AtomicBoolean();
        final Thread thread;

        Peer() throws SrtException {
            listener = new Builder("srt://127.0.0.1:0?mode=listener").listener().listen();
            thread = new Thread(() -> {
                try (Socket s = listener.accept(10_000); Receiver rx = s.intoReceiver()) {
                    byte[] got;
                    while ((got = rx.recvBytes()) != null) {
                        if (got.length > 0) {
                            gotBytes.set(true);
                            return;
                        }
                    }
                } catch (Exception ignored) {
                    // the test's own assertion reports a peer that got nothing
                }
            });
            thread.setDaemon(true);
            thread.start();
        }

        String url(String query) throws SrtException {
            return "srt://127.0.0.1:" + listener.localAddr().getPort() + "?mode=caller&latency=120" + query;
        }

        @Override public void close() {
            listener.close();
            thread.interrupt();
        }
    }

    @Test
    @Timeout(60)
    void managedSenderOpensWithTheKeyAndASendArrives() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (as the other managed pairs)");
        try (Peer peer = new Peer(); ManagedSender tx = ManagedSender.fromUrl(peer.url("&" + KEY + "=1500"))) {
            byte[] chunk = tsNullPacketsOnPid1fff(7);
            for (int i = 0; i < 100 && !peer.gotBytes.get(); i++) {
                tx.sendBytes(chunk);
                waitUntil(peer.gotBytes::get, 50);
            }
            assertTrue(peer.gotBytes.get(), "the peer must receive a send from the armed managed sender");
            assertEquals(0L, tx.reconnectStats().reconnectAttempts(), "no reconnect on a healthy link");
        }
    }

    @Test
    @Timeout(60)
    void managedMuxSenderOpensWithTheKeyAndASendArrives() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (as the other managed pairs)");
        try (Peer peer = new Peer();
             ManagedMuxSender tx = ManagedMuxSender.fromUrl(peer.url("&" + KEY + "=1500"), roundtripConfig())) {
            long pts = 0;
            for (int i = 0; i < 100 && !peer.gotBytes.get(); i++) {
                for (int k = 0; k < 6; k++, pts += 3000L) {
                    tx.sendVideo(syntheticH264Idr(), pts, true);
                }
                waitUntil(peer.gotBytes::get, 50);
            }
            assertTrue(peer.gotBytes.get(), "the peer must receive a send from the armed managed mux sender");
        }
    }

    @Test
    @Timeout(60)
    void managedSendersRefuseAWindowBelowTheFloor() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (as the other managed pairs)");
        try (Peer peer = new Peer()) {
            assertRefusedNamingKey(assertThrows(SrtException.class,
                () -> ManagedSender.fromUrl(peer.url("&" + KEY + "=999"))));
            assertRefusedNamingKey(assertThrows(SrtException.class,
                () -> ManagedMuxSender.fromUrl(peer.url("&" + KEY + "=999"), roundtripConfig())));
        }
    }

    @Test
    @Timeout(60)
    void managedReceiverRefusesTheKey() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (as the other managed pairs)");
        int port = freeUdpPort();
        assertRefusedNamingKey(assertThrows(SrtException.class,
            () -> ManagedReceiver.fromUrl("srt://127.0.0.1:" + port + "?mode=listener&" + KEY + "=1500")));
    }

    @Test
    @Timeout(60)
    void lowLevelBuilderRefusesTheKeyOnConnectAndListen() {
        assertRefusedNamingKey(assertThrows(SrtException.class,
            () -> new Builder("srt://127.0.0.1:9?mode=caller&" + KEY + "=1500").caller().connect()));
        assertRefusedNamingKey(assertThrows(SrtException.class,
            () -> new Builder("srt://127.0.0.1:0?mode=listener&" + KEY + "=1500").listener().listen()));
    }
}
