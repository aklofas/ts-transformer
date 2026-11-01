package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.rtspHeader;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Base64;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtspException;

/**
 * The live per-path mount rules: {@link RtspServer#mountRules()}, {@link MountRules} and
 * the config's default rule. The publisher is a raw socket sending one ANNOUNCE.
 */
class MountRulesTest {

    private static final String SDP_MP2T =
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=publish\r\nc=IN IP4 127.0.0.1\r\n"
            + "t=0 0\r\nm=video 0 RTP/AVP 33\r\na=rtpmap:33 MP2T/90000\r\n"
            + "a=control:streamid=0\r\n";

    @Test
    @Timeout(30)
    void requireOnPublishChallengesThenAdmits() throws Exception {
        assumeLinux();
        try (RtspServer server = start(RtspServerConfig.builder().bindAddr("127.0.0.1:0")
                .gracefulShutdownDrainMs(50).acceptUnregisteredPublishers(true).build());
             MountRules rules = server.mountRules()) {
            rules.set("/amber", MountRule.of(
                MountPolicy.require(MountCredential.of("amber", "pw")), MountPolicy.inherit()));
            rules.setChallenge(AuthScheme.BASIC, "lab");
            assertEquals(1, rules.size());
            int port = port(server);

            String refused = announce(port, "/amber", null);
            assertEquals(401, status(refused));
            assertEquals("Basic realm=\"lab\"", rtspHeader(refused, "WWW-Authenticate"));

            String basic = "Authorization: Basic " + Base64.getEncoder()
                .encodeToString("amber:pw".getBytes(StandardCharsets.UTF_8)) + "\r\n";
            assertEquals(200, status(announce(port, "/amber", basic)));
        }
    }

    @Test
    @Timeout(30)
    void refuseAnswersBeforeTheMountExists() throws Exception {
        assumeLinux();
        try (RtspServer server = start(RtspServerConfig.builder().bindAddr("127.0.0.1:0")
                .gracefulShutdownDrainMs(50).acceptUnregisteredPublishers(true).build());
             MountRules rules = server.mountRules()) {
            rules.set("/taken", MountRule.of(
                MountPolicy.refuse(RefuseStatus.NOT_ENOUGH_BANDWIDTH), MountPolicy.inherit()));
            assertEquals(453, status(announce(port(server), "/taken", null)));
            assertTrue(server.nextPublisher(200).isEmpty());
        }
    }

    @Test
    @Timeout(30)
    void defaultMountRuleInTheConfigGuardsTheFirstAnnounce() throws Exception {
        assumeLinux();
        try (RtspServer server = start(RtspServerConfig.builder().bindAddr("127.0.0.1:0")
                .gracefulShutdownDrainMs(50)
                .defaultMountRule(MountRule.both(MountPolicy.refuse(RefuseStatus.NOT_FOUND)))
                .acceptUnregisteredPublishers(true).build())) {
            assertEquals(404, status(announce(port(server), "/fresh", null)));
        }
    }

    @Test
    void invalidPathAndEmptyCredentialsThrowMountKind() throws Exception {
        assumeLinux();
        try (RtspServer server = start(RtspServerConfig.builder().bindAddr("127.0.0.1:0")
                .gracefulShutdownDrainMs(50).build());
             MountRules rules = server.mountRules()) {
            RtspException bad = assertThrows(RtspException.class,
                () -> rules.set("amber", MountRule.both(MountPolicy.open())));
            assertEquals(RtspException.Kind.MOUNT, bad.kind());
            assertThrows(IllegalArgumentException.class, MountPolicy::require);
            RtspException empty = assertThrows(RtspException.class,
                () -> rules.set("/a", MountRule.both(
                    MountPolicy.require(MountCredential.of("", "pw")))));
            assertEquals(RtspException.Kind.MOUNT, empty.kind());
            assertEquals(RtspException.Kind.MOUNT, assertThrows(RtspException.class,
                () -> rules.set("/a", MountRule.both(MountPolicy.require(
                    MountCredential.of("amber", "a"), MountCredential.of("amber", "b"))))).kind());
        }
    }

    @Test
    @Timeout(30)
    void rulesSurviveServerCloseAndThrowServerAfterStop() throws Exception {
        assumeLinux();
        RtspServer server = start(RtspServerConfig.builder().bindAddr("127.0.0.1:0")
            .gracefulShutdownDrainMs(50).build());
        MountRules rules = server.mountRules();
        rules.set("/a", MountRule.both(MountPolicy.open()));
        assertFalse(rules.isClosed());
        server.stop();
        RtspException stopped = assertThrows(RtspException.class,
            () -> rules.set("/b", MountRule.both(MountPolicy.open())));
        assertEquals(RtspException.Kind.SERVER, stopped.kind());
        assertTrue(rules.isClosed());
        server.close();
        assertEquals(1, rules.size());
        rules.close();
    }

    @Test
    void toStringRedactsPasswords() {
        String s = MountRule.both(MountPolicy.require(MountCredential.of("amber", "pw")))
            .toString();
        assertTrue(s.contains("amber"), s);
        assertFalse(s.contains("pw"), s);
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    private static void assumeLinux() {
        assumeTrue(isLinux(), "RTSP live publisher gated to Linux (real sockets + tokio runtime)");
    }

    private static RtspServer start(RtspServerConfig config) throws RtspException {
        return RtspServer.start(config);
    }

    private static int port(RtspServer server) {
        String addr = server.localAddr();
        assertNotNull(addr, "server must be bound");
        return Integer.parseInt(addr.substring(addr.lastIndexOf(':') + 1));
    }

    /** One ANNOUNCE on a fresh connection; returns the response head. */
    private static String announce(int port, String path, String extraHeaders) throws IOException {
        try (Socket sock = new Socket()) {
            sock.setSoTimeout(5000);
            sock.connect(new InetSocketAddress("127.0.0.1", port), 5000);
            InputStream in = sock.getInputStream();
            OutputStream out = sock.getOutputStream();
            String req = "ANNOUNCE rtsp://127.0.0.1:" + port + path + " RTSP/1.0\r\nCSeq: 1\r\n"
                + (extraHeaders == null ? "" : extraHeaders)
                + "Content-Type: application/sdp\r\nContent-Length: " + SDP_MP2T.length()
                + "\r\n\r\n" + SDP_MP2T;
            out.write(req.getBytes(StandardCharsets.US_ASCII));
            out.flush();
            ByteArrayOutputStream head = new ByteArrayOutputStream();
            List<Integer> tail = new ArrayList<>();
            while (true) {
                int b = in.read();
                if (b < 0) throw new IOException("server closed before answering");
                head.write(b);
                tail.add(b);
                if (tail.size() > 4) tail.remove(0);
                if (tail.equals(List.of(13, 10, 13, 10))) break;
            }
            return head.toString(StandardCharsets.US_ASCII);
        }
    }

    private static int status(String resp) {
        return Integer.parseInt(resp.split(" ")[1]);
    }

}
