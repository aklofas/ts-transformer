/**
 * tstrans JVM bindings — MPEG-TS + KLV + codec parsing and SRT/RTP/HLS/WebRTC transport.
 *
 * <p>Package layout mirrors the Python binding ({@code tstrans.*}).
 */
module org.tstrans {
    requires java.base;
    exports org.tstrans;
    exports org.tstrans.codec;
    exports org.tstrans.hls;
    exports org.tstrans.io;
    exports org.tstrans.klv;
    exports org.tstrans.mpegts;
    exports org.tstrans.pipeline;
    exports org.tstrans.rtp;
    exports org.tstrans.shells;
    exports org.tstrans.srt;
    exports org.tstrans.webrtc;
}
