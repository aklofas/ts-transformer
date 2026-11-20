package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.rtspHeader;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.syntheticH264Idr;
import static org.tstrans.rtp.RtspTestSupport.startServer;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.net.SocketTimeoutException;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtspException;

/**
 * The three ways to end an {@link RtspServer} and what each leaves behind: {@link
 * RtspServer#close()} and {@link RtspServer#stop()} send a PLAY session the RFC 7826
 * §13.5.1 Notice 5402 ANNOUNCE before its connection closes; the hard cancel ({@link
 * RtspServerCancelHandle#cancel()}) closes it with no Notice and refuses pushes, mount
 * registration and rule edits at once.
 *
 * <p>The reader is a plain {@link Socket} speaking RTSP by hand (OPTIONS, DESCRIBE,
 * SETUP over TCP-interleaved, PLAY): the library's RTSP client has no way to hand a
 * server-sent ANNOUNCE to the caller. Every read has a deadline. Linux-gated like the
 * other live RTSP server tests (real sockets + a tokio runtime).
 */
class RtspServerEndTest {

    /** Deadline for a connection to close and for a side thread to end. */
    private static final long END_MS = TimeUnit.SECONDS.toMillis(15);

    @Test
    @Timeout(40)
    void closeWithoutStopSendsTheNotice5402ToAPlayingSession() throws Exception {
        assumeLinux();
        RtspServer server = startServer();
        try (MountHandle mount = server.addUnicastMount("/live", roundtripConfig());
             RawPlayer player = new RawPlayer(port(server), "/live")) {
            // close() on a side thread, so this one reads the wire while close() runs.
            Thread closer = new Thread(server::close);
            closer.setDaemon(true);
            closer.start();
            Wire wire = player.readToEof(END_MS);
            closer.join(END_MS);
            assertFalse(closer.isAlive(), "close() did not return");
            assertTrue(wire.eof, () -> "close() left the connection open: " + wire.text);
            assertNotice(wire, player);
        } finally {
            server.close();
        }
    }

    @Test
    @Timeout(40)
    void stopSendsTheNotice5402ToAPlayingSession() throws Exception {
        assumeLinux();
        RtspServer server = startServer();
        try (MountHandle mount = server.addUnicastMount("/live", roundtripConfig());
             RawPlayer player = new RawPlayer(port(server), "/live")) {
            server.stop();
            server.close();
            Wire wire = player.readToEof(END_MS);
            assertTrue(wire.eof, () -> "stop() + close() left the connection open: " + wire.text);
            assertNotice(wire, player);
        } finally {
            server.close();
        }
    }

    @Test
    @Timeout(40)
    void hardCancelClosesAPlayingSessionWithoutANotice() throws Exception {
        assumeLinux();
        RtspServer server = startServer();
        try (MountHandle mount = server.addUnicastMount("/live", roundtripConfig());
             RawPlayer player = new RawPlayer(port(server), "/live");
             RtspServerCancelHandle cancel = server.cancelHandle()) {
            cancel.cancel();
            // No stop() and no close(): the cancel alone ends the session.
            Wire wire = player.readToEof(END_MS);
            assertTrue(wire.eof, () -> "the hard cancel left the connection open: " + wire.text);
            assertNoNotice(wire);
        } finally {
            server.close();
        }
    }

    @Test
    @Timeout(40)
    void stopAfterAHardCancelReturnsAndSendsNoNotice() throws Exception {
        assumeLinux();
        RtspServer server = startServer();
        try (MountHandle mount = server.addUnicastMount("/live", roundtripConfig());
             RawPlayer player = new RawPlayer(port(server), "/live");
             RtspServerCancelHandle cancel = server.cancelHandle()) {
            cancel.cancel();
            server.stop();
            Wire wire = player.readToEof(END_MS);
            assertTrue(wire.eof, () -> "the connection stayed open: " + wire.text);
            assertNoNotice(wire);
        } finally {
            server.close();
        }
    }

    @Test
    @Timeout(40)
    void closeAfterAHardCancelSendsNoNotice() throws Exception {
        assumeLinux();
        RtspServer server = startServer();
        try (MountHandle mount = server.addUnicastMount("/live", roundtripConfig());
             RawPlayer player = new RawPlayer(port(server), "/live");
             RtspServerCancelHandle cancel = server.cancelHandle()) {
            cancel.cancel();
            server.close();
            Wire wire = player.readToEof(END_MS);
            assertTrue(wire.eof, () -> "the connection stayed open: " + wire.text);
            assertNoNotice(wire);
        } finally {
            server.close();
        }
    }

    @Test
    @Timeout(30)
    void pushAfterAHardCancelThrowsClosed() throws Exception {
        assumeLinux();
        RtspServer server = startServer();
        try (MountHandle mount = server.addUnicastMount("/live", roundtripConfig());
             RtspServerCancelHandle cancel = server.cancelHandle()) {
            mount.pushVideo(syntheticH264Idr(), 0L, true);
            cancel.cancel();
            RtspException e = assertThrows(RtspException.class,
                () -> mount.pushVideo(syntheticH264Idr(), 3003L, true));
            assertEquals(RtspException.Kind.CLOSED, e.kind());
            // flush() and resetStats() are not refused.
            mount.flush();
            mount.resetStats();
        } finally {
            server.close();
        }
    }

