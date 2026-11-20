package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.freeUdpPort;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.scaledMs;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.io.ByteArrayOutputStream;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetAddress;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.BooleanSupplier;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.ValueSource;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.rtp.RtpStats;
import org.tstrans.srt.Builder;
import org.tstrans.srt.Listener;
import org.tstrans.srt.ManagedDemuxReceiver;
import org.tstrans.srt.Socket;

/**
 * The receivers' statistics records, read against what the test sent, and the RTP
 * counters a receiving shell over {@code rtp://} reaches without its receive slot.
 */
@Timeout(60)
class DemuxReceiverStatsTest {
    private static final int PMT_PID = 0x0100;
    private static final int VIDEO_PID = 0x0101;
    private static final long WAIT_MS = scaledMs(10_000);

    // ── TS construction ──────────────────────────────────────────────────────

    private static byte[] tsPacket(int pid, byte[] payload, boolean pusi, int cc) {
        byte[] p = new byte[188];
        p[0] = 0x47;
        p[1] = (byte) ((pusi ? 0x40 : 0) | ((pid >> 8) & 0x1F));
        p[2] = (byte) pid;
        p[3] = (byte) (0x10 | (cc & 0x0F));
        System.arraycopy(payload, 0, p, 4, payload.length);
        return p;
    }

    private static int crc32Mpeg(byte[] data) {
        int crc = 0xFFFFFFFF;
        for (byte b : data) {
            crc ^= (b & 0xFF) << 24;
            for (int i = 0; i < 8; i++) crc = (crc & 0x80000000) != 0 ? (crc << 1) ^ 0x04C11DB7 : crc << 1;
        }
        return crc;
    }

    /** One section, CRC added, after a zero pointer field; 0xFF stuffing after it. */
    private static byte[] psiPacket(int pid, byte[] sectionNoCrc, int cc) {
        int crc = crc32Mpeg(sectionNoCrc);
        byte[] payload = new byte[184];
        java.util.Arrays.fill(payload, (byte) 0xFF);
        payload[0] = 0;
        System.arraycopy(sectionNoCrc, 0, payload, 1, sectionNoCrc.length);
        int at = 1 + sectionNoCrc.length;
        payload[at] = (byte) (crc >>> 24);
        payload[at + 1] = (byte) (crc >>> 16);
        payload[at + 2] = (byte) (crc >>> 8);
        payload[at + 3] = (byte) crc;
        return tsPacket(pid, payload, true, cc);
    }

    /** PAT: program 1 on {@link #PMT_PID}. */
    private static byte[] patPacket() {
        return psiPacket(0, new byte[] {
            0x00, (byte) 0xB0, 13, 0x00, 0x01, (byte) 0xC1, 0x00, 0x00,
            0x00, 0x01, (byte) (0xE0 | (PMT_PID >> 8)), (byte) PMT_PID,
        }, 0);
    }

    /** PMT for program 1: one H.264 stream on {@link #VIDEO_PID}, its PCR PID. */
    private static byte[] pmtPacket(int version, int cc) {
        return psiPacket(PMT_PID, new byte[] {
            0x02, (byte) 0xB0, 18, 0x00, 0x01, (byte) (0xC1 | ((version & 0x1F) << 1)), 0x00, 0x00,
            (byte) (0xE0 | (VIDEO_PID >> 8)), (byte) VIDEO_PID, (byte) 0xF0, 0x00,
            0x1B, (byte) (0xE0 | (VIDEO_PID >> 8)), (byte) VIDEO_PID, (byte) 0xF0, 0x00,
        }, cc);
    }

