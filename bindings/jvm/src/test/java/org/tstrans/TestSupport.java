package org.tstrans;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.net.DatagramSocket;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.List;
import java.util.Locale;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import org.tstrans.codec.NalUnit;
import org.tstrans.codec.VideoUnit;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.Demuxer;
import org.tstrans.mpegts.Muxer;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;
import org.tstrans.srt.ManagedMuxSender;

/**
 * Test-only fixture builders and helpers shared across the JUnit suite (never shipped —
 * lives under {@code src/test/java}). Each method here previously existed as
 * an identical (or, for {@link #roundtripConfig()} /
 * {@link #roundtripConfigWithData()}, a two-shape) private copy in every test
 * class that needed it; import statically with the same unqualified name so
 * existing call sites are unchanged.
 */
public final class TestSupport {
    private TestSupport() {}

    /** Synthetic Annex-B IDR NAL: start code + IDR header + 15 filler bytes. */
    public static byte[] syntheticH264Idr() {
        byte[] buf = new byte[20];
        buf[0] = 0x00;
        buf[1] = 0x00;
        buf[2] = 0x00;
        buf[3] = 0x01;
        buf[4] = 0x65;
        for (int i = 0; i < 15; i++) {
            buf[5 + i] = (byte) (0xA5 ^ i);
        }
        return buf;
    }

    /**
     * {@code ms} scaled by the {@code TST_TEST_TIMEOUT_SCALE} environment variable (a
     * decimal factor, default 1; values below 1 count as 1), so slow runners can stretch
     * every live-socket budget at once.
     */
    public static long scaledMs(long ms) {
        double scale = 1.0;
        String raw = System.getenv("TST_TEST_TIMEOUT_SCALE");
        if (raw != null) {
            try {
                scale = Math.max(1.0, Double.parseDouble(raw.trim()));
            } catch (NumberFormatException e) {
                scale = 1.0;
            }
        }
        return (long) Math.ceil(ms * scale);
    }

