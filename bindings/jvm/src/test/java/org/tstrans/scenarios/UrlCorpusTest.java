package org.tstrans.scenarios;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.scenarioDir;
import static org.tstrans.TestSupport.scaledMs;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Base64;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import java.util.stream.Stream;
import org.junit.jupiter.api.Assumptions;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.Arguments;
import org.junit.jupiter.params.provider.MethodSource;
import org.tstrans.BindingException;
import org.tstrans.HlsException;
import org.tstrans.RistException;
import org.tstrans.RtpException;
import org.tstrans.RtspException;
import org.tstrans.SrtException;
import org.tstrans.TcpException;
import org.tstrans.UdpException;
import org.tstrans.WebrtcException;
import org.tstrans.hls.HlsPublisher;
import org.tstrans.rtp.H264Receiver;
import org.tstrans.rtp.RtspClient;
import org.tstrans.rtp.RtspClientConfig;
import org.tstrans.rtp.RtspSession;
import org.tstrans.shells.Receiver;
import org.tstrans.shells.Sender;
import org.tstrans.srt.Builder;
import org.tstrans.srt.Listener;
import org.tstrans.webrtc.WhepServer;

/**
 * The JVM leg of the URL corpus. Every row goes through the JVM entry point the corpus header's
 * {@code [legs.jvm]} table names for its scheme and role. The JVM exposes no parsed URL, so only
 * the outcome is compared: a refused row must throw the scheme's exception with the expected
 * kind; an accepted row must not throw one of the scheme's URL kinds (a refused connect is fine).
 * For the wire schemes (rtsp) a valid row is also connected to a recording peer, and the request
 * heads must show the record's credentials and keys.
 *
 * <p>The corpus is read from the JSON twin {@code gen-scenarios} writes into the scenario's
 * golden ({@code extensions.corpus}), because the test classpath has no TOML reader.
 */
class UrlCorpusTest {

    static final Map<String, Object> CORPUS = load();

    @SuppressWarnings("unchecked")
    private static Map<String, Object> load() {
        Path golden = scenarioDir("url-corpus").resolve("golden.json");
        try {
            Map<String, Object> g = (Map<String, Object>) Json.parse(Files.readString(golden, StandardCharsets.UTF_8));
            Map<String, Object> ext = (Map<String, Object>) g.get("extensions");
            Object corpus = ext == null ? null : ext.get("corpus");
            if (!(corpus instanceof Map)) {
                throw new AssertionError(golden + " has no extensions.corpus: regenerate with gen-scenarios");
            }
            return (Map<String, Object>) corpus;
        } catch (IOException e) {
            throw new AssertionError("cannot read " + golden, e);
        }
    }

    @SuppressWarnings("unchecked")
    static List<Map<String, Object>> rows() {
        return (List<Map<String, Object>>) CORPUS.get("url");
    }

    static String label(Map<String, Object> row) {
        return "[" + row.get("scheme") + " " + row.get("role") + " " + row.get("case") + "] " + row.get("url");
    }

    @SuppressWarnings("unchecked")
    static String family(Map<String, Object> row) {
        String f = (String) ((Map<String, Object>) CORPUS.get("families")).get((String) row.get("scheme"));
        if (f == null) throw new AssertionError("[families] has no " + row.get("scheme"));
        return f;
    }

    @SuppressWarnings("unchecked")
    static Map<String, Object> legTable(Map<String, Object> row) {
        Map<String, Object> jvm = (Map<String, Object>) ((Map<String, Object>) CORPUS.get("legs")).get("jvm");
        Map<String, Object> table = jvm == null ? null : (Map<String, Object>) jvm.get(family(row));
        if (table == null) throw new AssertionError("[legs.jvm] declares no " + family(row));
        return table;
    }

    @SuppressWarnings("unchecked")
    static Map<String, Object> expect(Map<String, Object> row) {
        return (Map<String, Object>) row.get("expect");
    }

    /** The exception class each family's entry points throw. */
    static final Map<String, Class<? extends BindingException>> ERROR_CLASS = Map.of(
        "srt", SrtException.class,
        "rtp", RtpException.class,
        "rtsp", RtspException.class,
        "udp", UdpException.class,
        "tcp", TcpException.class,
        "hls", HlsException.class,
        "rist", RistException.class,
        "whep", WebrtcException.class);

