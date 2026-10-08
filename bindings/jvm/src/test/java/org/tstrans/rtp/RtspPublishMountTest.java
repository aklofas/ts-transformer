package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.sha256Units;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.BooleanSupplier;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtpException;
import org.tstrans.RtspException;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.Demuxer;
import org.tstrans.mpegts.Muxer;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

/**
 * The RTSP publisher role: {@link RtspServer#addPublishMount}, {@link
 * RtspServer#nextPublisher}, {@link RtspServer#removeMount} and {@link PublishMount}.
 *
 * <p>The publisher is a plain {@link Socket} speaking RTSP by hand (ANNOUNCE, SETUP
 * {@code mode=record} over TCP-interleaved, RECORD): the library's RTSP client only
 * plays. Every socket read has a timeout, and every parked call is woken from a side
 * thread with a bounded join and a stop/cancel fallback, so a regression fails instead
 * of hanging the suite. Linux-gated like {@code RtspServerClientLoopbackTest} (real
 * sockets + a tokio runtime).
 */
class RtspPublishMountTest {

    private static final String SDP_MP2T =
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=publish\r\nc=IN IP4 127.0.0.1\r\n"
            + "t=0 0\r\nm=video 0 RTP/AVP 33\r\na=rtpmap:33 MP2T/90000\r\n"
            + "a=control:streamid=0\r\n";

    /** Seven MPEG-TS null packets (PID 0x1FFF): valid TS that demuxes to no events. */
    private static final byte[] NULL_BUNDLE = nullBundle();

    /** Bounded joins for side threads; they only ever expire on a regression. */
    private static final long JOIN_MS = TimeUnit.SECONDS.toMillis(15);

    /**
     * Timeout of the {@code nextPublisher} calls the shutdown tests park. Finite so a
     * regression that never wakes them still ends: the call then returns empty (or a
     * blocked side call completes), and the assertions fail instead of hanging.
     */
    private static final long PARK_MS = TimeUnit.SECONDS.toMillis(8);

    // ── Mount lifecycle without a publisher ──────────────────────────────────

    @Test
    @Timeout(30)
    void gettersBeforeAnyPublisher() throws Exception {
        assumeLinux();
        try (RtspServer server = start(false);
             PublishMount mount = server.addPublishMount("/cam")) {
            assertEquals("/cam", mount.mountPath());
            assertEquals(0L, mount.peerCount());
            assertEquals(0L, mount.generation());
            assertTrue(mount.publisher().isEmpty());
            PublishMountStats s = mount.stats();
            assertEquals(0L, s.rtpPacketsReceived());
            assertEquals(0L, s.generation());
            assertEquals(ClockAlignment.NOT_APPLICABLE, s.alignment());
            ServerStats st = server.stats();
            assertEquals(1L, st.mounts());
            assertEquals(0L, st.activePublishers());
            assertEquals(0L, st.totalRtpPacketsReceived());
            assertEquals(0L, st.totalRtpBytesReceived());
        }
    }

    @Test
    @Timeout(30)
    void duplicateAndInvalidPathsAreMountKind() throws Exception {
        assumeLinux();
        try (RtspServer server = start(false)) {
            server.addPublishMount("/cam").close();
            RtspException dup = assertThrows(RtspException.class,
                () -> server.addPublishMount("/cam"));
            assertEquals(RtspException.Kind.MOUNT, dup.kind());
            RtspException bad = assertThrows(RtspException.class,
                () -> server.addPublishMount("no-slash"));
            assertEquals(RtspException.Kind.MOUNT, bad.kind());
        }
    }

    @Test
    @Timeout(30)
    void nextPublisherIsEmptyOnTimeout() throws Exception {
        assumeLinux();
        try (RtspServer server = start(false)) {
            assertEquals(Optional.empty(), server.nextPublisher(100));
            assertEquals(Optional.empty(), server.nextPublisher(0));
            assertThrows(IllegalArgumentException.class, () -> server.nextPublisher(-1));
        }
    }

    @Test
    @Timeout(30)
    void removeMountTwiceIsMountKind() throws Exception {
        assumeLinux();
        try (RtspServer server = start(false);
             PublishMount mount = server.addPublishMount("/cam")) {
            server.removeMount("/cam");
            RtspException twice = assertThrows(RtspException.class,
                () -> server.removeMount("/cam"));
            assertEquals(RtspException.Kind.MOUNT, twice.kind());
            // The mount wrapper outlives its removal.
            assertEquals(0L, mount.stats().rtpPacketsReceived());
            // The path is free again.
            server.addPublishMount("/cam").close();
        }
    }

