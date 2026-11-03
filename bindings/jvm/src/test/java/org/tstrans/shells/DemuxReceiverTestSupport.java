package org.tstrans.shells;

import java.io.IOException;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.util.Arrays;
import java.util.List;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import org.tstrans.TestSupport;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.Muxer;

/** Helpers shared by the {@link DemuxReceiver} live-socket tests. */
final class DemuxReceiverTestSupport {
    private DemuxReceiverTestSupport() { }

    /** {@code n} null packets (PID 0x1FFF): the demuxer never produces an event from them. */
    static byte[] nullPackets(int n) {
        byte[] out = new byte[n * 188];
        Arrays.fill(out, (byte) 0xFF);
        for (int i = 0; i < n; i++) {
            out[i * 188] = 0x47;
            out[i * 188 + 1] = 0x1F;
            out[i * 188 + 2] = (byte) 0xFF;
            out[i * 188 + 3] = 0x10;
        }
        return out;
    }

    /** PMT PID of {@link TestSupport#roundtripConfig()}. */
    static final int PMT_PID = 0x1000;

    /** PAT, PMT and three video PES of {@link TestSupport#roundtripConfig()}: events follow. */
    static byte[] eventPackets() throws Exception {
        try (Muxer m = new Muxer(TestSupport.roundtripConfig())) {
            for (int i = 0; i < 3; i++) {
                m.pushVideo(TestSupport.syntheticH264Idr(), i * 3000L, true);
            }
            return TestSupport.drain(m);
        }
    }

    static int pid(byte[] pkt) {
        return ((pkt[1] & 0x1F) << 8) | (pkt[2] & 0xFF);
    }

    /** A TCP receiver, its connected peer, and a cancel handle taken before any sink. */
    static final class Pair implements AutoCloseable {
        final ServerSocket server;
        final DemuxReceiver rx;
        final java.net.Socket peer;
        final CancelHandle rescue;

        Pair() throws Exception {
            server = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
            rx = DemuxReceiver.open("tcp://127.0.0.1:" + server.getLocalPort());
            peer = server.accept();
            rescue = rx.cancelHandle();
        }

        void write(byte[] bytes) throws IOException {
            peer.getOutputStream().write(bytes);
            peer.getOutputStream().flush();
        }

        @Override public void close() throws IOException {
            rescue.cancel();
            rx.close();
            peer.close();
            server.close();
        }
    }

    /** One {@code recvEvent()} on a daemon thread: the event (as an Optional) or the throwable. */
    static CompletableFuture<Object> recvOnDaemon(DemuxReceiver rx) {
        CompletableFuture<Object> f = new CompletableFuture<>();
        Thread t = new Thread(() -> {
            try {
                f.complete(Optional.ofNullable(rx.recvEvent()));
            } catch (Throwable e) {
                f.complete(e);
            }
        }, "sink-recv");
        t.setDaemon(true);
        t.start();
        return f;
    }

    /** Iterate {@code rx} to its end on a daemon thread: the events, or the throwable. */
    static CompletableFuture<Object> drainOnDaemon(DemuxReceiver rx, List<DemuxEvent> events) {
        CompletableFuture<Object> f = new CompletableFuture<>();
        Thread t = new Thread(() -> {
            try {
                for (DemuxEvent e : rx) events.add(e);
                f.complete(events);
            } catch (Throwable e) {
                f.complete(e);
            }
        }, "sink-drain");
        t.setDaemon(true);
        t.start();
        return f;
    }

    static Object await(CompletableFuture<Object> f) throws Exception {
        return f.get(TestSupport.scaledMs(5_000), TimeUnit.MILLISECONDS);
    }
}
