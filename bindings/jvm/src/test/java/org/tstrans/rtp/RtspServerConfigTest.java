package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;

class RtspServerConfigTest {

    @Test
    void defaultsMatchTstPy() {
        RtspServerConfig c = RtspServerConfig.of("0.0.0.0:8554");
        assertEquals("0.0.0.0:8554", c.bindAddr());
        assertTrue(c.auth().isEmpty());
        assertEquals(100, c.maxSessions());
        assertEquals(60, c.sessionTimeoutSecs());
        assertEquals(256, c.fanoutCapacity());
        assertEquals(2000, c.gracefulShutdownDrainMs());
        assertTrue(c.tlsCert().isEmpty());
        assertTrue(c.tlsKey().isEmpty());
        assertFalse(c.acceptUnregisteredPublishers());
    }

    @Test
    void acceptUnregisteredPublishersRoundTrips() {
        RtspServerConfig on = RtspServerConfig.builder().acceptUnregisteredPublishers(true).build();
        assertTrue(on.acceptUnregisteredPublishers());
        assertTrue(on.toString().contains("acceptUnregisteredPublishers=true"));
        RtspServerConfig off = RtspServerConfig.builder().acceptUnregisteredPublishers(false).build();
        assertFalse(off.acceptUnregisteredPublishers());
    }

    @Test
    void builderUsesDefaultBindAddr() {
        RtspServerConfig c = RtspServerConfig.builder().build();
        assertEquals("0.0.0.0:8554", c.bindAddr());
    }

    @Test
    void rejectsNonPositiveMaxSessions() {
        assertThrows(IllegalArgumentException.class,
            () -> RtspServerConfig.builder().maxSessions(0).build());
    }

    @Test
    void rejectsNonPositiveSessionTimeout() {
        assertThrows(IllegalArgumentException.class,
            () -> RtspServerConfig.builder().sessionTimeoutSecs(0).build());
    }

    @Test
    void rejectsNonPositiveFanoutCapacity() {
        assertThrows(IllegalArgumentException.class,
            () -> RtspServerConfig.builder().fanoutCapacity(0).build());
    }

    @Test
    void rejectsNegativeDrain() {
        assertThrows(IllegalArgumentException.class,
            () -> RtspServerConfig.builder().gracefulShutdownDrainMs(-1).build());
    }

    @Test
    void rejectsTlsCertWithoutKey() {
        assertThrows(IllegalArgumentException.class,
            () -> RtspServerConfig.builder().tlsCert("cert.pem").build());
    }

    @Test
    void rejectsTlsKeyWithoutCert() {
        assertThrows(IllegalArgumentException.class,
            () -> RtspServerConfig.builder().tlsKey("key.pem").build());
    }

    @Test
    void acceptsBothTlsPaths() {
        RtspServerConfig c = RtspServerConfig.builder()
            .bindAddr("rtsps://127.0.0.1:0")
            .tlsCert("cert.pem").tlsKey("key.pem").build();
        assertTrue(c.tlsCert().isPresent());
        assertTrue(c.tlsKey().isPresent());
    }

    @Test
    void rejectsTlsPathsOnPlaintextBind() {
        IllegalArgumentException ex = assertThrows(IllegalArgumentException.class,
            () -> RtspServerConfig.builder()
                .bindAddr("127.0.0.1:0")
                .tlsCert("/tmp/cert.pem").tlsKey("/tmp/key.pem")
                .build());
        assertTrue(ex.getMessage().contains("rtsps://"));
    }

    @Test
    void tlsCertWithoutKeyIsRejected() {
        IllegalArgumentException ex = assertThrows(IllegalArgumentException.class,
            () -> RtspServerConfig.builder()
                .bindAddr("rtsps://127.0.0.1:0")
                .tlsCert("/tmp/cert.pem")
                .build());
        assertTrue(ex.getMessage().contains("both or neither"));
    }

    @Test
    void tlsPathsRoundTripThroughAccessors() {
        var cfg = RtspServerConfig.builder()
            .bindAddr("rtsps://127.0.0.1:0")
            .tlsCert("/tmp/cert.pem").tlsKey("/tmp/key.pem")
            .build();
        assertEquals("/tmp/cert.pem", cfg.tlsCert().orElseThrow());
        assertEquals("/tmp/key.pem", cfg.tlsKey().orElseThrow());
    }

    @Test
    void authAcceptsBasicAndDigestAndRejectsOther() {
        RtspServerConfig b = RtspServerConfig.builder()
            .auth(new BasicAuth("u", "p", "realm")).build();
        assertTrue(b.auth().get() instanceof BasicAuth);
        RtspServerConfig d = RtspServerConfig.builder()
            .auth(new DigestAuth("u", "p", DigestAlgorithm.MD5, "realm")).build();
        assertTrue(d.auth().get() instanceof DigestAuth);
        assertThrows(IllegalArgumentException.class,
            () -> RtspServerConfig.builder().auth("not-an-auth").build());
    }

    @Test
    void serverStatsRecordRoundTrips() {
        ServerStats s = new ServerStats(1L, 2L, 3L, 4L, 5L, 6L, 7L);
        assertEquals(1L, s.activeSessions());
        assertEquals(2L, s.totalRtpPacketsSent());
        assertEquals(3L, s.totalRtpBytesSent());
        assertEquals(4L, s.mounts());
        assertEquals(5L, s.activePublishers());
        assertEquals(6L, s.totalRtpPacketsReceived());
        assertEquals(7L, s.totalRtpBytesReceived());
    }

    @Test
    void mountStatsRecordRoundTrips() {
        MountStats m = new MountStats(10L, 20L, 30L, 40L);
        assertEquals(10L, m.bytesPushed());
        assertEquals(20L, m.packetsPushed());
        assertEquals(30L, m.peerCount());
        assertEquals(40L, m.framesDroppedTotal());
    }
}
