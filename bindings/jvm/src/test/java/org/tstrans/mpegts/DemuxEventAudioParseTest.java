package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.*;

import java.nio.ByteBuffer;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.tstrans.CodecParseException;
import org.tstrans.DemuxException;
import org.tstrans.codec.AdtsFrame;
import org.tstrans.codec.AudioFrame;
import org.tstrans.codec.Codec;
import org.tstrans.codec.Mpeg2AudioFrame;

/**
 * Behavioural tests for the lazy {@link DemuxEvent.Audio#parse()} model
 * (mirrors tst-py's {@code DemuxEvent.Audio.parse()} and the
 * {@code Video} shape). Lives in {@code org.tstrans.mpegts} so it can drive
 * the package-private {@link DemuxEventAudioNatives#nParseAudio} companion
 * directly (ordinal-drift + strict/lenient malformed paths that a demux-driven
 * test can't reach).
 */
class DemuxEventAudioParseTest {

    private static Path fixture(String name) {
        return Path.of(System.getProperty("user.dir"), "..", "..",
                "crates/tst-core/tests/fixtures/audio", name).normalize();
    }

    /** First {@link DemuxEvent.Audio} from a demuxed fixture (or {@code null}). */
    private static DemuxEvent.Audio firstAudio(String fixtureName) throws Exception {
        byte[] ts = Files.readAllBytes(fixture(fixtureName));
        try (Demuxer d = new Demuxer()) {
            d.feed(ts);
            d.flush();
            for (DemuxEvent e : d) {
                if (e instanceof DemuxEvent.Audio a) {
                    return a;
                }
            }
        }
        return null;
    }

    private static byte[] toBytes(ByteBuffer buf) {
        ByteBuffer view = buf.duplicate();
        byte[] out = new byte[view.remaining()];
        view.get(out);
        return out;
    }

    @Test
    void aacParseYieldsSameTypedFramesAsEagerPath() throws Exception {
        DemuxEvent.Audio a = firstAudio("aac-adts.ts");
        assertNotNull(a, "expected an AAC Audio event from aac-adts.ts");
        assertEquals(AudioCodec.AAC, a.codec(), "aac-adts.ts audio stream must be AAC");
        assertTrue(a.raw().remaining() > 0, "AAC Audio.raw() must carry the encoded ES");

        // The eager path built its typed list from frames_with_resync; parse()
        // (lenient) must reproduce it byte-for-byte.
        List<AdtsFrame> eager = Codec.parseAacFramesWithResync(toBytes(a.raw()));
        List<AudioFrame> lazy = a.parse();
        assertFalse(lazy.isEmpty(), "clean AAC parse() must yield typed frames");
        assertEquals(eager, lazy, "parse() must equal the eager frames_with_resync list");
        for (AudioFrame f : lazy) {
            assertTrue(f instanceof AdtsFrame, "AAC frames must be AdtsFrame, was " + f.getClass());
        }
        // A clean stream has no malformed frame, so strict == lenient.
        assertEquals(lazy, a.parse(true), "strict parse of a clean stream equals lenient");
        assertEquals(lazy, a.parse(false), "parse() delegates to parse(false)");
    }

    @Test
    void mp2ParseYieldsTypedMpeg2Frames() throws Exception {
        DemuxEvent.Audio a = firstAudio("mp2.ts");
        assertNotNull(a, "expected an MP2 Audio event from mp2.ts");
        assertEquals(AudioCodec.MP2, a.codec(), "mp2.ts audio stream must be MP2");
        assertTrue(a.raw().remaining() > 0, "MP2 Audio.raw() must carry the encoded ES");

        List<Mpeg2AudioFrame> eager = Codec.parseMpeg2AudioFramesWithResync(toBytes(a.raw()));
        List<AudioFrame> lazy = a.parse();
        assertFalse(lazy.isEmpty(), "clean MP2 parse() must yield typed frames");
        assertEquals(eager, lazy, "parse() must equal the eager frames_with_resync list");
        assertTrue(lazy.get(0) instanceof Mpeg2AudioFrame,
            "MP2 frames must be Mpeg2AudioFrame, was " + lazy.get(0).getClass());
    }

    @Test
    void latmAndAc3ParseReturnEmptyList() throws Exception {
        // Codecs with no typed parser: parse() is an empty list in BOTH modes,
        // raw() carries the bytes to read directly (mirrors tst-py's `_ =>` arm).
        record Case(String fixture, AudioCodec codec) {}
        for (Case c : List.of(new Case("aac-latm.ts", AudioCodec.AAC_LATM),
                              new Case("ac3.ts", AudioCodec.AC3))) {
            DemuxEvent.Audio a = firstAudio(c.fixture());
            assertNotNull(a, "expected an Audio event from " + c.fixture());
            assertEquals(c.codec(), a.codec(), c.fixture() + " must be tagged " + c.codec());
            assertTrue(a.raw().remaining() > 0, "raw() must carry the ES for " + c.codec());
            assertTrue(a.parse().isEmpty(), c.codec() + " parse() must be empty (no typed parser)");
            assertTrue(a.parse(true).isEmpty(), c.codec() + " strict parse() must be empty too");
        }
    }

    @Test
    void malformedAacIsLenientPastCorruptionButStrictThrows() {
        // Non-syncword garbage: strict (frames) throws on the first bad frame;
        // lenient (frames_with_resync) resyncs past it and returns an empty list.
        byte[] garbage = new byte[] {0, 0, 0, 0, 0, 0, 0, 0};
        int aacOrdinal = AudioCodec.AAC.ordinal();

        List<AudioFrame> lenient = assertDoesNotThrow(
            () -> DemuxEventAudioNatives.nParseAudio(garbage, aacOrdinal, false),
            "lenient parse of garbage must not throw");
        assertTrue(lenient.isEmpty(), "lenient parse of pure garbage yields no frames");

        assertThrows(CodecParseException.class,
            () -> DemuxEventAudioNatives.nParseAudio(garbage, aacOrdinal, true),
            "strict parse of garbage must throw CodecParseException");
    }

    @Test
    void outOfRangeCodecOrdinalThrows() {
        // The companion native validates the ordinal exactly (nSplitVideo pattern);
        // an out-of-range value means AudioCodec enum drift → loud failure.
        assertThrows(DemuxException.class,
            () -> DemuxEventAudioNatives.nParseAudio(new byte[] {0}, 99, false),
            "an out-of-range AudioCodec ordinal must throw DemuxException");
    }

    @Test
    void parseIsPositionIndependent() throws Exception {
        // A consumer that drained raw() must not truncate a subsequent parse():
        // parse() operates on a duplicate().clear() view of the stored buffer.
        DemuxEvent.Audio a = firstAudio("aac-adts.ts");
        assertNotNull(a, "expected an AAC Audio event from aac-adts.ts");
        List<AudioFrame> before = a.parse();
        // Advance the shared buffer's position to its end (simulating a reader).
        ByteBuffer raw = a.raw();
        raw.position(raw.limit());
        List<AudioFrame> after = a.parse();
        assertEquals(before, after, "parse() must ignore the shared buffer position");
    }
}
