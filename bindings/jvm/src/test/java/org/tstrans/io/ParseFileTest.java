package org.tstrans.io;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.inputTs;

import java.nio.file.Path;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.tstrans.mpegts.DemuxEvent;

class ParseFileTest {

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