    /**
     * PAT, PMT version 0 twice, a video PES whose third packet skips a continuity
     * counter, then PMT version 1 and three null packets (so the last PMT is not the
     * final packet of the datagram): two PMT versions, one continuity gap.
     */
    private static List<byte[]> statsCapture() {
        byte[] pesStart = {0, 0, 1, (byte) 0xE0, 0, 0, (byte) 0x80, 0, 0, 0, 0, 0, 1, 0x09, (byte) 0xF0};
        List<byte[]> c = new ArrayList<>();
        c.add(patPacket());
        c.add(pmtPacket(0, 0));
        c.add(pmtPacket(0, 1));
        c.add(tsPacket(VIDEO_PID, pesStart, true, 0));
        c.add(tsPacket(VIDEO_PID, new byte[0], false, 1));
        c.add(tsPacket(VIDEO_PID, new byte[0], false, 3)); // expected 2: one gap
        c.add(pmtPacket(1, 2));
        for (int i = 0; i < 3; i++) c.add(tsPacket(0x1FFF, new byte[0], false, i));
        return c;
    }

    private static byte[] concat(List<byte[]> packets) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        for (byte[] p : packets) out.writeBytes(p);
        return out.toByteArray();
    }

    private static void sendDatagram(int port, byte[] data) throws Exception {
        try (DatagramSocket s = new DatagramSocket()) {
            s.send(new DatagramPacket(data, data.length, InetAddress.getLoopbackAddress(), port));
        }
    }

    /** Receive until the first {@code ProgramMap} after a {@code Discontinuity}: PMT version 1. */
    private static void receiveUntilTheSecondProgramMap(DemuxReceiver rx) throws Exception {
        ShellPeers.within(WAIT_MS, "the second ProgramMap", () -> {
            boolean gap = false;
            while (true) {
                DemuxEvent e = rx.recvEvent();
                if (e == null) throw new AssertionError("the receiver ended");
                if (e instanceof DemuxEvent.Discontinuity) gap = true;
                if (gap && e instanceof DemuxEvent.ProgramMap) return null;
            }
        });
    }

    private static boolean waitFor(BooleanSupplier cond) throws InterruptedException {
        long end = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(WAIT_MS);
        while (System.nanoTime() < end) {
            if (cond.getAsBoolean()) return true;
            Thread.sleep(10);
        }
        return cond.getAsBoolean();
    }

    // ── the records ──────────────────────────────────────────────────────────

    @Test
    void demuxReceiverStatsValues() throws Exception {
        int port = freeUdpPort();
        List<byte[]> capture = statsCapture();
        DemuxReceiverStats s;
        try (DemuxReceiver rx = DemuxReceiver.open("udp://127.0.0.1:" + port)) {
            sendDatagram(port, concat(capture));
            receiveUntilTheSecondProgramMap(rx);
            s = rx.stats();
            assertEquals(2, s.programMapsSeen(), s.toString());
            assertEquals(2, s.pmtVersionsSeen(), s.toString());
            assertEquals(1, s.discontinuities(), s.toString());
            assertEquals(0, s.nonconformant(), s.toString());
            // Read at least up to the second PMT, at most the whole datagram.
            assertTrue(s.packetsReceived() >= 7 && s.packetsReceived() <= capture.size(), s.toString());
            assertEquals(s.packetsReceived() * 188, s.bytesReceived());
            rx.close();
            assertThrows(IllegalStateException.class, rx::stats);
        }
        // A record taken before the close keeps its values.
        assertEquals(2, s.programMapsSeen());
    }

    @Test
    void receiverStatsValues() throws Exception {
        int port = freeUdpPort();
        List<byte[]> packets = new ArrayList<>();
        for (int i = 0; i < 8; i++) packets.add(tsPacket(0x1FFF, new byte[0], false, i));
        byte[] datagram = new byte[5 + 8 * 188];
        System.arraycopy(concat(packets), 0, datagram, 5, 8 * 188);
        Receiver rx = Receiver.open("udp://127.0.0.1:" + port);
        try {
            sendDatagram(port, datagram);
            for (int i = 0; i < 4; i++) {
                assertEquals(188, ShellPeers.within(WAIT_MS, "recvBytes", rx::recvBytes).length);
            }
            ReceiverStats s = rx.stats();
            // Five bytes of garbage skipped, one lock, four packets read.
            assertEquals(4, s.packetsReceived(), s.toString());
            assertEquals(4 * 188, s.bytesReceived(), s.toString());
            assertEquals(5, s.bytesSkippedForSync(), s.toString());
            assertEquals(1, s.resyncEvents(), s.toString());
        } finally {
            rx.close();
        }
        assertThrows(IllegalStateException.class, rx::stats);
    }

    @Test
    void managedDemuxReceiverStats() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        CompletableFuture<Integer> portFuture = new CompletableFuture<>();
        CountDownLatch observed = new CountDownLatch(1);
        // The managed shell is the caller on this thread; the peer streams until observed.
        Thread peer = new Thread(() -> {
            try (Listener listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=120").listener().listen()) {
                portFuture.complete(listener.localAddr().getPort());
                try (Socket sock = listener.accept(null);
                     MuxSender ms = sock.intoMuxSender(roundtripConfig())) {
                    long pts = 0;
                    for (int round = 0; round < 400 && observed.getCount() > 0; round++) {
                        for (int i = 0; i < 6; i++, pts += 3000L) ms.sendVideo(syntheticH264Idr(), pts, true);
                        Thread.sleep(50);
                    }
                }
            } catch (Exception e) {
                portFuture.completeExceptionally(e);
            }
        }, "managed-stats-peer");
        peer.setDaemon(true);
        peer.start();
        int port = portFuture.get(WAIT_MS, TimeUnit.MILLISECONDS);
        ManagedDemuxReceiver rx = ManagedDemuxReceiver.fromUrl("srt://127.0.0.1:" + port + "?mode=caller&latency=120");
        try (ShellPeers.Watchdog w = new ShellPeers.Watchdog(rx.cancelHandle(), WAIT_MS)) {
            for (DemuxEvent e : rx) {
                if (e instanceof DemuxEvent.Video) break;
            }
            DemuxReceiverStats s = rx.stats();
            assertEquals(1, s.programMapsSeen(), s.toString());
            assertEquals(1, s.pmtVersionsSeen(), s.toString());
            assertEquals(0, s.discontinuities(), s.toString());
            assertTrue(s.packetsReceived() > 0, s.toString());
            assertEquals(s.packetsReceived() * 188, s.bytesReceived());
            // No reconnect: every packet read was fed to the demuxer.
            assertEquals(s.bytesReceived(), rx.tsBytesDemuxed());
        } finally {
            observed.countDown();
            rx.close();
        }
        assertThrows(IllegalStateException.class, rx::stats);
        peer.join(WAIT_MS);
    }

    // ── the RTP counters ─────────────────────────────────────────────────────

    /** A shell's receive and its RTP counters, whichever of Receiver / DemuxReceiver it is. */
    private interface RtpRx extends AutoCloseable {
        void recvOnce() throws Exception;
        Optional<RtpStats> rtpStats();
        CancelHandle cancelHandle();
        @Override void close();
    }

    private static RtpRx open(String shell, String url) throws Exception {
        if (shell.equals("Receiver")) {
            Receiver r = Receiver.open(url);
            return new RtpRx() {
                @Override public void recvOnce() throws Exception { r.recvBytes(); }
                @Override public Optional<RtpStats> rtpStats() { return r.rtpStats(); }
                @Override public CancelHandle cancelHandle() { return r.cancelHandle(); }
                @Override public void close() { r.close(); }
            };
        }
        DemuxReceiver d = DemuxReceiver.open(url);
        return new RtpRx() {
            @Override public void recvOnce() throws Exception { d.recvEvent(); }
            @Override public Optional<RtpStats> rtpStats() { return d.rtpStats(); }
            @Override public CancelHandle cancelHandle() { return d.cancelHandle(); }
            @Override public void close() { d.close(); }
        };
    }

    /** A receive parked on a daemon thread until the shell is closed; ends once it throws. */
    private static Thread park(RtpRx rx) {
        Thread t = new Thread(() -> {
            try {
                while (true) rx.recvOnce();
            } catch (Exception expected) {
                // CLOSED at close() ends it
            }
        }, "rtp-stats-parked");
        t.setDaemon(true);
        t.start();
        return t;
    }

    /** A datagram whose first byte says RTP version 1: dropped, counted malformed. */
    private static byte[] versionOneDatagram() {
        byte[] d = new byte[200];
        d[0] = 0x40;
        d[12] = 0x47;
        return d;
    }

    @ParameterizedTest
    @ValueSource(strings = {"Receiver", "DemuxReceiver"})
    void rtpStatsCountsAMalformedDatagram(String shell) throws Exception {
        int port = freeUdpPort();
        RtpRx rx = open(shell, "rtp://127.0.0.1:" + port);
        Thread t;
        try {
            t = park(rx);
            sendDatagram(port, versionOneDatagram());
            assertTrue(waitFor(() -> rx.rtpStats().orElseThrow().malformedPackets() == 1),
                () -> "rtpStats: " + rx.rtpStats());
            assertEquals(0, rx.rtpStats().orElseThrow().sourceRejected());
        } finally {
            rx.close();
        }
        t.join(WAIT_MS);
        assertFalse(t.isAlive());
    }

    @ParameterizedTest
    @ValueSource(strings = {"Receiver", "DemuxReceiver"})
    void rtpStatsIsEmptyOffRtp(String shell) throws Exception {
        RtpRx rx = open(shell, "udp://127.0.0.1:0");
        assertEquals(Optional.empty(), rx.rtpStats());
        rx.close();
        assertEquals(Optional.empty(), rx.rtpStats());
    }

    @ParameterizedTest
    @ValueSource(strings = {"Receiver", "DemuxReceiver"})
    void rtpStatsAnswersWhileParkedAndAfterClose(String shell) throws Exception {
        int port = freeUdpPort();
        RtpRx rx = open(shell, "rtp://127.0.0.1:" + port);
        Thread t = park(rx);
        try {
            for (int i = 0; i < 3; i++) sendDatagram(port, versionOneDatagram());
            assertTrue(waitFor(() -> rx.rtpStats().orElseThrow().malformedPackets() == 3),
                () -> "rtpStats: " + rx.rtpStats());
            // The receive has not returned: every datagram was dropped.
            assertTrue(t.isAlive());
            RtpStats got = ShellPeers.within(scaledMs(5_000), "rtpStats", () -> rx.rtpStats().orElseThrow());
            assertEquals(3, got.malformedPackets());
        } finally {
            rx.close();
        }
        t.join(WAIT_MS);
        assertFalse(t.isAlive());
        assertEquals(3, rx.rtpStats().orElseThrow().malformedPackets());
    }

    @ParameterizedTest
    @ValueSource(strings = {"Receiver", "DemuxReceiver"})
    void rtpStatsRacingCloseNeverFails(String shell) throws Exception {
        for (int round = 0; round < 20; round++) {
            RtpRx rx = open(shell, "rtp://127.0.0.1:" + freeUdpPort());
            AtomicBoolean stop = new AtomicBoolean();
            List<Object> answers = new CopyOnWriteArrayList<>();
            List<Throwable> errors = new CopyOnWriteArrayList<>();
            Thread reader = new Thread(() -> {
                while (!stop.get()) {
                    try {
                        answers.add(rx.rtpStats());
                    } catch (Throwable e) {
                        errors.add(e);
                        return;
                    }
                }
            }, "rtp-stats-reader");
            reader.setDaemon(true);
            reader.start();
            try {
                assertTrue(waitFor(() -> !answers.isEmpty()));
                rx.close();
                // Read on a little after the close, then stop.
                int n = answers.size();
                assertTrue(waitFor(() -> answers.size() > n + 10));
            } finally {
                stop.set(true);
                rx.close();
            }
            reader.join(WAIT_MS);
            assertFalse(reader.isAlive());
            assertEquals(List.of(), errors);
            for (Object a : answers) {
                assertInstanceOf(RtpStats.class, assertInstanceOf(Optional.class, a).orElseThrow());
            }
        }
    }

    @Test
    void rtpStatsAfterACloseUnderneathIsTheLastValue() throws Exception {
        int port = freeUdpPort();
        DemuxReceiver rx = DemuxReceiver.open("rtp://127.0.0.1:" + port);
        try {
            RtpStats before = rx.rtpStats().orElseThrow();
            org.tstrans.TestSupport.closeUnderneath(rx);
            assertEquals(Optional.of(before), rx.rtpStats());
        } finally {
            rx.close();
        }
    }
}
