package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.MuxException;
import org.tstrans.TcpException;
import org.tstrans.TestSupport;
import org.tstrans.internal.GatedMuxSender;
import org.tstrans.mpegts.Av1CarriageMode;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.KlvStreamHandle;
import org.tstrans.mpegts.KlvStreamType;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;
import org.tstrans.mpegts.VideoStreamHandle;
import org.tstrans.srt.ManagedMuxSender;

/**
 * {@link MuxSender#reconfigure}: the running sender takes a new program on the wire, the
 * handle getters follow the configuration now in force and never wait, a refused
 * configuration changes nothing, and {@code close()} ends a reconfigure parked in the
 * transport as it ends a parked send. The far side is a plain socket, read to its end.
 */
class MuxSenderReconfigureTest {
    private static final long END_MS = TestSupport.scaledMs(10_000);
    // An IDR slice long enough to span several TS packets, so a continuity counter that
    // restarted shows as a jump.
    private static final byte[] NAL_IDR = TestSupport.longH264Idr();
    private static final byte[] KLV_LS = {
        0x06, 0x0e, 0x2b, 0x34, 0x02, 0x0b, 0x01, 0x01,
        0x0e, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
        0x03, 0x02, 0x01, 0x02,
    };


    private static MuxerConfig.Builder program(int program, int pmtPid) {
        return MuxerConfig.builder().programNumber(program).pmtPid(pmtPid);
    }

    private static MuxerConfig video() {
        return program(1, 0x100).addVideo(0x101, VideoCodec.H264).build();
    }

