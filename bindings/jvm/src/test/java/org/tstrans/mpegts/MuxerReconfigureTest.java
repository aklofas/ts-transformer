package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.tstrans.TestSupport.drain;

import java.io.ByteArrayOutputStream;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.function.Supplier;
import org.junit.jupiter.api.Test;
import org.tstrans.MuxException;

/**
 * {@link Muxer#reconfigure}: the running muxer takes a new program. Survivors keep
 * their handles and continuity counters, new streams get handles, a removed stream's
 * handle is refused, and a refused config changes nothing.
 */
class MuxerReconfigureTest {
    // An IDR slice NAL long enough to span several TS packets, so a counter that
    // restarted would show as a continuity jump.
    private static final byte[] NAL_IDR = nalIdr();
    private static final byte[] KLV_LS = {
        0x06, 0x0e, 0x2b, 0x34, 0x02, 0x0b, 0x01, 0x01,
        0x0e, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
        0x03, 0x02, 0x01, 0x02,
    };

    private static byte[] nalIdr() {
        byte[] b = new byte[5 + 1024];
        b[3] = 1;
        b[4] = 0x65;
        for (int i = 0; i < 1024; i++) b[5 + i] = (byte) i;
        return b;
    }

    private static MuxerConfig video(int program, int pmtPid, int videoPid, VideoCodec codec) {
        return MuxerConfig.builder().programNumber(program).pmtPid(pmtPid)
            .addVideo(videoPid, codec).build();
    }

    private static MuxerConfig video(int pmtPid, int videoPid) {
        return video(1, pmtPid, videoPid, VideoCodec.H264);
    }

    private static MuxerConfig videoKlv(int pmtPid, int videoPid, int klvPid) {
        return MuxerConfig.builder().programNumber(1).pmtPid(pmtPid)
            .addVideo(videoPid, VideoCodec.H264)
            .addKlv(klvPid, KlvStreamType.SYNCHRONOUS_METADATA, true).build();
    }

    private static List<DemuxEvent> events(byte[] ts) throws Exception {
        List<DemuxEvent> out = new ArrayList<>();
        try (Demuxer d = new Demuxer()) {
            d.feed(ts);
            d.flush();
            for (DemuxEvent e : d) out.add(e);
        }
        return out;
    }

    private static byte[] pushFrames(Muxer m, VideoStreamHandle v, long firstPts, int count)
            throws MuxException {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        for (int i = 0; i < count; i++) {
            m.pushVideoTo(v, NAL_IDR, firstPts + i * 3000L, i == 0);
            out.writeBytes(drain(m));
        }
        return out.toByteArray();
    }

    private static byte[] bytes(ByteBuffer b) {
        byte[] out = new byte[b.remaining()];
        b.duplicate().get(out);
        return out;
    }

    @Test
    void aSurvivingStreamKeepsContinuityAcrossReconfigure() throws Exception {
        try (Muxer m = new Muxer(video(0x100, 0x101))) {
            VideoStreamHandle v = m.videoHandles().get(0);
            ByteArrayOutputStream ts = new ByteArrayOutputStream();
            ts.writeBytes(pushFrames(m, v, 90_000, 5));
            // The program gains a KLV stream; the video survives.
            m.reconfigure(videoKlv(0x100, 0x101, 0x102));
            assertEquals(List.of(v), m.videoHandles());
            ts.writeBytes(pushFrames(m, v, 90_000 + 5 * 3000, 5));

            List<DemuxEvent> events = events(ts.toByteArray());
            long jumps = events.stream().filter(e -> e instanceof DemuxEvent.Discontinuity d
                && d.stream().pid() == 0x101).count();
            assertEquals(0, jumps);
            List<DemuxEvent.Video> videos = new ArrayList<>();
            for (DemuxEvent e : events) {
                if (e instanceof DemuxEvent.Video vid && vid.stream().pid() == 0x101) videos.add(vid);
            }
            assertEquals(10, videos.size());
            for (DemuxEvent.Video vid : videos) {
                byte[] raw = bytes(vid.raw());
                byte[] tail = java.util.Arrays.copyOfRange(raw, raw.length - 1024, raw.length);
                assertArrayEquals(java.util.Arrays.copyOfRange(NAL_IDR, 5, NAL_IDR.length), tail);
            }
        }
    }

    @Test
    void anAddedStreamGetsAHandle() throws Exception {
        try (Muxer m = new Muxer(video(0x100, 0x101))) {
            assertEquals(List.of(), m.klvHandles());
            m.reconfigure(videoKlv(0x100, 0x101, 0x102));
            KlvStreamHandle k = m.klvHandles().get(0);
            VideoStreamHandle v = m.videoHandles().get(0);
            m.pushVideoTo(v, NAL_IDR, 90_000, true);
            m.pushKlvTo(k, KLV_LS, 90_000, 0);
            m.pushVideoTo(v, NAL_IDR, 93_000, false);
            List<byte[]> metadata = new ArrayList<>();
            for (DemuxEvent e : events(drain(m))) {
                if (e instanceof DemuxEvent.Metadata md && md.stream().pid() == 0x102) {
                    metadata.add(bytes(md.payload()));
                }
            }
            assertEquals(1, metadata.size());
            assertArrayEquals(KLV_LS, metadata.get(0));
        }
    }

