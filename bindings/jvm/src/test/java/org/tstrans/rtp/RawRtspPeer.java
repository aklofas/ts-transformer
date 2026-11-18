package org.tstrans.rtp;

import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import org.tstrans.mpegts.Muxer;

/**
 * A one-connection RTSP server whose replies the test controls. It answers OPTIONS /
 * DESCRIBE / SETUP / PLAY with {@code 200}; PAUSE and TEARDOWN get the status the test
 * chose, or no reply at all ({@code null}), and the peer keeps reading either way. After
 * the PLAY reply it can write a burst of interleaved {@code $} frames carrying RTP/MP2T
 * (payload type 33), so a {@code DemuxReceiver} over a TCP-interleaved session has
 * events to deliver.
 */
final class RawRtspPeer implements AutoCloseable {
    static final String MP2T_SDP =
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=tst test\r\nt=0 0\r\n"
            + "a=control:*\r\nm=video 0 RTP/AVP 33\r\na=control:trackID=0\r\n";

    static final String H264_SDP =
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=tst test\r\nt=0 0\r\n"
            + "a=control:*\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n"
            + "a=control:trackID=0\r\n";

    private final ServerSocket listener;
    private final Thread thread;
    private final String sdp;
    private final boolean media;
    private final Integer pauseStatus;
    private final Integer teardownStatus;
    private volatile Socket conn;
    final CountDownLatch pauseRead = new CountDownLatch(1);
    final CountDownLatch teardownRead = new CountDownLatch(1);
    private volatile RtspCancelHandle watched;
    /** The watched handle's {@code isCancelled()} when the peer read the TEARDOWN. */
    final AtomicReference<Boolean> cancelledAtTeardown = new AtomicReference<>();

    /**
     * @param sdp the DESCRIBE body
     * @param media whether to write the RTP/MP2T burst after the PLAY reply
     * @param pauseStatus the PAUSE reply's status, or {@code null} for none
     * @param teardownStatus the TEARDOWN reply's status, or {@code null} for none
     */
    RawRtspPeer(String sdp, boolean media, Integer pauseStatus, Integer teardownStatus)
            throws IOException {
        this.sdp = sdp;
        this.media = media;
        this.pauseStatus = pauseStatus;
        this.teardownStatus = teardownStatus;
        this.listener = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
        this.thread = new Thread(this::serve, "raw-rtsp-peer");
        thread.setDaemon(true);
        thread.start();
    }

    /** A TCP-interleaved session URL on this peer, keepalive pings off. */
    RtspClientConfig config() {
        return RtspClientConfig.builder("rtsp://127.0.0.1:" + listener.getLocalPort() + "/x")
            .transportPref(TransportPref.TCP)
            .keepalive(false)
            .build();
    }

    /** Closes the sockets and fails if the peer's thread is still running after 5 s. */
    @Override public void close() throws Exception {
        listener.close();
        Socket s = conn;
        if (s != null) s.close();
        thread.join(5_000);
        if (thread.isAlive()) throw new AssertionError("the RTSP peer thread did not end");
    }

    static String header(String request, String name) {
        String[] lines = request.split("\r\n");
        for (int i = 1; i < lines.length; i++) {
            int colon = lines[i].indexOf(':');
            if (colon > 0 && lines[i].substring(0, colon).trim().equalsIgnoreCase(name)) {
                return lines[i].substring(colon + 1).trim();
            }
        }
        return "";
    }

    /** Read one request through CRLFCRLF; null on EOF. */
    static String readRequest(InputStream in) throws IOException {
        StringBuilder sb = new StringBuilder();
        int b;
        while ((b = in.read()) != -1) {
            sb.append((char) b);
            if (sb.length() >= 4 && sb.substring(sb.length() - 4).equals("\r\n\r\n")) {
                return sb.toString();
            }
        }
        return null;
    }