    // ── Probes ───────────────────────────────────────────────────────────────

    @FunctionalInterface
    interface Open {
        void run(String url) throws Exception;
    }

    private static final Pattern AUTHORITY = Pattern.compile("^[a-z]+://(?:[^@/?]*@)?(\\[[^\\]]*\\]|[^:/?\\[]*)(?::(\\d+))?");

    /**
     * The loopback address of an srt URL as a bare {@code srt://} URL, or null when it names no
     * port. The peer goes up whatever the record says: should the JVM accept a URL the record
     * calls invalid, the open still returns (and the mismatch is reported) instead of waiting in
     * accept forever.
     */
    static String srtPeerUrl(String url) {
        Matcher m = AUTHORITY.matcher(url);
        if (!m.find() || m.group(2) == null) return null;
        int port = Integer.parseInt(m.group(2));
        if (port > 65535) return null;
        String host = m.group(1).isEmpty() ? "127.0.0.1" : m.group(1);
        return "srt://" + host + ":" + port;
    }

    static void openSrtCaller(String url) throws Exception {
        // An SRT caller would wait out its connect timeout against a closed port, so the URL's
        // address gets a loopback listener. The peer sets no options: a handshake it rejects (a
        // passphrase it lacks) still returns at once, as a connect failure.
        String peerUrl = srtPeerUrl(url);
        Listener peer = peerUrl == null ? null : new Builder(peerUrl + "?mode=listener").listen();
        try (Sender s = Sender.open(url)) {
            // opened: nothing to send
        } finally {
            if (peer != null) peer.close();
        }
    }

    static void openSrtListener(String url) throws Exception {
        // The listener's open blocks in accept, so the URL's address gets a loopback caller; its
        // handshake retries until the bind lands.
        String peerUrl = srtPeerUrl(url);
        List<org.tstrans.srt.Socket> dialed = new CopyOnWriteArrayList<>();
        Thread caller = null;
        if (peerUrl != null) {
            caller = new Thread(() -> {
                try {
                    dialed.add(new Builder(peerUrl).connectTimeoutMs((int) scaledMs(5000)).connect());
                } catch (SrtException ignored) {
                    // the open under test reports the outcome
                }
            });
            caller.setDaemon(true);
            caller.start();
        }
        // The open parks in an accept no cancel handle reaches (none exists until it returns),
        // so it runs on a daemon thread: past the deadline the row fails by name, and one more
        // dial unparks the accept.
        CompletableFuture<Receiver> opened = new CompletableFuture<>();
        Thread open = new Thread(() -> {
            try {
                opened.complete(Receiver.open(url));
            } catch (Throwable t) {
                opened.completeExceptionally(t);
            }
        });
        open.setDaemon(true);
        open.start();
        try {
            opened.get(scaledMs(15_000), TimeUnit.MILLISECONDS).close();
        } catch (ExecutionException e) {
            if (e.getCause() instanceof Exception cause) throw cause;
            throw e;
        } catch (TimeoutException e) {
            if (peerUrl != null) {
                try {
                    dialed.add(new Builder(peerUrl).connectTimeoutMs((int) scaledMs(2000)).connect());
                } catch (SrtException ignored) {
                    // the accept stays parked; the daemon thread dies with the JVM
                }
            }
            opened.thenAccept(Receiver::close);
            throw new AssertionError(url + ": Receiver.open still waiting in accept after "
                + scaledMs(15_000) + " ms (no caller reached it)");
        } finally {
            if (caller != null) caller.join(scaledMs(10_000));
            for (org.tstrans.srt.Socket s : dialed) s.close();
        }
    }

    /**
     * A loopback TCP listener that accepts and drops every connection. A caller dialing a closed
     * loopback port is refused at once on Linux but only after about two seconds of SYN retries
     * on Windows, so the tcp caller rows and the rtsps refusal row dial one of these instead.
     */
    static final class DropPeer implements AutoCloseable {
        private static final Set<Integer> SHARED = new TreeSet<>();
        final ServerSocket listener;

