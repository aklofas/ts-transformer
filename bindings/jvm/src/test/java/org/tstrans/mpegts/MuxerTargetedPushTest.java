package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.drain;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.util.ArrayList;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.tstrans.MuxException;

/**
 * Exercises the targeted {@code push*To} family on the offline {@link Muxer}:
 * routing to a specific stream and forged-handle rejection.
 */
class MuxerTargetedPushTest {

    /**
     * Synthetic ADTS frame (mirrors tst-integration's {@code synthetic_adts_frame()}):
     * AAC-LC 44100 Hz stereo, frame_length=15 (7-byte header + 8 payload bytes).
     */
    private static byte[] syntheticAdtsFrame() {
        return new byte[]{
            (byte) 0xFF, (byte) 0xF1, 0x50, (byte) 0x80, 0x01, (byte) 0xFF, (byte) 0xFC,
            (byte) 0xA0, (byte) 0xA1, (byte) 0xA2, (byte) 0xA3,
            (byte) 0xA4, (byte) 0xA5, (byte) 0xA6, (byte) 0xA7
        };
    }

    // -------------------------------------------------------------------------
    // Routing assertions — targeted push lands on the right PID.
    // -------------------------------------------------------------------------

    @Test
    void pushVideoToRoutesToTargetedPid() throws Exception {
        // Two video streams on PIDs 0x100 and 0x101. Push to the second (index 1).
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .addVideo(0x101, VideoCodec.H264)
            .build();
        List<DemuxEvent.Video> videos = new ArrayList<>();
        try (Muxer m = new Muxer(cfg)) {
            VideoStreamHandle h = m.videoStreamHandle(1).orElseThrow();
            m.pushVideoTo(h, syntheticH264Idr(), 9000L, true);
            byte[] ts = drain(m);
            try (Demuxer d = new Demuxer()) {
                d.feed(ts);
                d.flush();
                for (DemuxEvent ev : d) {
                    if (ev instanceof DemuxEvent.Video v) videos.add(v);
                }
            }
        }
        assertEquals(1, videos.size(), "expected exactly one video event");
        assertEquals(0x101, videos.get(0).stream().pid(), "AU must land on PID 0x101");
    }

    @Test
    void pushVideoWireToRoutesToTargetedPid() throws Exception {
        // Wire-push variant: Annex-B IDR bytes are valid wire for H.264 (no re-wrap).
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .addVideo(0x101, VideoCodec.H264)
            .build();
        List<DemuxEvent.Video> videos = new ArrayList<>();
        try (Muxer m = new Muxer(cfg)) {
            VideoStreamHandle h = m.videoStreamHandle(1).orElseThrow();
            m.pushVideoWireTo(h, syntheticH264Idr(), 9000L, true);
            byte[] ts = drain(m);
            try (Demuxer d = new Demuxer()) {
                d.feed(ts);
                d.flush();
                for (DemuxEvent ev : d) {
                    if (ev instanceof DemuxEvent.Video v) videos.add(v);
                }
            }
        }
        assertEquals(1, videos.size());
        assertEquals(0x101, videos.get(0).stream().pid(), "wire push must land on PID 0x101");
    }

    @Test
    void pushKlvToRoutesToTargetedPid() throws Exception {
        // Two KLV streams on 0x120 and 0x121. Push to the second (index 1).
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .addKlv(0x120, KlvStreamType.SYNCHRONOUS_METADATA, true)
            .addKlv(0x121, KlvStreamType.SYNCHRONOUS_METADATA, true)
            .build();
        // Minimal raw KLV bytes (tag 1, length 1, value 0xFF).
        byte[] klv = {(byte) 0x01, (byte) 0x01, (byte) 0xFF};
        List<DemuxEvent> events = new ArrayList<>();
        try (Muxer m = new Muxer(cfg)) {
            KlvStreamHandle h = m.klvStreamHandle(1).orElseThrow();
            m.pushVideo(syntheticH264Idr(), 0L, true);  // PCR-eligible
            m.pushKlvTo(h, klv, 9000L, 0);
            byte[] ts = drain(m);
            try (Demuxer d = new Demuxer()) {
                d.feed(ts);
                d.flush();
                for (DemuxEvent ev : d) events.add(ev);
            }
        }
        boolean found = events.stream()
            .anyMatch(ev -> ev instanceof DemuxEvent.Metadata me
                           && me.stream().pid() == 0x121);
        assertTrue(found, "KLV push must land on PID 0x121");
    }

