package org.tstrans.io;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.inputTs;
import static org.tstrans.TestSupport.isLinux;

import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.io.TempDir;
import org.tstrans.mpegts.DemuxEvent;

class ParseFileTest {

    /** A demuxer that cannot be built must not leave the opened input behind. */
    @Test
    @Timeout(60)
    void parseFileClosesItsInputWhenTheNativeLibraryCannotLoad(@TempDir Path tmp) throws Exception {
        assumeTrue(isLinux(), "counts descriptors through /proc/self/fd");
        Path input = Files.write(tmp.resolve("input.ts"), new byte[188]);
        String java = Path.of(System.getProperty("java.home"), "bin", "java").toString();
        Process child = new ProcessBuilder(List.of(java,
                "-Dtstrans.native.lib=/nonexistent/libtstjni.so",
                "-cp", System.getProperty("java.class.path"),
                ParseFileOpenFailureChild.class.getName(), input.toString()))
            .redirectErrorStream(true).start();
        String out = new String(child.getInputStream().readAllBytes(), StandardCharsets.UTF_8);
        assertTrue(child.waitFor(30, TimeUnit.SECONDS), out);
        assertEquals(0, child.exitValue(), out);
        assertTrue(out.contains("THROWN=java.lang.UnsatisfiedLinkError"), out);
        assertTrue(out.contains("OPEN=0"), out);
    }

    @Test
    void parseFileYieldsDemuxEvents() throws Exception {
        Path in = inputTs();
        assertTrue(java.nio.file.Files.isRegularFile(in), "fixture missing: " + in);
        List<DemuxEvent> events;
        try (var stream = Io.parseFile(in)) {
            events = stream.toList();
        }
        assertFalse(events.isEmpty(), "parseFile must yield events from the fixture");
        assertTrue(events.stream().anyMatch(e -> e instanceof DemuxEvent.ProgramMap),
            "expected a ProgramMap event");
        DemuxEvent.Video video = events.stream()
            .filter(e -> e instanceof DemuxEvent.Video)
            .map(e -> (DemuxEvent.Video) e)
            .findFirst()
            .orElseThrow(() -> new AssertionError("expected a Video event"));
        assertNotNull(video.raw(), "Video events carry the raw encoded AU");
        assertTrue(video.raw().remaining() > 0, "raw AU must be non-empty");
    }

    @Test
    void parseFileEqualsInMemoryFeed() throws Exception {
        Path in = inputTs();
        byte[] bytes = java.nio.file.Files.readAllBytes(in);

        java.util.List<String> feedShapes = new java.util.ArrayList<>();
        try (var d = new org.tstrans.mpegts.Demuxer()) {
            d.feed(bytes);
            d.flush();
            for (DemuxEvent e : d) feedShapes.add(e.getClass().getSimpleName());
        }

        java.util.List<String> fileShapes = new java.util.ArrayList<>();
        try (var stream = Io.parseFile(in)) {
            stream.forEach(e -> fileShapes.add(e.getClass().getSimpleName()));
        }

        assertEquals(feedShapes, fileShapes,
            "file-path event sequence must equal the in-memory feed-path sequence");
    }

    @Test
    void parseFileCloseMidStreamReleasesResources() throws Exception {
        Path in = inputTs();
        assertDoesNotThrow(() -> {
            try (var stream = Io.parseFile(in)) {
                stream.limit(1).forEach(e -> { /* consume one, then close */ });
            }
        });
    }

    @Test
    void probeSummarizesFixture() throws Exception {
        ProbeResult r = Io.probe(inputTs());
        assertTrue(r.sizeBytes() > 0);
        assertTrue(r.packetCount() > 0);
        assertFalse(r.programs().isEmpty(), "probe must find at least one program");
        assertFalse(r.videoCodecs().isEmpty(), "fixture carries H.264 video");
        assertTrue(r.hasKlv(), "h264-st0601-mp carries an ST 0601 KLV stream");
    }
}