        DropPeer(int port) throws IOException {
            listener = new ServerSocket(port, 50, InetAddress.getByName("127.0.0.1"));
            Thread t = new Thread(() -> {
                while (true) {
                    try (Socket conn = listener.accept()) {
                        // dropped
                    } catch (IOException e) {
                        if (listener.isClosed()) return;
                    }
                }
            });
            t.setDaemon(true);
            t.start();
        }

        /** A peer on {@code port} for the rest of the run, started once per port. */
        static synchronized void shared(int port) throws IOException {
            if (SHARED.add(port)) new DropPeer(port);
        }

        int port() { return listener.getLocalPort(); }

        @Override public void close() throws IOException { listener.close(); }
    }

    /** The port written in {@code url}'s authority, or -1 when there is none. */
    static int urlPort(String url) {
        Matcher m = AUTHORITY.matcher(url);
        if (!m.find() || m.group(2) == null) return -1;
        int port = Integer.parseInt(m.group(2));
        return port > 0 && port <= 65535 ? port : -1;
    }

    static void openTcpCaller(String url) throws Exception {
        int port = urlPort(url);
        if (port > 0) DropPeer.shared(port);
        try (Sender s = Sender.open(url)) { }
    }

    static void openRtspRefusalOnly(String url) throws Exception {
        // The row is refusal-only (no TLS peer), so the port it names is never observed: a
        // drop peer's port takes its place, and the TLS handshake fails at once.
        try (DropPeer peer = new DropPeer(0);
             RtspSession s = RtspClient.connect(RtspClientConfig.of(withPort(url, peer.port())))) { }
    }

    static void openHls(String url) throws Exception {
        // `build()` is where the JVM parses the URL, and it binds the HTTP server: the overlay
        // keeps it off the URL's port and out of the working directory.
        Path dir = Files.createTempDirectory("tst-url-corpus-hls");
        try (HlsPublisher p = HlsPublisher.builder().fromUrl(url).bind("127.0.0.1:0")
                .outputDir(dir.toString()).build()) {
            // built
        } finally {
            try (Stream<Path> walk = Files.walk(dir)) {
                walk.sorted((a, b) -> b.compareTo(a)).forEach(p -> p.toFile().delete());
            }
        }
    }

    static final Map<String, Open> ENTRIES = Map.of(
        "shells.Sender.open", url -> { try (Sender s = Sender.open(url)) { } },
        "shells.Receiver.open", url -> { try (Receiver r = Receiver.open(url)) { } },
        "rtp.H264Receiver.listen", url -> { try (H264Receiver r = H264Receiver.listen(url)) { } },
        "rtp.RtspClient.connect", url -> {
            try (RtspSession s = RtspClient.connect(RtspClientConfig.of(url))) { }
        },
        "hls.HlsPublisherBuilder.build", UrlCorpusTest::openHls,
        "webrtc.WhepServer.bind", url -> { try (WhepServer w = WhepServer.bind(url)) { } });

    /** Open the row's URL through {@code entry}; the refused kind's name, or null. */
    static String probe(String entry, Map<String, Object> row) throws Exception {
        String family = family(row);
        Open run;
        boolean valid = expect(row).get("error_kind") == null;
        if (family.equals("srt")) {
            run = switch (entry) {
                case "shells.Sender.open" -> UrlCorpusTest::openSrtCaller;
                case "shells.Receiver.open" -> UrlCorpusTest::openSrtListener;
                default -> throw new AssertionError("no srt probe for " + entry);
            };
        } else if (family.equals("tcp") && entry.equals("shells.Sender.open")) {
            run = UrlCorpusTest::openTcpCaller;
        } else if (family.equals("rtsp") && valid) {
            // An invalid row keeps its own port: rewriting it could change how the URL parses.
            run = UrlCorpusTest::openRtspRefusalOnly;
        } else {
            run = ENTRIES.get(entry);
            if (run == null) throw new AssertionError("no JVM probe for entry point " + entry);
        }
        try {
            run.run((String) row.get("url"));
        } catch (BindingException e) {
            if (!ERROR_CLASS.get(family).isInstance(e)) {
                throw new AssertionError(label(row) + " via " + entry + ": threw "
                    + e.getClass().getSimpleName() + ", not " + ERROR_CLASS.get(family).getSimpleName(), e);
            }
            return ((Enum<?>) e.getClass().getMethod("kind").invoke(e)).name();
        }
        return null;
    }

    // ── The RTSP wire probe ──────────────────────────────────────────────────

