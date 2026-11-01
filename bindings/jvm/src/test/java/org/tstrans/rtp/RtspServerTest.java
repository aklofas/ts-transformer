package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.syntheticH264Idr;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtspException;
import org.tstrans.mpegts.DataStreamHandle;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;
import org.tstrans.mpegts.VideoStreamHandle;

class RtspServerTest {

    @Test @Timeout(15)
    void startBindsAndReportsLocalAddr() throws Exception {
        try (RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            String addr = s.localAddr();
            assertNotNull(addr, "localAddr should be populated after start()");
            assertTrue(addr.startsWith("127.0.0.1:"));
            ServerStats st = s.stats();
            assertEquals(0L, st.activeSessions());
            assertEquals(0L, st.mounts());
        }
    }

    @Test @Timeout(15)
    void cancelHandleToggles() throws Exception {
        try (RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            try (RtspServerCancelHandle h = s.cancelHandle()) {
                assertFalse(h.isCancelled());
                h.cancel();
                assertTrue(h.isCancelled());
            }
        }
    }

    @Test @Timeout(15)
    void stopIsIdempotent() throws Exception {
        RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"));
        s.stop();
        s.stop(); // no-op second time
        s.close();
    }

    @Test @Timeout(15)
    void authWithoutRealmRejected() {
        RtspServerConfig cfg = RtspServerConfig.builder()
            .bindAddr("127.0.0.1:0")
            .auth(new BasicAuth("u", "p")) // no realm
            .build();
        assertThrows(IllegalArgumentException.class, () -> RtspServer.start(cfg));
    }

    @Test @Timeout(15)
    void startWithBasicAuthSucceeds() throws Exception {
        RtspServerConfig cfg = RtspServerConfig.builder()
            .bindAddr("127.0.0.1:0")
            .auth(new BasicAuth("admin", "secret", "tst"))
            .build();
        try (RtspServer s = RtspServer.start(cfg)) {
            assertNotNull(s.localAddr());
        }
    }

    @Test @Timeout(15)
    void startWithDigestAuthSucceeds() throws Exception {
        RtspServerConfig cfg = RtspServerConfig.builder()
            .bindAddr("127.0.0.1:0")
            .auth(new DigestAuth("admin", "secret", DigestAlgorithm.SHA256, "tst"))
            .build();
        try (RtspServer s = RtspServer.start(cfg)) {
            assertNotNull(s.localAddr());
        }
    }

    /** A DigestAuth client never answers a Basic-only server: the connect fails, no Basic header. */
    @Test @Timeout(15)
    void digestAuthClientRefusesBasicOnlyServer() throws Exception {
        RtspServerConfig cfg = RtspServerConfig.builder()
            .bindAddr("127.0.0.1:0")
            .auth(new BasicAuth("admin", "secret", "tst"))
            .build();
        try (RtspServer s = RtspServer.start(cfg)) {
            s.addUnicastMount("/live", org.tstrans.TestSupport.roundtripConfig());
            String addr = s.localAddr();
            String url = "rtsp://127.0.0.1:" + addr.substring(addr.lastIndexOf(':') + 1) + "/live";
            RtspClientConfig client = RtspClientConfig.builder(url)
                .auth(new DigestAuth("admin", "secret"))
                .build();
            RtspException ex = assertThrows(RtspException.class, () -> RtspClient.connect(client));
            assertTrue(ex.getMessage().contains("DigestOnly"), ex.getMessage());
        }
    }

    @Test @Timeout(15)
    void usingAfterCloseThrows() throws Exception {
        RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"));
        s.close();
        assertThrows(IllegalStateException.class, s::stats);
    }

