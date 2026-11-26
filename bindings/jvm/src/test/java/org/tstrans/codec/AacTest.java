package org.tstrans.codec;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.tstrans.TestSupport.unsigned;

import java.util.List;
import org.junit.jupiter.api.Test;
import org.tstrans.CodecParseException;

class AacTest {
    // Reproduces tst-integration's synthetic_adts_frame() byte-for-byte:
    // AAC-LC, 44100 Hz (index 4), stereo (channel_config 2), frame_length 15
    // (7-byte header + 8 payload bytes), protection_absent=1 (no CRC). The
    // generator's ID bit decodes to MPEG-4 (see mpegVersion assertion below).
    // Header bytes computed from the generator:
    //   FF F1 50 80 01 FF FC  +  A0 A1 A2 A3 A4 A5 A6 A7
    private static final byte[] ADTS_FRAME = unsigned(
            0xFF, 0xF1, 0x50, 0x80, 0x01, 0xFF, 0xFC,
            0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7);

    @Test
    void parseAacFramesYieldsOneFrame() throws CodecParseException {
        List<AdtsFrame> frames = Codec.parseAacFrames(ADTS_FRAME);
        assertEquals(1, frames.size());
        AdtsFrame f = frames.get(0);
        assertSame(AacProfile.LC, f.profile());
        assertEquals(44100L, f.sampleRateHz());
        assertEquals(2, f.channelConfiguration());
        assertEquals(15L, f.frameLengthBytes());
        assertEquals(1024, f.samplesPerFrame());
        assertEquals(1, f.numRawDataBlocks());
        assertFalse(f.hasCrc());
        // h[1] = 0b1111_0001 → ID bit (bit 3 of the low nibble) is 0 → MPEG-4.
        // (The tst-integration generator's "ID=MPEG-2" comment is mislabelled;
        // the parser is the source of truth and reads id_bit == 0 as MPEG-4.)
        assertSame(MpegVersion.MPEG4, f.mpegVersion());

        // Flattened channel layout: stereo → not PCE-defined, channels == 2.
        AacChannelLayout layout = f.channelLayout();
        assertFalse(layout.pceDefined());
        assertEquals(Integer.valueOf(2), layout.channels());

        // rawHeader = 7 bytes (no CRC); payload = full frame (header + body).
        assertEquals(7, f.rawHeader().remaining());
        assertEquals(15, f.payload().remaining());
    }

    @Test
    void parseAacFramesBadSyncThrows() {
        // First byte not 0xFF → 12-bit syncword mismatch → BAD_SYNC_WORD.
        byte[] garbage = unsigned(0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00);
        CodecParseException ex = assertThrows(
                CodecParseException.class, () -> Codec.parseAacFrames(garbage));
        assertSame(CodecParseException.Kind.BAD_SYNC_WORD, ex.kind());
        assertEquals("aac", ex.codec());
    }

    @Test
    void parseAacFramesWithResyncSkipsErrors() {
        // Resync is best-effort: it never throws and yields only frames that
        // parse. A pure-garbage buffer therefore yields an empty list.
        byte[] garbage = unsigned(0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66);
        List<AdtsFrame> frames = Codec.parseAacFramesWithResync(garbage);
        assertTrue(frames.isEmpty());

        // A valid frame is still recovered through the resync path.
        List<AdtsFrame> good = Codec.parseAacFramesWithResync(ADTS_FRAME);
        assertEquals(1, good.size());
        assertSame(AacProfile.LC, good.get(0).profile());
    }

    @Test
    void protectedMultiBlockRawHeader() throws CodecParseException {
        // MPEG-2 AAC-LC, 44100 Hz, stereo, protection_absent = 0, two raw data
        // blocks (wire value 1), frame_length 64: the 7 fixed and variable
        // header bytes, one raw_data_block_position word and the CRC word.
        byte[] frame = new byte[64];
        byte[] header = unsigned(0xFF, 0xF8, 0x50, 0x80, 0x08, 0x1F, 0xFD);
        System.arraycopy(header, 0, frame, 0, header.length);
        List<AdtsFrame> frames = Codec.parseAacFrames(frame);
        assertEquals(1, frames.size());
        AdtsFrame f = frames.get(0);
        assertTrue(f.hasCrc());
        assertEquals(2, f.numRawDataBlocks());
        assertEquals(11, f.rawHeader().remaining());
        assertEquals(64, f.frameLengthBytes());
    }
}
