package org.tstrans.codec;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.tstrans.TestSupport.unsigned;

import java.nio.ByteBuffer;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.tstrans.CodecParseException;

class Av1Test {
    // Minimal Sequence Header OBU body — captured byte-for-byte from the
    // tst_core unit tests (codec::av1::decode::obu_stream::tests::
    // minimal_seq_header_body / sequence_header::tests). Main profile,
    // level 2.0, seq_tier 0 (Main), 320x240, 8-bit 4:2:0, no color desc, no timing info.
    private static final byte[] SEQ_HEADER_320X240 =
            unsigned(0, 0, 0, 4, 60, 255, 188, 0, 0, 0);

    // Minimal KEY_FRAME header body: show_existing_frame=0, frame_type=0,
    // show_frame=1 → high-nibble 0001 = 0x10.
    private static final byte[] KEYFRAME = unsigned(0x10);

    @Test
    void parseSequenceHeaderDimensions() throws CodecParseException {
        Av1SequenceHeader seq = Codec.parseAv1SequenceHeader(SEQ_HEADER_320X240);
        assertEquals(0, seq.profile());
        assertEquals(0, seq.level());
        assertEquals(0, seq.tier());
        assertEquals(320L, seq.maxFrameWidth());
        assertEquals(240L, seq.maxFrameHeight());
        assertEquals(8, seq.bitDepth());
        assertFalse(seq.monochrome());
        assertSame(ChromaFormat.YUV420, seq.chromaFormat());
        assertFalse(seq.stillPicture());
        assertFalse(seq.reducedStillPictureHeader());
        // No timing info → no frame rate.
        assertEquals(null, seq.frameRate());
        // AV1 always carries a color_range bit → colorInfo is populated.
        assertNotNull(seq.colorInfo());
        // raw preserved byte-for-byte.
        assertEquals(SEQ_HEADER_320X240.length, seq.raw().remaining());
    }

    @Test
    void parseSequenceHeaderEmptyThrowsTruncated() {
        CodecParseException ex = assertThrows(
                CodecParseException.class,
                () -> Codec.parseAv1SequenceHeader(new byte[0]));
        assertSame(CodecParseException.Kind.TRUNCATED_RBSP, ex.kind());
        assertEquals("av1", ex.codec());
    }

    @Test
    void parseFrameHeaderLightKeyframe() throws CodecParseException {
        Av1SequenceHeader seq = Codec.parseAv1SequenceHeader(SEQ_HEADER_320X240);
        Av1FrameHeaderLight fh = Codec.parseAv1FrameHeaderLight(KEYFRAME, seq);
        assertEquals(0, fh.frameType()); // KEY_FRAME
        assertTrue(fh.showFrame());
        assertFalse(fh.showExistingFrame());
        // Light scope: frame_size is always null.
        assertEquals(null, fh.frameSize());
        assertEquals(KEYFRAME.length, fh.raw().remaining());
    }

    @Test
    void parseFrameHeaderLightEmptyThrows() throws CodecParseException {
        Av1SequenceHeader seq = Codec.parseAv1SequenceHeader(SEQ_HEADER_320X240);
        assertThrows(
                CodecParseException.class,
                () -> Codec.parseAv1FrameHeaderLight(new byte[0], seq));
    }

    @Test
    void parseObuStreamCollectsSeqThenFrameHeader() {
        // OBU types per §6.2.2: TD=2, SEQUENCE_HEADER=1, FRAME=3 (treated as a
        // frame header by the light parser).
        List<Obu> obus = List.of(
                new Obu(2, null, ByteBuffer.wrap(new byte[0])),
                new Obu(1, null, ByteBuffer.wrap(SEQ_HEADER_320X240)),
                new Obu(3, null, ByteBuffer.wrap(KEYFRAME)));
        Av1ObuStream stream = Codec.parseAv1ObuStream(obus);
        assertEquals(1, stream.sequenceHeaders().size());
        assertEquals(1, stream.frameHeaders().size());
        assertTrue(stream.unparseable().isEmpty());
        Av1SequenceHeader seq = stream.sequenceHeaders().get(0);
        assertEquals(0, seq.profile());
        assertEquals(320L, seq.maxFrameWidth());
        assertEquals(240L, seq.maxFrameHeight());
        assertEquals(0, stream.frameHeaders().get(0).frameType());
    }

    @Test
    void parseObuStreamRecordsFailuresInUnparseable() {
        // A truncated SEQUENCE_HEADER (type 1, empty body) lands in unparseable.
        List<Obu> obus = List.of(new Obu(1, null, ByteBuffer.wrap(new byte[0])));
        Av1ObuStream stream = Codec.parseAv1ObuStream(obus);
        assertTrue(stream.sequenceHeaders().isEmpty());
        assertEquals(1, stream.unparseable().size());
        assertEquals(1, stream.unparseable().get(0).obuType());
        assertNotNull(stream.unparseable().get(0).error());
    }

    @Test
    void parseObuStreamFrameHeaderBeforeSeqHeaderIsUnparseable() {
        // FRAME (type 3) with no preceding SEQUENCE_HEADER → synthesised error.
        List<Obu> obus = List.of(new Obu(3, null, ByteBuffer.wrap(KEYFRAME)));
        Av1ObuStream stream = Codec.parseAv1ObuStream(obus);
        assertTrue(stream.sequenceHeaders().isEmpty());
        assertTrue(stream.frameHeaders().isEmpty());
        assertEquals(1, stream.unparseable().size());
        assertEquals(3, stream.unparseable().get(0).obuType());
    }

    @Test
    void parseObuStreamRejectsOutOfRangeObuType() {
        // obuType is a u8 on the Rust side; an out-of-range int (300) must be
        // rejected at the input boundary (checked narrowing) rather than
        // silently truncated to (300 & 0xFF) == 44. The validation surfaces as
        // an unchecked IllegalArgumentException, not a CodecParseException.
        List<Obu> obus = List.of(new Obu(300, null, ByteBuffer.wrap(new byte[0])));
        assertThrows(IllegalArgumentException.class, () -> Codec.parseAv1ObuStream(obus));
    }

    @Test
    void parseObuStreamSkipsUnknownObuTypes() {
        // Metadata (5) + TileGroup (4) carry no metadata for this parser and
        // are skipped silently (no unparseable entries).
        List<Obu> obus = List.of(
                new Obu(5, null, ByteBuffer.wrap(unsigned(0x00))),
                new Obu(4, null, ByteBuffer.wrap(unsigned(0x00))));
        Av1ObuStream stream = Codec.parseAv1ObuStream(obus);
        assertTrue(stream.sequenceHeaders().isEmpty());
        assertTrue(stream.frameHeaders().isEmpty());
        assertTrue(stream.unparseable().isEmpty());
    }
}
