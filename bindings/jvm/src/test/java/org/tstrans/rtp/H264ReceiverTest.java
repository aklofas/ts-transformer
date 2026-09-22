package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;

import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetSocketAddress;
import java.util.ArrayList;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtpException;
import org.tstrans.RtspException;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

/**
 * Unit tests for {@link H264Receiver}.
 *
 * <p>Mostly offline — the loopback test hand-builds a single-NALU RFC 3550 +
 * RFC 6184 packet (same bytes as the Rust and Python loopback tests) and sends
 * it to a locally-bound receiver via {@link DatagramSocket}. Tests 8a/8b spin
 * the in-JVM {@link RtspServer} MP2T fixture (Linux-gated, same pattern as
 * {@link RtspServerClientLoopbackTest}) to exercise the
 * {@link RtspSession#intoH264Receiver()} guard paths.
 *
 * <h2>Packet layout (RFC 3550 §5.1 + RFC 6184 §5.6)</h2>
 * <pre>
 *   Byte 0:    0x80  (V=2, P=0, X=0, CC=0)
 *   Byte 1:    0x80 | 96  (M=1, PT=96)
 *   Bytes 2-3: seq=1 (big-endian)
 *   Bytes 4-7: ts=0x00002328 = 9000 ticks (big-endian)
 *   Bytes 8-11: ssrc=9 (big-endian)
 *   Byte 12:   0x65 (NALU type 5 = IDR)
 *   Bytes 13+: payload bytes 0xAB, 0xCD
 * </pre>
 * Expected AU: annexb = [0,0,0,1, 0x65, 0xAB, 0xCD]; pts = 0 (zero-based first
 * AU); keyFrame = true; rtpTimestamp = 9000.
 */
class H264ReceiverTest {

    /** Hand-built RFC 3550 + RFC 6184 single-NALU IDR packet. */
    private static final byte[] IDR_PACKET = {
        (byte)0x80,           // V=2, P=0, X=0, CC=0
        (byte)(0x80 | 96),   // M=1, PT=96
        0, 1,                 // seq=1
        0, 0, 0x23, 0x28,    // ts=9000
        0, 0, 0, 9,          // ssrc=9
        0x65,                 // NALU type 5 = IDR slice
        (byte)0xAB, (byte)0xCD  // payload bytes
    };

    // ── Test 1: UDP loopback — single IDR packet ──────────────────────────────

    /**
     * Bind an H264Receiver on an ephemeral port, push one canned IDR packet via
     * DatagramSocket, verify all AU fields.
     *
     * <p>Mirrors the Rust test {@code h264_receiver_udp_loopback_single_au} and the
     * Python test in {@code test_rtp.py}. The port-discovery pattern (bind-to-:0,
     * read port via localAddr()) is required: {@code rtp://127.0.0.1:0} hands port
     * selection to the OS and we discover the actual port from the receiver.
     */
    @Test
    void udpLoopbackSingleIdrPacket() throws Exception {
        try (H264Receiver rx = H264Receiver.listen("rtp://127.0.0.1:0?pt=96")) {
            // Discover ephemeral port — localAddr() returns "host:port".
            String addrStr = rx.localAddr();
            assertNotNull(addrStr, "localAddr() must be non-null for UDP receiver");
            int port = Integer.parseInt(addrStr.substring(addrStr.lastIndexOf(':') + 1));
            assertTrue(port > 0, "bound port must be positive");

            // Push the canned packet from a throwaway DatagramSocket.
            try (DatagramSocket tx = new DatagramSocket(
                    new InetSocketAddress("127.0.0.1", 0))) {
                DatagramPacket pkt = new DatagramPacket(IDR_PACKET, IDR_PACKET.length,
                    new InetSocketAddress("127.0.0.1", port));
                tx.send(pkt);
            }

            // Receive — blocks until packet arrives; returns null at EOS.
            H264AccessUnit au = rx.recvAu();
            assertNotNull(au, "first recvAu() must return an AU");

            // Annex B framing: [0,0,0,1] start code prepended to 0x65, 0xAB, 0xCD.
            byte[] expectedAnnexb = {0, 0, 0, 1, 0x65, (byte)0xAB, (byte)0xCD};
            assertArrayEquals(expectedAnnexb, au.annexb(),
                "annexb must have 4-byte start code + NALU bytes");

            // PTS: zero-based at the first AU (anchor = first RTP ts = 9000;
            // pts = 9000 - 9000 = 0 ticks).
            assertEquals(0L, au.pts(), "pts must be zero-based at first AU");

            assertTrue(au.keyFrame(), "IDR NALU (type 5) must set keyFrame=true");

            // RTP timestamp from the packet header (unsigned 32-bit → long).
            assertEquals(9000L, au.rtpTimestamp(), "rtpTimestamp must equal RTP ts field");

            // toString must not throw.
            assertNotNull(au.toString());
        }
    }

