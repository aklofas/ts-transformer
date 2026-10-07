package org.tstrans.hls;

import static org.junit.jupiter.api.Assertions.*;

import java.io.IOException;
import java.net.ServerSocket;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.stream.Stream;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import org.tstrans.HlsException;

class HlsPublisherTest {
    /** 376 bytes = 2 × 188-byte TS packets (aligned). */
    static byte[] twoPackets() {
        byte[] b = new byte[376];
        b[0] = 0x47;
        b[188] = 0x47;
        return b;
    }

    static HlsPublisher open(Path dir) throws HlsException {
        return HlsPublisher.builder().bind("127.0.0.1:0").outputDir(dir.toString())
            .segmentDurationMs(1000).playlistWindow(3).mode(HlsMode.LIVE).build();
    }

    static long countTs(Path dir) throws IOException {
        try (Stream<Path> s = Files.list(dir)) {
            return s.filter(p -> p.toString().endsWith(".ts")).count();
        }
    }

    @Test
    void pushCutFinishWritesFiles(@TempDir Path dir) throws Exception {
        HlsPublisher pub = open(dir);
        assertTrue(pub.localAddr().isPresent());
        assertTrue(pub.localAddr().get().startsWith("127.0.0.1:"));
        assertTrue(pub.localPort() > 0);
        assertEquals("HlsPublisher(open)", pub.toString());

        pub.pushTs(twoPackets());
        pub.cutSegment();
        pub.pushTs(twoPackets());
        pub.cutSegment();

        PublisherStats s = pub.stats();
        assertEquals(2, s.segmentsWritten());
        assertEquals(752, s.bytesWritten());
        HlsStats h = pub.hlsStats();
        assertEquals(2, h.segmentsWritten());
        assertEquals(0, h.forcedCuts());
        assertTrue(pub.renderPlaylist(false).contains("#EXTM3U"));

        pub.finish();
        assertEquals("HlsPublisher(finished)", pub.toString());
        assertTrue(Files.exists(dir.resolve("playlist.m3u8")));
        assertTrue(countTs(dir) > 0, "no .ts segments written");
    }

    @Test
    void opsAfterFinishThrowIllegalState(@TempDir Path dir) throws Exception {
        HlsPublisher pub = open(dir);
        pub.finish();
        assertThrows(IllegalStateException.class, () -> pub.pushTs(twoPackets()));
        assertThrows(IllegalStateException.class, pub::localPort);
        assertThrows(IllegalStateException.class, pub::finish);
        pub.close(); // quiet, idempotent
        pub.close();
    }

    @Test
    void closeIsQuietAndIdempotent(@TempDir Path dir) throws Exception {
        HlsPublisher pub = open(dir);
        pub.pushTs(twoPackets());
        pub.close();
        pub.close();
        assertTrue(Files.exists(dir.resolve("playlist.m3u8")), "close() finishes the publisher");
        assertThrows(IllegalStateException.class, pub::stats);
    }

    @Test
    void unalignedPushRejected(@TempDir Path dir) throws Exception {
        try (HlsPublisher pub = open(dir)) {
            HlsException e = assertThrows(HlsException.class, () -> pub.pushTs(new byte[187]));
            assertEquals(HlsException.Kind.UNALIGNED_PUSH_TS, e.kind());
        }
    }

    @Test
    void badBindThrowsIllegalArgument() {
        assertThrows(IllegalArgumentException.class,
            () -> HlsPublisher.builder().bind("not-an-addr").build());
    }

    @Test
    void fromUrlBadSchemeThrowsUrlKind() {
        HlsException e = assertThrows(HlsException.class,
            () -> HlsPublisher.builder().fromUrl("rtsp://example.com:8000").build());
        assertEquals(HlsException.Kind.URL, e.kind());
    }

    @Test
    void fromUrlThenBindOverridesPort(@TempDir Path dir) throws Exception {
        // URL names port 1 (unbindable without privileges); the later bind() must win.
        try (HlsPublisher pub = HlsPublisher.builder()
                .fromUrl("hls://127.0.0.1:1")
                .outputDir(dir.toString())
                .bind("127.0.0.1:0")
                .build()) {
            assertTrue(pub.localPort() > 1);
        }
    }

    @Test
    void bindFailedOnOccupiedPort(@TempDir Path dir) throws Exception {
        try (ServerSocket occupied = new ServerSocket(0, 1, java.net.InetAddress.getLoopbackAddress())) {
            int port = occupied.getLocalPort();
            HlsException e = assertThrows(HlsException.class, () -> HlsPublisher.builder()
                .bind("127.0.0.1:" + port).outputDir(dir.toString()).build());
            assertEquals(HlsException.Kind.BIND_FAILED, e.kind());
        }
    }

    @Test
    void invalidConfigRejectedAtBuild(@TempDir Path dir) {
        // LIVE needs playlist_window × segment_duration ≥ 3 × ceil(segment_duration).
        HlsException e = assertThrows(HlsException.class, () -> HlsPublisher.builder()
            .bind("127.0.0.1:0").outputDir(dir.toString())
            .segmentDurationMs(1000).playlistWindow(1).mode(HlsMode.LIVE).build());
        assertEquals(HlsException.Kind.INVALID_CONFIG, e.kind());
    }

    @Test
    void unreadableCertIsTlsKind(@TempDir Path dir) {
        // Adjusted under controller ruling (2): crates/tst-hls/src/tls.rs's
        // load_server_config reads the cert file with std::fs::read(cert)
        // .map_err(HlsError::Io) BEFORE any TLS-specific parsing, so a missing
        // PEM file surfaces as HlsError::Io -> BindingErrorKind::HlsIo ->
        // HlsException.Kind.IO, not TLS. tst-hls only raises HlsError::Tls
        // once the file has been read and PEM/key parsing itself fails, which
        // is not reachable with a missing file. Asserting IO here matches the
        // library as it stands; tst-hls was not changed.
        HlsException e = assertThrows(HlsException.class, () -> HlsPublisher.builder()
            .bind("127.0.0.1:0").outputDir(dir.toString())
            .enableTls(dir.resolve("missing.pem").toString(), dir.resolve("missing.key").toString())
            .build());
        assertEquals(HlsException.Kind.IO, e.kind());
    }

    @Test
    void cutWithDurationRecordsExtinf(@TempDir Path dir) throws Exception {
        HlsPublisher pub = HlsPublisher.builder().bind("127.0.0.1:0").outputDir(dir.toString())
            .segmentDurationMs(1000).playlistWindow(3).mode(HlsMode.EVENT).build();
        pub.pushTs(twoPackets());
        pub.cutSegmentWithDuration(3_200_000L);
        String pl = pub.renderPlaylist(false);
        pub.finish();
        assertTrue(pl.contains("#EXTINF:3.200,"), pl);
    }

    @Test
    void maxSegmentDurationSetAndZero(@TempDir Path dir) throws Exception {
        try (HlsPublisher a = HlsPublisher.builder().bind("127.0.0.1:0").outputDir(dir.resolve("a").toString())
                .segmentDurationMs(4000).maxSegmentDurationMs(8000).build()) {
            assertNotNull(a);
        }
        try (HlsPublisher b = HlsPublisher.builder().bind("127.0.0.1:0").outputDir(dir.resolve("b").toString())
                .maxSegmentDurationMs(0).build()) {
            assertNotNull(b);
        }
    }

    @Test
    void implementsPublisher(@TempDir Path dir) throws Exception {
        try (HlsPublisher pub = open(dir)) {
            assertTrue(pub instanceof Publisher);
        }
    }
}
