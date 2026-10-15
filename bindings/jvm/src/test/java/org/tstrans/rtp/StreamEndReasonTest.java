package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;

/**
 * {@code endReason()} / {@code endDetail()} on {@link Receiver}, {@link
 * DemuxReceiver}, and {@link H264Receiver}. Sockets bind to an ephemeral
 * port ({@code :0}) and never recv, so these tests are fully deterministic —
 * no peer, no wall clock.
 */
class StreamEndReasonTest {

    // (a) value-pin: fromWireOrdinal is package-private, callable directly
    // from this same-package test. Mirrors tst-py's
    // test_stream_end_reason_values_are_pinned.
    @Test
    void wireOrdinalsArePinned() {
        assertEquals(StreamEndReason.CLEAN_TEARDOWN, StreamEndReason.fromWireOrdinal(1));
        assertEquals(StreamEndReason.SESSION_EXPIRED, StreamEndReason.fromWireOrdinal(2));
        assertEquals(StreamEndReason.KEEPALIVE_FAILED, StreamEndReason.fromWireOrdinal(3));
        assertEquals(StreamEndReason.TRANSPORT_FAILED, StreamEndReason.fromWireOrdinal(4));
        assertEquals(StreamEndReason.PROTOCOL_ERROR, StreamEndReason.fromWireOrdinal(5));
        assertEquals(StreamEndReason.CANCELLED, StreamEndReason.fromWireOrdinal(6));
        // -1 (the native "not ended" sentinel), 0, and any other unrecognized
        // value (a future non-exhaustive tst_core::transport::EndReason variant)
        // all map to null, never throw.
        assertNull(StreamEndReason.fromWireOrdinal(-1));
        assertNull(StreamEndReason.fromWireOrdinal(0));
        assertNull(StreamEndReason.fromWireOrdinal(7));
    }

    @Test
    void receiverFreshNullThenClosedCancelled() throws Exception {
        Receiver r = Receiver.fromUrl("rtp://127.0.0.1:0");
        assertNull(r.endReason());
        assertNull(r.endDetail());

        r.close();
        assertEquals(StreamEndReason.CANCELLED, r.endReason());
        assertNull(r.endDetail());

        // Idempotent second close must not clobber the recorded reason.
        r.close();
        assertEquals(StreamEndReason.CANCELLED, r.endReason());
        assertNull(r.endDetail());
    }

    @Test
    void demuxReceiverFreshNullThenClosedCancelled() throws Exception {
        DemuxReceiver rx = DemuxReceiver.fromUrl("rtp://127.0.0.1:0");
        assertNull(rx.endReason());
        assertNull(rx.endDetail());

        rx.close();
        assertEquals(StreamEndReason.CANCELLED, rx.endReason());
        assertNull(rx.endDetail());

        rx.close();
        assertEquals(StreamEndReason.CANCELLED, rx.endReason());
        assertNull(rx.endDetail());
    }

    @Test
    void h264ReceiverFreshNullThenClosedCancelled() throws Exception {
        H264Receiver rx = H264Receiver.listen("rtp://127.0.0.1:0?pt=96");
        assertNull(rx.endReason());
        assertNull(rx.endDetail());

        rx.close();
        assertEquals(StreamEndReason.CANCELLED, rx.endReason());
        assertNull(rx.endDetail());

        rx.close();
        assertEquals(StreamEndReason.CANCELLED, rx.endReason());
        assertNull(rx.endDetail());
    }
}
