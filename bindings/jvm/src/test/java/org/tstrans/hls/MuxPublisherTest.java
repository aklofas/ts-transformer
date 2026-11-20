package org.tstrans.hls;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.syntheticH264Idr;
import static org.tstrans.TestSupport.syntheticH264IdrPrefix;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReference;
import java.util.stream.Stream;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.io.TempDir;
import org.tstrans.HlsException;
import org.tstrans.MuxException;
import org.tstrans.codec.MispTimestamp;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.Demuxer;
import org.tstrans.mpegts.KlvStreamType;
import org.tstrans.mpegts.MetadataKind;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

class MuxPublisherTest {
    /** ST 0601 UL + tiny body; demuxed back out of a segment. */
    static final byte[] KLV = org.tstrans.TestSupport.unsigned(
        0x06, 0x0e, 0x2b, 0x34, 0x02, 0x0b, 0x01, 0x01,
        0x0e, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
        0x06, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01);

    static MuxerConfig video() {
        return MuxerConfig.builder().programNumber(1).pmtPid(0x100).addVideo(0x101, VideoCodec.H264).build();
    }

    static MuxerConfig videoKlv() {
        return MuxerConfig.builder().programNumber(1).pmtPid(0x100)
            .addVideo(0x101, VideoCodec.H264)
            .addKlv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, true).build();
    }

    static HlsPublisher pub(Path dir, HlsMode mode) throws HlsException {
        return HlsPublisher.builder().bind("127.0.0.1:0").outputDir(dir.toString())
            .segmentDurationMs(1000).playlistWindow(3).mode(mode).build();
    }

    static Path firstSegment(Path dir) throws Exception {
        try (Stream<Path> s = Files.list(dir)) {
            return s.filter(p -> p.toString().endsWith(".ts")).sorted().findFirst().orElseThrow();
        }
    }

    @Test
    void videoThenFinishIntoPublisher(@TempDir Path dir) throws Exception {
        HlsPublisher source = pub(dir, HlsMode.LIVE);
        MuxPublisher mp = MuxPublisher.withConfigHls(source, video());
        assertThrows(IllegalStateException.class, source::stats, "source consumed");
        for (int i = 0; i < 3; i++) mp.pushVideo(syntheticH264Idr(), i * 90_000L, true);
        MuxPublisherStats ms = mp.stats();
        assertTrue(ms.bytesPushed() > 0);
        assertTrue(ms.cutCalls() >= 2, "3 keyframes → ≥ 2 cuts");
        assertNotNull(mp.publisherStats());
        HlsPublisher recovered = mp.finishIntoPublisher();
        recovered.finish();
        assertTrue(Files.exists(dir.resolve("playlist.m3u8")));
        assertThrows(IllegalStateException.class, mp::finishIntoPublisher);
        assertThrows(IllegalStateException.class, () -> mp.pushVideo(syntheticH264Idr(), 0, true));
        mp.close(); // quiet after consume
    }

    @Test
    void pushVideoWithDtsWritesBothTimestamps(@TempDir Path dir) throws Exception {
        MuxPublisher mp = MuxPublisher.withConfigHls(pub(dir, HlsMode.LIVE), video());
        mp.pushVideoWithDts(syntheticH264Idr(), 18_000L, 0L, true);
        mp.cutSegment();
        mp.finishIntoPublisher().finish();
        byte[] seg = Files.readAllBytes(firstSegment(dir));
        int pes = indexOf(seg, new byte[] {0, 0, 1, (byte) 0xE0});
        assertTrue(pes >= 0, "video PES start");
        assertEquals((byte) 0xC0, seg[pes + 7], "PES flags: PTS and DTS present");
        assertEquals(10, seg[pes + 8], "PES header data length");
    }

    @Test
    void withConfigHlsPassesThePcrLead(@TempDir Path dir) throws Exception {
        MuxPublisher mp = MuxPublisher.withConfigHls(pub(dir, HlsMode.LIVE),
            org.tstrans.TestSupport.pcrLead250Config());
        mp.pushVideo(syntheticH264Idr(), 90_000L, true);
        mp.cutSegment();
        mp.finishIntoPublisher().finish();
        assertEquals(org.tstrans.TestSupport.PCR_LEAD_250_FIRST_PCR,
            org.tstrans.TestSupport.firstPcrBase(Files.readAllBytes(firstSegment(dir))));
    }

    @Test
    void pushVideoWithDtsNeedsExactlyOneVideoStream(@TempDir Path dir) throws Exception {
        MuxerConfig two = MuxerConfig.builder().programNumber(1).pmtPid(0x100)
            .addVideo(0x101, VideoCodec.H264).addVideo(0x102, VideoCodec.H264).build();
        try (MuxPublisher mp = MuxPublisher.withConfigHls(pub(dir, HlsMode.LIVE), two)) {
            MuxException ex = assertThrows(MuxException.class,
                () -> mp.pushVideoWithDts(syntheticH264Idr(), 3_000L, 0L, true));
            assertEquals(MuxException.Kind.INVALID_USAGE, ex.kind());
        }
    }

    @Test
    void klvPreservedInSegment(@TempDir Path dir) throws Exception {
        MuxPublisher mp = MuxPublisher.withConfigHls(pub(dir, HlsMode.LIVE), videoKlv());
        mp.pushKlv(KLV, 0L, 0);
        mp.pushVideo(syntheticH264Idr(), 90_000L, true);
        mp.cutSegment();
        mp.finishIntoPublisher().finish();
        byte[] seg = Files.readAllBytes(firstSegment(dir));

        List<DemuxEvent.Metadata> klv = new ArrayList<>();
        for (DemuxEvent ev : demux(seg)) {
            if (ev instanceof DemuxEvent.Metadata m) klv.add(m);
        }
        assertEquals(1, klv.size(), "one KLV unit in the segment");
        DemuxEvent.Metadata m = klv.get(0);
        assertEquals(0x102, m.stream().pid(), "on the KLV PID");
        assertEquals(MetadataKind.KLV_SYNC_AU_CELL, m.kind());
        assertEquals(1, m.cellCount());
        assertArrayEquals(KLV, bytes(m.payload()), "the KLV unit, AU cell stripped");

        // The demuxer strips the Metadata AU cell; read its 5-byte header off the wire.
        byte[] pes = firstPesPayload(seg, 0x102);
        assertEquals(0, pes[0] & 0xFF, "metadata_service_id");
        assertEquals(0xC0, pes[2] & 0xC0, "cell_fragment_indication: one complete cell");
        assertEquals(KLV.length, ((pes[3] & 0xFF) << 8) | (pes[4] & 0xFF), "AU_cell_data_length");
        assertArrayEquals(KLV, Arrays.copyOfRange(pes, 5, 5 + KLV.length), "the cell's data");
    }

    @Test
    void pushDataReachesItsPid(@TempDir Path dir) throws Exception {
        MuxerConfig cfg = MuxerConfig.builder().programNumber(1).pmtPid(0x100)
            .addVideo(0x101, VideoCodec.H264)
            .addData(0x103, 0xF0, true).build();
        byte[] payload = {(byte) 0xDE, (byte) 0xAD, (byte) 0xBE, (byte) 0xEF, 7};
        MuxPublisher mp = MuxPublisher.withConfigHls(pub(dir, HlsMode.LIVE), cfg);
        mp.pushVideo(syntheticH264Idr(), 0L, true);
        mp.pushData(payload, 45_000L);
        mp.cutSegment();
        mp.finishIntoPublisher().finish();

        List<DemuxEvent.UnknownSample> data = new ArrayList<>();
        for (DemuxEvent ev : demux(Files.readAllBytes(firstSegment(dir)))) {
            if (ev instanceof DemuxEvent.UnknownSample u) data.add(u);
        }
        assertEquals(1, data.size());
        assertEquals(0x103, data.get(0).stream().pid());
        assertEquals(0xF0, data.get(0).streamType());
        assertEquals(45_000L, data.get(0).pts());
        assertArrayEquals(payload, bytes(data.get(0).payload()));
    }

    @Test
    void pushDataNeedsExactlyOneDataStream(@TempDir Path dir) throws Exception {
        try (MuxPublisher mp = MuxPublisher.withConfigHls(pub(dir, HlsMode.LIVE), video())) {
            MuxException ex = assertThrows(MuxException.class, () -> mp.pushData(new byte[] {1}, 0L));
            assertEquals(MuxException.Kind.INVALID_USAGE, ex.kind());
        }
    }

    @Test
    void pushVideoMispRecoversTheTimestamp(@TempDir Path dir) throws Exception {
        MispTimestamp input = MispTimestamp.micros(0x0005_F5E1_0000_0001L, 0x1F);
        MuxPublisher mp = MuxPublisher.withConfigHls(pub(dir, HlsMode.LIVE), video());
        mp.pushVideoMisp(syntheticH264IdrPrefix(), 9_000L, true, input);
        mp.cutSegment();
        mp.finishIntoPublisher().finish();

        List<DemuxEvent.Video> videos = new ArrayList<>();
        for (DemuxEvent ev : demux(Files.readAllBytes(firstSegment(dir)))) {
            if (ev instanceof DemuxEvent.Video v) videos.add(v);
        }
        assertEquals(1, videos.size());
        assertEquals(9_000L, videos.get(0).pts());
        MispTimestamp got = MispTimestamp.extract(bytes(videos.get(0).raw()), VideoCodec.H264);
        assertNotNull(got, "the MISP SEI is in the demuxed access unit");
        assertEquals(input.kind(), got.kind());
        assertEquals(input.timeStatus(), got.timeStatus());
        assertEquals(input.value(), got.value());
    }

    /** {@code keyFrame} reaches the muxer: only the key frame's PES is a random access point. */
    @Test
    void pushVideoMispPassesTheKeyFrameFlag(@TempDir Path dir) throws Exception {
        MispTimestamp misp = MispTimestamp.micros(0x0005_F5E1_0000_0001L, 0x1F);
        MuxPublisher mp = MuxPublisher.withConfigHls(pub(dir, HlsMode.LIVE), video());
        mp.pushVideoMisp(syntheticH264IdrPrefix(), 9_000L, true, misp);
        mp.pushVideoMisp(syntheticH264IdrPrefix(), 12_000L, false, misp);
        mp.cutSegment();
        mp.finishIntoPublisher().finish();

        List<DemuxEvent.Video> videos = new ArrayList<>();
        for (DemuxEvent ev : demux(Files.readAllBytes(firstSegment(dir)))) {
            if (ev instanceof DemuxEvent.Video v) videos.add(v);
        }
        assertEquals(2, videos.size());
        assertTrue(videos.get(0).randomAccessIndicator(), "the key frame");
        assertFalse(videos.get(1).randomAccessIndicator(), "the frame pushed with keyFrame=false");
    }

    @Test
    void pushVideoMispRefusesANanoTimestampOnH264(@TempDir Path dir) throws Exception {
        try (MuxPublisher mp = MuxPublisher.withConfigHls(pub(dir, HlsMode.LIVE), video())) {
            MuxException ex = assertThrows(MuxException.class, () -> mp.pushVideoMisp(
                syntheticH264IdrPrefix(), 0L, true, MispTimestamp.nanos(1L, 0)));
            assertEquals(MuxException.Kind.MISP_TIME, ex.kind());
        }
    }

    static List<DemuxEvent> demux(byte[] ts) throws Exception {
        List<DemuxEvent> out = new ArrayList<>();
        try (Demuxer d = new Demuxer()) {
            d.feed(ts);
            d.flush();
            for (DemuxEvent ev : d) out.add(ev);
        }
        return out;
    }

    static byte[] bytes(java.nio.ByteBuffer b) {
        byte[] out = new byte[b.remaining()];
        b.duplicate().get(out);
        return out;
    }

    /** The payload of the first PES on {@code pid}, as far as its first TS packet carries it. */
    static byte[] firstPesPayload(byte[] ts, int pid) {
        for (int off = 0; off + 188 <= ts.length; off += 188) {
            int p = ((ts[off + 1] & 0x1F) << 8) | (ts[off + 2] & 0xFF);
            if (p != pid || (ts[off + 1] & 0x40) == 0) continue;
            int at = off + 4;
            if (((ts[off + 3] >> 4) & 0x3) == 3) at += 1 + (ts[at] & 0xFF);
            assertArrayEquals(new byte[] {0, 0, 1}, Arrays.copyOfRange(ts, at, at + 3), "PES start");
            at += 9 + (ts[at + 8] & 0xFF);
            return Arrays.copyOfRange(ts, at, off + 188);
        }
        throw new AssertionError("no PES on PID 0x" + Integer.toHexString(pid));
    }

    static int indexOf(byte[] hay, byte[] needle) {
        outer:
        for (int i = 0; i + needle.length <= hay.length; i++) {
            for (int j = 0; j < needle.length; j++) if (hay[i + j] != needle[j]) continue outer;
            return i;
        }
        return -1;
    }

    @Test
    void extinfIsMediaDerived(@TempDir Path dir) throws Exception {
        MuxPublisher mp = MuxPublisher.withConfigHls(pub(dir, HlsMode.EVENT), video());
        mp.pushVideo(syntheticH264Idr(), 0L, true);
        mp.pushVideo(syntheticH264Idr(), 45_000L, false);
        mp.pushVideo(syntheticH264Idr(), 108_000L, true); // past the 1 s target: cuts 0..108000 = 1.200 s
        HlsPublisher hls = mp.finishIntoPublisher();
        String pl = hls.renderPlaylist(false);
        hls.finish();
        assertTrue(pl.contains("#EXTINF:1.200,"), pl);
    }

    @Test
    void withConfigHlsOnConsumedPublisherThrows(@TempDir Path dir) throws Exception {
        HlsPublisher source = pub(dir, HlsMode.LIVE);
        source.finish();
        assertThrows(IllegalStateException.class, () -> MuxPublisher.withConfigHls(source, video()));
    }

    @Test
    void badConfigLeavesPublisherUsable(@TempDir Path dir) throws Exception {
        HlsPublisher source = pub(dir, HlsMode.LIVE);
        MuxerConfig bad = MuxerConfig.builder().programNumber(0).pmtPid(0x100)
            .addVideo(0x101, VideoCodec.H264).build();
        assertThrows(MuxException.class, () -> MuxPublisher.withConfigHls(source, bad));
        assertNotNull(source.stats(), "a rejected config must not consume the publisher");
        source.finish();
        assertTrue(Files.exists(dir.resolve("playlist.m3u8")));
    }

    @Test
    void closeWithoutFinishIsQuiet(@TempDir Path dir) throws Exception {
        MuxPublisher mp = MuxPublisher.withConfigHls(pub(dir, HlsMode.LIVE), video());
        mp.pushVideo(syntheticH264Idr(), 0L, true);
        mp.close();
        mp.close();
        assertTrue(Files.exists(dir.resolve("playlist.m3u8")), "close() finishes shell + publisher");
    }

    @Test
    @Timeout(60)
    void withConfigHlsRacingPushNeverCrashes(@TempDir Path dir) throws Exception {
        HlsPublisher source = pub(dir, HlsMode.LIVE);
        CountDownLatch started = new CountDownLatch(1);
        AtomicReference<Throwable> failed = new AtomicReference<>();
        AtomicBoolean sawClosed = new AtomicBoolean(false);
        byte[] chunk = new byte[188 * 64];
        for (int i = 0; i < 64; i++) chunk[i * 188] = 0x47;
        Thread t = new Thread(() -> {
            try {
                while (true) { source.pushTs(chunk); started.countDown(); }
            } catch (IllegalStateException consumed) {
                sawClosed.set(true);
                started.countDown();
            } catch (Throwable t2) {
                failed.set(t2); started.countDown();
            }
        }, "race-pusher");
        t.setDaemon(true);
        t.start();
        assertTrue(started.await(20, TimeUnit.SECONDS));
        MuxPublisher mp = MuxPublisher.withConfigHls(source, video());
        t.join(20_000);
        assertFalse(t.isAlive());
        assertNull(failed.get(), String.valueOf(failed.get()));
        assertTrue(sawClosed.get(), "pusher must observe the consume");
        mp.finishIntoPublisher().finish();
    }
}
