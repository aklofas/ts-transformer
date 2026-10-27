package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.freeUdpPort;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.net.DatagramSocket;
import java.net.InetSocketAddress;
import org.junit.jupiter.api.Test;
import org.tstrans.shells.MuxSender;
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.shells.TransportStats;

/**
 * Standalone (non-live) unit tests for the RTP convenience wrappers. The live
 * MuxSender→RTP→DemuxReceiver cross-binding parity proof is in
 * {@link RtpMuxDemuxLoopbackTest}; this class covers construction, closed-state,
 * stats shape, and the push path.
 *
 * <p>Each {@code MuxSender} targets a port held open by a throwaway
 * {@link DatagramSocket} peer for the duration of the push. This is required on
 * Linux: an {@code rtp://} sender uses a <em>connected</em> UDP socket, and a
 * connected send to a port with no listener fails with {@code ECONNREFUSED}
 * (the kernel surfaces the ICMP port-unreachable). With a bound peer the send
 * succeeds, so the push and the resulting muxer stats are genuinely exercised.
 */
class RtpConvenienceTest {

    /** Bind a throwaway UDP peer on 127.0.0.1 and return it (caller closes). */
    private static DatagramSocket peer() throws Exception {
        return new DatagramSocket(new InetSocketAddress("127.0.0.1", 0));
    }


    @Test
    void muxSenderConstructsPushesAndReportsStats() throws Exception {
        try (DatagramSocket peer = peer()) {
            String url = "rtp://127.0.0.1:" + peer.getLocalPort();
            try (MuxSender s = MuxSender.open(url, roundtripConfig())) {
                assertTrue(s.isAlive());
                s.sendVideo(syntheticH264Idr(), 0L, true);
                s.sendVideo(syntheticH264Idr(), 3000L, true);

                TransportStats st = s.stats();
                assertNotNull(st);
                assertNotNull(st.socketStats());
                assertNotNull(st.muxerStats());
                // The muxer emitted at least the PAT/PMT + video PES packets.
                assertTrue(st.muxerStats().tsPacketsEmitted() > 0,
                    "muxer should have emitted TS packets after two video sends");

                assertTrue(s.videoHandle().isPresent());
                assertTrue(s.klvHandle().isEmpty());
                assertTrue(s.dataHandle().isEmpty());
            }
        }
    }

    @Test
    void muxSenderWithExplicitPktSize() throws Exception {
        try (DatagramSocket peer = peer()) {
            String url = "rtp://127.0.0.1:" + peer.getLocalPort();
            // pkt_size is the whole UDP datagram; the 12-byte RTP header comes
            // out of it, so 376 is the smallest size whose payload (364 bytes)
            // still holds one 188-byte TS packet.
            try (MuxSender s = MuxSender.open(url + "?pkt_size=376", roundtripConfig())) {
                assertTrue(s.isAlive());
                s.sendVideo(syntheticH264Idr(), 0L, true);
            }
        }
    }

    @Test
    void muxSenderRefusesPktSizeBelowOnePacket() throws Exception {
        try (DatagramSocket peer = peer()) {
            String url = "rtp://127.0.0.1:" + peer.getLocalPort();
            // A 188-byte datagram leaves 176 bytes after the RTP header: not
            // enough for one TS packet, so the send is refused rather than
            // leaving the muxer output unsent.
            try (MuxSender s = MuxSender.open(url + "?pkt_size=188", roundtripConfig())) {
                org.tstrans.RtpException e = assertThrows(org.tstrans.RtpException.class,
                    () -> s.sendVideo(syntheticH264Idr(), 0L, true));
                assertTrue(e.getMessage().contains("exceeds payload-size cap"),
                    "unexpected message: " + e.getMessage());
            }
        }
    }

    @Test
    void muxSenderRejectsNegativePktSize() {
        assertThrows(org.tstrans.RtpException.class,
            () -> MuxSender.open("rtp://127.0.0.1:5006?pkt_size=-1", roundtripConfig()));
    }

    @Test
    void muxSenderClosedStateThrows() throws Exception {
        MuxSender s = MuxSender.open("rtp://127.0.0.1:5007", roundtripConfig());
        s.close();
        assertFalse(s.isAlive());
        assertThrows(IllegalStateException.class, () -> s.sendVideo(syntheticH264Idr(), 0L, true));
        assertThrows(IllegalStateException.class, s::stats);
        s.close(); // idempotent
    }

    @Test
    void muxSenderRejectsBadUrl() {
        assertThrows(IllegalArgumentException.class,
            () -> MuxSender.open("not-a-url", roundtripConfig()));
    }

    // ── DemuxReceiver standalone (non-live) tests ──────────────────────────

    @Test
    void demuxReceiverConstructsAndReportsClosedState() throws Exception {
        DemuxReceiver rx = DemuxReceiver.open("rtp://127.0.0.1:" + freeUdpPort());
        assertTrue(rx.isAlive());
        // No sender → stats are zeroed but the call succeeds.
        TransportStats st = rx.stats();
        assertNotNull(st.socketStats());
        assertNotNull(st.muxerStats());
        // addByteSink before iterating is allowed.
        rx.addByteSink(pkt -> { /* no-op */ });
        rx.close();
        assertFalse(rx.isAlive());
        assertThrows(IllegalStateException.class, () -> rx.addByteSink(pkt -> {}));
        assertThrows(IllegalStateException.class, rx::stats);
        rx.close(); // idempotent
    }

    @Test
    void demuxReceiverRejectsBadUrl() {
        assertThrows(IllegalArgumentException.class,
            () -> DemuxReceiver.open("not-a-url"));
    }

    @Test
    void demuxReceiverWithConfig() throws Exception {
        org.tstrans.mpegts.DemuxerConfig cfg =
            org.tstrans.mpegts.DemuxerConfig.builder().build();
        try (DemuxReceiver rx = DemuxReceiver.open("rtp://127.0.0.1:" + freeUdpPort(), cfg)) {
            assertTrue(rx.isAlive());
        }
    }
}