    private static final String SDP = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=tst\r\nt=0 0\r\n"
        + "a=control:*\r\nm=video 0 RTP/AVP 33\r\na=control:trackID=0\r\n";

    static String header(String head, String name) {
        String[] lines = head.split("\r\n");
        for (int i = 1; i < lines.length; i++) {
            int colon = lines[i].indexOf(':');
            if (colon > 0 && lines[i].substring(0, colon).trim().equalsIgnoreCase(name)) {
                return lines[i].substring(colon + 1).trim();
            }
        }
        return null;
    }

    /**
     * A loopback RTSP peer on port 0 that records every request head. Answers 461 to a UDP
     * SETUP (so the SETUP sequence shows the transport choice), or with {@code challenge}
     * answers 401 to a DESCRIBE without credentials and stops at the first one with.
     */
    static final class RecordingPeer implements AutoCloseable {
        final ServerSocket listener;
        final List<String> heads = new CopyOnWriteArrayList<>();
        final AtomicBoolean accepted = new AtomicBoolean();
        final Thread server;

        RecordingPeer(boolean challenge) throws IOException {
            listener = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
            server = new Thread(() -> serve(challenge));
            server.setDaemon(true);
            server.start();
        }

        int port() { return listener.getLocalPort(); }

        private void serve(boolean challenge) {
            Socket sock;
            try {
                sock = listener.accept();
                accepted.set(true);
            } catch (IOException e) {
                return;
            } finally {
                try {
                    listener.close();
                } catch (IOException ignored) {
                    // the accept is over either way
                }
            }
            try (Socket s = sock) {
                s.setSoTimeout((int) scaledMs(10_000));
                InputStream in = s.getInputStream();
                OutputStream out = s.getOutputStream();
                ByteArrayOutputStream buf = new ByteArrayOutputStream();
                while (true) {
                    String head = readHead(in, buf);
                    if (head == null) return;
                    heads.add(head);
                    String requestLine = head.split("\r\n", 2)[0];
                    String method = requestLine.split(" ", 2)[0];
                    String version = requestLine.substring(requestLine.lastIndexOf(' ') + 1);
                    String cseq = Objects.requireNonNullElse(header(head, "cseq"), "1");
                    String transport = Objects.requireNonNullElse(header(head, "transport"), "");
                    boolean authorized = header(head, "authorization") != null;
                    if (challenge && method.equals("DESCRIBE") && authorized) return;
                    String status = "200 OK";
                    String extra = "";
                    String body = "";
                    if (method.equals("DESCRIBE") && challenge) {
                        status = "401 Unauthorized";
                        extra = "WWW-Authenticate: Basic realm=\"t\"\r\n";
                    } else if (method.equals("OPTIONS")) {
                        extra = "Public: OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN\r\n";
                    } else if (method.equals("DESCRIBE")) {
                        body = SDP;
                        extra = "Content-Type: application/sdp\r\nContent-Length: " + body.length() + "\r\n";
                    } else if (method.equals("SETUP") && !transport.contains("/TCP")) {
                        status = "461 Unsupported Transport";
                    } else if (method.equals("SETUP")) {
                        extra = "Session: DEADBEEF;timeout=60\r\nTransport: " + transport + "\r\n";
                    } else {
                        extra = "Session: DEADBEEF\r\n";
                    }
                    out.write((version + " " + status + "\r\nCSeq: " + cseq + "\r\n" + extra + "\r\n" + body)
                        .getBytes(StandardCharsets.ISO_8859_1));
                    out.flush();
                    if (method.equals("TEARDOWN")) return;
                }
            } catch (IOException ignored) {
                // the client hung up; the heads so far are the record
            }
        }

        /** One request head (without the blank line), or null at end of stream. */
        private static String readHead(InputStream in, ByteArrayOutputStream buf) throws IOException {
            while (true) {
                String text = buf.toString(StandardCharsets.ISO_8859_1);
                int end = text.indexOf("\r\n\r\n");
                if (end >= 0) {
                    byte[] rest = text.substring(end + 4).getBytes(StandardCharsets.ISO_8859_1);
                    buf.reset();
                    buf.write(rest);
                    return text.substring(0, end);
                }
                byte[] chunk = new byte[8192];
                int n = in.read(chunk);
                if (n < 0) return null;
                buf.write(chunk, 0, n);
            }
        }