    @Test
    void pushAudioToRoutesToTargetedPid() throws Exception {
        // Two audio streams on 0x110 and 0x111. Push to the second (index 1).
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .addAudio(0x110, AudioCodec.AAC)
            .addAudio(0x111, AudioCodec.AAC)
            .build();
        List<DemuxEvent.Audio> audios = new ArrayList<>();
        try (Muxer m = new Muxer(cfg)) {
            AudioStreamHandle h = m.audioStreamHandle(1).orElseThrow();
            m.pushVideo(syntheticH264Idr(), 0L, true);  // PCR-eligible
            m.pushAudioTo(h, syntheticAdtsFrame(), 9000L);
            byte[] ts = drain(m);
            try (Demuxer d = new Demuxer()) {
                d.feed(ts);
                d.flush();
                for (DemuxEvent ev : d) {
                    if (ev instanceof DemuxEvent.Audio a) audios.add(a);
                }
            }
        }
        assertEquals(1, audios.size(), "expected exactly one audio event");
        assertEquals(0x111, audios.get(0).stream().pid(), "audio push must land on PID 0x111");
    }

    @Test
    void pushSubtitleToRoutesToTargetedPid() throws Exception {
        // Two subtitle streams on 0x130 and 0x131. Push to the second (index 1).
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .addSubtitle(0x130, SubtitleCodec.CEA708_STANDALONE)
            .addSubtitle(0x131, SubtitleCodec.CEA708_STANDALONE)
            .build();
        byte[] payload = {0x01, 0x02, 0x03};
        List<DemuxEvent.Subtitle> subs = new ArrayList<>();
        try (Muxer m = new Muxer(cfg)) {
            SubtitleStreamHandle h = m.subtitleStreamHandle(1).orElseThrow();
            m.pushVideo(syntheticH264Idr(), 0L, true);  // PCR-eligible
            m.pushSubtitleTo(h, 9000L, payload);
            byte[] ts = drain(m);
            try (Demuxer d = new Demuxer()) {
                d.feed(ts);
                d.flush();
                for (DemuxEvent ev : d) {
                    if (ev instanceof DemuxEvent.Subtitle s) subs.add(s);
                }
            }
        }
        assertEquals(1, subs.size(), "expected exactly one subtitle event");
        assertEquals(0x131, subs.get(0).stream().pid(), "subtitle push must land on PID 0x131");
    }

    // -------------------------------------------------------------------------
    // Forged-handle rejection — INVALID_USAGE for all four stream kinds.
    // -------------------------------------------------------------------------

    @Test
    void forgedVideoHandleThrowsInvalidUsage() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .build();
        try (Muxer m = new Muxer(cfg)) {
            // 0x1000L has bits above the packed-u8 layout → try_from_raw rejects it.
            VideoStreamHandle forged = VideoStreamHandle.fromRaw(0x1000L);
            MuxException ex = assertThrows(MuxException.class,
                () -> m.pushVideoTo(forged, syntheticH264Idr(), 9000L, true));
            assertEquals(MuxException.Kind.INVALID_USAGE, ex.kind());
        }
    }

    @Test
    void forgedKlvHandleThrowsInvalidUsage() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .addKlv(0x120, KlvStreamType.SYNCHRONOUS_METADATA, true)
            .build();
        try (Muxer m = new Muxer(cfg)) {
            KlvStreamHandle forged = KlvStreamHandle.fromRaw(0x1000L);
            MuxException ex = assertThrows(MuxException.class,
                () -> m.pushKlvTo(forged, new byte[]{0x01}, 9000L, 0));
            assertEquals(MuxException.Kind.INVALID_USAGE, ex.kind());
        }
    }

    @Test
    void forgedAudioHandleThrowsInvalidUsage() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .addAudio(0x110, AudioCodec.AAC)
            .build();
        try (Muxer m = new Muxer(cfg)) {
            AudioStreamHandle forged = AudioStreamHandle.fromRaw(0x1000L);
            MuxException ex = assertThrows(MuxException.class,
                () -> m.pushAudioTo(forged, new byte[]{0x01}, 9000L));
            assertEquals(MuxException.Kind.INVALID_USAGE, ex.kind());
        }
    }

    @Test
    void forgedSubtitleHandleThrowsInvalidUsage() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .addSubtitle(0x130, SubtitleCodec.CEA708_STANDALONE)
            .build();
        try (Muxer m = new Muxer(cfg)) {
            SubtitleStreamHandle forged = SubtitleStreamHandle.fromRaw(0x1000L);
            MuxException ex = assertThrows(MuxException.class,
                () -> m.pushSubtitleTo(forged, 9000L, new byte[]{0x01}));
            assertEquals(MuxException.Kind.INVALID_USAGE, ex.kind());
        }
    }
}