    // ── Test 1a: recvAu(Integer) per-call timeout, then real delivery ────────

    /**
     * A quiet {@code H264Receiver} (no {@code ?recv_timeout=} URL knob) must
     * raise {@code RtpException(BACKPRESSURE)} from {@code recvAu(200)}, and must
     * stay usable afterward: a real IDR packet (same fixture as
     * {@link #udpLoopbackSingleIdrPacket()}) delivered via {@code recvAu(2000)}.
     */
    @Test
    void recvAuPerCallTimeoutRaisesTimeoutThenDeliversRealAu() throws Exception {
        try (H264Receiver rx = H264Receiver.listen("rtp://127.0.0.1:0?pt=96")) {
            String addrStr = rx.localAddr();
            assertNotNull(addrStr);
            int port = Integer.parseInt(addrStr.substring(addrStr.lastIndexOf(':') + 1));

            RtpException ex = assertThrows(RtpException.class, () -> rx.recvAu(200));
            assertEquals(RtpException.Kind.BACKPRESSURE, ex.kind());

            // Receiver stays alive after a TIMEOUT (retryable): send the canned
            // IDR packet and confirm recvAu(2000) delivers it.
            try (DatagramSocket tx = new DatagramSocket(
                    new InetSocketAddress("127.0.0.1", 0))) {
                DatagramPacket pkt = new DatagramPacket(IDR_PACKET, IDR_PACKET.length,
                    new InetSocketAddress("127.0.0.1", port));
                tx.send(pkt);
            }
            H264AccessUnit au = rx.recvAu(2000);
            assertNotNull(au, "recvAu(2000) must return the delivered AU");
            byte[] expectedAnnexb = {0, 0, 0, 1, 0x65, (byte)0xAB, (byte)0xCD};
            assertArrayEquals(expectedAnnexb, au.annexb());
            assertTrue(au.keyFrame());
        }
    }

    // ── Test 1b: for-each iterator collects 2 AUs then terminates ────────────

    /**
     * Push 2 canned IDR packets to an H264Receiver, close it after sending,
     * then collect via for-each and assert both AUs arrive and the loop terminates.
     *
     * <p>Uses the marker-bit (M=1) property: the helper packet has M=1, so the
     * depacketizer emits one AU per packet immediately. Two packets → 2 AUs.
     * After the second packet arrives the test thread breaks out of the iterator
     * (the receiver is still open); a close() from the main thread after joining
     * ensures cleanup. This mirrors {@code DemuxReceiver}'s iteration contract.
     */
    @Test
    @Timeout(10)
    void iteratorCollectsTwoAus() throws Exception {
        try (H264Receiver rx = H264Receiver.listen("rtp://127.0.0.1:0?pt=96")) {
            String addrStr = rx.localAddr();
            assertNotNull(addrStr);
            int port = Integer.parseInt(addrStr.substring(addrStr.lastIndexOf(':') + 1));

            List<H264AccessUnit> collected = new ArrayList<>();
            // Consumer thread: collect exactly 2 AUs then break.
            Thread consumer = new Thread(() -> {
                for (H264AccessUnit au : rx) {
                    collected.add(au);
                    if (collected.size() >= 2) break;
                }
            });
            consumer.setDaemon(true);
            consumer.start();

            // Let the consumer park on the first recvAu().
            Thread.sleep(50);

            // Send 2 packets. M=1 on each → one AU emitted per packet.
            // Second packet uses a distinct RTP timestamp so PTS ordering is clear.
            try (DatagramSocket tx = new DatagramSocket(
                    new InetSocketAddress("127.0.0.1", 0))) {
                byte[] pkt1 = buildPkt((short) 1, 0, (byte) 0x65);
                byte[] pkt2 = buildPkt((short) 2, 3000, (byte) 0x65);
                tx.send(new DatagramPacket(pkt1, pkt1.length,
                    new InetSocketAddress("127.0.0.1", port)));
                Thread.sleep(10);
                tx.send(new DatagramPacket(pkt2, pkt2.length,
                    new InetSocketAddress("127.0.0.1", port)));
            }

            consumer.join(3_000);
            assertFalse(consumer.isAlive(), "consumer thread must finish within 3 s");
            assertEquals(2, collected.size(),
                "for-each must collect exactly 2 AUs (one per M=1 packet)");
            for (H264AccessUnit au : collected) {
                byte[] annexb = au.annexb();
                // All AUs start with the 4-byte Annex B start code.
                assertEquals(4, annexbStartCodeLen(annexb),
                    "AU must start with 4-byte Annex B start code");
            }
        }
    }