        @Override public void close() throws Exception {
            // Unblocks a peer still waiting in accept (the client never dialed). Only then: the
            // peer holds its listener until the accept returns, so this connect finds it
            // listening; dialing a peer that already accepted would hit a closed port, which
            // Windows refuses only after about two seconds.
            if (accepted.get()) {
                server.join(scaledMs(10_000));
                return;
            }
            try (Socket poke = new Socket()) {
                poke.connect(new InetSocketAddress(InetAddress.getLoopbackAddress(), port()), 1000);
            } catch (IOException ignored) {
                // the peer already accepted
            }
            server.join(scaledMs(10_000));
        }
    }

    /** {@code url} with its port replaced by {@code port} (inserted when it has none). */
    static String withPort(String url, int port) {
        int sep = url.indexOf("://");
        String scheme = url.substring(0, sep);
        String rest = url.substring(sep + 3);
        int end = rest.length();
        for (char c : new char[] {'/', '?'}) {
            int i = rest.indexOf(c);
            if (i >= 0 && i < end) end = i;
        }
        String authority = rest.substring(0, end);
        String tail = rest.substring(end);
        int at = authority.lastIndexOf('@');
        String userinfo = authority.substring(0, at + 1);
        String hostport = authority.substring(at + 1);
        String host = hostport.contains("]")
            ? hostport.substring(0, hostport.indexOf(']') + 1)
            : hostport.split(":", 2)[0];
        return scheme + "://" + userinfo + host + ":" + port + tail;
    }

    /** Connect {@code url} through the JVM's RTSP client against a peer; the heads. */
    static List<String> rtspRun(String url, boolean challenge) throws Exception {
        RecordingPeer peer = new RecordingPeer(challenge);
        try {
            // Keepalive off: the recorded heads are only the connect's.
            RtspClientConfig config = RtspClientConfig.builder(withPort(url, peer.port())).keepalive(false).build();
            try (RtspSession session = RtspClient.connect(config)) {
                session.teardown();
            } catch (RtspException ignored) {
                // a refusal still leaves the heads that reached the peer
            }
        } finally {
            peer.close();
        }
        return new ArrayList<>(peer.heads);
    }

    /** Project the request heads of the two runs into a record. */
    static Map<String, Object> rtspWireRecord(List<String> plain, List<String> challenged) {
        Map<String, Object> rec = new LinkedHashMap<>();
        Map<String, Object> keys = new LinkedHashMap<>();
        if (!plain.isEmpty()) {
            String[] parts = plain.get(0).split("\r\n", 2)[0].split(" ");
            String uri = parts.length > 1 ? parts[1] : "";
            if (parts.length > 2 && parts[2].startsWith("RTSP/")) {
                keys.put("rtsp_version", parts[2].substring("RTSP/".length()));
            }
            // A request URI carries no userinfo; one that leaked would show here.
            int sep = uri.indexOf("://");
            String rest = sep >= 0 ? uri.substring(sep + 3) : uri;
            int end = rest.indexOf('/') >= 0 ? rest.indexOf('/') : rest.length();
            String authority = rest.substring(0, end);
            int colon = authority.lastIndexOf(':');
            String host = colon >= 0 && !authority.endsWith("]") ? authority.substring(0, colon) : authority;
            rec.put("host", host.replace("[", "").replace("]", ""));
            rec.put("path", rest.substring(end));
        }
        List<Boolean> setups = new ArrayList<>();
        for (String h : plain) {
            if (h.startsWith("SETUP ")) {
                setups.add(Objects.requireNonNullElse(header(h, "transport"), "").contains("/TCP"));
            }
        }
        if (setups.equals(List.of(true))) {
            keys.put("transport", "tcp");
        } else if (setups.equals(List.of(false))) {
            keys.put("transport", "udp");
        } else if (!setups.equals(List.of(false, true))) {
            keys.put("transport", "SETUP sequence " + setups);
        }
        for (String h : challenged) {
            String a = header(h, "authorization");
            if (a != null && a.startsWith("Basic ")) {
                String decoded = new String(Base64.getDecoder().decode(a.substring("Basic ".length())),
                    StandardCharsets.UTF_8);
                rec.put("username", decoded.split(":", 2)[0]);
                // The client sends credentials only with a password (an empty one counts), so a
                // Basic header at all means one was set.
                rec.put("password_set", true);
                break;
            }
        }
        rec.put("keys", keys);
        return rec;
    }

