package org.tstrans.mpegts;

/** AV1 PES carriage mode the demuxer expects. Mirrors {@code tst_core::mpegts::mux::Av1CarriageMode}. */
public enum Av1CarriageMode {
    MPEG2_TS_BINDING(0),
    INTEROP_RAW_OBU(1);

    private final int code;

    Av1CarriageMode(int code) {
        this.code = code;
    }

    /** @return the value native code receives for this constant; stable across releases. */
    public int code() {
        return code;
    }
}
