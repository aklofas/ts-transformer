package org.tstrans.webrtc;

import java.net.Inet4Address;
import java.net.InetSocketAddress;
import java.time.Duration;
import java.util.Optional;
import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.hostPort;

import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;

class WhepServerTest {
    /** A minimal Annex-B access unit: SPS, PPS and an IDR slice header. */
    static byte[] tinyIdrAu() {
        return new byte[] {
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f, (byte) 0xe9, 0x40,
            0, 0, 0, 1, 0x68, (byte) 0xce, 0x38, (byte) 0x80,
            0, 0, 0, 1, 0x65, (byte) 0x88, (byte) 0x84, 0x00, 0x33,
        };
    }

    @Test @Timeout(15)
    void localAddrAndMediaAddrAreInetSocketAddresses() throws Exception {
        WhepServer s = WhepServer.bind("whep://127.0.0.1:0");
        for (InetSocketAddress a : new InetSocketAddress[] {s.localAddr(), s.mediaAddr()}) {
            assertInstanceOf(Inet4Address.class, a.getAddress());
            assertEquals("127.0.0.1", a.getAddress().getHostAddress());
            assertTrue(a.getPort() > 0, "the kernel's port for :0");
        }
        s.close();
        assertThrows(IllegalStateException.class, s::localAddr);
        assertThrows(IllegalStateException.class, s::mediaAddr);
    }

    /** The record of a viewer that has not connected: no address, no time connected. */
    @Test
    void sessionStatsAreEmptyBeforeConnect() {
        SessionStats before = SessionStats.fromNative("s1", "/cam", null, "new", -1L,
            0, 0, 0, 0, 0, 0, 0);
        assertEquals(Optional.empty(), before.remoteAddr());
        assertEquals(Optional.empty(), before.connectedFor());
        assertEquals("new", before.iceState());
        SessionStats after = SessionStats.fromNative("s1", "/cam",
            new byte[] {(byte) 192, 0, 2, 7, 0x13, (byte) 0x88}, "connected", 1_500_000_000L,
            1, 2, 3, 4, 5, 6, 7);
        assertEquals(Optional.of(new InetSocketAddress("192.0.2.7", 5000)), after.remoteAddr());
        assertEquals(Optional.of(Duration.ofMillis(1500)), after.connectedFor());
        assertEquals(7L, after.dcMessagesDropped());
    }

    @Test @Timeout(15)
    void bindMountPushStatsRemove() throws Exception {
        try (WhepServer s = WhepServer.bind(new WhepServerConfig("whep://127.0.0.1:0"))) {
            assertNotEquals(0, s.localAddr().getPort(), "the kernel-picked port is reported");
            assertEquals(0L, s.stats().mounts());

            try (MountHandle m = s.addMount("/cam", MountConfig.defaults())) {
                assertEquals("/cam", m.mountPath());
                m.pushVideo(tinyIdrAu(), 0L, true);
                assertEquals(1L, s.stats().mounts());
                assertEquals(1L, m.stats().framesPushed());
                assertEquals(1L, m.stats().keyframesPushed());
                assertEquals(0L, m.stats().viewers());
                assertTrue(m.sessions().isEmpty());
                s.removeMount("/cam");
                assertEquals(0L, s.stats().mounts());
            }
        }
    }

    /**
     * Neither trickle ICE nor ICE restart is supported; a client that sees
     * PATCH in the method list switches to trickle and then needs an ETag.
     */
    @Test @Timeout(15)
    void preflightDoesNotAdvertisePatch() throws Exception {
        try (WhepServer s = WhepServer.bind("whep://127.0.0.1:0")) {
            HttpClient client = HttpClient.newBuilder().version(HttpClient.Version.HTTP_1_1).build();
            HttpRequest req = HttpRequest.newBuilder(URI.create("http://" + hostPort(s.localAddr()) + "/cam"))
                    .method("OPTIONS", HttpRequest.BodyPublishers.noBody())
                    .build();
            HttpResponse<Void> resp = client.send(req, HttpResponse.BodyHandlers.discarding());
            assertEquals(204, resp.statusCode());
            assertEquals(
                    "POST, DELETE, OPTIONS",
                    resp.headers().firstValue("Access-Control-Allow-Methods").orElse(null));
        }
    }

    @Test @Timeout(15)
    void defaultsMirrorTheRustDefaults() {
        MountConfig d = MountConfig.defaults();
        assertEquals(MountKind.ELEMENTARY, d.kind());
        assertEquals(org.tstrans.mpegts.VideoCodec.H264, d.video());
        assertEquals(AudioPolicy.DROP, d.audio());
        assertEquals(KlvCarriage.SEI_AND_DATA_CHANNEL, d.klv());
        assertEquals(64 * 1024, d.seiBudgetBytes());
        assertEquals(2048, d.viewerQueuePackets());
    }

    @Test @Timeout(15)
    void stopIsIdempotentAndCloseAfterStopIsFine() throws Exception {
        WhepServer s = WhepServer.bind("whep://127.0.0.1:0");
        s.stop();
        s.stop();
        s.close();
        s.close();
    }

    @Test @Timeout(15)
    void useAfterCloseThrowsIllegalState() throws Exception {
        WhepServer s = WhepServer.bind("whep://127.0.0.1:0");
        MountHandle m = s.addMount("/cam", MountConfig.defaults());
        s.close();
        assertThrows(IllegalStateException.class, s::stats);
        assertThrows(IllegalStateException.class, s::localAddr);
        assertThrows(IllegalStateException.class, s::stop);
        assertThrows(IllegalStateException.class,
            () -> s.addMount("/other", MountConfig.defaults()));
        m.close();
        assertThrows(IllegalStateException.class, m::stats);
        assertThrows(IllegalStateException.class, () -> m.pushVideo(tinyIdrAu(), 0L, true));
    }

    @Test @Timeout(15)
    void mountConfigRejectsNulls() {
        assertThrows(NullPointerException.class, () -> new WhepServerConfig(null));
        assertThrows(NullPointerException.class,
            () -> new MountConfig(null, org.tstrans.mpegts.VideoCodec.H264, AudioPolicy.DROP,
                KlvCarriage.SEI_ONLY, 1024, 64));
    }
}