    @SuppressWarnings("unchecked")
    static Object field(Map<String, Object> rec, String name) {
        if (name.startsWith("keys.")) {
            Map<String, Object> keys = (Map<String, Object>) rec.get("keys");
            return keys == null ? null : keys.get(name.substring("keys.".length()));
        }
        return rec.get(name);
    }

    static List<String> wireFields(Map<String, Object> row) {
        Map<String, Object> table = legTable(row);
        String schemes = (String) table.getOrDefault("wire_schemes", "");
        if (!List.of(schemes.split("\\s+")).contains((String) row.get("scheme"))) return null;
        return List.of(((String) table.get("wire")).split("\\s+"));
    }

    static void checkWire(Map<String, Object> row, List<String> fields) throws Exception {
        String url = (String) row.get("url");
        Map<String, Object> observed = rtspWireRecord(rtspRun(url, false), rtspRun(url, true));
        Set<String> absent = new TreeSet<>(List.of(((String) row.getOrDefault("wire_absent", "")).split("\\s+")));
        List<String> mismatches = new ArrayList<>();
        for (String f : fields) {
            Object want = absent.contains(f) ? null : field(expect(row), f);
            Object got = field(observed, f);
            if (!Objects.equals(want, got)) {
                mismatches.add(f + " expected " + want + ", observed " + got);
            }
        }
        assertTrue(mismatches.isEmpty(), label(row) + " on the wire: " + String.join("; ", mismatches));
    }

    /** The error kinds this family's refused rows name. */
    static Set<String> urlKinds(String family) {
        Set<String> kinds = new TreeSet<>();
        for (Map<String, Object> r : rows()) {
            Object k = expect(r).get("error_kind");
            if (k != null && family(r).equals(family)) kinds.add((String) k);
        }
        return kinds;
    }

    static boolean checkOutcome(Map<String, Object> row) throws Exception {
        String entry = (String) legTable(row).get((String) row.get("role"));
        if (entry == null) {
            throw new AssertionError("[legs.jvm] declares no " + family(row) + " role " + row.get("role"));
        }
        if (entry.equals("absent")) return false;
        String observed = probe(entry, row);
        String expected = (String) expect(row).get("error_kind");
        if (expected != null) {
            assertEquals(expected, observed, label(row) + " via " + entry);
        } else {
            assertTrue(observed == null || !urlKinds(family(row)).contains(observed),
                label(row) + " via " + entry + ": expected the URL to be accepted, observed " + observed);
        }
        return true;
    }

    /** Why the row does not run on this platform, or null when it does. */
    static String gated(Map<String, Object> row) {
        if (isLinux()) return null;
        if (family(row).equals("srt")) return "srt live-socket loopback gated to Linux";
        if (wireFields(row) != null && expect(row).get("error_kind") == null) {
            return "live RTSP tests gated to Linux";
        }
        return null;
    }

    /**
     * Assert the row's outcome through the JVM matches its record (and, for a valid row of a wire
     * scheme, what reached the wire); false when the corpus declares no JVM entry point for the
     * row's role or the row is gated off this platform.
     */
    static boolean checkRow(Map<String, Object> row) throws Exception {
        if (gated(row) != null) return false;
        List<String> fields = wireFields(row);
        if (fields != null && expect(row).get("error_kind") == null) {
            checkWire(row, fields);
            return true;
        }
        return checkOutcome(row);
    }

    static Stream<Arguments> corpusRows() {
        return rows().stream().map(r -> Arguments.of(label(r), r));
    }

    @ParameterizedTest(name = "{0}")
    @MethodSource("corpusRows")
    void urlCorpusRowMatchesItsRecord(String label, Map<String, Object> row) throws Exception {
        String gate = gated(row);
        Assumptions.assumeTrue(gate == null, gate);
        if (!checkRow(row)) {
            Assumptions.abort("no JVM entry point for " + family(row) + " " + row.get("role"));
        }
    }