    @Test
    @Timeout(30)
    void addUnicastMountAfterAHardCancelThrowsClosed() throws Exception {
        assumeLinux();
        RtspServer server = startServer();
        try (RtspServerCancelHandle cancel = server.cancelHandle()) {
            cancel.cancel();
            RtspException e = assertThrows(RtspException.class,
                () -> server.addUnicastMount("/late", roundtripConfig()));
            assertEquals(RtspException.Kind.CLOSED, e.kind());
        } finally {
            server.close();
        }
    }

    @Test
    @Timeout(30)
    void mountRulesEditAfterAHardCancelThrowsClosed() throws Exception {
        assumeLinux();
        RtspServer server = startServer();
        try (MountRules rules = server.mountRules();
             RtspServerCancelHandle cancel = server.cancelHandle()) {
            rules.set("/a", MountRule.both(MountPolicy.open()));
            cancel.cancel();
            RtspException e = assertThrows(RtspException.class,
                () -> rules.set("/b", MountRule.both(MountPolicy.open())));
            assertEquals(RtspException.Kind.CLOSED, e.kind());
            assertTrue(rules.isClosed());
            assertEquals(1, rules.size());
        } finally {
            server.close();
        }
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    private static void assumeLinux() {
        assumeTrue(isLinux(), "RTSP live server gated to Linux (real sockets + tokio runtime)");
    }

    private static int port(RtspServer server) {
        return server.localAddr().orElseThrow(() -> new AssertionError("server must be bound"))
            .getPort();
    }

    private static void assertNotice(Wire wire, RawPlayer player) {
        assertTrue(wire.text.contains("ANNOUNCE " + player.uri + " RTSP/1.0\r\n"),
            () -> "no ANNOUNCE before the connection closed: " + wire.text);
        assertTrue(wire.text.contains("Notice: 5402"),
            () -> "the ANNOUNCE carries no Notice 5402: " + wire.text);
        assertTrue(wire.text.contains("Session: " + player.session),
            () -> "the ANNOUNCE names another session: " + wire.text);
    }

    private static void assertNoNotice(Wire wire) {
        assertFalse(wire.text.contains("ANNOUNCE"),
            () -> "a Notice was sent: " + wire.text);
    }

    /** What a reader received after PLAY, and whether the server closed the connection. */
    private record Wire(String text, boolean eof) {}

    /**
     * A reader in PLAY on one unicast mount: OPTIONS, DESCRIBE, SETUP over
     * TCP-interleaved, PLAY. The server pushes nothing on it unless the mount is pushed.
     */
    private static final class RawPlayer implements AutoCloseable {
        private final Socket sock;
        private final InputStream in;
        private final OutputStream out;
        final String uri;
        final String session;
        private int cseq = 1;

        RawPlayer(int port, String path) throws IOException {
            sock = new Socket();
            sock.setTcpNoDelay(true);
            sock.setSoTimeout(5000);
            sock.connect(new InetSocketAddress("127.0.0.1", port), 5000);
            in = sock.getInputStream();
            out = sock.getOutputStream();
            uri = "rtsp://127.0.0.1:" + port + path;
            expect200(request("OPTIONS " + uri, "\r\n"));
            expect200(request("DESCRIBE " + uri, "Accept: application/sdp\r\n\r\n"));
            String setup = expect200(request("SETUP " + uri,
                "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"));
            session = rtspHeader(setup, "Session").split(";")[0].trim();
            expect200(request("PLAY " + uri, "Session: " + session + "\r\n\r\n"));
        }

        /** Read until the server closes the connection or {@code ms} pass. */
        Wire readToEof(long ms) throws IOException {
            sock.setSoTimeout(100);
            long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(ms);
            ByteArrayOutputStream acc = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            while (System.nanoTime() < deadline) {
                int n;
                try {
                    n = in.read(buf);
                } catch (SocketTimeoutException e) {
                    continue;
                } catch (IOException e) {
                    // A reset after the server's close: the connection is gone.
                    return new Wire(acc.toString(StandardCharsets.ISO_8859_1), true);
                }
                if (n < 0) return new Wire(acc.toString(StandardCharsets.ISO_8859_1), true);
                acc.write(buf, 0, n);
            }
            return new Wire(acc.toString(StandardCharsets.ISO_8859_1), false);
        }

        /** Send {@code start} + CSeq + {@code rest}; read the whole response, body included. */
        private String request(String start, String rest) throws IOException {
            String req = start + " RTSP/1.0\r\nCSeq: " + (cseq++) + "\r\n" + rest;
            out.write(req.getBytes(StandardCharsets.US_ASCII));
            out.flush();
            ByteArrayOutputStream head = new ByteArrayOutputStream();
            int matched = 0;
            byte[] end = {13, 10, 13, 10};
            while (matched < 4) {
                int b = in.read();
                if (b < 0) throw new IOException("server closed before answering " + start);
                head.write(b);
                matched = b == end[matched] ? matched + 1 : (b == 13 ? 1 : 0);
            }
            String resp = head.toString(StandardCharsets.US_ASCII);
            int length = 0;
            for (String line : resp.split("\r\n")) {
                if (line.toLowerCase(java.util.Locale.ROOT).startsWith("content-length:")) {
                    length = Integer.parseInt(line.substring(line.indexOf(':') + 1).trim());
                }
            }
            in.readNBytes(length);
            return resp;
        }

        private static String expect200(String resp) {
            assertTrue(resp.startsWith("RTSP/1.0 200"), () -> "refused: " + resp);
            return resp;
        }

        @Override public void close() throws IOException { sock.close(); }
    }
}
