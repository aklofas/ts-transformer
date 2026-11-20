package org.tstrans.mpegts;

/**
 * Audio codec tag for a demuxed elementary stream. Mirrors the Rust enum
 * {@code tst_core::mpegts::demux::event::AudioCodec}.
 */
public enum AudioCodec {
    MP2(0),
    AAC(1),
    AAC_LATM(2),
    AC3(3);

    private final int code;

    AudioCodec(int code) {
        this.code = code;
    }

    /** @return the value native code receives for this constant; stable across releases. */
    public int code() {
        return code;
    }
}
