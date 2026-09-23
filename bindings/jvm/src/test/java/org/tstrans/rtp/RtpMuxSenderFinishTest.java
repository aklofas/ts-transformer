package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.net.DatagramSocket;
import java.net.InetSocketAddress;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtpException;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

/**
 * Arc 2 R3 (DEBT-14): {@code org.tstrans.rtp.MuxSender.finish()} drains,
 * reports, closes; a second call is quiet.
 *
 * <p>The RTP twin of {@code org.tstrans.srt.SrtMuxSenderFinishTest}. The peer
 * is a bound throwaway {@link DatagramSocket} for the same reason
 * {@link RtpConvenienceTest} needs one: an {@code rtp://} sender uses a
 * <em>connected</em> UDP socket, so a send to a port with no listener fails
 * with {@code ECONNREFUSED}.
 */
class RtpMuxSenderFinishTest {

    private static MuxerConfig videoConfig() {
        return MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264)
            .build();
    }

    private static byte[] idr() {
        byte[] b = new byte[20];
        b[0] = 0; b[1] = 0; b[2] = 0; b[3] = 1; b[4] = 0x65;
        for (int i = 0; i < 15; i++) b[5 + i] = (byte) (0xA5 ^ i);
        return b;
    }

    @Test
    @Timeout(60)
    void finishClosesAndIsIdempotent() throws Exception {
        try (DatagramSocket peer = new DatagramSocket(new InetSocketAddress("127.0.0.1", 0))) {
            String url = "rtp://127.0.0.1:" + peer.getLocalPort();
            MuxSender s = MuxSender.fromUrl(url, videoConfig());
            try {
                s.sendVideo(idr(), 0L, true);
                s.finish();
                assertFalse(s.isAlive());
                s.finish();                  // quiet
                RtpException ex = assertThrows(RtpException.class,
                    () -> s.sendVideo(idr(), 3_000L, true));
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
            MuxSender s = MuxSender.fromUrl(url, videoConfig());
            s.close();
            s.finish();                      // no handle left: quiet, not a throw
        }
    }
}