    /**
     * Poll every 10 ms until {@code ref} holds a value or {@code ms} elapses.
     *
     * @return whether {@code ref} holds a value
     */
    public static boolean awaitNonNull(AtomicReference<?> ref, long ms) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(ms);
        while (ref.get() == null) {
            if (System.nanoTime() >= deadline) return false;
            Thread.sleep(10);
        }
        return true;
    }

    /** A finished child process: its exit status and its combined stdout and stderr. */
    public record ChildResult(int exitValue, String output) {}

    /**
     * Start {@code pb} with stdout and stderr redirected to {@code log}, wait at most
     * {@code scaledMs(ms)}, and kill the child on timeout or interruption. The output is
     * read only after the child has exited, so a hung child cannot block the read.
     */
    public static ChildResult runChild(ProcessBuilder pb, Path log, long ms)
            throws IOException, InterruptedException {
        Process child = pb.redirectErrorStream(true).redirectOutput(log.toFile()).start();
        boolean exited;
        try {
            exited = child.waitFor(scaledMs(ms), TimeUnit.MILLISECONDS);
        } catch (InterruptedException e) {
            child.destroyForcibly();
            throw e;
        }
        if (!exited) {
            child.destroyForcibly().waitFor(scaledMs(5_000), TimeUnit.MILLISECONDS);
        }
        String out = Files.readString(log, StandardCharsets.UTF_8);
        if (!exited) throw new AssertionError("child did not exit within " + scaledMs(ms) + " ms: " + out);
        return new ChildResult(child.exitValue(), out);
    }

    /** The value of response header {@code name} (case-insensitive); fails when absent. */
    public static String rtspHeader(String resp, String name) {
        for (String line : resp.split("\r\n")) {
            int c = line.indexOf(':');
            if (c > 0 && line.substring(0, c).trim().equalsIgnoreCase(name)) {
                return line.substring(c + 1).trim();
            }
        }
        throw new AssertionError("no " + name + " header in " + resp);
    }

    /** Whether the current OS is Linux, for gating live-socket fixtures. */
    public static boolean isLinux() {
        return System.getProperty("os.name", "").toLowerCase(Locale.ROOT).contains("linux");
    }

    /** Pack `int` literals (0-255) into a `byte[]`, one cast per element. */
    public static byte[] unsigned(int... vals) {
        byte[] out = new byte[vals.length];
        for (int i = 0; i < vals.length; i++) {
            out[i] = (byte) vals[i];
        }
        return out;
    }

    /**
     * Lowercase hex SHA-256 over the concatenated payload bytes of every
     * {@link NalUnit} in {@code units} (Annex-B start codes already stripped
     * by the demuxer).
     */
    public static String sha256Units(List<VideoUnit> units) throws NoSuchAlgorithmException {
        MessageDigest md = MessageDigest.getInstance("SHA-256");
        for (VideoUnit u : units) {
            NalUnit n = (NalUnit) u;
            ByteBuffer view = n.payload().duplicate();
            byte[] bytes = new byte[view.remaining()];
            view.get(bytes);
            md.update(bytes);
        }
        byte[] digest = md.digest();
        StringBuilder sb = new StringBuilder(digest.length * 2);
        for (byte b : digest) {
            sb.append(Character.forDigit((b >> 4) & 0xF, 16));
            sb.append(Character.forDigit(b & 0xF, 16));
        }
        return sb.toString();
    }

    /** Single-program H.264-only {@link MuxerConfig} shared by roundtrip fixtures. */
    public static MuxerConfig roundtripConfig() {
        return MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264)
            .build();
    }

    /**
     * {@link #roundtripConfig()} plus one private-data stream (user-private
     * {@code stream_type} 0xF0, PID 0x0100) — the shape the SRT/RTP managed
     * and mux/demux loopback fixtures need.
     */
    public static MuxerConfig roundtripConfigWithData() {
        return MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264)
            .addData(0x0100, 0xF0, true)
            .build();
    }

    /** Bind a throwaway UDP socket to :0, read the kernel-picked port, release it. */
    public static int freeUdpPort() throws IOException {
        try (DatagramSocket s = new DatagramSocket(new InetSocketAddress("127.0.0.1", 0))) {
            return s.getLocalPort();
        }
    }

    /**
     * 8-byte Annex-B IDR prefix (start code + IDR header + three slice
     * bytes) — the DTS/MISP muxer tests' fixture, distinct from the 20-byte
     * {@link #syntheticH264Idr()}.
     */
    public static byte[] syntheticH264IdrPrefix() {
        return new byte[]{0x00, 0x00, 0x00, 0x01, 0x65, (byte) 0x88, (byte) 0x84, 0x00};
    }

    /** Drain every buffered TS byte from {@code m}. */
    public static byte[] drain(Muxer m) {
        ByteArrayOutputStream acc = new ByteArrayOutputStream();
        byte[] buf = new byte[8192];
        int n;
        while ((n = m.pull(buf)) > 0) acc.write(buf, 0, n);
        return acc.toByteArray();
    }

    /** {@code crates/tst-integration/tests/fixtures/scenarios/<scenarioId>}, from the Gradle project dir. */
    public static Path scenarioDir(String scenarioId) {
        return Path.of(System.getProperty("user.dir"), "..", "..",
                "crates/tst-integration/tests/fixtures/scenarios", scenarioId).normalize();
    }

    /** The {@code h264-st0601-mp} scenario's input transport stream. */
    public static Path inputTs() {
        return scenarioDir("h264-st0601-mp").resolve("input.ts");
    }

    /**
     * Open a {@link ManagedMuxSender} caller with {@link #roundtripConfig()},
     * retrying every 50 ms until {@code budgetMs} elapses (the listener may
     * not be accepting yet).
     */
    public static ManagedMuxSender connectSender(String url, long budgetMs) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(budgetMs);
        SrtException last = null;
        while (System.nanoTime() < deadline) {
            try {
                return ManagedMuxSender.fromUrl(url, roundtripConfig());
            } catch (SrtException e) {
                last = e;
                Thread.sleep(50);
            }
        }
        throw new AssertionError("caller could not connect within " + budgetMs + " ms", last);
    }

    /** Concatenate byte arrays in order. */
    public static byte[] cat(byte[]... arrays) {
        int total = 0;
        for (byte[] a : arrays) total += a.length;
        byte[] out = new byte[total];
        int pos = 0;
        for (byte[] a : arrays) {
            System.arraycopy(a, 0, out, pos, a.length);
            pos += a.length;
        }
        return out;
    }

    /** {@code val} as {@code n} big-endian bytes. */
    public static byte[] beBytes(long val, int n) {
        byte[] out = new byte[n];
        for (int i = n - 1; i >= 0; i--) {
            out[i] = (byte) (val & 0xFF);
            val >>= 8;
        }
        return out;
    }

    /** Demux {@code tsBytes} and return {@link #sha256Units} of the first typed Video event. */
    public static String sha256OfDemuxedVideoUnits(byte[] tsBytes) throws Exception {
        try (Demuxer d = new Demuxer()) {
            d.feed(tsBytes);
            d.flush();
            for (DemuxEvent e : d) {
                if (e instanceof DemuxEvent.Video v && !v.parse().isEmpty()) {
                    return sha256Units(v.parse());
                }
            }
        }
        throw new AssertionError("no typed Video event found in the demuxed bytes");
    }
}
