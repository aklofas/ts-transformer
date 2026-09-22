package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.SrtException;

/**
 * The one-shot listener opens ({@link Receiver#fromUrl} /
 * {@link DemuxReceiver#fromUrl}) report a BIND fault as
 * {@code SrtException(BROKEN)} with a {@code bind: }-prefixed message.
 *
 * <p>0.7.0 change: both opens now go through the shared
 * {@code SrtUrl::accept_one} path — the same composition the C ABI's
 * {@code listen_srt} has always used — which wraps bind and accept faults as
 * {@code TransportError::Broken} carrying a {@code bind: } / {@code accept: }
 * prefix. Before, a failed bind surfaced as {@code CONNECT_FAILED}.
 * {@link Listener#accept} is unaffected and keeps its typed accept kinds.
 *
 * <p>An already-bound UDP port is the deterministic bind fault: no timing, no
 * peer, no network — the second bind cannot succeed.
 */
class SrtListenerOpenFaultTest {

    @Test
    @Timeout(30)
    void bindConflictIsBrokenWithBindPrefix() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        // Hold the port with a real SRT listener, then race a second open at it.
        Listener held = new Builder("srt://127.0.0.1:0?mode=listener").listener().listen();
        try {
            int port = held.localAddr().port();
            String url = "srt://127.0.0.1:" + port + "?mode=listener";

            SrtException e = assertThrows(SrtException.class, () -> Receiver.fromUrl(url));
            assertEquals(SrtException.Kind.BROKEN, e.kind(),
                "a bind fault on the one-shot open is BROKEN (was CONNECT_FAILED before 0.7.0)");
            assertTrue(e.getMessage().contains("bind: "),
                "the message names the failing stage, got: " + e.getMessage());

            SrtException d = assertThrows(SrtException.class, () -> DemuxReceiver.fromUrl(url));
            assertEquals(SrtException.Kind.BROKEN, d.kind(), "DemuxReceiver.fromUrl agrees");
            assertTrue(d.getMessage().contains("bind: "),
                "the message names the failing stage, got: " + d.getMessage());
        } finally {
            held.close();
        }
    }

    @Test
    @Timeout(30)
    void nonListenerUrlIsConfigInvalid() {
        // No socket involved: the mode pre-check fires before any bind.
        SrtException e = assertThrows(SrtException.class,
            () -> Receiver.fromUrl("srt://127.0.0.1:9000?mode=caller"));
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }
}
