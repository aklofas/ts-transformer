package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.*;

import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetAddress;
import org.junit.jupiter.api.Test;
import org.tstrans.TestSupport;
import org.tstrans.UdpException;
import org.tstrans.rtp.H264Receiver;

/**
 * A cancel is terminal for a shell's buffered output: after
 * {@code cancelHandle().cancel()} a receive, a sub-bundle send, a flush and a
 * finish all throw the transport's {@code CLOSED}, even with packets or bytes
 * still buffered. The H.264 receiver is the documented exception: it drains to
 * end of stream.
 */
class CancelTerminalTest {
    @Test
    void senderCancelThenPartialSendAndFlushThrowClosed() throws Exception {
        try (Sender tx = Sender.open("udp://127.0.0.1:" + TestSupport.freeUdpPort())) {
            tx.cancelHandle().cancel();
            UdpException send = assertThrows(UdpException.class,
                () -> tx.sendBytes(ShellPeers.TS_PACKET),
                "a sub-bundle send after cancel must not be buffered");
            assertEquals("CLOSED", ShellPeers.kindName(send));
            UdpException flush = assertThrows(UdpException.class, tx::flush);
            assertEquals("CLOSED", ShellPeers.kindName(flush));
        }
    }

    @Test
    void receiverCancelThenRecvThrowsClosedNotBufferedBytes() throws Exception {
        int port = TestSupport.freeUdpPort();
        try (Receiver rx = Receiver.open("udp://127.0.0.1:" + port);
             DatagramSocket peer = new DatagramSocket()) {
            // 16 packets in ONE datagram: the syncer locks after four aligned
            // packets, so one receive returns a packet and the rest stay queued.
            byte[] burst = new byte[16 * 188];
            for (int i = 0; i < 16; i++) {
                System.arraycopy(ShellPeers.TS_PACKET, 0, burst, i * 188, 188);
            }
            peer.send(new DatagramPacket(burst, burst.length,
                InetAddress.getLoopbackAddress(), port));
            byte[] first = ShellPeers.within(5_000, "first recvBytes", rx::recvBytes);
            assertEquals(188, first.length);
            rx.cancelHandle().cancel();
            UdpException e = assertThrows(UdpException.class, rx::recvBytes,
                "buffered packets must not be delivered after cancel");
            assertEquals("CLOSED", ShellPeers.kindName(e));
        }
    }

    @Test
    void muxSenderCancelThenFinishThrowsClosed() throws Exception {
        try (MuxSender tx = MuxSender.open("udp://127.0.0.1:" + TestSupport.freeUdpPort(),
                TestSupport.roundtripConfig())) {
            tx.cancelHandle().cancel();
            UdpException e = assertThrows(UdpException.class, tx::finish,
                "an empty finish after cancel is CLOSED, not a quiet no-op");
            assertEquals("CLOSED", ShellPeers.kindName(e));
        }
    }

    /** The documented exemption: the H.264 receiver drains to EOS on cancel. */
    @Test
    void h264ReceiverDrainsToEosAfterCancelNotClosed() throws Exception {
        try (H264Receiver rx = H264Receiver.listen("rtp://127.0.0.1:0?pt=96")) {
            rx.cancelHandle().cancel();
            assertNull(rx.recvAu(), "a cancelled H.264 receiver reports end of stream");
        }
    }
}