    /** Build a minimal M=1 single-NALU RTP packet with given seq, ts, and NALU type. */
    private static byte[] buildPkt(short seq, int ts, byte naluType) {
        return new byte[] {
            (byte) 0x80,
            (byte) (0x80 | 96),
            (byte) (seq >> 8), (byte) seq,
            (byte) (ts >> 24), (byte) (ts >> 16), (byte) (ts >> 8), (byte) ts,
            0, 0, 0, 9,          // ssrc = 9
            naluType, (byte) 0xAB, (byte) 0xCD
        };
    }

    /** Returns the length of the leading Annex B start code (3 or 4), or 0 if absent. */
    private static int annexbStartCodeLen(byte[] b) {
        if (b.length >= 4 && b[0] == 0 && b[1] == 0 && b[2] == 0 && b[3] == 1) return 4;
        if (b.length >= 3 && b[0] == 0 && b[1] == 0 && b[2] == 1) return 3;
        return 0;
    }

    // ── Test 2: close-then-recvAu → IllegalStateException ─────────────────────

    /**
     * After close(), recvAu() must throw {@link IllegalStateException}.
     * Mirrors DemuxReceiver's closed-state contract.
     */
    @Test
    void closePreventsFurtherRecv() throws Exception {
        H264Receiver rx = H264Receiver.listen("rtp://127.0.0.1:0?pt=96");
        rx.close();
        assertThrows(IllegalStateException.class, rx::recvAu,
            "recvAu() on closed receiver must throw IllegalStateException");
        // Idempotent close.
        rx.close();
    }

    // ── Test 3: config defaults observable from Java ──────────────────────────

    @Test
    void configDefaults() {
        H264DepayConfig cfg = H264DepayConfig.defaults();
        assertEquals(96, cfg.payloadType(), "default payloadType must be 96");
        assertEquals(ParameterSetInjection.BEFORE_IDR, cfg.parameterSetInjection(),
            "default parameterSetInjection must be BEFORE_IDR");
        assertTrue(cfg.initialParameterSets().isEmpty(),
            "default initialParameterSets must be empty");
        assertEquals(8 * 1024 * 1024L, cfg.maxAuBytes(),
            "default maxAuBytes must be 8 MiB (8388608)");
    }

    // ── Test 4: listen with explicit config ──────────────────────────────────

    @Test
    void listenWithConfig() throws Exception {
        H264DepayConfig cfg = H264DepayConfig.builder()
            .parameterSetInjection(ParameterSetInjection.NONE)
            .maxAuBytes(4 * 1024 * 1024L)
            .build();
        try (H264Receiver rx = H264Receiver.listen("rtp://127.0.0.1:0?pt=96", cfg)) {
            assertNotNull(rx.localAddr());
        }
    }

    // ── Test 5: DepayStats and RtpStats constructible ─────────────────────────

    /**
     * depayStats() and rtpStats() must return non-null snapshots with zero counters
     * before any AU has been received.
     */
    @Test
    void statsZeroedBeforeReceive() throws Exception {
        try (H264Receiver rx = H264Receiver.listen("rtp://127.0.0.1:0?pt=96")) {
            H264DepayStats depay = rx.depayStats();
            assertNotNull(depay);
            assertEquals(0L, depay.ausEmitted());
            assertEquals(0L, depay.seqGaps());

            RtpStats rtp = rx.rtpStats();
            assertNotNull(rtp);
            assertEquals(0L, rtp.malformedPackets());

            SocketStats sock = rx.socketStats();
            assertNotNull(sock);
            assertEquals(0L, sock.packetsReceived());
        }
    }

    // ── Test 6: cancelHandle() is usable ─────────────────────────────────────

