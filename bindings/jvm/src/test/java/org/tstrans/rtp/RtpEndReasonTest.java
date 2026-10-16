package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.shells.EndReason;

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
        // all map to null, never throw.
        assertEquals(EndReason.END_OF_STREAM, EndReason.fromWireValue(7));
        assertEquals(EndReason.RECONNECT_EXHAUSTED, EndReason.fromWireValue(8));
        assertNull(EndReason.fromWireValue(-1));
        assertNull(EndReason.fromWireValue(0));
        assertNull(EndReason.fromWireValue(9));
    }

    @Test
    void receiverFreshNullThenClosedCancelled() throws Exception {
        RecvTransport r = RecvTransport.fromUrl("rtp://127.0.0.1:0");
        assertNull(r.endReason());
        assertNull(r.endDetail());

        r.close();
        assertEquals(EndReason.CANCELLED, r.endReason());
        assertNull(r.endDetail());

        // Idempotent second close must not clobber the recorded reason.
        r.close();
        assertEquals(EndReason.CANCELLED, r.endReason());
        assertNull(r.endDetail());
    }

    @Test
    void demuxReceiverFreshNullThenClosedCancelled() throws Exception {
        DemuxReceiver rx = DemuxReceiver.open("rtp://127.0.0.1:0");
        assertNull(rx.endReason());
        assertNull(rx.endDetail());

        rx.close();
        assertEquals(EndReason.CANCELLED, rx.endReason());
        assertNull(rx.endDetail());

        rx.close();
        assertEquals(EndReason.CANCELLED, rx.endReason());
        assertNull(rx.endDetail());
    }

    @Test
    void h264ReceiverFreshNullThenClosedCancelled() throws Exception {
        H264Receiver rx = H264Receiver.listen("rtp://127.0.0.1:0?pt=96");
        assertNull(rx.endReason());
        assertNull(rx.endDetail());

        rx.close();
        assertEquals(EndReason.CANCELLED, rx.endReason());
        assertNull(rx.endDetail());

        rx.close();
        assertEquals(EndReason.CANCELLED, rx.endReason());
        assertNull(rx.endDetail());
    }
}