    @Test
    @Timeout(30)
    void intoDemuxReceiverIsTakeOnce() throws Exception {
        assumeLinux();
        try (RtspServer server = start(false);
             PublishMount mount = server.addPublishMount("/cam");
             DemuxReceiver rx = mount.intoDemuxReceiver()) {
            RtspException twice = assertThrows(RtspException.class, mount::intoDemuxReceiver);
            assertEquals(RtspException.Kind.CLOSED, twice.kind());
            // The take does not consume the mount wrapper.
            assertEquals(0L, mount.stats().generation());
            assertEquals("/cam", mount.mountPath());
        }
    }

    @Test
    @Timeout(30)
    void cancelEndsTheReceiverWithClosed() throws Exception {
        assumeLinux();
        try (RtspServer server = start(false);
             PublishMount mount = server.addPublishMount("/cam");
             DemuxReceiver rx = mount.intoDemuxReceiver()) {
            mount.cancel();
            RtpException e = assertThrows(RtpException.class, rx::recvEvent);
            assertEquals(RtpException.Kind.CLOSED, e.kind());
            assertEquals(StreamEndReason.CANCELLED, rx.endReason());
        }
    }

    @Test
    @Timeout(30)
    void removeMountEndsAParkedReceiverAtEndOfStream() throws Exception {
        assumeLinux();
        try (RtspServer server = start(false);
             PublishMount mount = server.addPublishMount("/cam");
             DemuxReceiver rx = mount.intoDemuxReceiver()) {
            Parked<DemuxEvent> parked = park(rx::recvEvent);
            server.removeMount("/cam");
            if (!parked.join()) {
                mount.cancel(); // unpark so the suite does not hang
                parked.join();
                fail("removeMount did not wake the parked receiver");
            }
            // END_OF_STREAM: recvEvent returns null, never an exception.
            assertNull(parked.error.get(), () -> "unexpected " + parked.error.get());
            assertNull(parked.value.get());
            assertEquals(StreamEndReason.CLEAN_TEARDOWN, rx.endReason());
        }
    }

    @Test
    @Timeout(30)
    void methodsAfterStopThrowServer() throws Exception {
        assumeLinux();
        try (RtspServer server = start(false);
             PublishMount mount = server.addPublishMount("/cam")) {
            server.stop();
            assertEquals(RtspException.Kind.SERVER,
                assertThrows(RtspException.class, () -> server.nextPublisher(100)).kind());
            assertEquals(RtspException.Kind.SERVER,
                assertThrows(RtspException.class, () -> server.addPublishMount("/x")).kind());
            assertEquals(RtspException.Kind.SERVER,
                assertThrows(RtspException.class, () -> server.removeMount("/cam")).kind());
            // The mount wrapper still answers.
            assertEquals(0L, mount.stats().rtpPacketsReceived());
            server.close();
            // A closed server object refuses every call before reaching the native.
            assertThrows(IllegalStateException.class, () -> server.nextPublisher(100));
            assertThrows(IllegalStateException.class, () -> server.removeMount("/cam"));
        }
    }

    // ── With a publisher ─────────────────────────────────────────────────────

