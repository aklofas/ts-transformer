package org.tstrans;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.lang.reflect.Method;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetAddress;
import java.util.ArrayList;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;
import org.tstrans.rtp.RtspServer;
import org.tstrans.rtp.RtspServerConfig;
import org.tstrans.shells.Receiver;
import org.tstrans.shells.ShellPeers;
import org.tstrans.srt.ManagedReceiver;
import org.tstrans.srt.ManagedSender;

/**
 * Arguments and methods that do nothing are deprecated for removal in 0.12.0, on the
 * exact overload; the overload without the argument is not, and behaves the same.
 */
class DeprecatedSurfaceTest {
    private static final byte[] NULL = nullPacket();

    private static byte[] nullPacket() {
        byte[] p = new byte[188];
        p[0] = 0x47;
        p[1] = 0x1F;
        p[2] = (byte) 0xFF;
        p[3] = 0x10;
        for (int i = 4; i < 188; i++) p[i] = (byte) 0xFF;
        return p;
    }

    @Test
    void theFiveCarryDeprecatedForRemoval() throws Exception {
        List<Method> deprecated = List.of(
            RtspServer.class.getMethod("stop", long.class),
            Receiver.class.getMethod("recvBytes", int.class),
            ManagedReceiver.class.getMethod("recvBytes", int.class),
            ManagedSender.class.getMethod("srtStats"),
            ManagedReceiver.class.getMethod("srtStats"));
        for (Method m : deprecated) {
            Deprecated d = m.getAnnotation(Deprecated.class);
            assertNotNull(d, m + " is deprecated");
            assertTrue(d.forRemoval(), m + " is deprecated for removal");
            assertEquals("0.11.0", d.since(), m.toString());
        }
        List<Method> kept = List.of(
            RtspServer.class.getMethod("stop"),
            Receiver.class.getMethod("recvBytes"),
            ManagedReceiver.class.getMethod("recvBytes"),
            ManagedSender.class.getMethod("socketStats"),
            ManagedReceiver.class.getMethod("socketStats"));
        for (Method m : kept) assertNull(m.getAnnotation(Deprecated.class), m + " is not deprecated");
    }

    private static RtspServer server() throws Exception {
        return RtspServer.start(RtspServerConfig.builder()
            .bindAddr("127.0.0.1:0").gracefulShutdownDrainMs(50).build());
    }

    /** An ended server refuses a mount edit with {@code RtspException(CLOSED)}. */
    private static void assertStopped(RtspServer s) {
        MuxerConfig cfg = MuxerConfig.builder().programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264).build();
        RtspException e = assertThrows(RtspException.class, () -> s.addUnicastMount("/after", cfg));
        assertEquals(RtspException.Kind.CLOSED, e.kind());
    }

    @Test
    @Timeout(30)
    @SuppressWarnings("removal")
    void stopWithAndWithoutDrainStopTheSameWay() throws Exception {
        try (RtspServer with = server(); RtspServer without = server()) {
            with.stop(10_000);
            without.stop();
            assertStopped(with);
            assertStopped(without);
            // A second stop is quiet either way.
            with.stop(10);
            without.stop();
        }
    }

    @Test
    @Timeout(30)
    @SuppressWarnings("removal")
    void recvBytesWithAndWithoutMaxLenReturnTheSame() throws Exception {
        try (Receiver rx = Receiver.open("udp://127.0.0.1:0");
             DatagramSocket tx = new DatagramSocket()) {
            byte[] datagram = new byte[188 * 8];
            for (int i = 0; i < 8; i++) System.arraycopy(NULL, 0, datagram, i * 188, 188);
            tx.send(new DatagramPacket(datagram, datagram.length, InetAddress.getLoopbackAddress(),
                rx.localPort().getAsInt()));
            List<byte[]> got = new ArrayList<>();
            try (ShellPeers.Watchdog w = new ShellPeers.Watchdog(rx.cancelHandle(), TestSupport.scaledMs(10_000))) {
                // One packet per call either way, even a maxLen below one packet.
                got.add(rx.recvBytes());
                got.add(rx.recvBytes(1));
                got.add(rx.recvBytes());
                got.add(rx.recvBytes(1500));
            }
            for (byte[] p : got) assertArrayEquals(NULL, p);
        }
    }
}
