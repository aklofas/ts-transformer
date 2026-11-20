package org.tstrans.mpegts;

/**
 * Subtitle codec tag for a demuxed elementary stream. Mirrors the Rust enum
 * {@code tst_core::mpegts::demux::event::SubtitleCodec}.
 */
public enum SubtitleCodec {
    DVB_SUBTITLING(0),
    DVB_TELETEXT(1),
    CEA708_STANDALONE(2),
    WEBVTT_IN_TS(3);

    private final int code;

    SubtitleCodec(int code) {
        this.code = code;
    }

    /** @return the value native code receives for this constant; stable across releases. */
    public int code() {
        return code;
    }
}
