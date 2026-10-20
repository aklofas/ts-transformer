package org.tstrans.webrtc;

import static org.junit.jupiter.api.Assertions.*;

import java.util.Arrays;
import java.util.Set;
import java.util.stream.Collectors;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.WebrtcException;
import org.tstrans.mpegts.VideoCodec;

class WebrtcErrorModelTest {
    /** Exactly the WebRTC domain of scripts/ratchets/kind-equivalence.tsv. */
    @Test
    void kindMembersMatchTheWebrtcDomain() {
        Set<String> expected = Set.of("URL", "IO", "TLS", "MOUNT", "SERVER", "PUSH",
            "INTERNAL", "CLOSED");
        Set<String> actual = Arrays.stream(WebrtcException.Kind.values())
            .map(Enum::name).collect(Collectors.toSet());
        assertEquals(expected, actual);
    }

    private static WebrtcException.Kind kindOf(org.junit.jupiter.api.function.Executable call) {
        return assertThrows(WebrtcException.class, call).kind();
    }

    @Test @Timeout(15)
    void hostnameBindIsUrl() {
        assertEquals(WebrtcException.Kind.URL, kindOf(() -> WhepServer.bind("whep://localhost:1")));
        assertEquals(WebrtcException.Kind.URL, kindOf(() -> WhepServer.bind("http://127.0.0.1:0")));
    }

    @Test @Timeout(15)
    void unreadableCertificateIsTls() {
        assertEquals(WebrtcException.Kind.TLS, kindOf(() -> WhepServer.bind(
            "wheps://127.0.0.1:0?cert=/nonexistent/server.crt&key=/nonexistent/server.key")));
    }

    @Test @Timeout(15)
    void portInUseIsIo() throws Exception {
        try (WhepServer a = WhepServer.bind("whep://127.0.0.1:0")) {
            String taken = a.localAddr();
            assertEquals(WebrtcException.Kind.IO,
                kindOf(() -> WhepServer.bind("whep://" + taken)));
        }
    }

    @Test @Timeout(15)
    void mountRefusalsAreMount() throws Exception {
        try (WhepServer s = WhepServer.bind("whep://127.0.0.1:0")) {
            assertEquals(WebrtcException.Kind.MOUNT,
                kindOf(() -> s.addMount("nope", MountConfig.defaults())));
            assertEquals(WebrtcException.Kind.MOUNT, kindOf(() -> s.addMount("/av1",
                new MountConfig(MountKind.ELEMENTARY, VideoCodec.AV1, AudioPolicy.DROP,
                    KlvCarriage.SEI_ONLY, 1024, 64))));
            assertEquals(WebrtcException.Kind.MOUNT, kindOf(() -> s.addMount("/neg",
                new MountConfig(MountKind.ELEMENTARY, VideoCodec.H264, AudioPolicy.DROP,
                    KlvCarriage.SEI_ONLY, -1, 64))));
            assertEquals(WebrtcException.Kind.MOUNT, kindOf(() -> s.removeMount("/absent")));
            s.addMount("/cam", MountConfig.defaults()).close();
            assertEquals(WebrtcException.Kind.MOUNT,
                kindOf(() -> s.addMount("/cam", MountConfig.defaults())));
        }
    }

    @Test @Timeout(15)
    void refusedPushesArePush() throws Exception {
        try (WhepServer s = WhepServer.bind("whep://127.0.0.1:0");
             MountHandle m = s.addMount("/cam", MountConfig.defaults())) {
            assertEquals(WebrtcException.Kind.PUSH, kindOf(() -> m.pushTs(new byte[188])));
            assertEquals(WebrtcException.Kind.PUSH,
                kindOf(() -> m.pushVideo(new byte[0], 0L, false)));
            assertEquals(WebrtcException.Kind.PUSH,
                kindOf(() -> m.pushKlv(new byte[300 * 1024], 0L, 0)));
            assertThrows(IllegalArgumentException.class, () -> m.pushKlv(new byte[4], 0L, 256));
        }
    }

    @Test @Timeout(15)
    void callsAfterStopAreClosed() throws Exception {
        try (WhepServer s = WhepServer.bind("whep://127.0.0.1:0");
             MountHandle m = s.addMount("/cam", MountConfig.defaults())) {
            s.stop();
            assertEquals(WebrtcException.Kind.CLOSED,
                kindOf(() -> m.pushVideo(WhepServerTest.tinyIdrAu(), 0L, true)));
            assertEquals(WebrtcException.Kind.CLOSED,
                kindOf(() -> s.addMount("/late", MountConfig.defaults())));
        }
    }

    /** The mount outlives its server object: close() stops the server first. */
    @Test @Timeout(15)
    void mountPushAfterServerCloseIsClosed() throws Exception {
        WhepServer s = WhepServer.bind("whep://127.0.0.1:0");
        try (MountHandle m = s.addMount("/cam", MountConfig.defaults())) {
            s.close();
            assertEquals(WebrtcException.Kind.CLOSED, kindOf(() -> m.pushTs(new byte[188])));
        }
    }

    @Test
    void carriesKindAndMessage() {
        var e = new WebrtcException(WebrtcException.Kind.PUSH, "boom");
        assertEquals(WebrtcException.Kind.PUSH, e.kind());
        assertEquals("boom", e.getMessage());
    }
}
