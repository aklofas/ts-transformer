package org.tstrans.mpegts;

import static org.tstrans.TestSupport.drain;
import static org.tstrans.TestSupport.syntheticAdtsFrame;
import static org.tstrans.TestSupport.syntheticH264Idr;

import org.tstrans.MuxException;
import org.tstrans.codec.MispTimestamp;

/**
 * The mux recipes of the shared {@code roundtrip} scenarios, one per scenario id, each a
 * port of its Rust original in {@code crates/tst-integration/src/scenarios/mod.rs}. The
 * muxer is integer-only and deterministic, so each recipe's bytes equal the committed
 * {@code output.ts} of its scenario.
 */
public final class ScenarioMuxRecipes {
    private ScenarioMuxRecipes() {}

    /** {@code video_roundtrip_ts_bytes()}: one H.264 IDR at PTS 0. */
    public static byte[] videoRoundtrip() throws MuxException {
        try (Muxer m = new Muxer(videoOnly())) {
            m.pushVideo(syntheticH264Idr(), /*pts=*/ 0L, /*keyFrame=*/ true);
            return drain(m);
        }
    }

    /** {@code video_dts_roundtrip_ts_bytes()}: one IDR at PTS 9000 / DTS 6000. */
    public static byte[] videoDtsRoundtrip() throws MuxException {
        try (Muxer m = new Muxer(videoOnly())) {
            VideoStreamHandle h = m.videoStreamHandle(0)
                .orElseThrow(() -> new IllegalStateException("no video handle at index 0"));
            m.pushVideoToWithDts(h, syntheticH264Idr(), /*pts=*/ 9000L, /*dts=*/ 6000L,
                                 /*keyFrame=*/ true);
            return drain(m);
        }
    }

    /** {@code video_misp_roundtrip_ts_bytes()}: one IDR at PTS 9000 carrying a MISP timestamp. */
    public static byte[] videoMispRoundtrip() throws MuxException {
        MispTimestamp misp = MispTimestamp.micros(0x0005_F5E1_0000_0001L, 0x1F);
        try (Muxer m = new Muxer(videoOnly())) {
            VideoStreamHandle h = m.videoStreamHandle(0)
                .orElseThrow(() -> new IllegalStateException("no video handle at index 0"));
            m.pushVideoMispTo(h, syntheticH264Idr(), /*pts=*/ 9000L, /*keyFrame=*/ true, misp);
            return drain(m);
        }
    }

    /** {@code audio_klv_roundtrip_ts_bytes()}: H.264 + AAC + synchronous KLV, all at PTS 0. */
    public static byte[] audioKlvRoundtrip() throws MuxException {
        MuxerConfig cfg = MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264)
            .addAudio(0x1021, AudioCodec.AAC)
            .addKlv(0x1031, KlvStreamType.SYNCHRONOUS_METADATA, /*carriesPts=*/ true)
            .build();
        try (Muxer m = new Muxer(cfg)) {
            m.pushVideo(syntheticH264Idr(), 0L, true);
            m.pushAudio(syntheticAdtsFrame(), 0L);
            // Raw LS bytes: the muxer adds the AU cell header.
            m.pushKlv(minimalSt0601Ls(), 0L, /*metadataServiceId=*/ 0);
            return drain(m);
        }
    }

    /** {@code minimal_st0601_ls()}: the 16-byte ST 0601 UAS Datalink LS UL and a zero BER length. */
    public static byte[] minimalSt0601Ls() {
        return new byte[]{
            0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01,
            0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
            0x00,
        };
    }

    private static MuxerConfig videoOnly() {
        return MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264)
            .build();
    }
}
