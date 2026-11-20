package org.tstrans.mpegts;

/**
 * Video codec tag for a demuxed elementary stream. Mirrors the Rust enum
 * {@code tst_core::mpegts::demux::event::VideoCodec}.
 */
public enum VideoCodec {
    H264(0),
    H265(1),
    H266(2),
    AV1(3);

    private final int code;

    VideoCodec(int code) {
        this.code = code;
    }

    /** @return the value native code receives for this constant; stable across releases. */
    public int code() {
        return code;
    }
}