    @Test
    void cancelHandleUsable() throws Exception {
        try (H264Receiver rx = H264Receiver.listen("rtp://127.0.0.1:0?pt=96")) {
            CancelHandle ch = rx.cancelHandle();
            assertNotNull(ch);
            // cancel() must not throw.
            ch.cancel();
            ch.close();
        }
    }

    // ── Test 7: listen without ?pt= must throw RtpException ──────────────────

    @Test
    void listenWithoutPtThrows() {
        assertThrows(RtpException.class,
            () -> H264Receiver.listen("rtp://127.0.0.1:0"),
            "listen without ?pt= must throw RtpException");
    }

    // ── Test 8a: MP2T session → intoH264Receiver() → PROTOCOL ────────────────

    /**
     * A session created by {@link RtspClient#connect} (the MP2T path) must reject
     * {@link RtspSession#intoH264Receiver()} with {@link RtspException} of kind
     * {@code PROTOCOL} — the "no H264DepayConfig stashed" guard. Fixture: the
     * in-JVM {@link RtspServer} with a unicast MP2T mount; no media needs to
     * flow — {@code connect()} drives OPTIONS/DESCRIBE/SETUP/PLAY against the
     * mount's static SDP.
     *
     * <p>Also pins the consume-on-failure contract (NativeHandle contract item
     * 3): the failed call consumed the session wrapper, so control methods
     * throw {@link IllegalStateException} and {@code close()} is a harmless
     * no-op (the native already tore the session down best-effort).
     */
    @Test
    @Timeout(20)
    void mp2tSessionIntoH264ReceiverThrowsProtocol() throws Exception {
        assumeTrue(isLinux(),
            "RTSP live fixture gated to Linux (real sockets + tokio runtime)");
        try (RtspServer server = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            String addr = server.localAddr();
            assertNotNull(addr, "server must be bound");
            int port = Integer.parseInt(addr.substring(addr.lastIndexOf(':') + 1));
            MountHandle mount = server.addUnicastMount("/live", mp2tCfg());
            try {
                RtspSession session = RtspClient.connect(
                    RtspClientConfig.of("rtsp://127.0.0.1:" + port + "/live"));
                RtspException ex = assertThrows(RtspException.class,
                    session::intoH264Receiver,
                    "intoH264Receiver() on a connect()-created (MP2T) session must throw");
                assertEquals(RtspException.Kind.PROTOCOL, ex.kind(),
                    "MP2T-vs-H264 guard must map to PROTOCOL");
                // Consume-on-failure: the wrapper is dead; close() is a no-op.
                assertThrows(IllegalStateException.class, session::pause,
                    "session must be consumed by the failed intoH264Receiver()");
                session.close();
            } finally {
                mount.close();
            }
        }
    }

    // ── Test 8b: closed session → intoH264Receiver() → IllegalStateException ─

    /**
     * {@code close()} then {@code intoH264Receiver()} must throw
     * {@link IllegalStateException} from the {@code ensureOpen} guard — the
     * consume-first path never reaches the native on an already-closed wrapper.
     */
    @Test
    @Timeout(20)
    void closedSessionIntoH264ReceiverThrowsIllegalState() throws Exception {
        assumeTrue(isLinux(),
            "RTSP live fixture gated to Linux (real sockets + tokio runtime)");
        try (RtspServer server = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            String addr = server.localAddr();
            assertNotNull(addr, "server must be bound");
            int port = Integer.parseInt(addr.substring(addr.lastIndexOf(':') + 1));
            MountHandle mount = server.addUnicastMount("/live", mp2tCfg());
            try {
                RtspSession session = RtspClient.connect(
                    RtspClientConfig.of("rtsp://127.0.0.1:" + port + "/live"));
                session.close(); // best-effort teardown + handle zeroed
                assertThrows(IllegalStateException.class, session::intoH264Receiver,
                    "intoH264Receiver() on a closed session must throw IllegalStateException");
            } finally {
                mount.close();
            }
        }
    }

    // ── Test 9: ParameterSetInjection enum ordinals ──────────────────────────

    @Test
    void parameterSetInjectionOrdinals() {
        // Ordinal stability: the JNI layer passes ordinals to Rust.
        assertEquals(0, ParameterSetInjection.NONE.ordinal());
        assertEquals(1, ParameterSetInjection.BEFORE_IDR.ordinal());
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    /** Minimal single-program H.264 MP2T mount config for the RTSP fixture. */
    private static MuxerConfig mp2tCfg() {
        return MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264).build();
    }
}
