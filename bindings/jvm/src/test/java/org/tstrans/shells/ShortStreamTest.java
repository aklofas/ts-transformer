package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.TcpException;
import org.tstrans.TestSupport;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.Muxer;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

/**
 * A stream of one or two TS packets arrives, both ways. The far side of each shell is a plain
 * socket, never one of this library's syncing receivers, and a TCP peer ends on its end of
 * file, so no timing is involved.
 */
class ShortStreamTest {
    private static final long END_MS = TestSupport.scaledMs(10_000);

    /** {@code n} TS packets: sync byte, a distinct PID per packet, a payload that holds no 0x47. */
    private static byte[] packets(int n) {
        byte[] out = new byte[n * 188];
        for (int i = 0; i < n; i++) {
            int off = i * 188;
            out[off] = 0x47;
            out[off + 1] = 0x01;
            out[off + 2] = (byte) (0x10 + i);
            out[off + 3] = 0x10;
            for (int j = 4; j < 188; j++) out[off + j] = (byte) ((j % 0x40) | 0x80);
        }
        return out;
    }

    @Test
    @Timeout(30)
    void finishSendsAOnePacketStream() throws Exception {
        byte[] one = packets(1);
        try (TcpSink sink = new TcpSink(); Sender tx = Sender.open(sink.url())) {
            tx.sendBytes(one);
            tx.finish();
            assertArrayEquals(one, sink.awaitEnd(END_MS));
        }
    }

    @Test
    @Timeout(30)
    void flushSendsATwoPacketStream() throws Exception {
        byte[] two = packets(2);
        try (TcpSink sink = new TcpSink(); Sender tx = Sender.open(sink.url())) {
            tx.sendBytes(two);
            tx.flush();
            // On the wire before any end: the flush sent them.
            sink.awaitBytes(two.length, END_MS);
            tx.finish();
            assertArrayEquals(two, sink.awaitEnd(END_MS));
        }
    }

    @Test
    @Timeout(30)
    void finishReportsAPartialLastPacket() throws Exception {
        byte[] input = Arrays.copyOf(packets(2), 188 + 100);
        try (TcpSink sink = new TcpSink(); Sender tx = Sender.open(sink.url())) {
            tx.sendBytes(input);
            TcpException e = assertThrows(TcpException.class, tx::finish);
            assertEquals(TcpException.Kind.INPUT_MALFORMED, e.kind(), e.getMessage());
            // Everything it could send went out first, and the transport is closed.
            assertArrayEquals(Arrays.copyOf(input, 188), sink.awaitEnd(END_MS));
            tx.finish();   // a second finish is quiet
        }
    }

    /** The PAT and the PMT a muxer writes first: a two-packet stream with one event in it. */
    private static byte[] patAndPmt() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder().programNumber(1).pmtPid(0x100)
            .addVideo(0x101, VideoCodec.H264).build();
        try (Muxer m = new Muxer(cfg)) {
            m.pushVideo(TestSupport.syntheticH264Idr(), 90_000L, true);
            byte[] ts = TestSupport.drain(m);
            assertEquals(0, (ts[1] & 0x1F) << 8 | (ts[2] & 0xFF), "the first packet is the PAT");
            assertEquals(0x100, (ts[189] & 0x1F) << 8 | (ts[190] & 0xFF), "the second is the PMT");
            return Arrays.copyOf(ts, 2 * 188);
        }
    }

    /** A loopback server that writes {@code bytes} to its one connection and closes it. */
    private static ServerSocket serving(byte[] bytes) throws Exception {
        ServerSocket server = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
        CompletableFuture.runAsync(() -> {
            try (Socket s = server.accept(); OutputStream out = s.getOutputStream()) {
                out.write(bytes);
            } catch (Exception ignored) {
                // the test failed and closed the server
            }
        });
        return server;
    }

    @Test
    @Timeout(30)
    void aTwoPacketStreamIsReceived() throws Exception {
        byte[] two = patAndPmt();
        try (ServerSocket server = serving(two);
             Receiver rx = Receiver.open("tcp://127.0.0.1:" + server.getLocalPort())) {
            assertArrayEquals(Arrays.copyOfRange(two, 0, 188), rx.recvBytes());
            assertArrayEquals(Arrays.copyOfRange(two, 188, 376), rx.recvBytes());
            TcpException end = assertThrows(TcpException.class, rx::recvBytes);
            assertEquals(TcpException.Kind.END_OF_STREAM, end.kind());
            assertEquals(2, rx.stats().packetsReceived());
        }
        try (ServerSocket server = serving(two);
             DemuxReceiver rx = DemuxReceiver.open("tcp://127.0.0.1:" + server.getLocalPort())) {
            List<DemuxEvent> events = new ArrayList<>();
            DemuxEvent e;
            while ((e = rx.recvEvent()) != null) events.add(e);
            assertEquals(1, events.size(), events.toString());
            DemuxEvent.ProgramMap pm = (DemuxEvent.ProgramMap) events.get(0);
            assertEquals(0x100, pm.pmtPid());
            assertEquals(List.of(0x101), pm.elementaryPids());
            // The end, for every later call.
            assertNull(rx.recvEvent());
            assertNull(rx.recvEvent());
        }
    }

    @Test
    @Timeout(30)
    void theEndIsReportedAfterTheReleasedPackets() throws Exception {
        // Three packets, then a partial packet whose first byte is the sync byte: all three
        // arrive, then the end. The partial packet's own bytes are dropped.
        byte[] input = Arrays.copyOf(packets(4), 3 * 188 + 10);
        try (ServerSocket server = serving(input);
             Receiver rx = Receiver.open("tcp://127.0.0.1:" + server.getLocalPort())) {
            for (int i = 0; i < 3; i++) {
                assertArrayEquals(Arrays.copyOfRange(input, i * 188, (i + 1) * 188), rx.recvBytes());
            }
            assertEquals(TcpException.Kind.END_OF_STREAM, assertThrows(TcpException.class, rx::recvBytes).kind());
            assertEquals(3, rx.stats().packetsReceived(), rx.stats().toString());
        }
    }
}