    @Test
    void aRemovedStreamsHandleRaisesInvalidUsage() throws Exception {
        try (Muxer m = new Muxer(videoKlv(0x100, 0x101, 0x102))) {
            KlvStreamHandle gone = m.klvHandles().get(0);
            VideoStreamHandle kept = m.videoHandles().get(0);
            m.reconfigure(video(0x100, 0x101));
            MuxException e = assertThrows(MuxException.class,
                () -> m.pushKlvTo(gone, KLV_LS, 90_000, 0));
            assertEquals(MuxException.Kind.INVALID_USAGE, e.kind());
            assertEquals(List.of(), m.klvHandles());
            m.pushVideoTo(kept, NAL_IDR, 90_000, true);
        }
    }

    @Test
    void aRemovedStreamsHandleWorksAgainWhenItsPidComesBack() throws Exception {
        try (Muxer m = new Muxer(videoKlv(0x100, 0x101, 0x102))) {
            KlvStreamHandle old = m.klvHandles().get(0);
            VideoStreamHandle v = m.videoHandles().get(0);
            m.reconfigure(video(0x100, 0x101));
            assertThrows(MuxException.class, () -> m.pushKlvTo(old, KLV_LS, 90_000, 0));
            // The KLV stream comes back on its PID: it takes its old slot, so the old
            // handle names it again.
            m.reconfigure(videoKlv(0x100, 0x101, 0x102));
            assertEquals(List.of(old), m.klvHandles());
            m.pushVideoTo(v, NAL_IDR, 90_000, true);
            m.pushKlvTo(old, KLV_LS, 90_000, 0);
            m.pushVideoTo(v, NAL_IDR, 93_000, false);
            List<byte[]> metadata = new ArrayList<>();
            for (DemuxEvent e : events(drain(m))) {
                if (e instanceof DemuxEvent.Metadata md && md.stream().pid() == 0x102) {
                    metadata.add(bytes(md.payload()));
                }
            }
            assertEquals(1, metadata.size());
            assertArrayEquals(KLV_LS, metadata.get(0));
        }
    }

    @Test
    void aRefusedConfigRaisesConfigInvalidAndChangesNothing() throws Exception {
        Map<String, Supplier<MuxerConfig>> refused = Map.of(
            // The PMT PID of the program moved.
            "pmt-pid-moved", () -> video(0x110, 0x101),
            // The same PID now carries another codec.
            "codec-changed", () -> video(1, 0x100, 0x101, VideoCodec.H265),
            // The PID moved to another program (a new program number is a new program).
            "pid-moved-program", () -> video(2, 0x100, 0x101, VideoCodec.H264),
            // A muxer-wide setting differs from the running one.
            "muxer-setting-changed", () -> MuxerConfig.builder().programNumber(1).pmtPid(0x100)
                .addVideo(0x101, VideoCodec.H264).pcrIntervalMs(20).build());
        for (Map.Entry<String, Supplier<MuxerConfig>> c : refused.entrySet()) {
            MuxerConfig start = video(0x100, 0x101);
            try (Muxer m = new Muxer(start); Muxer twin = new Muxer(start)) {
                VideoStreamHandle v = m.videoHandles().get(0);
                VideoStreamHandle tv = twin.videoHandles().get(0);
                byte[] a = pushFrames(m, v, 90_000, 3);
                byte[] b = pushFrames(twin, tv, 90_000, 3);
                MuxException e = assertThrows(MuxException.class, () -> m.reconfigure(c.getValue().get()),
                    c.getKey());
                assertEquals(MuxException.Kind.CONFIG_INVALID, e.kind(), c.getKey());
                assertEquals(List.of(v), m.videoHandles(), c.getKey());
                // The output after the refusal is the twin's, which never made the call.
                assertArrayEquals(b, a, c.getKey());
                assertArrayEquals(pushFrames(twin, tv, 99_000, 3), pushFrames(m, v, 99_000, 3), c.getKey());
            }
        }
    }

    @Test
    void reconfigureOnAClosedMuxerOrWithNullIsRefused() throws Exception {
        Muxer m = new Muxer(video(0x100, 0x101));
        assertThrows(NullPointerException.class, () -> m.reconfigure(null));
        m.close();
        IllegalStateException e = assertThrows(IllegalStateException.class,
            () -> m.reconfigure(video(0x100, 0x101)));
        assertTrue(e.getMessage().contains("closed"), e.getMessage());
    }
}
