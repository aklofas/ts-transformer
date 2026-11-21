package org.tstrans.shells;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicLong;
import org.tstrans.TestSupport;

/**
 * A plain loopback TCP peer for a sender shell: it accepts one connection and reads every
 * byte until the sender closes it. A sender's end is then the peer's end of file, so a test
 * reads exactly what was sent with no timing involved.
 */
final class TcpSink implements AutoCloseable {
    private final ServerSocket server;
    private final CompletableFuture<byte[]> received = new CompletableFuture<>();
    private final AtomicLong count = new AtomicLong();

    TcpSink() throws IOException {
        server = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
        Thread t = new Thread(this::run, "tcp-sink");
        t.setDaemon(true);
        t.start();
    }

    private void run() {
        try (Socket s = server.accept(); InputStream in = s.getInputStream()) {
            ByteArrayOutputStream acc = new ByteArrayOutputStream();
            byte[] buf = new byte[65536];
            int n;
            while ((n = in.read(buf)) >= 0) {
                acc.write(buf, 0, n);
                count.addAndGet(n);
            }
            received.complete(acc.toByteArray());
        } catch (IOException e) {
            received.completeExceptionally(e);
        }
    }

    /** A {@code tcp://} URL that reaches this peer. */
    String url() {
        return "tcp://127.0.0.1:" + server.getLocalPort();
    }

    /** Wait until the peer has read at least {@code n} bytes, the connection still open. */
    void awaitBytes(long n, long ms) throws InterruptedException {
        TestSupport.waitFor(n + " bytes at the peer (read " + count.get() + ")", () -> count.get() >= n, ms);
    }

    /** Every byte the peer read, once the sender closed the connection; fails after {@code ms}. */
    byte[] awaitEnd(long ms) throws Exception {
        try {
            return received.get(ms, TimeUnit.MILLISECONDS);
        } catch (TimeoutException e) {
            throw new AssertionError("the sender did not close its connection within " + ms + " ms");
        }
    }

    @Override public void close() throws IOException {
        server.close();
    }
}
