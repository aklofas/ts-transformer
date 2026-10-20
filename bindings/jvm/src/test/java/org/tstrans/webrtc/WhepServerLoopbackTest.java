package org.tstrans.webrtc;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.condition.EnabledIfSystemProperty;
import org.tstrans.internal.WebrtcProbe;

/**
 * A real WebRTC viewer plays a mount fed from Java. The viewer is the Rust
 * test viewer of tst-webrtc's loopback tests, reached through a jni-test-hooks
 * probe, because a WHEP client in Java would be a second implementation.
 */
class WhepServerLoopbackTest {
    /** The 30-frame H.264 GOP of tst-webrtc's fixtures, split at each AUD. */
    static List<byte[]> gop() throws Exception {
        byte[] all = Files.readAllBytes(Path.of(System.getProperty("user.dir"), "..", "..",
            "crates/tst-webrtc/tests/fixtures/h264/gop.h264").normalize());
        List<Integer> aud = new ArrayList<>();
        for (int i = 0; i + 4 < all.length; i++) {
            if (all[i] == 0 && all[i + 1] == 0 && all[i + 2] == 0 && all[i + 3] == 1
                    && (all[i + 4] & 0x1f) == 9) {
                aud.add(i);
            }
        }
        List<byte[]> aus = new ArrayList<>();
        for (int k = 0; k < aud.size(); k++) {
            int end = k + 1 < aud.size() ? aud.get(k + 1) : all.length;
            aus.add(java.util.Arrays.copyOfRange(all, aud.get(k), end));
        }
        return aus;
    }

    @Test @Timeout(60)
    @EnabledIfSystemProperty(named = "tst.jniTestHooks", matches = "true")
    void aViewerReceivesFramesPushedFromJava() throws Exception {
        assumeTrue(isLinux(), "live WebRTC loopback gated to Linux");
        List<byte[]> aus = gop();
        assertEquals(30, aus.size());
        try (WhepServer s = WhepServer.bind("whep://127.0.0.1:0");
             MountHandle m = s.addMount("/cam", MountConfig.defaults())) {
            // A viewer joins at a keyframe, so the pusher loops the GOP until the
            // probe returns; a viewer that connects late still gets the next IDR.
            AtomicBoolean done = new AtomicBoolean();
            AtomicReference<Throwable> pushFailure = new AtomicReference<>();
            Thread pusher = new Thread(() -> {
                long pts = 0;
                try {
                    while (!done.get()) {
                        for (int i = 0; i < aus.size() && !done.get(); i++) {
                            m.pushVideo(aus.get(i), pts, i == 0);
                            pts += 3000;
                            Thread.sleep(33);
                        }
                    }
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                } catch (Throwable t) {
                    pushFailure.set(t);
                }
            }, "whep-pusher");
            pusher.setDaemon(true);
            pusher.start();
            int frames;
            try {
                frames = WebrtcProbe.nLoopbackViewerProbe(s.nativeHandleForTest(), "/cam");
            } finally {
                done.set(true);
                pusher.join(5_000);
            }
            assertNull(pushFailure.get(), () -> "push failed: " + pushFailure.get());
            assertTrue(frames > 0, "the viewer received no frame");
            assertTrue(m.stats().framesPushed() > 0);
            assertTrue(s.stats().rtpPacketsSent() > 0);
            assertEquals(1L, s.stats().sessionsTotal());
        }
    }
}
