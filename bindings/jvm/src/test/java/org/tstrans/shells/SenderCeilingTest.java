package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.*;

import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetAddress;
import org.junit.jupiter.api.Test;

/**
 * Seven TS packets through {@code udp://...?pkt_size=1128} (six packets) must
 * arrive as a 1128-byte datagram then a 188-byte one.
 */
class SenderCeilingTest {
    private static byte[] tsPackets(int n) {
        byte[] out = new byte[n * 188];
        for (int i = 0; i < out.length; i++) {
            out[i] = (i % 188 == 0) ? (byte) 0x47 : (byte) ((i % 251) | 1);
        }
        return out;
    }

    @Test
    void senderSplitsBundleAtPktSizeCeiling() throws Exception {
        try (DatagramSocket rx = new DatagramSocket(0, InetAddress.getLoopbackAddress())) {
            rx.setSoTimeout(5000);
            int port = rx.getLocalPort();

            byte[] data = tsPackets(7);
            try (Sender tx = Sender.open("udp://127.0.0.1:" + port + "?pkt_size=1128")) {
                tx.sendBytes(data);
                tx.flush();
            }

            DatagramPacket first = new DatagramPacket(new byte[2048], 2048);
            rx.receive(first);
            DatagramPacket second = new DatagramPacket(new byte[2048], 2048);
            rx.receive(second);
            assertEquals(1128, first.getLength());
            assertEquals(188, second.getLength());

            byte[] received = new byte[first.getLength() + second.getLength()];
            System.arraycopy(first.getData(), 0, received, 0, first.getLength());
            System.arraycopy(second.getData(), 0, received, first.getLength(), second.getLength());
            assertArrayEquals(data, received, "concatenated datagrams differ from the input");
        }
    }
}
