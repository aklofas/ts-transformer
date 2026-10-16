package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.net.DatagramSocket;
import java.net.InetSocketAddress;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtpException;
import org.tstrans.shells.MuxSender;

/**
 * {@code org.tstrans.shells.MuxSender.finish()} drains,
 * reports, closes; a second call is quiet.
 *
 * <p>The RTP twin of {@code org.tstrans.srt.SrtMuxSenderFinishTest}. The peer
 * is a bound throwaway {@link DatagramSocket} for the same reason
 * {@link RtpConvenienceTest} needs one: an {@code rtp://} sender uses a
 * <em>connected</em> UDP socket, so a send to a port with no listener fails
 * with {@code ECONNREFUSED}.
 */
class RtpMuxSenderFinishTest {

    @Test
    @Timeout(60)
    void finishClosesAndIsIdempotent() throws Exception {
        try (DatagramSocket peer = new DatagramSocket(new InetSocketAddress("127.0.0.1", 0))) {
            String url = "rtp://127.0.0.1:" + peer.getLocalPort();
            MuxSender s = MuxSender.open(url, roundtripConfig());
            try {
                s.sendVideo(syntheticH264Idr(), 0L, true);
                s.finish();
                assertFalse(s.isAlive());
                s.finish();                  // quiet
                RtpException ex = assertThrows(RtpException.class,
                    () -> s.sendVideo(syntheticH264Idr(), 3_000L, true));
                assertEquals(RtpException.Kind.CLOSED, ex.kind());
            } finally {
                s.close();
            }
        }
    }

    @Test
    @Timeout(60)
    void finishAfterCloseIsQuiet() throws Exception {
        try (DatagramSocket peer = new DatagramSocket(new InetSocketAddress("127.0.0.1", 0))) {
            String url = "rtp://127.0.0.1:" + peer.getLocalPort();
            MuxSender s = MuxSender.open(url, roundtripConfig());
            s.close();
            s.finish();                      // no handle left: quiet, not a throw
        }
    }
}
