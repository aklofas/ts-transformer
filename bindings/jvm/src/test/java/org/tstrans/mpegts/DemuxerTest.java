package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.*;
import java.io.ByteArrayOutputStream;
import java.nio.ByteBuffer;
import java.nio.file.*;
import org.junit.jupiter.api.Test;

class DemuxerTest {
    private static final Path FIXTURE =
        Path.of(System.getProperty("user.dir"), "..", "..",
                "crates/tst-core/tests/fixtures/audio/mp2.ts").normalize();

    @Test
    void demuxesFixtureToEvents() throws Exception {
        byte[] ts = Files.readAllBytes(FIXTURE);
        int events = 0; boolean sawProgramMap = false;
        try (Demuxer d = new Demuxer()) {
            d.feed(ts);
            d.flush();
            for (DemuxEvent e : d) {
                events++;
                if (e instanceof DemuxEvent.ProgramMap pm) {
                    sawProgramMap = true;
                    assertFalse(pm.elementaryPids().isEmpty(), "expected >=1 elementary stream");
                    assertEquals(0x1000, pm.pmtPid(), "mp2.ts PAT declares PMT at 0x1000");
                }
                if (e instanceof DemuxEvent.Audio a) {
                    assertTrue(a.stream().pid() > 0);
                    // mp2.ts is a clean MP2 stream → raw ES bytes are present,
                    // and opt-in parse() yields the typed Mpeg2AudioFrame list.
                    assertNotNull(a.raw(), "audio events carry the raw encoded ES");
                    assertTrue(a.raw().remaining() > 0, "expected non-empty raw audio ES");
                    assertFalse(a.parse().isEmpty(), "clean MP2 parse() yields typed frames");
                }
            }
        }
        assertTrue(events > 0, "expected demux events");
        assertTrue(sawProgramMap, "expected a ProgramMap event");
    }

    @Test
    void samplePayloadIsRetainableHeapCopy() throws Exception {
        // mp2.ts yields DemuxEvent.Audio events; opt-in parse() gives typed
        // Mpeg2AudioFrames. Each frame's payload is a JVM-owned heap copy (not a
        // direct buffer over Rust memory), so it stays valid even after further
        // pulls and close().
        byte[] ts = Files.readAllBytes(FIXTURE);
        java.nio.ByteBuffer retained = null;
        byte[] snapshot = null;
        try (Demuxer d = new Demuxer()) {
            d.feed(ts);
            d.flush();
            for (DemuxEvent e : d) {
                if (e instanceof DemuxEvent.Audio a) {
                    java.util.List<org.tstrans.codec.AudioFrame> frames = a.parse();
                    if (frames.isEmpty()) {
                        continue;
                    }
                    org.tstrans.codec.Mpeg2AudioFrame frame =
                        (org.tstrans.codec.Mpeg2AudioFrame) frames.get(0);
                    retained = frame.payload();
                    assertFalse(retained.isDirect(),
                        "frame payload is a copied heap ByteBuffer, safe to retain");
                    assertTrue(retained.remaining() > 0, "expected non-empty frame payload");
                    snapshot = new byte[retained.remaining()];
                    retained.duplicate().get(snapshot);
                    break;
                }
            }
            // Drain the rest — this would clobber a zero-copy backing store, but
            // the heap copy is independent of demuxer state.
            for (DemuxEvent ignored : d) {
                // intentionally empty
            }
        }
        // Demuxer is now closed; the JVM-owned copy is still readable and intact.
        assertNotNull(retained, "expected at least one Audio frame from mp2.ts");
        byte[] afterClose = new byte[retained.remaining()];
        retained.duplicate().get(afterClose);
        assertArrayEquals(snapshot, afterClose,
            "retained heap payload must stay valid after further pulls and close()");
    }

    @Test
    void feedAfterCloseThrows() {
        Demuxer d = new Demuxer();
        d.close();
        assertThrows(IllegalStateException.class, () -> d.feed(new byte[] {0x47}));
    }

    @Test
    void videoRawIsExactEncodedAccessUnit() throws Exception {
        // Transmux property: DemuxEvent.Video.raw is the exact
        // encoded AU that was muxed — byte-for-byte — as a JVM-owned heap
        // copy (JDK < 22 forbids direct buffers over Rust memory).
        byte[] au = new byte[20];
        au[0] = 0x00; au[1] = 0x00; au[2] = 0x00; au[3] = 0x01;
        au[4] = 0x65; // IDR
        for (int i = 0; i < 15; i++) {
            au[5 + i] = (byte) (0xA5 ^ i);
        }
        MuxerConfig cfg = MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264)
            .build();
        ByteArrayOutputStream acc = new ByteArrayOutputStream();
        byte[] out = new byte[8192];
        try (Muxer m = new Muxer(cfg)) {
            m.pushVideo(au, /*pts=*/ 0L, /*keyFrame=*/ true);
            int n;
            while ((n = m.pull(out)) > 0) {
                acc.write(out, 0, n);
            }
        }
        DemuxEvent.Video video = null;
        try (Demuxer d = new Demuxer()) {
            d.feed(acc.toByteArray());
            d.flush();
            for (DemuxEvent e : d) {
                if (e instanceof DemuxEvent.Video v) {
                    video = v;
                    break;
                }
            }
        }
        assertNotNull(video, "expected a Video event");
        ByteBuffer raw = video.raw();
        assertNotNull(raw, "Video events carry the raw encoded AU");
        assertFalse(raw.isDirect(), "raw is a JVM-owned heap copy");
        byte[] got = new byte[raw.remaining()];
        raw.duplicate().get(got);
        // Demuxer is closed; the heap copy must still be intact (and equal
        // to the pushed AU — the byte-faithful transmux property).
        assertArrayEquals(au, got, "raw must be the exact pushed encoded AU");
        // Opt-in parse() yields the typed NAL units; verify non-empty.
        assertFalse(video.parse().isEmpty(), "parse() yields typed units from raw");
    }
}