    @Test
    @Timeout(30)
    void publisherCountersAndInfoAroundTeardown() throws Exception {
        assumeLinux();
        try (RtspServer server = start(false);
             PublishMount mount = server.addPublishMount("/cam");
             DemuxReceiver rx = mount.intoDemuxReceiver()) {
            long beforeMs = System.currentTimeMillis();
            try (RawPublisher pub = new RawPublisher(port(server), "/cam")) {
                for (int i = 0; i < 5; i++) pub.sendRtp(NULL_BUNDLE);
                assertTrue(waitFor(() -> mount.stats().rtpPacketsReceived() >= 5),
                    () -> "stats never reached 5 packets: " + mount.stats());
                PublishMountStats s = mount.stats();
                assertEquals(5L, s.rtpPacketsReceived());
                assertEquals(5L * (12 + NULL_BUNDLE.length), s.bytesReceived());
                assertEquals(0L, s.malformedPackets());
                assertEquals(0L, s.sourceRejected());
                assertEquals(ClockAlignment.NOT_APPLICABLE, s.alignment());

                PublisherInfo info = mount.publisher().orElseThrow();
                assertEquals(pub.localAddr(), info.peer());
                assertEquals(PublishShape.MP2T, info.shape());
                assertFalse(info.klv());
                assertEquals(0L, info.generation());
                // A wall-clock instant, not a duration: the ANNOUNCE came after
                // beforeMs (1 s slack for clock steps).
                assertTrue(info.sinceUnixMs() >= beforeMs - 1000,
                    () -> "sinceUnixMs " + info.sinceUnixMs() + " < " + beforeMs);

                ServerStats st = server.stats();
                assertEquals(1L, st.activePublishers());
                assertTrue(st.totalRtpPacketsReceived() >= 5);
                assertTrue(st.totalRtpBytesReceived() >= 5L * (12 + NULL_BUNDLE.length));

                assertEquals(200, pub.teardown());
                // The publisher's end bumps the generation and frees the slot.
                assertTrue(waitFor(() -> mount.generation() == 1),
                    () -> "generation stayed " + mount.generation());
                assertTrue(waitFor(() -> mount.publisher().isEmpty()));
                assertEquals(1L, mount.stats().generation());
                assertTrue(waitFor(() -> server.stats().activePublishers() == 0));
            }
        }
    }

    @Test
    @Timeout(30)
    void muxedStreamDemuxesToTheOfflineReferenceVideo() throws Exception {
        assumeLinux();
        byte[] ts = offlineTs();
        String offlineSha = offlineSha(ts);
        try (RtspServer server = start(false);
             PublishMount mount = server.addPublishMount("/cam");
             DemuxReceiver rx = mount.intoDemuxReceiver()) {
            // Watchdog: a regression that never delivers video ends the read with
            // RtpException(CLOSED) instead of hanging.
            CountDownLatch done = new CountDownLatch(1);
            Thread watchdog = daemon(() -> {
                try {
                    if (!done.await(JOIN_MS, TimeUnit.MILLISECONDS)) mount.cancel();
                } catch (InterruptedException ignored) {
                    // test finished
                }
            });
            try (RawPublisher pub = new RawPublisher(port(server), "/cam")) {
                for (int off = 0; off < ts.length; off += 7 * 188) {
                    int len = Math.min(7 * 188, ts.length - off);
                    byte[] bundle = new byte[len];
                    System.arraycopy(ts, off, bundle, 0, len);
                    pub.sendRtp(bundle);
                }
                String liveSha = null;
                DemuxEvent ev;
                while ((ev = rx.recvEvent()) != null) {
                    if (ev instanceof DemuxEvent.Video v && !v.parse().isEmpty()) {
                        liveSha = sha256Units(v.parse());
                        break;
                    }
                }
                assertEquals(offlineSha, liveSha,
                    "the published stream must demux to the offline Muxer→Demuxer video payload");
            } finally {
                done.countDown();
                watchdog.join(JOIN_MS);
            }
        }
    }

    @Test
    @Timeout(30)
    void nextPublisherHandsOutTheOnDemandMount() throws Exception {
        assumeLinux();
        try (RtspServer server = start(true);
             RawPublisher pub = new RawPublisher(port(server), "/auto")) {
            try (PublishMount mount = server.nextPublisher(TimeUnit.SECONDS.toMillis(5))
                    .orElseThrow(() -> new AssertionError("no on-demand mount arrived"))) {
                assertEquals("/auto", mount.mountPath());
                PublisherInfo info = mount.publisher().orElseThrow();
                assertEquals(PublishShape.MP2T, info.shape());
                assertEquals(pub.localAddr(), info.peer());
                try (DemuxReceiver rx = mount.intoDemuxReceiver()) {
                    // Take-once holds for the handle nextPublisher returned too.
                    RtspException twice = assertThrows(RtspException.class,
                        mount::intoDemuxReceiver);
                    assertEquals(RtspException.Kind.CLOSED, twice.kind());
                    pub.sendRtp(NULL_BUNDLE);
                    assertTrue(waitFor(() -> mount.stats().rtpPacketsReceived() == 1),
                        () -> "stats: " + mount.stats());
                }
                assertEquals(1L, server.stats().mounts());
                server.removeMount("/auto");
                assertEquals(0L, server.stats().mounts());
            }
        }
    }

