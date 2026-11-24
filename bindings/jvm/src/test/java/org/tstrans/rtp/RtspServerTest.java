package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.rtspHeader;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.DatagramSocket;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.Base64;
import java.util.HexFormat;
import java.util.Locale;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.MuxException;
import org.tstrans.RtspException;
import org.tstrans.mpegts.DataStreamHandle;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;
import org.tstrans.mpegts.VideoStreamHandle;

class RtspServerTest {

    @Test @Timeout(15)
    void startBindsAndReportsLocalAddr() throws Exception {
        try (RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            assertTrue(s.localAddr().isPresent(), "localAddr should be populated after start()");
            ServerStats st = s.stats();
            assertEquals(0L, st.activeSessions());
            assertEquals(0L, st.mounts());
        }
    }

    @Test @Timeout(15)
    void localAddrIsAnInetSocketAddress() throws Exception {
        RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"));
        java.net.InetSocketAddress a = s.localAddr().orElseThrow();
        assertInstanceOf(java.net.Inet4Address.class, a.getAddress());
        assertEquals("127.0.0.1", a.getAddress().getHostAddress());
        assertTrue(a.getPort() > 0, "the kernel's port for :0");
        s.close();
        assertThrows(IllegalStateException.class, s::localAddr);
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
            assertTrue(s.localAddr().isPresent());
        }
    }

    @Test @Timeout(15)
    void startWithDigestAuthSucceeds() throws Exception {
        RtspServerConfig cfg = RtspServerConfig.builder()
            .bindAddr("127.0.0.1:0")
            .auth(new DigestAuth("admin", "secret", DigestAlgorithm.SHA256, "tst"))
            .build();
        try (RtspServer s = RtspServer.start(cfg)) {
            assertTrue(s.localAddr().isPresent());
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
            String url = "rtsp://127.0.0.1:" + s.localAddr().orElseThrow().getPort() + "/live";
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

    /**
     * With {@code maxReaders(1)} below {@code maxSessions(4)}, one reader plays and the next
     * reader's SETUP reads 453 and its connection is closed; this library's client reports it
     * as {@code PROTOCOL}. Swapping the two values (a cap of 4, one session) fails both halves.
     */
    @Test @Timeout(15)
    void aReaderOverTheCapIsRefusedAndOneUnderItPlays() throws Exception {
        RtspServerConfig cfg = RtspServerConfig.builder()
            .bindAddr("127.0.0.1:0").maxSessions(4).maxReaders(1).build();
        try (RtspServer s = RtspServer.start(cfg);
             MountHandle m = s.addUnicastMount("/live", roundtripConfig());
             Raw first = Raw.connect(port(s))) {
            int port = port(s);
            String uri = "rtsp://127.0.0.1:" + port + "/live";
            String setup = first.request("SETUP " + uri + "/trackID=0",
                "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n");
            assertEquals(200, Raw.status(setup), setup);
            String sid = rtspHeader(setup, "Session").split(";")[0].trim();
            String play = first.request("PLAY " + uri, "Session: " + sid + "\r\n");
            assertEquals(200, Raw.status(play), play);

            try (Raw second = Raw.connect(port)) {
                String refused = second.request("SETUP " + uri + "/trackID=0",
                    "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n");
                assertEquals(453, Raw.status(refused), refused);
                assertTrue(second.closedByPeer(), "the refused reader's connection stays open");
            }
            RtspClientConfig client = RtspClientConfig.builder(uri)
                .transportPref(TransportPref.TCP).keepalive(false).build();
            RtspException e = assertThrows(RtspException.class, () -> RtspClient.connect(client));
            assertEquals(RtspException.Kind.PROTOCOL, e.kind(), e.getMessage());
            assertTrue(e.getMessage().contains("453"), e.getMessage());
        }
    }

    /** A server that does not allow UDP media refuses a multicast mount before it exists. */
    @Test @Timeout(15)
    void aMulticastMountIsRefusedWhereUdpMediaIsOff() throws Exception {
        try (RtspServer s = RtspServer.start(RtspServerConfig.builder()
                .bindAddr("127.0.0.1:0").allowUdpMedia(false).build())) {
            MuxerConfig cfg = MuxerConfig.builder()
                .programNumber(1).pmtPid(0x1000).addVideo(0x1011, VideoCodec.H264).build();
            RtspException e = assertThrows(RtspException.class,
                () -> s.addMulticastMount("/mc", "239.0.0.1", 5004, cfg));
            assertEquals(RtspException.Kind.MOUNT, e.kind(), e.getMessage());
            assertTrue(e.getMessage().contains("UDP media"), e.getMessage());
            assertEquals(0, s.stats().mounts());
        }
    }

    /**
     * No client in this suite reads a mount's raw TS, so the lead is probed through
     * the muxer's range check instead of the wire. 1001 is out of range for the lead
     * only; had another scalar of the config taken its place, the lead would be valid
     * and the refusal would be missing or name a different field.
     */
    @Test @Timeout(15)
    void mountsPassThePcrLeadToTheMuxer() throws Exception {
        try (RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            MuxerConfig cfg = MuxerConfig.builder()
                .programNumber(1).pmtPid(0x1000).addVideo(0x1011, VideoCodec.H264)
                .pcrLeadMs(1001).bufferPackets(500).build();
            MuxException unicast = assertThrows(MuxException.class,
                () -> s.addUnicastMount("/live", cfg));
            assertEquals(MuxException.Kind.CONFIG_INVALID, unicast.kind());
            assertTrue(unicast.getMessage().contains("pcr_lead_ms"), unicast.getMessage());
            MuxException multicast = assertThrows(MuxException.class,
                () -> s.addMulticastMount("/mc", "239.0.0.1", 5004, cfg));
            assertEquals(MuxException.Kind.CONFIG_INVALID, multicast.kind());
            assertTrue(multicast.getMessage().contains("pcr_lead_ms"), multicast.getMessage());
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

    @Test @Timeout(15)
    void pushVideoWithDtsOnOneVideoStreamCountsBytes() throws Exception {
        try (RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            MuxerConfig cfg = MuxerConfig.builder()
                .programNumber(1).pmtPid(0x1000).addVideo(0x1011, VideoCodec.H264).build();
            try (MountHandle m = s.addUnicastMount("/dts", cfg)) {
                m.pushVideoWithDts(syntheticH264Idr(), 18_000L, 0L, true);
                m.flush();
                assertTrue(m.stats().bytesPushed() > 0);
            }
        }
    }

    @Test @Timeout(15)
    void pushVideoWithDtsWithTwoVideoStreamsThrows() throws Exception {
        try (RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            MuxerConfig cfg = MuxerConfig.builder()
                .programNumber(1).pmtPid(0x1000)
                .addVideo(0x1011, VideoCodec.H264).addVideo(0x1012, VideoCodec.H264).build();
            try (MountHandle m = s.addUnicastMount("/dts2", cfg)) {
                RtspException ex = assertThrows(RtspException.class,
                    () -> m.pushVideoWithDts(syntheticH264Idr(), 3_000L, 0L, true));
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


    // ── what the server's options and credentials do on the wire ─────────────

    /** A plain server told not to allow UDP media answers 461 to a UDP SETUP. */
    @Test @Timeout(15)
    void aServerWithoutUdpMediaAnswers461ToUdpAndServesInterleaved() throws Exception {
        RtspServerConfig cfg = RtspServerConfig.builder()
            .bindAddr("127.0.0.1:0").allowUdpMedia(false).build();
        try (RtspServer s = RtspServer.start(cfg);
             MountHandle m = s.addUnicastMount("/live", roundtripConfig());
             DatagramSocket media = new DatagramSocket(0, InetAddress.getLoopbackAddress())) {
            int port = port(s);
            String udp = readerSetup(Raw.connect(port), port, udpTransport(media));
            assertEquals(461, Raw.status(udp), udp);
            String tcp = readerSetup(Raw.connect(port), port,
                "RTP/AVP/TCP;unicast;interleaved=0-1");
            assertEquals(200, Raw.status(tcp), tcp);
        }
    }

    /** Unset, a plain {@code rtsp://} server serves a reader over UDP. */
    @Test @Timeout(15)
    void aPlainServerServesUdpMediaByDefault() throws Exception {
        try (RtspServer s = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"));
             MountHandle m = s.addUnicastMount("/live", roundtripConfig());
             DatagramSocket media = new DatagramSocket(0, InetAddress.getLoopbackAddress())) {
            int port = port(s);
            String udp = readerSetup(Raw.connect(port), port, udpTransport(media));
            assertEquals(200, Raw.status(udp), udp);
            assertTrue(rtspHeader(udp, "Transport").contains("server_port"), udp);
        }
    }

    /**
     * The Digest answer GStreamer 1.24 sends (the RFC 2069 form: no {@code qop}, {@code nc}
     * or {@code cnonce}) is accepted only with {@code acceptDigestWithoutQop}; the challenge
     * offers {@code qop="auth"} either way.
     */
    @Test @Timeout(15)
    void aDigestAnswerWithoutQopIsAcceptedOnlyWithTheOption() throws Exception {
        for (boolean option : new boolean[] {true, false}) {
            RtspServerConfig cfg = RtspServerConfig.builder()
                .bindAddr("127.0.0.1:0")
                .auth(new DigestAuth("admin", "secret", DigestAlgorithm.MD5, "test-realm"))
                .acceptDigestWithoutQop(option)
                .build();
            try (RtspServer s = RtspServer.start(cfg);
                 MountHandle m = s.addUnicastMount("/live", roundtripConfig());
                 Raw raw = Raw.connect(port(s))) {
                String uri = "rtsp://127.0.0.1:" + port(s) + "/live";
                String first = raw.request("DESCRIBE " + uri, "");
                assertEquals(401, Raw.status(first), first);
                String challenge = rtspHeader(first, "WWW-Authenticate");
                assertTrue(challenge.contains("qop=\"auth\""), challenge);
                String nonce = challenge.split("nonce=\"")[1].split("\"")[0];
                String ha1 = md5("admin:test-realm:secret");
                String ha2 = md5("DESCRIBE:" + uri);
                String answer = "Authorization: Digest username=\"admin\", realm=\"test-realm\", "
                    + "nonce=\"" + nonce + "\", uri=\"" + uri + "\", response=\""
                    + md5(ha1 + ":" + nonce + ":" + ha2) + "\"\r\n";
                String answered = raw.request("DESCRIBE " + uri, answer);
                assertEquals(option ? 200 : 401, Raw.status(answered),
                    "acceptDigestWithoutQop(" + option + "): " + answered);
            }
        }
    }

    /**
     * A bind address that carries a user name or a password does not start a server (through
     * 0.10.0 the credential was dropped and the server ran open); the message names neither.
     */
    @Test @Timeout(15)
    void aBindAddressWithUserinfoIsRefused() {
        String[] forms = {
            "rtsp://admin:secret@127.0.0.1:0", "rtsp://admin@127.0.0.1:0",
            "admin:secret@127.0.0.1:0", "rtsps://admin:secret@127.0.0.1:0",
        };
        for (String bind : forms) {
            for (boolean auth : new boolean[] {false, true}) {
                RtspServerConfig.Builder b = RtspServerConfig.builder().bindAddr(bind);
                if (auth) b.auth(new BasicAuth("admin", "secret", "realm"));
                RtspException e = assertThrows(RtspException.class,
                    () -> RtspServer.start(b.build()), bind);
                assertEquals(RtspException.Kind.CONFIG_INVALID, e.kind(),
                    bind + ": " + e.getMessage());
                assertTrue(e.getMessage().contains("invalid server config"), e.getMessage());
                assertTrue(e.getMessage().contains("bind URL"), e.getMessage());
                assertFalse(e.getMessage().contains("admin"), e.getMessage());
                assertFalse(e.getMessage().contains("secret"), e.getMessage());
            }
        }
    }

    /** Basic splits {@code user:password} at the first colon; Digest carries the name quoted. */
    @Test @Timeout(15)
    void aBasicUserNameWithAColonIsRefusedAndADigestOneStarts() throws Exception {
        RtspException e = assertThrows(RtspException.class, () -> RtspServer.start(
            RtspServerConfig.builder().bindAddr("127.0.0.1:0")
                .auth(new BasicAuth("ad:min", "secret", "realm")).build()));
        assertEquals(RtspException.Kind.CONFIG_INVALID, e.kind(), e.getMessage());
        assertFalse(e.getMessage().contains("ad:min"), e.getMessage());
        assertFalse(e.getMessage().contains("secret"), e.getMessage());
        for (DigestAlgorithm alg : DigestAlgorithm.values()) {
            try (RtspServer s = RtspServer.start(RtspServerConfig.builder()
                    .bindAddr("127.0.0.1:0")
                    .auth(new DigestAuth("ad:min", "secret", alg, "realm")).build())) {
                assertTrue(s.localAddr().isPresent(), alg.toString());
            }
        }
    }

    /** An empty password is a password: it admits its user, and "no credentials" stays refused. */
    @Test @Timeout(15)
    void anEmptyPasswordAdmitsItsUserAndRefusesNone() throws Exception {
        RtspServerConfig cfg = RtspServerConfig.builder()
            .bindAddr("127.0.0.1:0").auth(new BasicAuth("viewer", "", "realm")).build();
        try (RtspServer s = RtspServer.start(cfg);
             MountHandle m = s.addUnicastMount("/live", roundtripConfig())) {
            String uri = "rtsp://127.0.0.1:" + port(s) + "/live";
            try (Raw raw = Raw.connect(port(s))) {
                assertEquals(401, Raw.status(raw.request("DESCRIBE " + uri, "")));
                assertEquals(200, Raw.status(raw.request("DESCRIBE " + uri, basic("viewer", ""))));
            }
            try (Raw raw = Raw.connect(port(s))) {
                assertEquals(401,
                    Raw.status(raw.request("DESCRIBE " + uri, basic("viewer", "secret"))));
            }
        }
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    private static int port(RtspServer s) {
        return s.localAddr().orElseThrow(() -> new AssertionError("server must be bound"))
            .getPort();
    }

    private static String udpTransport(DatagramSocket media) {
        int p = media.getLocalPort();
        return "RTP/AVP;unicast;client_port=" + p + "-" + (p + 1);
    }

    /** One reader SETUP of {@code /live} on {@code raw}, which it closes. */
    static String readerSetup(Raw raw, int port, String transport) throws IOException {
        try (raw) {
            return raw.request("SETUP rtsp://127.0.0.1:" + port + "/live/trackID=0",
                "Transport: " + transport + "\r\n");
        }
    }

    static String md5(String s) throws Exception {
        return HexFormat.of().formatHex(
            MessageDigest.getInstance("MD5").digest(s.getBytes(StandardCharsets.UTF_8)));
    }

    static String basic(String user, String password) {
        return "Authorization: Basic " + Base64.getEncoder()
            .encodeToString((user + ":" + password).getBytes(StandardCharsets.UTF_8)) + "\r\n";
    }

    /**
     * One RTSP connection spoken by hand: each request gets the next CSeq and its whole
     * response, body included, is read with a 5 s deadline per read.
     */
    static final class Raw implements AutoCloseable {
        private final Socket sock;
        private final InputStream in;
        private final OutputStream out;
        private int cseq = 1;

        Raw(Socket sock) throws IOException {
            this.sock = sock;
            sock.setSoTimeout(5000);
            in = sock.getInputStream();
            out = sock.getOutputStream();
        }

        static Raw connect(int port) throws IOException {
            Socket sock = new Socket();
            sock.connect(new InetSocketAddress("127.0.0.1", port), 5000);
            return new Raw(sock);
        }

        /** Send {@code start RTSP/1.0}, the CSeq and {@code headers}; the response head. */
        String request(String start, String headers) throws IOException {
            String req = start + " RTSP/1.0\r\nCSeq: " + (cseq++) + "\r\n" + headers + "\r\n";
            out.write(req.getBytes(StandardCharsets.US_ASCII));
            out.flush();
            ByteArrayOutputStream head = new ByteArrayOutputStream();
            int matched = 0;
            byte[] end = {13, 10, 13, 10};
            while (matched < 4) {
                int b = in.read();
                if (b < 0) throw new IOException("server closed before answering " + start);
                head.write(b);
                matched = b == end[matched] ? matched + 1 : (b == 13 ? 1 : 0);
            }
            String resp = head.toString(StandardCharsets.US_ASCII);
            for (String line : resp.split("\r\n")) {
                if (line.toLowerCase(Locale.ROOT).startsWith("content-length:")) {
                    in.readNBytes(Integer.parseInt(line.substring(line.indexOf(':') + 1).trim()));
                }
            }
            return resp;
        }

        /**
         * Whether the server ends the connection: a read returns the end of the stream, or a
         * reset where the platform reports one; false when bytes or nothing arrive in 5 s.
         */
        boolean closedByPeer() throws IOException {
            try {
                return in.read() < 0;
            } catch (java.net.SocketTimeoutException e) {
                return false;
            } catch (IOException reset) {
                return true;
            }
        }

        static int status(String resp) {
            return Integer.parseInt(resp.split(" ")[1]);
        }

        @Override public void close() throws IOException { sock.close(); }
    }
}
