package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtspException;

/**
 * A cancel fired while a control call is parked ends that
 * call with {@code RtspException(CLOSED)} — the one cancel outcome every
 * tstrans handle shares (it was {@code PROTOCOL}). The cancel handle only
 * exists on a live session, so the parked call is PAUSE, not connect: a
 * {@link RawRtspPeer} answers OPTIONS / DESCRIBE / SETUP / PLAY and then never
 * answers PAUSE. Asserts the kind only, never elapsed time.
 */
class RtspCancelOutcomeTest {
    @Test
    @Timeout(30)
    void cancelOfAParkedControlCallIsClosed() throws Exception {
        try (RawRtspPeer peer = new RawRtspPeer(RawRtspPeer.MP2T_SDP, false, null, null);
             RtspSession session = RtspClient.connect(peer.config());
             RtspCancelHandle cancel = session.cancelHandle()) {
            // Whether the cancel lands before or after PAUSE parks, the
            // call ends at its next poll with the same kind.
            Thread canceller = new Thread(() -> {
                try {
                    Thread.sleep(200);
                } catch (InterruptedException ignored) {
                    return;
                }
                cancel.cancel();
            });
            canceller.setDaemon(true);
            canceller.start();
            RtspException e = assertThrows(RtspException.class, session::pause);
            assertEquals(RtspException.Kind.CLOSED, e.kind());
            canceller.join(5_000);
        }
    }
}
