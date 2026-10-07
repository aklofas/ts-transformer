package org.tstrans.hls;

import java.util.Objects;
import org.tstrans.HlsException;

/**
 * Builder for {@link HlsPublisher}. Pure Java: setters accumulate, {@link #build()}
 * makes one native call. {@link #fromUrl(String)} seeds the native builder from
 * an {@code hls://} / {@code hlss://} URL first; every setter called on this
 * builder then overlays it (tst-py's "replace, then overlay" order), whatever
 * the call order on the Java side.
 */
public final class HlsPublisherBuilder {
    private String url;
    private String bind;
    private String outputDir;
    private long segmentDurationMs;      // 0 = unset
    private long maxSegmentDurationMs;   // 0 = unset (library default)
    private int playlistWindow = -1;     // -1 = unset
    private HlsMode mode;                // null = unset
    private String authUser;
    private String authPass;
    private String tlsCert;
    private String tlsKey;

    HlsPublisherBuilder() {}

    /** HTTP server bind address, e.g. {@code "127.0.0.1:0"} for a kernel-picked port. */
    public HlsPublisherBuilder bind(String addr) { this.bind = Objects.requireNonNull(addr, "addr"); return this; }

    /** Directory for {@code .ts} segments and {@code playlist.m3u8}. */
    public HlsPublisherBuilder outputDir(String path) { this.outputDir = Objects.requireNonNull(path, "path"); return this; }

    /** Target segment duration in milliseconds. */
    public HlsPublisherBuilder segmentDurationMs(long ms) { this.segmentDurationMs = ms; return this; }

    /**
     * Force-cut cap on an open segment's wall-clock age. {@code 0} leaves the
     * library default ({@code 2 × segmentDuration}); it does not reset an
     * earlier non-zero value.
     */
    public HlsPublisherBuilder maxSegmentDurationMs(long ms) { if (ms != 0) this.maxSegmentDurationMs = ms; return this; }

    /** Segments visible in a LIVE playlist. */
    public HlsPublisherBuilder playlistWindow(int n) { this.playlistWindow = n; return this; }

    /** Playlist mode. */
    public HlsPublisherBuilder mode(HlsMode mode) { this.mode = Objects.requireNonNull(mode, "mode"); return this; }

    /** HTTP Basic auth. */
    public HlsPublisherBuilder basicAuth(String user, String password) {
        this.authUser = Objects.requireNonNull(user, "user");
        this.authPass = Objects.requireNonNull(password, "password");
        return this;
    }

    /** HTTPS with PEM cert + key file paths. */
    public HlsPublisherBuilder enableTls(String certPath, String keyPath) {
        this.tlsCert = Objects.requireNonNull(certPath, "certPath");
        this.tlsKey = Objects.requireNonNull(keyPath, "keyPath");
        return this;
    }

    /** Seed from an {@code hls://} / {@code hlss://} URL; a bad URL throws {@code HlsException(URL)} from {@link #build()}. */
    public HlsPublisherBuilder fromUrl(String url) { this.url = Objects.requireNonNull(url, "url"); return this; }

    /**
     * Build the publisher (binds the HTTP server immediately).
     *
     * @throws HlsException {@code URL}, {@code BIND_FAILED}, {@code INVALID_CONFIG},
     *     {@code TLS}, {@code TLS_DISABLED} per the failure
     * @throws IllegalArgumentException on a malformed bind address
     */
    public HlsPublisher build() throws HlsException {
        long h = HlsPublisher.nBuild(url, bind, outputDir, segmentDurationMs, maxSegmentDurationMs,
            playlistWindow, mode == null ? -1 : mode.ordinal(), authUser, authPass, tlsCert, tlsKey);
        if (h == 0) {
            throw new HlsException(HlsException.Kind.INTERNAL, "nBuild returned 0 without throwing");
        }
        return new HlsPublisher(h);
    }

    @Override public String toString() { return "HlsPublisherBuilder(...)"; }
}
