package org.tstrans.mpegts;

/** Demuxer strictness ladder. Mirrors {@code tst_core::mpegts::demux::StrictMode} (PSI_ONLY ↔ Rust DescriptorsOnly). */
public enum StrictMode {
    OFF(0),
    TIMING_ONLY(1),
    PSI_ONLY(2),
    FULL(3);

    private final int code;

    StrictMode(int code) {
        this.code = code;
    }

    /** @return the value native code receives for this constant; stable across releases. */
    public int code() {
        return code;
    }
}
