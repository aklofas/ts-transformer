package org.tstrans.hls;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReference;
import java.util.stream.Stream;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.io.TempDir;
import org.tstrans.HlsException;
import org.tstrans.mpegts.KlvStreamType;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

class MuxPublisherTest {
    /** ST 0601 UL + tiny body; asserted verbatim inside a segment. */
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
        for (int i = 0; i < 3; i++) mp.sendVideo(syntheticH264Idr(), i * 90_000L, true);
        MuxPublisherStats ms = mp.stats();
        assertTrue(ms.bytesPushed() > 0);
        assertTrue(ms.cutCalls() >= 2, "3 keyframes → ≥ 2 cuts");
        assertNotNull(mp.publisherStats());
        HlsPublisher recovered = mp.finishIntoPublisher();
        recovered.finish();
        assertTrue(Files.exists(dir.resolve("playlist.m3u8")));
        assertThrows(IllegalStateException.class, mp::finishIntoPublisher);
        assertThrows(IllegalStateException.class, () -> mp.sendVideo(syntheticH264Idr(), 0, true));
        mp.close(); // quiet after consume
    }

    @Test
    void klvPreservedInSegment(@TempDir Path dir) throws Exception {
        MuxPublisher mp = MuxPublisher.withConfigHls(pub(dir, HlsMode.LIVE), videoKlv());
        mp.sendKlv(KLV, 0L, 0);
        mp.sendVideo(syntheticH264Idr(), 90_000L, true);
        mp.cutSegment();
        mp.finishIntoPublisher().finish();
        byte[] seg = Files.readAllBytes(firstSegment(dir));
        assertTrue(indexOf(seg, KLV) >= 0, "KLV bytes not found in segment");
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
        mp.sendVideo(syntheticH264Idr(), 0L, true);
        mp.sendVideo(syntheticH264Idr(), 90_000L, false);
        mp.sendVideo(syntheticH264Idr(), 261_000L, true); // cuts 0..261000 = 2.900 s
        HlsPublisher hls = mp.finishIntoPublisher();
        String pl = hls.renderPlaylist(false);
        hls.finish();
        assertTrue(pl.contains("#EXTINF:2.900,"), pl);
    }

    @Test
    void withConfigHlsOnConsumedPublisherThrows(@TempDir Path dir) throws Exception {
        HlsPublisher source = pub(dir, HlsMode.LIVE);
        source.finish();
        assertThrows(IllegalStateException.class, () -> MuxPublisher.withConfigHls(source, video()));
    }

    @Test
    void closeWithoutFinishIsQuiet(@TempDir Path dir) throws Exception {
        MuxPublisher mp = MuxPublisher.withConfigHls(pub(dir, HlsMode.LIVE), video());
        mp.sendVideo(syntheticH264Idr(), 0L, true);
        mp.close();
        mp.close();
        assertTrue(Files.exists(dir.resolve("playlist.m3u8")), "close() finishes shell + publisher");
    }

    @Test
    @Timeout(60)
    void withConfigHlsRacingPushNeverCrashes(@TempDir Path dir) throws Exception {
        HlsPublisher source = pub(dir, HlsMode.LIVE);
        AtomicBoolean stop = new AtomicBoolean(false);
        CountDownLatch started = new CountDownLatch(1);
        AtomicReference<Throwable> failed = new AtomicReference<>();
        byte[] chunk = new byte[188 * 64];
        for (int i = 0; i < 64; i++) chunk[i * 188] = 0x47;
        Thread t = new Thread(() -> {
            try {
                while (!stop.get()) { source.pushTs(chunk); started.countDown(); }
            } catch (IllegalStateException consumed) {
                started.countDown();
            } catch (Throwable t2) {
                failed.set(t2); started.countDown();
            }
        }, "race-pusher");
        t.setDaemon(true);
        t.start();
        assertTrue(started.await(20, TimeUnit.SECONDS));
        MuxPublisher mp = MuxPublisher.withConfigHls(source, video());
        stop.set(true);
        t.join(20_000);
        assertFalse(t.isAlive());
        assertNull(failed.get(), String.valueOf(failed.get()));
        mp.finishIntoPublisher().finish();
    }
}
