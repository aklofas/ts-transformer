package org.tstrans.rtp;

import java.util.Objects;
import java.util.Optional;
import org.tstrans.Redact;

/**
 * RTSP client connection configuration. Mirrors tst-py
 * {@code tstrans.rtp.RtspClientConfig} (a frozen dataclass). Build via
 * {@link #of(String)} (all defaults) or {@link #builder(String)}.
 *
 * <p>{@code transportPref} and {@code rtspVersion} override the URL's
 * {@code ?transport=} / {@code ?rtsp_version=} when not at their defaults.
 * {@code tlsRootCertsPem} is read at connect too: a PEM bundle of custom trust
 * anchors for {@code rtsps://} servers with a private CA; see {@link RtspClient}.
 */
public final class RtspClientConfig {
    private final String url;
    private final Object auth;            // BasicAuth | DigestAuth | null
    private final TransportPref transportPref;
    private final byte[] tlsRootCertsPem; // nullable; defensively copied
    private final boolean keepalive;
    private final RtspVersion rtspVersion;

    private RtspClientConfig(Builder b) {
        this.url = b.url;
        this.auth = b.auth;
        this.transportPref = b.transportPref;
        this.tlsRootCertsPem = b.tlsRootCertsPem == null ? null : b.tlsRootCertsPem.clone();
        this.keepalive = b.keepalive;
        this.rtspVersion = b.rtspVersion;
    }

    /** All-defaults config for {@code url}. */
    public static RtspClientConfig of(String url) { return builder(url).build(); }

    public static Builder builder(String url) { return new Builder(url); }

    public String url() { return url; }

    /** @return the auth credential ({@link BasicAuth} or {@link DigestAuth}), if set. */
    public Optional<Object> auth() { return Optional.ofNullable(auth); }

    public TransportPref transportPref() { return transportPref; }

    /** @return a defensive copy of the PEM bytes, if set. */
    public Optional<byte[]> tlsRootCertsPem() {
        return Optional.ofNullable(tlsRootCertsPem).map(byte[]::clone);
    }

    public boolean keepalive() { return keepalive; }

    public RtspVersion rtspVersion() { return rtspVersion; }

    @Override public String toString() {
        return "RtspClientConfig(url=" + Redact.url(url)
            + ", auth=" + (auth != null ? "<auth>" : "None")
            + ", transportPref=" + transportPref
            + ", tlsRootCertsPem=" + (tlsRootCertsPem != null ? "<bytes>" : "None")
            + ", keepalive=" + keepalive
            + ", rtspVersion=" + rtspVersion + ")";
    }

    /** Builder for {@link RtspClientConfig}. */
    public static final class Builder {
        private final String url;
        private Object auth;
        private TransportPref transportPref = TransportPref.AUTO;
        private byte[] tlsRootCertsPem;
        private boolean keepalive = true;
        private RtspVersion rtspVersion = RtspVersion.V1_0;

        private Builder(String url) {
            Objects.requireNonNull(url, "url");
            if (url.isEmpty()) throw new IllegalArgumentException("url must not be empty");
            this.url = url;
        }

        /** @param auth a {@link BasicAuth}, {@link DigestAuth}, or {@code null}. */
        public Builder auth(Object auth) {
            if (auth != null && !(auth instanceof BasicAuth) && !(auth instanceof DigestAuth)) {
                throw new IllegalArgumentException("auth must be BasicAuth, DigestAuth, or null");
            }
            this.auth = auth;
            return this;
        }

        public Builder transportPref(TransportPref p) {
            this.transportPref = Objects.requireNonNull(p); return this;
        }

        public Builder tlsRootCertsPem(byte[] pem) {
            this.tlsRootCertsPem = pem == null ? null : pem.clone(); return this;
        }

        public Builder keepalive(boolean v) { this.keepalive = v; return this; }

        public Builder rtspVersion(RtspVersion v) {
            this.rtspVersion = Objects.requireNonNull(v); return this;
        }

        public RtspClientConfig build() { return new RtspClientConfig(this); }
    }
}