    // ── Shutdown wakes parked calls ──────────────────────────────────────────

    @Test
    @Timeout(40)
    void closeWakesAParkedNextPublisherAndAParkedReceiver() throws Exception {
        assumeLinux();
        RtspServer server = start(true);
        PublishMount mount = server.addPublishMount("/cam");
        DemuxReceiver rx = mount.intoDemuxReceiver();
        try {
            Parked<Optional<PublishMount>> next = park(() -> server.nextPublisher(PARK_MS));
            Parked<DemuxEvent> recv = park(rx::recvEvent);
            Parked<Object> closer = park(() -> { server.close(); return null; });
            boolean woke = closer.join() && next.join() && recv.join();
            if (!woke) {
                server.close();
                mount.cancel();
                closer.join();
                next.join();
                recv.join();
                fail("close() did not wake the parked calls");
            }
            // close() runs stop(): the parked nextPublisher wakes with SERVER ...
            Throwable e = next.error.get();
            assertTrue(e instanceof RtspException,
                () -> "nextPublisher: expected RtspException(SERVER), got "
                    + (e != null ? e : "value " + next.value.get()));
            assertEquals(RtspException.Kind.SERVER, ((RtspException) e).kind());
            // ... and the publish mount's receiver reads end of stream.
            assertNull(recv.error.get(), () -> "receiver: unexpected " + recv.error.get());
            assertNull(recv.value.get());
            assertEquals(StreamEndReason.CLEAN_TEARDOWN, rx.endReason());
        } finally {
            rx.close();
            mount.close();
            server.close();
        }
    }

    @Test
    @Timeout(40)
    void aParkedNextPublisherHoldsNoServerLock() throws Exception {
        assumeLinux();
        RtspServer server = start(true);
        try {
            Parked<Optional<PublishMount>> next = park(() -> server.nextPublisher(PARK_MS));
            // Give the side thread time to reach the native wait. Not an assertion:
            // the correct code passes whether or not it got there first.
            Thread.sleep(200);
            // Other server calls answer while it waits.
            Parked<ServerStats> stats = park(server::stats);
            boolean answered = stats.join(TimeUnit.SECONDS.toMillis(4));
            Parked<Object> closer = park(() -> { server.close(); return null; });
            boolean closed = closer.join() && next.join();
            assertTrue(answered, "stats() blocked behind a parked nextPublisher");
            assertNull(stats.error.get());
            assertNotNull(stats.value.get());
            assertTrue(closed, "close() did not complete while nextPublisher was parked");
            assertTrue(next.error.get() instanceof RtspException,
                () -> "nextPublisher: expected RtspException(SERVER), got " + next.value.get());
            assertEquals(RtspException.Kind.SERVER, ((RtspException) next.error.get()).kind());
        } finally {
            server.close();
        }
    }

    // ── Records ──────────────────────────────────────────────────────────────

    @Test
    void recordsRoundTrip() {
        PublishMountStats s = new PublishMountStats(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
            ClockAlignment.SENDER_REPORT, 13, 14, 15, 16);
        assertEquals(4L, s.sourceRejected());
        assertEquals(10L, s.ausReordered());
        assertEquals(ClockAlignment.SENDER_REPORT, s.alignment());
        assertEquals(14L, s.ssrcChanges());
        assertEquals(16L, s.peerCount());
        PublisherInfo i = new PublisherInfo("127.0.0.1:1", PublishShape.ELEMENTARY, true, 7L, 2L);
        assertEquals(PublishShape.ELEMENTARY, i.shape());
        assertTrue(i.klv());
        // Ordinals are numbered as in the C ABI and the Python IntEnums.
        assertEquals(0, PublishShape.MP2T.ordinal());
        assertEquals(1, PublishShape.ELEMENTARY.ordinal());
        assertEquals(0, ClockAlignment.NOT_APPLICABLE.ordinal());
        assertEquals(1, ClockAlignment.PENDING.ordinal());
        assertEquals(2, ClockAlignment.PROVISIONAL.ordinal());
        assertEquals(3, ClockAlignment.SENDER_REPORT.ordinal());
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    private static void assumeLinux() {
        assumeTrue(isLinux(), "RTSP live publisher gated to Linux (real sockets + tokio runtime)");
    }

    private static RtspServer start(boolean acceptUnregistered) throws RtspException {
        return RtspServer.start(RtspServerConfig.builder()
            .bindAddr("127.0.0.1:0")
            .gracefulShutdownDrainMs(50)
            .acceptUnregisteredPublishers(acceptUnregistered)
            .build());
    }

    private static int port(RtspServer server) {
        String addr = server.localAddr();
        assertNotNull(addr, "server must be bound");
        return Integer.parseInt(addr.substring(addr.lastIndexOf(':') + 1));
    }

    /** Poll {@code cond} every 20 ms for up to 5 s. */
    private static boolean waitFor(BooleanSupplier cond) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (System.nanoTime() < deadline) {
            if (cond.getAsBoolean()) return true;
            Thread.sleep(20);
        }
        return cond.getAsBoolean();
    }

