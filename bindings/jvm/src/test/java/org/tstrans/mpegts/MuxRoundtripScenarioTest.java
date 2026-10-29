package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.scenarioDir;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.tstrans.GoldenSupport;
import org.tstrans.codec.MispTimeKind;
import org.tstrans.codec.MispTimestamp;

/**
 * Cross-binding byte-exactness proofs for the offline {@link Muxer}.
 *
 * <p>Each test replicates one of the shared mux recipes (single source of truth =
 * {@code crates/tst-integration/src/scenarios/mod.rs}) and asserts the SHA-256
 * of the JNI muxer's TS output equals the committed golden's
 * {@code extensions.output_sha256}. The muxer is integer-only/deterministic, so
 * the digest matches the Rust and C bindings byte-for-byte.
 *
 * <ul>
 *   <li>{@code video-roundtrip} — single H.264 IDR at PTS=0, PTS-only PES header.</li>
 *   <li>{@code video-dts-roundtrip} — single H.264 IDR at PTS=9000/DTS=6000 via
 *       {@link Muxer#pushVideoToWithDts}, emitting a PES header with both PTS and
 *       DTS fields.</li>
 * </ul>
 */
class MuxRoundtripScenarioTest {


    // ── video-roundtrip ─────────────────────────────────────────────────────

    @Test
    void reproducesVideoRoundtripBytesExactly() throws Exception {
        Path goldenPath = scenarioDir("video-roundtrip").resolve("golden.json");
        assertTrue(Files.isRegularFile(goldenPath),
            "shared golden missing (expected committed fixture): " + goldenPath);
        String goldenJson = Files.readString(goldenPath, StandardCharsets.UTF_8);
        // Sanity backstop: this is the committed golden digest
        // (2aa000852931462b875ec9b2548dd8bf5846fef36e5a1084d149cdd636d09a24).
        String expectedSha = GoldenSupport.stringField(goldenJson, "output_sha256");

        byte[] tsOut = videoRoundtripMuxAndDrain();
        assertEquals(expectedSha, sha256Hex(tsOut),
            "JNI muxer output must be byte-identical to the Rust/C video-roundtrip golden");
    }

