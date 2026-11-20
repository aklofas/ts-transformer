package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;

import java.util.Optional;
import org.junit.jupiter.api.Test;
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.shells.EndReason;
import org.tstrans.shells.Receiver;

/**
 * {@code endReason()} / {@code endDetail()} on {@link RecvTransport}, {@link
 * DemuxReceiver}, and {@link H264Receiver}. Sockets bind to an ephemeral
 * port ({@code :0}) and never recv, so these tests are fully deterministic —
 * no peer, no wall clock.
 */
class RtpEndReasonTest {

    // (a) value-pin of the cross-surface wire numbering. Mirrors tst-py's
    // test_stream_end_reason_values_are_pinned.
    @Test
    void wireOrdinalsArePinned() {
        assertEquals(EndReason.CLEAN_TEARDOWN, EndReason.fromWireValue(1));
        assertEquals(EndReason.SESSION_EXPIRED, EndReason.fromWireValue(2));
        assertEquals(EndReason.KEEPALIVE_FAILED, EndReason.fromWireValue(3));
        assertEquals(EndReason.TRANSPORT_FAILED, EndReason.fromWireValue(4));
        assertEquals(EndReason.PROTOCOL_ERROR, EndReason.fromWireValue(5));
        assertEquals(EndReason.CANCELLED, EndReason.fromWireValue(6));
        // -1 (the native "not ended" sentinel), 0, and any other unrecognized
        // value (a future non-exhaustive tst_core::transport::EndReason variant)
        // all map to null, never throw (the getters turn null into an empty Optional).
        assertEquals(EndReason.END_OF_STREAM, EndReason.fromWireValue(7));
        assertEquals(EndReason.RECONNECT_EXHAUSTED, EndReason.fromWireValue(8));
        assertNull(EndReason.fromWireValue(-1));
        assertNull(EndReason.fromWireValue(0));
        assertNull(EndReason.fromWireValue(9));
    }

    @Test
    void receiverFreshEmptyThenClosedCancelled() throws Exception {
        RecvTransport r = RecvTransport.fromUrl("rtp://127.0.0.1:0");
        assertEquals(Optional.empty(), r.endReason());
        assertEquals(Optional.empty(), r.endDetail());

        r.close();
        assertEquals(Optional.of(EndReason.CANCELLED), r.endReason());
        assertEquals(Optional.empty(), r.endDetail());

        // Idempotent second close must not clobber the recorded reason.
        r.close();
        assertEquals(Optional.of(EndReason.CANCELLED), r.endReason());
        assertEquals(Optional.empty(), r.endDetail());
    }

    @Test
    void demuxReceiverFreshEmptyThenClosedCancelled() throws Exception {
        DemuxReceiver rx = DemuxReceiver.open("rtp://127.0.0.1:0");
        assertEquals(Optional.empty(), rx.endReason());
        assertEquals(Optional.empty(), rx.endDetail());

        rx.close();
        assertEquals(Optional.of(EndReason.CANCELLED), rx.endReason());
        assertEquals(Optional.empty(), rx.endDetail());

        rx.close();
        assertEquals(Optional.of(EndReason.CANCELLED), rx.endReason());
        assertEquals(Optional.empty(), rx.endDetail());
    }

    @Test
    void h264ReceiverFreshEmptyThenClosedCancelled() throws Exception {
        H264Receiver rx = H264Receiver.listen("rtp://127.0.0.1:0?pt=96");
        assertEquals(Optional.empty(), rx.endReason());
        assertEquals(Optional.empty(), rx.endDetail());

        rx.close();
        assertEquals(Optional.of(EndReason.CANCELLED), rx.endReason());
        assertEquals(Optional.empty(), rx.endDetail());

        rx.close();
        assertEquals(Optional.of(EndReason.CANCELLED), rx.endReason());
        assertEquals(Optional.empty(), rx.endDetail());
    }

    @Test
    void endReasonIsEmptyWhileLive() throws Exception {
        try (RecvTransport t = RecvTransport.fromUrl("rtp://127.0.0.1:0");
                Receiver r = Receiver.open("rtp://127.0.0.1:0");
                DemuxReceiver d = DemuxReceiver.open("rtp://127.0.0.1:0");
                H264Receiver h = H264Receiver.listen("rtp://127.0.0.1:0?pt=96")) {
            Optional<EndReason> none = Optional.empty();
            assertEquals(none, t.endReason());
            assertEquals(none, r.endReason());
            assertEquals(none, d.endReason());
            assertEquals(none, h.endReason());
            assertEquals(Optional.empty(), t.endDetail());
            assertEquals(Optional.empty(), r.endDetail());
            assertEquals(Optional.empty(), d.endDetail());
            assertEquals(Optional.empty(), h.endDetail());
        }
    }

    @Test
    void endReasonIsPresentAfterTheEnd() throws Exception {
        RecvTransport t = RecvTransport.fromUrl("rtp://127.0.0.1:0");
        Receiver r = Receiver.open("rtp://127.0.0.1:0");
        DemuxReceiver d = DemuxReceiver.open("rtp://127.0.0.1:0");
        H264Receiver h = H264Receiver.listen("rtp://127.0.0.1:0?pt=96");
        // A cancel ends each one with CANCELLED while it is still open.
        t.cancelHandle().cancel();
        r.cancelHandle().cancel();
        d.cancelHandle().cancel();
        h.cancelHandle().cancel();
        assertThrows(org.tstrans.RtpException.class, t::recv);
        assertThrows(org.tstrans.RtpException.class, r::recvBytes);
        assertThrows(org.tstrans.RtpException.class, d::recvEvent);
        assertThrows(org.tstrans.RtpException.class, h::recvAu);
        Optional<EndReason> cancelled = Optional.of(EndReason.CANCELLED);
        assertEquals(cancelled, t.endReason());
        assertEquals(cancelled, r.endReason());
        assertEquals(cancelled, d.endReason());
        assertEquals(cancelled, h.endReason());
        t.close();
        r.close();
        d.close();
        h.close();
        assertEquals(cancelled, t.endReason());
        assertEquals(cancelled, r.endReason());
        assertEquals(cancelled, d.endReason());
        assertEquals(cancelled, h.endReason());
    }

    @Test
    void lastSeenIsEmptyForAPidNeverSeen() throws Exception {
        try (DemuxReceiver d = DemuxReceiver.open("rtp://127.0.0.1:0")) {
            assertEquals(Optional.empty(), d.lastSeen(0x1011));
            assertEquals(Optional.empty(), d.lastSeen(0x1FFF));
            assertThrows(IllegalArgumentException.class, () -> d.lastSeen(0x2000));
            assertThrows(IllegalArgumentException.class, () -> d.lastSeen(-1));
        }
    }
}
