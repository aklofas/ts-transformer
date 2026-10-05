package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtspException;

/**
 * A cancel fired while a control call is parked ends that
 * call with {@code RtspException(CLOSED)} — the one cancel outcome every
 * tstrans handle shares (it was {@code PROTOCOL}). The cancel handle only
 * exists on a live session, so the parked call is PAUSE, not connect: a
 * hand-rolled peer answers OPTIONS / DESCRIBE / SETUP / PLAY and then never
 * answers PAUSE. Asserts the kind only, never elapsed time.
 */
class RtspCancelOutcomeTest {
    private static final String SDP =
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=tst test\r\nt=0 0\r\n"
            + "a=control:*\r\nm=video 0 RTP/AVP 33\r\na=control:trackID=0\r\n";

    private static String header(String request, String name) {
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
    private static String readRequest(InputStream in) throws IOException {
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

    /** Answers every request until PAUSE, which it reads and never answers. */
    private static void serveSilentOnPause(ServerSocket listener, CountDownLatch done) {
        try (Socket sock = listener.accept()) {
            sock.setSoTimeout(10_000);
            InputStream in = sock.getInputStream();
            OutputStream out = sock.getOutputStream();
            while (true) {
                String req = readRequest(in);
                if (req == null) return;
                String method = req.split(" ", 2)[0];
                if (method.equals("PAUSE")) {
                    done.await(10, TimeUnit.SECONDS);
                    return;
                }
                String extra = "";
                byte[] body = new byte[0];
                switch (method) {
                    case "OPTIONS" -> extra = "Public: OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN\r\n";
                    case "DESCRIBE" -> {
                        body = SDP.getBytes(StandardCharsets.US_ASCII);
                        extra = "Content-Type: application/sdp\r\nContent-Length: " + body.length + "\r\n";
                    }
                    case "SETUP" -> extra = "Session: DEADBEEF;timeout=60\r\nTransport: "
                        + header(req, "Transport") + "\r\n";
                    case "PLAY" -> extra = "Session: DEADBEEF\r\n";
                    default -> { }
                }
                String cseq = header(req, "CSeq");
                out.write(("RTSP/1.0 200 OK\r\nCSeq: " + (cseq.isEmpty() ? "1" : cseq) + "\r\n"
                    + extra + "\r\n").getBytes(StandardCharsets.US_ASCII));
                out.write(body);
                out.flush();
            }
        } catch (IOException | InterruptedException ignored) {
            // The test's assertion is on the client side; a peer error
            // surfaces there as a non-CLOSED kind.
        }
    }

    @Test
    @Timeout(30)
    void cancelOfAParkedControlCallIsClosed() throws Exception {
        CountDownLatch done = new CountDownLatch(1);
        try (ServerSocket listener = new ServerSocket(0, 1, InetAddress.getLoopbackAddress())) {
            int port = listener.getLocalPort();
            Thread server = new Thread(() -> serveSilentOnPause(listener, done));
            server.setDaemon(true);
            server.start();
            RtspClientConfig cfg = RtspClientConfig.builder("rtsp://127.0.0.1:" + port + "/x")
                .transportPref(TransportPref.TCP)
                .rtcp(false)
                .keepalive(false)
                .build();
            try (RtspSession session = RtspClient.connect(cfg);
                 RtspCancelHandle cancel = session.cancelHandle()) {
                // Whether the cancel lands before or after PAUSE parks, the
                // call ends at its next poll with the same kind.
                Thread canceller = new Thread(() -> {
                    try {
                        Thread.sleep(200);
                    } catch (InterruptedException ignored) {
                        return;
                    }
                    cancel.cancel();
                });
                canceller.setDaemon(true);
                canceller.start();
                RtspException e = assertThrows(RtspException.class, session::pause);
                assertEquals(RtspException.Kind.CLOSED, e.kind());
                canceller.join(5_000);
            } finally {
                done.countDown();
                server.join(5_000);
            }
        }
    }
}