    /** Replicate {@code video_roundtrip_ts_bytes()}: config + one pushVideo + drain. */
    private static byte[] videoRoundtripMuxAndDrain() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264)
            .build();
        ByteArrayOutputStream acc = new ByteArrayOutputStream();
        byte[] out = new byte[8192];
        try (Muxer m = new Muxer(cfg)) {
            m.pushVideo(syntheticH264Idr(), /*pts=*/ 0L, /*keyFrame=*/ true);
            int n;
            while ((n = m.pull(out)) > 0) {
                acc.write(out, 0, n);
            }
        }
        return acc.toByteArray();
    }

    // ── video-dts-roundtrip ───────────────────────────────────────

    /**
     * Distinct PTS and DTS survive identically
     * across core, C, and JVM.
     *
     * <p>Replicates {@code video_dts_roundtrip_ts_bytes()} from
     * {@code crates/tst-integration/src/scenarios/mod.rs}: a single-video-stream
     * muxer pushes one synthetic H.264 IDR at PTS=9000 / DTS=6000 (90 kHz ticks)
     * via {@link Muxer#pushVideoToWithDts}.  The muxer emits a PES header with
     * {@code PTS_DTS_flags='11'} (both timestamps present), producing bytes that
     * are identical to the Rust and C re-muxes and match the committed
     * {@code video-dts-roundtrip/golden.json} SHA-256.
     */
    @Test
    void reproducesVideoDtsRoundtripBytesExactly() throws Exception {
        Path goldenPath = scenarioDir("video-dts-roundtrip").resolve("golden.json");
        assertTrue(Files.isRegularFile(goldenPath),
            "video-dts-roundtrip golden missing (expected committed fixture): " + goldenPath);
        String goldenJson = Files.readString(goldenPath, StandardCharsets.UTF_8);
        String expectedSha = GoldenSupport.stringField(goldenJson, "output_sha256");

        byte[] tsOut = videoDtsRoundtripMuxAndDrain();
        assertEquals(expectedSha, sha256Hex(tsOut),
            "JNI muxer DTS output must be byte-identical to the Rust/C video-dts-roundtrip golden");
    }

    /**
     * Replicate {@code video_dts_roundtrip_ts_bytes()}: config + videoStreamHandle(0)
     * + pushVideoToWithDts(handle, au, pts=9000, dts=6000, keyFrame=true) + drain.
     */
    private static byte[] videoDtsRoundtripMuxAndDrain() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264)
            .build();
        ByteArrayOutputStream acc = new ByteArrayOutputStream();
        byte[] out = new byte[8192];
        try (Muxer m = new Muxer(cfg)) {
            VideoStreamHandle h = m.videoStreamHandle(0)
                .orElseThrow(() -> new IllegalStateException("no video handle at index 0"));
            // PTS=9000 / DTS=6000 ticks (90 kHz) — fixed so golden is stable.
            m.pushVideoToWithDts(h, syntheticH264Idr(), /*pts=*/ 9000L, /*dts=*/ 6000L,
                                 /*keyFrame=*/ true);
            int n;
            while ((n = m.pull(out)) > 0) {
                acc.write(out, 0, n);
            }
        }
        return acc.toByteArray();
    }

    // ── video-misp-roundtrip ────────────────────────────────────────────────

    /**
     * Cross-binding acceptance criterion for the ST 0604 MISP timestamp mux path.
     *
     * <p>Replicates {@code video_misp_roundtrip_ts_bytes()} from
     * {@code crates/tst-integration/src/scenarios/mod.rs}: a single-video-stream
     * muxer pushes one synthetic H.264 IDR at PTS=9000 (90 kHz ticks) via
     * {@link Muxer#pushVideoMispTo} with
     * {@code MispTimestamp.micros(0x0005_F5E1_0000_0001L, 0x1F)}.
     *
     * <p>The test:
     * <ol>
     *   <li>Asserts the SHA-256 of the JNI muxer's TS output equals the committed
     *       golden's {@code extensions.output_sha256}.</li>
     *   <li>Demuxes the produced bytes, finds the first Video event, extracts the
     *       MISP timestamp, and asserts it matches the golden's
     *       {@code misp_kind} / {@code misp_time_status} / {@code misp_value}.</li>
     * </ol>
     */
    @Test
    void reproducesVideoMispRoundtripBytesExactly() throws Exception {
        Path goldenPath = scenarioDir("video-misp-roundtrip").resolve("golden.json");
        assertTrue(Files.isRegularFile(goldenPath),
            "video-misp-roundtrip golden missing (expected committed fixture): " + goldenPath);
        String goldenJson = Files.readString(goldenPath, StandardCharsets.UTF_8);

        String expectedSha = GoldenSupport.stringField(goldenJson, "output_sha256");
        long goldenMispKind    = GoldenSupport.unsignedNumber(goldenJson, "misp_kind");
        long goldenTimeStatus  = GoldenSupport.unsignedNumber(goldenJson, "misp_time_status");
        long goldenMispValue   = GoldenSupport.unsignedNumber(goldenJson, "misp_value");

        byte[] tsOut = videoMispRoundtripMuxAndDrain();

        // 1. SHA-256 byte-identity parity.
        assertEquals(expectedSha, sha256Hex(tsOut),
            "JNI muxer MISP output must be byte-identical to the committed golden");

        // 2. MISP extract equality: demux, find the video AU, extract and compare.
        List<DemuxEvent.Video> videos = new ArrayList<>();
        try (Demuxer d = new Demuxer()) {
            d.feed(tsOut);
            d.flush();
            for (DemuxEvent ev : d) {
                if (ev instanceof DemuxEvent.Video v) videos.add(v);
            }
        }
        assertFalse(videos.isEmpty(), "video-misp-roundtrip: no video event after demux");
        DemuxEvent.Video v = videos.get(0);

        byte[] rawBytes = new byte[v.raw().remaining()];
        v.raw().duplicate().get(rawBytes);

        MispTimestamp extracted = MispTimestamp.extract(rawBytes, VideoCodec.H264);
        assertNotNull(extracted, "MISP SEI must be present in the demuxed AU");

        MispTimeKind expectedKind = (goldenMispKind == 0) ? MispTimeKind.MICRO : MispTimeKind.NANO;
        assertEquals(expectedKind, extracted.kind(), "misp kind mismatch");
        assertEquals((int) goldenTimeStatus, extracted.timeStatus(), "misp time_status mismatch");
        assertEquals(goldenMispValue, extracted.value(), "misp value mismatch (unsigned 64-bit)");
    }

    /**
     * Replicate {@code video_misp_roundtrip_ts_bytes()}: config + videoStreamHandle(0)
     * + pushVideoMispTo(handle, au, pts=9000, keyFrame=true,
     *   MispTimestamp.micros(0x0005_F5E1_0000_0001L, 0x1F)) + drain.
     */
    private static byte[] videoMispRoundtripMuxAndDrain() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264)
            .build();
        MispTimestamp misp = MispTimestamp.micros(0x0005_F5E1_0000_0001L, 0x1F);
        ByteArrayOutputStream acc = new ByteArrayOutputStream();
        byte[] out = new byte[8192];
        try (Muxer m = new Muxer(cfg)) {
            VideoStreamHandle h = m.videoStreamHandle(0)
                .orElseThrow(() -> new IllegalStateException("no video handle at index 0"));
            // PTS=9000 ticks (90 kHz) — fixed so golden is stable.
            m.pushVideoMispTo(h, syntheticH264Idr(), /*pts=*/ 9000L, /*keyFrame=*/ true, misp);
            int n;
            while ((n = m.pull(out)) > 0) {
                acc.write(out, 0, n);
            }
        }
        return acc.toByteArray();
    }

    // ── Shared helpers ───────────────────────────────────────────────────────

    private static String sha256Hex(byte[] bytes) throws Exception {
        byte[] digest = MessageDigest.getInstance("SHA-256").digest(bytes);
        StringBuilder sb = new StringBuilder(digest.length * 2);
        for (byte b : digest) {
            sb.append(Character.forDigit((b >> 4) & 0xF, 16));
            sb.append(Character.forDigit(b & 0xF, 16));
        }
        return sb.toString();
    }
}
