package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.ByteArrayOutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.util.ArrayList;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.Demuxer;
import org.tstrans.TcpException;

/**
 * The raw-observation recipe: a {@link Receiver} for the packets (to record
 * or forward), a {@link Demuxer} fed the same bytes for the events. Replaces
 * the removed byte sink.
 */
@Timeout(value = 60, threadMode = Timeout.ThreadMode.SEPARATE_THREAD)
class ReceiverDemuxerCompositionTest {

    /** {@code (event class, pid)}: the comparison key of the trace. */
    private static String key(DemuxEvent e) {
        int pid = -1;
        if (e instanceof DemuxEvent.ProgramMap m) pid = m.pmtPid();
        else if (e instanceof DemuxEvent.Video v) pid = v.stream().pid();
        else if (e instanceof DemuxEvent.Audio a) pid = a.stream().pid();
        else if (e instanceof DemuxEvent.Subtitle s) pid = s.stream().pid();
        else if (e instanceof DemuxEvent.UnknownSample u) pid = u.stream().pid();
        else if (e instanceof DemuxEvent.Metadata m) pid = m.stream().pid();
        else if (e instanceof DemuxEvent.NonConformant n) pid = n.stream().pid();
        else if (e instanceof DemuxEvent.Discontinuity d) pid = d.stream().pid();
        return e.getClass().getSimpleName() + "@" + pid;
    }

    /** A server that writes {@code bytes} to the one connection and closes it. */
    private static ServerSocket serveOnce(byte[] bytes) throws Exception {
        ServerSocket server = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
        Thread t = new Thread(() -> {
            try (Socket s = server.accept()) {
                s.getOutputStream().write(bytes);
                s.getOutputStream().flush();
            } catch (Exception ignored) {
                // the test fails on the receiving side
            }
        }, "composition-peer");
        t.setDaemon(true);
        t.start();
        return server;
    }

    @Test
    void packetsConcatenateToTheBytesWrittenAndDemuxToTheSameEvents() throws Exception {
        byte[] written = DemuxReceiverTestSupport.eventPackets();

        // Composition: Receiver chunks -> concatenation + Demuxer.
        ByteArrayOutputStream got = new ByteArrayOutputStream();
        List<String> composed = new ArrayList<>();
        try (ServerSocket server = serveOnce(written);
             Receiver rx = Receiver.open("tcp://127.0.0.1:" + server.getLocalPort());
             Demuxer demux = new Demuxer()) {
            while (true) {
                byte[] chunk;
                try {
                    chunk = rx.recvBytes();
                } catch (TcpException e) {
                    assertEquals(TcpException.Kind.END_OF_STREAM, e.kind());
                    break;
                }
                assertEquals(0, chunk.length % 188);
                got.write(chunk);
                demux.feed(chunk);
                for (DemuxEvent ev; (ev = demux.nextEvent()) != null; ) composed.add(key(ev));
            }
            demux.flush();
            for (DemuxEvent ev; (ev = demux.nextEvent()) != null; ) composed.add(key(ev));
        }
        assertArrayEquals(written, got.toByteArray());

        // Reference: a DemuxReceiver over the same bytes.
        List<String> shell = new ArrayList<>();
        try (DemuxReceiverTestSupport.Pair p = new DemuxReceiverTestSupport.Pair()) {
            p.write(written);
            p.peer.shutdownOutput();
            List<DemuxEvent> events = new ArrayList<>();
            Object r = DemuxReceiverTestSupport.await(
                DemuxReceiverTestSupport.drainOnDaemon(p.rx, events));
            assertFalse(r instanceof Throwable, String.valueOf(r));
            for (DemuxEvent ev : events) shell.add(key(ev));
        }

        assertTrue(shell.size() > 0);
        assertEquals(shell, composed);
    }
}