    private static byte[] nullBundle() {
        byte[] b = new byte[7 * 188];
        for (int p = 0; p < 7; p++) {
            int o = p * 188;
            b[o] = 0x47; b[o + 1] = 0x1F; b[o + 2] = (byte) 0xFF; b[o + 3] = 0x10;
            for (int i = 4; i < 188; i++) b[o + i] = (byte) 0xFF;
        }
        return b;
    }

    private static MuxerConfig cfg() {
        return MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000).addVideo(0x1011, VideoCodec.H264).build();
    }

    private static byte[] idr(int i) {
        byte[] b = new byte[300];
        b[3] = 1;
        b[4] = (byte) (i == 0 ? 0x65 : 0x41);
        for (int k = 5; k < b.length; k++) b[k] = (byte) (((k + i) & 0xFF) | 1);
        return b;
    }

    /** The publisher's TS: a real Muxer fed 24 synthetic H.264 access units. */
    private static byte[] offlineTs() throws Exception {
        ByteArrayOutputStream acc = new ByteArrayOutputStream();
        byte[] out = new byte[8192];
        try (Muxer m = new Muxer(cfg())) {
            for (int i = 0; i < 24; i++) {
                m.pushVideo(idr(i), i * 3003L, i == 0);
            }
            int n;
            while ((n = m.pull(out)) > 0) acc.write(out, 0, n);
        }
        return acc.toByteArray();
    }

    /** SHA-256 of the first Video sample's units, demuxed offline from {@code ts}. */
    private static String offlineSha(byte[] ts) throws Exception {
        try (Demuxer d = new Demuxer()) {
            d.feed(ts);
            d.flush();
            for (DemuxEvent e : d) {
                if (e instanceof DemuxEvent.Video v && !v.parse().isEmpty()) {
                    return sha256Units(v.parse());
                }
            }
        }
        throw new AssertionError("offline reference produced no Video event");
    }

    @FunctionalInterface
    private interface Call<T> { T call() throws Exception; }

    /** A call running on a daemon thread; {@link #join} is bounded. */
    private static final class Parked<T> {
        final AtomicReference<T> value = new AtomicReference<>();
        final AtomicReference<Throwable> error = new AtomicReference<>();
        Thread thread;

        boolean join() throws InterruptedException { return join(JOIN_MS); }

        boolean join(long ms) throws InterruptedException {
            thread.join(ms);
            return !thread.isAlive();
        }
    }

    private static <T> Parked<T> park(Call<T> call) throws InterruptedException {
        Parked<T> p = new Parked<>();
        CountDownLatch entered = new CountDownLatch(1);
        p.thread = daemon(() -> {
            entered.countDown();
            try {
                p.value.set(call.call());
            } catch (Throwable t) {
                p.error.set(t);
            }
        });
        entered.await();
        return p;
    }

    private static Thread daemon(Runnable r) {
        Thread t = new Thread(r);
        t.setDaemon(true);
        t.start();
        return t;
    }

    /**
     * A minimal MP2T publisher: ANNOUNCE, SETUP {@code mode=record} over
     * TCP-interleaved, RECORD. Holds the mount while the control connection is open.
     */
    private static final class RawPublisher implements AutoCloseable {
        private final Socket sock;
        private final InputStream in;
        private final OutputStream out;
        private final String uri;
        private final String session;
        private final int channel;
        private int cseq = 1;
        private int seq;

        RawPublisher(int port, String path) throws IOException {
            sock = new Socket();
            sock.setTcpNoDelay(true);
            sock.setSoTimeout(5000);
            sock.connect(new InetSocketAddress("127.0.0.1", port), 5000);
            in = sock.getInputStream();
            out = sock.getOutputStream();
            uri = "rtsp://127.0.0.1:" + port + path;
            String r = request("ANNOUNCE " + uri + " RTSP/1.0\r\n",
                "Content-Type: application/sdp\r\nContent-Length: " + SDP_MP2T.length()
                    + "\r\n\r\n" + SDP_MP2T);
            assertEquals(200, status(r), () -> "ANNOUNCE refused: " + r);
            String s = request("SETUP " + uri + "/streamid=0 RTSP/1.0\r\n",
                "Transport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\n\r\n");
            assertEquals(200, status(s), () -> "SETUP refused: " + s);
            session = header(s, "Session").split(";")[0].trim();
            int ch = -1;
            for (String part : header(s, "Transport").split(";")) {
                part = part.trim();
                if (part.startsWith("interleaved=")) {
                    ch = Integer.parseInt(part.substring("interleaved=".length()).split("-")[0]);
                }
            }
            assertTrue(ch >= 0, () -> "no interleaved channel in " + s);
            channel = ch;
            String rec = request("RECORD " + uri + " RTSP/1.0\r\n",
                "Session: " + session + "\r\n\r\n");
            assertEquals(200, status(rec), () -> "RECORD refused: " + rec);
        }

        /** Send TEARDOWN and return the response status. */
        int teardown() throws IOException {
            return status(request("TEARDOWN " + uri + " RTSP/1.0\r\n",
                "Session: " + session + "\r\n\r\n"));
        }

        /** One RTP packet (PT 33) in one RFC 2326 §10.12 interleaved frame. */
        void sendRtp(byte[] tsBundle) throws IOException {
            int len = 12 + tsBundle.length;
            byte[] frame = new byte[4 + len];
            frame[0] = '$';
            frame[1] = (byte) channel;
            frame[2] = (byte) (len >> 8);
            frame[3] = (byte) len;
            frame[4] = (byte) 0x80;
            frame[5] = 33;
            frame[6] = (byte) (seq >> 8);
            frame[7] = (byte) seq;
            int ts = seq * 3003;
            frame[8] = (byte) (ts >> 24); frame[9] = (byte) (ts >> 16);
            frame[10] = (byte) (ts >> 8); frame[11] = (byte) ts;
            frame[12] = 0x12; frame[13] = 0x34; frame[14] = 0x56; frame[15] = 0x78;
            System.arraycopy(tsBundle, 0, frame, 16, tsBundle.length);
            seq++;
            out.write(frame);
            out.flush();
        }

        String localAddr() {
            return sock.getLocalAddress().getHostAddress() + ":" + sock.getLocalPort();
        }

        /** Send {@code requestLine} + CSeq + {@code rest}; read the response head. */
        private String request(String requestLine, String rest) throws IOException {
            String req = requestLine + "CSeq: " + (cseq++) + "\r\n" + rest;
            out.write(req.getBytes(StandardCharsets.US_ASCII));
            out.flush();
            // Byte by byte up to the blank line: never reads past the response head.
            ByteArrayOutputStream head = new ByteArrayOutputStream();
            List<Integer> tail = new ArrayList<>();
            while (true) {
                int b = in.read();
                if (b < 0) throw new IOException("server closed before answering " + requestLine);
                head.write(b);
                tail.add(b);
                if (tail.size() > 4) tail.remove(0);
                if (tail.equals(List.of(13, 10, 13, 10))) break;
            }
            return head.toString(StandardCharsets.US_ASCII);
        }

        private static int status(String resp) {
            return Integer.parseInt(resp.split(" ")[1]);
        }

        private static String header(String resp, String name) {
            for (String line : resp.split("\r\n")) {
                int c = line.indexOf(':');
                if (c > 0 && line.substring(0, c).trim().equalsIgnoreCase(name)) {
                    return line.substring(c + 1).trim();
                }
            }
            throw new AssertionError("no " + name + " header in " + resp);
        }

        @Override public void close() throws IOException { sock.close(); }
    }
}