    /**
     * The mount factories lease the server registry entry (twice on the native side). A server
     * closed before the factory runs must surface a clean {@link IllegalStateException} rather
     * than touching freed memory — the registry makes that deterministic.
     */
    @Test @Timeout(15)
    void addMountAfterCloseThrows() throws Exception {
        RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"));
        s.close();
        MuxerConfig cfg = MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000).addVideo(0x1011, VideoCodec.H264).build();
        assertThrows(IllegalStateException.class, () -> s.addUnicastMount("/live", cfg));
    }

    @Test @Timeout(15)
    void addUnicastMountPushAndStats() throws Exception {
        try (RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            MuxerConfig cfg = MuxerConfig.builder()
                .programNumber(1).pmtPid(0x1000).addVideo(0x1011, VideoCodec.H264).build();
            try (MountHandle m = s.addUnicastMount("/live", cfg)) {
                assertEquals("/live", m.mountPath());
                assertEquals("unicast", m.mountKind());
                assertEquals(1, s.stats().mounts());
                assertTrue(m.videoHandle().isPresent());
                assertEquals(1, m.videoHandles().size());
                MountStats before = m.stats();
                m.pushVideo(syntheticH264Idr(), 0L, true);
                m.flush();
                assertTrue(m.stats().bytesPushed() > before.bytesPushed());
                m.resetStats();
                assertEquals(0L, m.stats().bytesPushed());
            }
        }
    }

    @Test @Timeout(15)
    void addUnicastMountDataPushAndStats() throws Exception {
        try (RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            MuxerConfig cfg = MuxerConfig.builder()
                .programNumber(1).pmtPid(0x1000)
                .addVideo(0x1011, VideoCodec.H264)
                .addData(0x0100, 0xF0, true).build();
            try (MountHandle m = s.addUnicastMount("/data", cfg)) {
                // The config declares one data stream → both accessors surface it.
                assertTrue(m.dataHandle().isPresent());
                assertEquals(1, m.dataHandles().size());
                MountStats before = m.stats();
                // pushData (lone-data-stream shorthand) advances the flow counters.
                m.pushData(new byte[] {(byte) 0xD0, 'D', 'A', 'T', 'A'}, 0L);
                m.flush();
                assertTrue(m.stats().bytesPushed() > before.bytesPushed());
                // pushDataTo with the configured handle also succeeds.
                m.pushDataTo(m.dataHandle().get(), new byte[] {1, 2, 3}, 90_000L);
                // Strict handle decode: a forged/negative handle is rejected with
                // RtspException(MOUNT) in the JNI shim before reaching the mount.
                RtspException forged = assertThrows(RtspException.class,
                    () -> m.pushDataTo(DataStreamHandle.fromRaw(-1L), new byte[] {1}, 0L));
                assertEquals(RtspException.Kind.MOUNT, forged.kind());
            }
        }
    }

    @Test @Timeout(15)
    void addMulticastMountKind() throws Exception {
        try (RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            MuxerConfig cfg = MuxerConfig.builder()
                .programNumber(1).pmtPid(0x1000).addVideo(0x1011, VideoCodec.H264).build();
            try (MountHandle m = s.addMulticastMount("/mc", "239.0.0.1", 5004, cfg)) {
                assertEquals("multicast", m.mountKind());
            }
        }
    }

    @Test @Timeout(15)
    void invalidMountPathThrowsMount() throws Exception {
        try (RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            MuxerConfig cfg = MuxerConfig.builder()
                .programNumber(1).pmtPid(0x1000).addVideo(0x1011, VideoCodec.H264).build();
            RtspException ex = assertThrows(RtspException.class,
                () -> s.addUnicastMount("live", cfg));  // no leading slash
            assertEquals(RtspException.Kind.MOUNT, ex.kind());
        }
    }

    @Test @Timeout(15)
    void duplicateMountThrowsMount() throws Exception {
        try (RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            MuxerConfig cfg = MuxerConfig.builder()
                .programNumber(1).pmtPid(0x1000).addVideo(0x1011, VideoCodec.H264).build();
            try (MountHandle ignored = s.addUnicastMount("/live", cfg)) {
                RtspException ex = assertThrows(RtspException.class,
                    () -> s.addUnicastMount("/live", cfg));
                assertEquals(RtspException.Kind.MOUNT, ex.kind());
            }
        }
    }

    /**
     * Strict handle decode in {@code pushVideoTo}: a negative jlong or a value
     * exceeding {@code u32::MAX} must be rejected with
     * {@code RtspException(MOUNT)} without truncating into a plausible handle.
     * Regression for the pre-3.4 {@code as u32} cast that would silently wrap
     * {@code -1L} to {@code 0xFFFF_FFFF} and {@code 0x1_0000_0000L} to {@code 0}.
     */
    @Test @Timeout(15)
    void pushVideoToRejectsForgedStreamHandle() throws Exception {
        try (RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            MuxerConfig cfg = MuxerConfig.builder()
                .programNumber(1).pmtPid(0x1000)
                .addVideo(0x1011, VideoCodec.H264).build();
            try (MountHandle m = s.addUnicastMount("/v", cfg)) {
                byte[] nal = syntheticH264Idr();
                // Negative jlong: rejected by the u32::try_from leg before try_from_raw.
                RtspException neg = assertThrows(RtspException.class,
                    () -> m.pushVideoTo(VideoStreamHandle.fromRaw(-1L), nal, 0L, true));
                assertEquals(RtspException.Kind.MOUNT, neg.kind(),
                    "negative VideoStreamHandle must raise RtspException(MOUNT)");
                // Out-of-u32 jlong: also rejected by the u32::try_from leg.
                RtspException over = assertThrows(RtspException.class,
                    () -> m.pushVideoTo(VideoStreamHandle.fromRaw(0x1_0000_0000L), nal, 0L, true));
                assertEquals(RtspException.Kind.MOUNT, over.kind(),
                    "out-of-u32 VideoStreamHandle must raise RtspException(MOUNT)");
            }
        }
    }

}