    private static MuxerConfig videoKlv() {
        return program(1, 0x100).addVideo(0x101, VideoCodec.H264)
            .addKlv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, true).build();
    }

    private static void sendFrames(MuxSender s, long firstPts, int count) throws Exception {
        for (int i = 0; i < count; i++) s.sendVideo(NAL_IDR, firstPts + i * 3000L, i == 0);
    }


    private static byte[] bytes(ByteBuffer b) {
        byte[] out = new byte[b.remaining()];
        b.duplicate().get(out);
        return out;
    }

    /** The version_number of each PMT section start on {@code pmtPid}, in wire order. */
    private static List<Integer> pmtVersions(byte[] ts, int pmtPid) {
        List<Integer> out = new ArrayList<>();
        for (int off = 0; off + 188 <= ts.length; off += 188) {
            int pid = ((ts[off + 1] & 0x1F) << 8) | (ts[off + 2] & 0xFF);
            boolean start = (ts[off + 1] & 0x40) != 0;
            if (pid != pmtPid || !start) continue;
            int p = off + 4;
            if ((ts[off + 3] & 0x20) != 0) p += 1 + (ts[off + 4] & 0xFF);
            int section = p + 1 + (ts[p] & 0xFF);
            out.add((ts[section + 5] >> 1) & 0x1F);
        }
        return out;
    }

    private static long discontinuities(List<DemuxEvent> events, int pid) {
        return events.stream()
            .filter(e -> e instanceof DemuxEvent.Discontinuity d && d.stream().pid() == pid)
            .count();
    }

    private static List<DemuxEvent.Video> videos(List<DemuxEvent> events, int pid) {
        List<DemuxEvent.Video> out = new ArrayList<>();
        for (DemuxEvent e : events) {
            if (e instanceof DemuxEvent.Video v && v.stream().pid() == pid) out.add(v);
        }
        return out;
    }

    private static List<byte[]> metadata(List<DemuxEvent> events, int pid) {
        List<byte[]> out = new ArrayList<>();
        for (DemuxEvent e : events) {
            if (e instanceof DemuxEvent.Metadata m && m.stream().pid() == pid) out.add(bytes(m.payload()));
        }
        return out;
    }

    /** What the survivor carries after a reconfigure: ten whole frames, no counter jump. */
    private static void assertTheSurvivorIsWhole(byte[] wire) throws Exception {
        List<DemuxEvent> events = TestSupport.demuxAll(wire);
        assertEquals(0, discontinuities(events, 0x101), "a continuity jump on the surviving video");
        List<DemuxEvent.Video> vids = videos(events, 0x101);
        assertEquals(10, vids.size());
        byte[] body = Arrays.copyOfRange(NAL_IDR, 5, NAL_IDR.length);
        for (DemuxEvent.Video v : vids) {
            byte[] raw = bytes(v.raw());
            assertArrayEquals(body, Arrays.copyOfRange(raw, raw.length - body.length, raw.length));
        }
        List<Integer> versions = pmtVersions(wire, 0x100);
        assertEquals(0, versions.get(0), "the first PMT's version: " + versions);
        assertEquals(1, versions.get(versions.size() - 1), "the PMT after the reconfigure: " + versions);
    }

    @Test
    void reconfigureKeepsTheSurvivorAndMovesThePmtVersion() throws Exception {
        try (TcpSink sink = new TcpSink(); MuxSender s = MuxSender.open(sink.url(), video())) {
            VideoStreamHandle v = s.videoHandle().orElseThrow();
            sendFrames(s, 90_000, 5);
            s.reconfigure(videoKlv());
            assertEquals(Optional.of(v), s.videoHandle(), "the survivor keeps its handle");
            sendFrames(s, 90_000 + 5 * 3000, 5);
            s.finish();
            assertTheSurvivorIsWhole(sink.awaitEnd(END_MS));
        }
    }

    @Test
    void reconfigureAddsAKlvStreamAndKlvHandleReadsIt() throws Exception {
        try (TcpSink sink = new TcpSink(); MuxSender s = MuxSender.open(sink.url(), video())) {
            assertEquals(Optional.empty(), s.klvHandle());
            sendFrames(s, 90_000, 1);
            s.reconfigure(videoKlv());
            KlvStreamHandle k = s.klvHandle().orElseThrow(() -> new AssertionError("no KLV handle"));
            s.sendKlv(KLV_LS, 93_000, 0);
            s.sendKlvTo(k, KLV_LS, 96_000, 0);
            sendFrames(s, 99_000, 1);
            s.finish();
            List<byte[]> md = metadata(TestSupport.demuxAll(sink.awaitEnd(END_MS)), 0x102);
            assertEquals(2, md.size());
            for (byte[] m : md) assertArrayEquals(KLV_LS, m);
        }
    }

    @Test
    void aRemovedStreamsKeptHandleIsRefused() throws Exception {
        try (TcpSink sink = new TcpSink(); MuxSender s = MuxSender.open(sink.url(), videoKlv())) {
            KlvStreamHandle gone = s.klvHandle().orElseThrow();
            s.reconfigure(video());
            assertEquals(Optional.empty(), s.klvHandle());
            MuxException e = assertThrows(MuxException.class, () -> s.sendKlvTo(gone, KLV_LS, 90_000, 0));
            assertEquals(MuxException.Kind.INVALID_USAGE, e.kind());
            sendFrames(s, 90_000, 1);   // the sender is still usable
        }
    }

    /** {@code refused} on one sender, nothing on its twin: the two wires are the same bytes. */
    private static void assertRefusedChangesNothing(String what, MuxerConfig start, MuxerConfig refused)
            throws Exception {
        try (TcpSink a = new TcpSink(); TcpSink b = new TcpSink();
             MuxSender s = MuxSender.open(a.url(), start); MuxSender twin = MuxSender.open(b.url(), start)) {
            sendFrames(s, 90_000, 3);
            sendFrames(twin, 90_000, 3);
            Optional<VideoStreamHandle> v = s.videoHandle();
            MuxException e = assertThrows(MuxException.class, () -> s.reconfigure(refused), what);
            assertEquals(MuxException.Kind.CONFIG_INVALID, e.kind(), what);
            assertEquals(v, s.videoHandle(), what);
            assertEquals(Optional.empty(), s.klvHandle(), what);
            sendFrames(s, 99_000, 3);
            sendFrames(twin, 99_000, 3);
            s.finish();
            twin.finish();
            assertArrayEquals(b.awaitEnd(END_MS), a.awaitEnd(END_MS), what);
        }
    }

    @Test
    void aRefusedConfigChangesNothing() throws Exception {
        Map<String, MuxerConfig> refused = new LinkedHashMap<>();
        refused.put("pmt-pid-moved", program(1, 0x110).addVideo(0x101, VideoCodec.H264).build());
        refused.put("codec-changed", program(1, 0x100).addVideo(0x101, VideoCodec.H265).build());
        refused.put("pid-moved-program", program(2, 0x100).addVideo(0x101, VideoCodec.H264).build());
        for (Map.Entry<String, MuxerConfig> c : refused.entrySet()) {
            assertRefusedChangesNothing(c.getKey(), video(), c.getValue());
        }
    }

    @Test
    void aChangedNonProgramFieldIsRefused() throws Exception {
        Map<String, MuxerConfig> refused = new LinkedHashMap<>();
        refused.put("pcrIntervalMs", program(1, 0x100).addVideo(0x101, VideoCodec.H264)
            .addKlv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, true).pcrIntervalMs(20).build());
        refused.put("pcrLeadMs", program(1, 0x100).addVideo(0x101, VideoCodec.H264)
            .addKlv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, true).pcrLeadMs(200).build());
        refused.put("psiIntervalMs", program(1, 0x100).addVideo(0x101, VideoCodec.H264)
            .addKlv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, true).psiIntervalMs(50).build());
        refused.put("bufferPackets", program(1, 0x100).addVideo(0x101, VideoCodec.H264)
            .addKlv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, true).bufferPackets(5_000).build());
        refused.put("av1Carriage", program(1, 0x100).addVideo(0x101, VideoCodec.H264)
            .addKlv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, true)
            .av1Carriage(Av1CarriageMode.INTEROP_RAW_OBU).build());
        for (Map.Entry<String, MuxerConfig> c : refused.entrySet()) {
            // The program part alone (videoKlv) is accepted; the field is what is refused.
            assertRefusedChangesNothing(c.getKey(), video(), c.getValue());
        }
    }

    @Test
    void reconfigureAfterCloseThrows() throws Exception {
        try (TcpSink sink = new TcpSink()) {
            MuxSender s = MuxSender.open(sink.url(), video());
            assertThrows(NullPointerException.class, () -> s.reconfigure(null));
            try (CancelHandle h = s.cancelHandle()) {
                h.cancel();
            }
            TcpException e = assertThrows(TcpException.class, () -> s.reconfigure(videoKlv()));
            assertEquals(TcpException.Kind.CLOSED, e.kind());
            assertEquals(Optional.empty(), s.klvHandle(), "a refused reconfigure changes no handle");
            s.close();
            assertThrows(IllegalStateException.class, () -> s.reconfigure(videoKlv()));
        }
    }

    /**
     * A gated sender whose reconfigure is parked in the transport: an earlier send was
     * refused, so the reconfigure first sends the output it kept, and that send waits.
     */
    private static ShellPeers.Parked parkedReconfigure(GatedMuxSender s) throws Exception {
        TcpException refused = assertThrows(TcpException.class, () -> sendFrames(s, 90_000, 1));
        assertEquals(TcpException.Kind.BACKPRESSURE, refused.kind());
        // cancel-handle: the watchdog closes it
        return new ShellPeers.Parked(s, s.cancelHandle(), "the reconfigure returned", () -> {
            s.reconfigure(videoKlv());
            return null;
        });
    }

    @Test
    @Timeout(60)
    void handleGettersAnswerWhileAReconfigureIsParked() throws Exception {
        assumeTrue(ShellPeers.HOOKS, "the gated transport is a jni-test-hooks probe");
        GatedMuxSender s = GatedMuxSender.open(video());
        try (ShellPeers.Parked parked = parkedReconfigure(s)) {
            long ms = TestSupport.scaledMs(2_000);
            assertTrue(ShellPeers.within(ms, "videoHandle()", s::videoHandle).isPresent());
            // Parked before the new configuration took effect: still the old stream set.
            assertEquals(Optional.empty(), ShellPeers.within(ms, "klvHandle()", s::klvHandle));
            assertEquals(Optional.empty(), ShellPeers.within(ms, "audioHandle()", s::audioHandle));
            assertEquals(Optional.empty(), ShellPeers.within(ms, "subtitleHandle()", s::subtitleHandle));
            assertEquals(Optional.empty(), ShellPeers.within(ms, "dataHandle()", s::dataHandle));
            parked.assertStillParked();
        } finally {
            s.close();
        }
    }

    @Test
    @Timeout(60)
    void closeEndsAParkedReconfigure() throws Exception {
        assumeTrue(ShellPeers.HOOKS, "the gated transport is a jni-test-hooks probe");
        GatedMuxSender s = GatedMuxSender.open(video());
        try (ShellPeers.Parked parked = parkedReconfigure(s)) {
            CompletableFuture<Void> closed = CompletableFuture.runAsync(s::close);
            closed.get(TestSupport.scaledMs(5_000), TimeUnit.MILLISECONDS);
            Throwable end = parked.awaitEnd(TestSupport.scaledMs(5_000));
            assertTrue(end instanceof TcpException, "the parked reconfigure ended with " + end);
            assertEquals(TcpException.Kind.CLOSED, ((TcpException) end).kind());
            assertFalse(s.isAlive());
            assertThrows(IllegalStateException.class, () -> s.reconfigure(videoKlv()));
        } finally {
            s.close();
        }
    }

    // ── ManagedMuxSender ───────────────────────────────────────────────────

    private static final int LATENCY_MS = 120;

    /**
     * Run {@code body} against a managed SRT sender whose peer is a plain receiver; return
     * every packet the receiver read once it holds as many bytes as the sender sent.
     */
    private interface ManagedBody {
        void run(ManagedMuxSender s) throws Exception;
    }

    private static byte[] overManagedSrt(ManagedBody body) throws Exception {
        int port = TestSupport.freeUdpPort();
        CompletableFuture<Receiver> peer = new CompletableFuture<>();
        Thread listen = new Thread(() -> {
            try {
                peer.complete(Receiver.open("srt://:" + port + "?mode=listener&latency=" + LATENCY_MS));
            } catch (Exception e) {
                peer.completeExceptionally(e);
            }
        }, "reconfigure-srt-listener");
        listen.setDaemon(true);
        listen.start();
        ManagedMuxSender s = TestSupport.connectSender(
            "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS, 5_000);
        Receiver rx = peer.get(10, TimeUnit.SECONDS);
        java.io.ByteArrayOutputStream got = new java.io.ByteArrayOutputStream();
        AtomicLong read = new AtomicLong();
        Thread reader = new Thread(() -> {
            try {
                while (true) {
                    byte[] p = rx.recvBytes();
                    synchronized (got) {
                        got.write(p, 0, p.length);
                    }
                    read.addAndGet(p.length);
                }
            } catch (Exception ended) {
                // the receiver was closed by the test
            }
        }, "reconfigure-srt-reader");
        reader.setDaemon(true);
        reader.start();
        try {
            body.run(s);
            long sent = s.stats().muxerStats().tsBytesEmitted();
            TestSupport.waitFor(sent + " bytes at the receiver", () -> read.get() >= sent, END_MS);
            synchronized (got) {
                return got.toByteArray();
            }
        } finally {
            s.close();
            rx.close();
        }
    }

    @Test
    @Timeout(60)
    void managedReconfigureKeepsTheSurvivorAndMovesThePmtVersion() throws Exception {
        assumeTrue(TestSupport.isLinux(), "SRT live-socket test gated to Linux");
        byte[] wire = overManagedSrt(s -> {
            VideoStreamHandle v = s.videoHandle().orElseThrow();
            sendManagedFrames(s, 90_000, 5);
            s.reconfigure(videoKlvOn(TestSupport.roundtripConfig()));
            assertEquals(Optional.of(v), s.videoHandle());
            sendManagedFrames(s, 90_000 + 5 * 3000, 5);
        });
        List<DemuxEvent> events = TestSupport.demuxAll(wire);
        int videoPid = TestSupport.roundtripConfig().streamPids()[0];
        assertEquals(0, discontinuities(events, videoPid));
        assertEquals(10, videos(events, videoPid).size());
        List<Integer> versions = pmtVersions(wire, TestSupport.roundtripConfig().pmtPid());
        assertEquals(0, versions.get(0), versions.toString());
        assertEquals(1, versions.get(versions.size() - 1), versions.toString());
    }

    @Test
    @Timeout(60)
    void managedReconfigureAddsAKlvStreamAndKlvHandleReadsIt() throws Exception {
        assumeTrue(TestSupport.isLinux(), "SRT live-socket test gated to Linux");
        MuxerConfig withKlv = videoKlvOn(TestSupport.roundtripConfig());
        byte[] wire = overManagedSrt(s -> {
            assertEquals(Optional.empty(), s.klvHandle());
            sendManagedFrames(s, 90_000, 1);
            s.reconfigure(withKlv);
            KlvStreamHandle k = s.klvHandle().orElseThrow(() -> new AssertionError("no KLV handle"));
            s.sendKlv(KLV_LS, 93_000, 0);
            s.sendKlvTo(k, KLV_LS, 96_000, 0);
            sendManagedFrames(s, 99_000, 2);
        });
        List<byte[]> md = metadata(TestSupport.demuxAll(wire), KLV_PID);
        assertEquals(2, md.size());
        for (byte[] m : md) assertArrayEquals(KLV_LS, m);
    }

    private static final int KLV_PID = 0x1F00;

    /** {@code base}'s program (one video stream) plus a synchronous KLV stream. */
    private static MuxerConfig videoKlvOn(MuxerConfig base) {
        assertEquals(1, base.streamPids().length, "the round-trip program has one stream");
        return MuxerConfig.builder().programNumber(base.programNumber()).pmtPid(base.pmtPid())
            .addVideo(base.streamPids()[0], VideoCodec.H264)
            .addKlv(KLV_PID, KlvStreamType.SYNCHRONOUS_METADATA, true).build();
    }

    private static void sendManagedFrames(ManagedMuxSender s, long firstPts, int count) throws Exception {
        for (int i = 0; i < count; i++) s.sendVideo(NAL_IDR, firstPts + i * 3000L, i == 0);
    }
}