    /** Every family in the header has a JVM table with a probe and the roles its rows use. */
    @Test
    void everyFamilyDeclaresAJvmEntryPoint() {
        Map<String, Set<String>> roles = new TreeMap<>();
        for (Map<String, Object> r : rows()) {
            roles.computeIfAbsent(family(r), f -> new TreeSet<>()).add((String) r.get("role"));
        }
        for (Map.Entry<String, Set<String>> e : roles.entrySet()) {
            Map<String, Object> table = legTable(rows().stream()
                .filter(r -> family(r).equals(e.getKey())).findFirst().orElseThrow());
            assertTrue(table.containsKey("probe"), "[legs.jvm] " + e.getKey() + " has no probe");
            for (String role : e.getValue()) {
                Object entry = table.get(role);
                if (entry == null) fail("[legs.jvm] " + e.getKey() + " declares no role " + role);
                boolean srtProbe = e.getKey().equals("srt")
                    && Set.of("shells.Sender.open", "shells.Receiver.open").contains(entry);
                assertTrue(entry.equals("absent") || ENTRIES.containsKey(entry) || srtProbe,
                    "[legs.jvm] " + e.getKey() + " " + role + " names " + entry + ", which no probe runs");
            }
        }
    }

    // ── A JSON reader for the golden: the test classpath has no JSON library ──

    static final class Json {
        private final String s;
        private int i;

        private Json(String s) { this.s = s; }

        static Object parse(String text) {
            Json j = new Json(text);
            Object v = j.value();
            j.ws();
            if (j.i != text.length()) throw j.error("trailing text");
            return v;
        }

        private AssertionError error(String what) {
            return new AssertionError("golden JSON: " + what + " at offset " + i);
        }

        private void ws() {
            while (i < s.length() && Character.isWhitespace(s.charAt(i))) i++;
        }

        private Object value() {
            ws();
            if (i >= s.length()) throw error("unexpected end");
            char c = s.charAt(i);
            switch (c) {
                case '{': return object();
                case '[': return array();
                case '"': return string();
                default:
                    if (s.startsWith("true", i)) { i += 4; return Boolean.TRUE; }
                    if (s.startsWith("false", i)) { i += 5; return Boolean.FALSE; }
                    if (s.startsWith("null", i)) { i += 4; return null; }
                    return number();
            }
        }

        private Map<String, Object> object() {
            Map<String, Object> m = new LinkedHashMap<>();
            i++;
            ws();
            if (s.charAt(i) == '}') { i++; return m; }
            while (true) {
                ws();
                if (s.charAt(i) != '"') throw error("expected a key");
                String k = string();
                ws();
                if (s.charAt(i++) != ':') throw error("expected ':'");
                m.put(k, value());
                ws();
                char c = s.charAt(i++);
                if (c == '}') return m;
                if (c != ',') throw error("expected ',' or '}'");
            }
        }

        private List<Object> array() {
            List<Object> l = new ArrayList<>();
            i++;
            ws();
            if (s.charAt(i) == ']') { i++; return l; }
            while (true) {
                l.add(value());
                ws();
                char c = s.charAt(i++);
                if (c == ']') return l;
                if (c != ',') throw error("expected ',' or ']'");
            }
        }

        private String string() {
            StringBuilder b = new StringBuilder();
            i++;
            while (true) {
                char c = s.charAt(i++);
                if (c == '"') return b.toString();
                if (c != '\\') { b.append(c); continue; }
                char e = s.charAt(i++);
                switch (e) {
                    case '"', '\\', '/' -> b.append(e);
                    case 'b' -> b.append('\b');
                    case 'f' -> b.append('\f');
                    case 'n' -> b.append('\n');
                    case 'r' -> b.append('\r');
                    case 't' -> b.append('\t');
                    case 'u' -> { b.append((char) Integer.parseInt(s.substring(i, i + 4), 16)); i += 4; }
                    default -> throw error("bad escape \\" + e);
                }
            }
        }

        private Object number() {
            int start = i;
            while (i < s.length() && "+-0123456789.eE".indexOf(s.charAt(i)) >= 0) i++;
            if (i == start) throw error("unexpected character '" + s.charAt(i) + "'");
            String n = s.substring(start, i);
            return n.matches("-?\\d+") ? (Object) Long.valueOf(n) : (Object) Double.valueOf(n);
        }
    }
}