    private void serve() {
        try (Socket sock = listener.accept()) {
            conn = sock;
            sock.setSoTimeout(30_000);
            InputStream in = sock.getInputStream();
            OutputStream out = sock.getOutputStream();
            int channel = 0;
            while (true) {
                String req = readRequest(in);
                if (req == null) return;
                String method = req.split(" ", 2)[0];
                int status = 200;
                String extra = "";
                byte[] body = new byte[0];
                switch (method) {
                    case "OPTIONS" -> extra = "Public: OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN\r\n";
                    case "DESCRIBE" -> {
                        body = sdp.getBytes(StandardCharsets.US_ASCII);
                        extra = "Content-Type: application/sdp\r\nContent-Length: " + body.length + "\r\n";
                    }
                    case "SETUP" -> {
                        String transport = header(req, "Transport");
                        channel = interleavedChannel(transport);
                        extra = "Session: DEADBEEF;timeout=60\r\nTransport: " + transport + "\r\n";
                    }
                    case "PLAY" -> extra = "Session: DEADBEEF\r\n";
                    case "PAUSE" -> {
                        pauseRead.countDown();
                        if (pauseStatus == null) continue;
                        status = pauseStatus;
                    }
                    case "TEARDOWN" -> {
                        RtspCancelHandle w = watched;
                        if (w != null) cancelledAtTeardown.set(w.isCancelled());
                        teardownRead.countDown();
                        if (teardownStatus == null) continue;
                        status = teardownStatus;
                    }
                    default -> { }
                }
                String cseq = header(req, "CSeq");
                out.write(("RTSP/1.0 " + status + " " + (status == 200 ? "OK" : "Refused")
                    + "\r\nCSeq: " + (cseq.isEmpty() ? "1" : cseq) + "\r\n"
                    + extra + "\r\n").getBytes(StandardCharsets.US_ASCII));
                out.write(body);
                if (method.equals("PLAY") && media) out.write(interleavedTs(channel));
                out.flush();
            }
        } catch (Exception ignored) {
            // The client closed the connection, or close() closed the sockets.
        }
    }

    private static int interleavedChannel(String transport) {
        int at = transport.indexOf("interleaved=");
        if (at < 0) return 0;
        int end = at + "interleaved=".length();
        int stop = end;
        while (stop < transport.length() && Character.isDigit(transport.charAt(stop))) stop++;
        return stop > end ? Integer.parseInt(transport.substring(end, stop)) : 0;
    }

    /** 24 muxed IDRs as RTP/MP2T packets of seven TS packets, each framed with {@code $}. */
    private static byte[] interleavedTs(int channel) throws Exception {
        ByteArrayOutputStream ts = new ByteArrayOutputStream();
        byte[] buf = new byte[8192];
        try (Muxer m = new Muxer(roundtripConfig())) {
            for (int i = 0; i < 24; i++) {
                m.pushVideo(syntheticH264Idr(), i * 3000L, true);
            }
            int n;
            while ((n = m.pull(buf)) > 0) ts.write(buf, 0, n);
        }
        byte[] all = ts.toByteArray();
        ByteArrayOutputStream wire = new ByteArrayOutputStream();
        int seq = 0;
        for (int off = 0; off < all.length; off += 7 * 188) {
            int len = Math.min(7 * 188, all.length - off);
            int rtpLen = 12 + len;
            wire.write('$');
            wire.write(channel);
            wire.write(rtpLen >> 8);
            wire.write(rtpLen & 0xff);
            wire.write(0x80);
            wire.write(33);
            wire.write(seq >> 8);
            wire.write(seq & 0xff);
            long rtpTs = seq * 3000L;
            for (int shift = 24; shift >= 0; shift -= 8) wire.write((int) (rtpTs >> shift) & 0xff);
            for (int shift = 24; shift >= 0; shift -= 8) wire.write((0x1234abcd >> shift) & 0xff);
            wire.write(all, off, len);
            seq++;
        }
        return wire.toByteArray();
    }

    /**
     * Record {@code handle.isCancelled()} at the moment the peer reads the TEARDOWN,
     * before it replies, in {@link #cancelledAtTeardown}.
     */
    void watchCancelAtTeardown(RtspCancelHandle handle) {
        watched = handle;
    }

    /** Wait up to {@code ms} for the peer to read a PAUSE. */
    boolean awaitPause(long ms) throws InterruptedException {
        return pauseRead.await(ms, TimeUnit.MILLISECONDS);
    }

    /** Wait up to {@code ms} for the peer to read a TEARDOWN. */
    boolean awaitTeardown(long ms) throws InterruptedException {
        return teardownRead.await(ms, TimeUnit.MILLISECONDS);
    }
}
