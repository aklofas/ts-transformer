package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtpException;

class RtpTransportTest {

    @Test
    void senderConstructsStatsCancelClose() throws Exception {
        // UDP "connect" sets the default destination; no peer is required, so
        // construction succeeds with no receiver listening.
        try (Sender s = Sender.fromUrl("rtp://127.0.0.1:50000")) {
            SocketStats st = s.socketStats();
            assertNotNull(st);
            assertEquals(0L, st.bytesSent());
            try (CancelHandle ch = s.cancelHandle()) {
                ch.cancel(); // idempotent, no throw
            }
        }
    }

    @Test
    void senderClosedThenSendThrowsIllegalState() throws Exception {
        Sender s = Sender.fromUrl("rtp://127.0.0.1:50001");
        s.close();
        s.close(); // idempotent
        assertThrows(IllegalStateException.class, () -> s.send(new byte[] {0x47}));
    }

    @Test
    void senderMalformedUrlThrowsRtpUrl() {
        RtpException ex = assertThrows(RtpException.class, () -> Sender.fromUrl("not-a-url"));
        // 0.7.0: a URL-parse failure is URL, not the retired catch-all TRANSPORT.
        assertEquals(RtpException.Kind.URL, ex.kind());
    }

    @Test
    void senderNegativePktSizeThrowsIllegalArgument() {
        assertThrows(IllegalArgumentException.class,
            () -> Sender.fromUrl("rtp://127.0.0.1:50002", -1, null));
    }

    @Test
    void senderSsrcOutOfRangeThrowsIllegalArgument() {
        assertThrows(IllegalArgumentException.class,
            () -> Sender.fromUrl("rtp://127.0.0.1:50003", 1316, 0x1_0000_0000L));
    }

    @Test
    void receiverConstructsStatsCancelClose() throws Exception {
        // Bind to an ephemeral port; do NOT call recv() (it would block).
        try (Receiver r = Receiver.fromUrl("rtp://127.0.0.1:0")) {
            assertNotNull(r.socketStats());
            try (CancelHandle ch = r.cancelHandle()) {
                ch.cancel();
            }
        }
    }

    @Test
    void receiverClosedThenRecvThrowsIllegalState() throws Exception {
        Receiver r = Receiver.fromUrl("rtp://127.0.0.1:0");
        r.close();
        assertThrows(IllegalStateException.class, r::recv);
    }

    @Test
    void receiverMalformedUrlThrowsRtpUrl() {
        RtpException ex = assertThrows(RtpException.class, () -> Receiver.fromUrl("not-a-url"));
        // 0.7.0: a URL-parse failure is URL, not the retired catch-all TRANSPORT.
        assertEquals(RtpException.Kind.URL, ex.kind());
    }

    @Test
    void receiverRejectsPktSizeQuery() {
        RtpException ex = assertThrows(RtpException.class,
                () -> Receiver.fromUrl("rtp://127.0.0.1:0?pkt_size=1316"));
        assertTrue(ex.getMessage().contains("send-side knob"),
                "teaching text expected, got: " + ex.getMessage());
    }

    @Test
    void receiverRecvTimeoutRaisesTimeoutKind() throws Exception {
        // `?recv_timeout=<ms>` arms a persistent recv deadline (wired by
        // RtpRecvSocketBuilder::from_url). A quiet socket (no sender) must throw
        // RtpException(BACKPRESSURE) once the deadline expires — distinct from
        // TRANSPORT, since the receiver stays open and usable (retry recv() again).
        try (Receiver r = Receiver.fromUrl("rtp://127.0.0.1:50004?recv_timeout=200")) {
            RtpException ex = assertThrows(RtpException.class, r::recv);
            assertEquals(RtpException.Kind.BACKPRESSURE, ex.kind());
            // The receiver is still alive after a TIMEOUT — a second recv on the
            // same (still-quiet) socket raises TIMEOUT again, not TRANSPORT.
            RtpException ex2 = assertThrows(RtpException.class, r::recv);
            assertEquals(RtpException.Kind.BACKPRESSURE, ex2.kind());
        }
    }

    /** Build a single 188-byte MPEG-TS packet (0x47 sync byte + filler). The
     *  RTP recv path enforces MP2T shape (188-byte aligned, 0x47-prefixed) and
     *  silently drops anything else — see {@code is_valid_mp2t_payload} in
     *  {@code tst-rtp/src/transport.rs} — so tests exercising real delivery
     *  through {@link Receiver#recv} must use TS-shaped payloads, not
     *  arbitrary bytes. */
    private static byte[] tsPacket(byte filler) {
        byte[] pkt = new byte[188];
        pkt[0] = 0x47;
        java.util.Arrays.fill(pkt, 1, pkt.length, filler);
        return pkt;
    }

    @Test
    void recvPerCallTimeoutRaisesTimeoutThenDeliversRealBytes() throws Exception {
        // No `?recv_timeout=` URL knob here — the deadline comes solely from
        // the per-call `recv(Integer)` argument.
        try (Receiver r = Receiver.fromUrl("rtp://127.0.0.1:50005")) {
            RtpException ex = assertThrows(RtpException.class, () -> r.recv(200));
            assertEquals(RtpException.Kind.BACKPRESSURE, ex.kind());

            // The receiver stays alive after a TIMEOUT (retryable): a real send
            // must be delivered on a subsequent recv(timeoutMs) call.
            byte[] sent = tsPacket((byte) 0xAB);
            try (Sender s = Sender.fromUrl("rtp://127.0.0.1:50005")) {
                s.send(sent);
            }
            byte[] received = r.recv(2000);
            assertArrayEquals(sent, received);
        }
    }

    @Test
    @Timeout(10)
    void recvNullTimeoutBlocksLikeNoArgRecv() throws Exception {
        // `recv((Integer) null)` must behave exactly like `recv()`: block past
        // a short delay rather than expiring, then return the delivered bytes.
        // The send is delayed on another thread so a bug that flattened `null`
        // to a short numeric deadline (instead of the -1 "block forever"
        // sentinel) would surface as a spurious TIMEOUT instead of silently
        // passing on an already-buffered datagram.
        //
        // A generous persistent `?recv_timeout=4000` backstop bounds the native
        // call itself: `recv((Integer) null)` maps to the untimed `recv_bytes`
        // path, which still honors this URL-armed deadline (same mechanism
        // `nRecv`'s existing tests exercise). The 300 ms delayed send clears
        // comfortably inside the 4 s window, so this still proves null doesn't
        // flatten to a short deadline — but if a real bug or a lost packet ever
        // left nothing to receive, the call throws RtpException(BACKPRESSURE) loudly
        // after 4 s instead of parking forever. @Timeout(5) alone is NOT a
        // sufficient guard here: it cannot interrupt a thread blocked in a
        // native socket read, so an unbounded native call guarded only by
        // @Timeout is a real, unkillable-hang risk in CI (this exact class of
        // bug pinned a Gradle daemon for 44 minutes during this test's
        // development — see task-D2-report.md).
        try (Receiver r = Receiver.fromUrl("rtp://127.0.0.1:50006?recv_timeout=4000")) {
            byte[] sent = tsPacket((byte) 0xCD);
            Thread sender = new Thread(() -> {
                try {
                    Thread.sleep(300);
                    try (Sender s = Sender.fromUrl("rtp://127.0.0.1:50006")) {
                        s.send(sent);
                    }
                } catch (Exception e) {
                    throw new RuntimeException(e);
                }
            });
            sender.setDaemon(true);
            sender.start();

            byte[] received = r.recv((Integer) null);
            assertArrayEquals(sent, received);
            sender.join(2_000);
        }
    }
}
