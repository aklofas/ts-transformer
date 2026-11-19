package org.tstrans.rtp;

import org.tstrans.RtspException;

/** Fixtures the RTSP test classes of this package share. */
final class RtspTestSupport {
    private RtspTestSupport() {}

    /** The library's RTSP server on an ephemeral loopback port, with a 50 ms drain. */
    static RtspServer startServer() throws RtspException {
        return RtspServer.start(RtspServerConfig.builder()
            .bindAddr("127.0.0.1:0")
            .gracefulShutdownDrainMs(50)
            .build());
    }

    /** Seven MPEG-TS null packets (PID 0x1FFF): the largest payload of one RTP/MP2T packet. */
    static byte[] nullBundle() {
        byte[] b = new byte[7 * 188];
        for (int p = 0; p < 7; p++) {
            int o = p * 188;
            b[o] = 0x47; b[o + 1] = 0x1F; b[o + 2] = (byte) 0xFF; b[o + 3] = 0x10;
            for (int i = 4; i < 188; i++) b[o + i] = (byte) 0xFF;
        }
        return b;
    }
}
