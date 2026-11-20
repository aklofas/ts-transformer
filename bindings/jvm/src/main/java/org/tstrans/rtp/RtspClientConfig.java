package org.tstrans.rtp;

import java.util.Objects;
import java.util.Optional;
import java.util.OptionalLong;
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
 *
 * <p>The keepalive interval, the connect and request timeouts and the user agent are
 * empty until set; empty keeps the client's own default (an interval of half the
 * session timeout the SETUP reply names, 10&nbsp;s for each timeout, {@code tst-rtp/0.1}).
 * They apply to {@link RtspClient#connect}, {@link RtspClient#connectH264} and
 * {@link RtspClient#push} alike.
 */
public final class RtspClientConfig {
    private final String url;
    private final Object auth;            // BasicAuth | DigestAuth | null
    private final TransportPref transportPref;
    private final byte[] tlsRootCertsPem; // nullable; defensively copied
    private final boolean keepalive;
    private final RtspVersion rtspVersion;
    private final Long keepaliveIntervalMs; // null = the client's default
    private final Long connectTimeoutMs;
    private final Long requestTimeoutMs;
    private final String userAgent;

    private RtspClientConfig(Builder b) {
        this.url = b.url;
        this.auth = b.auth;
        this.transportPref = b.transportPref;
        this.tlsRootCertsPem = b.tlsRootCertsPem == null ? null : b.tlsRootCertsPem.clone();
        this.keepalive = b.keepalive;
        this.rtspVersion = b.rtspVersion;
        this.keepaliveIntervalMs = b.keepaliveIntervalMs;
        this.connectTimeoutMs = b.connectTimeoutMs;
        this.requestTimeoutMs = b.requestTimeoutMs;
        this.userAgent = b.userAgent;
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

    /** @return the keepalive ping interval in milliseconds, if set. */
    public OptionalLong keepaliveIntervalMs() { return optional(keepaliveIntervalMs); }

    /** @return the connect budget in milliseconds, if set. */
    public OptionalLong connectTimeoutMs() { return optional(connectTimeoutMs); }

    /** @return the per-request deadline in milliseconds, if set. */
    public OptionalLong requestTimeoutMs() { return optional(requestTimeoutMs); }

    /** @return the {@code User-Agent} header value, if set. */
    public Optional<String> userAgent() { return Optional.ofNullable(userAgent); }

    private static OptionalLong optional(Long v) {
        return v == null ? OptionalLong.empty() : OptionalLong.of(v);
    }

    @Override public String toString() {
        return "RtspClientConfig(url=" + Redact.url(url)
            + ", auth=" + (auth != null ? "<auth>" : "None")
            + ", transportPref=" + transportPref
            + ", tlsRootCertsPem=" + (tlsRootCertsPem != null ? "<bytes>" : "None")
            + ", keepalive=" + keepalive
            + ", rtspVersion=" + rtspVersion
            + ", keepaliveIntervalMs=" + (keepaliveIntervalMs != null ? keepaliveIntervalMs : "None")
            + ", connectTimeoutMs=" + (connectTimeoutMs != null ? connectTimeoutMs : "None")
            + ", requestTimeoutMs=" + (requestTimeoutMs != null ? requestTimeoutMs : "None")
            + ", userAgent=" + (userAgent != null ? userAgent : "None") + ")";
    }

    /** Builder for {@link RtspClientConfig}. */
    public static final class Builder {
        private final String url;
        private Object auth;
        private TransportPref transportPref = TransportPref.AUTO;
        private byte[] tlsRootCertsPem;
        private boolean keepalive = true;
        private RtspVersion rtspVersion = RtspVersion.V1_0;
        private Long keepaliveIntervalMs;
        private Long connectTimeoutMs;
        private Long requestTimeoutMs;
        private String userAgent;

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

        /**
         * How often the client pings the server (OPTIONS or GET_PARAMETER) to keep the
         * session alive; unset, half the session timeout the SETUP reply names. A server
         * that ends a quiet session sooner than that (mediamtx: 10&nbsp;s for a publisher)
         * needs a shorter one.
         *
         * @param ms the interval, positive; checked at {@link #build()}
         */
        public Builder keepaliveIntervalMs(long ms) { this.keepaliveIntervalMs = ms; return this; }

        /**
         * The connect budget: the TCP connect and, for {@code rtsps://}, the TLS handshake
         * together; unset, 10&nbsp;s. Expiry is {@code RtspException(IO)}.
         *
         * @param ms the budget, positive; checked at {@link #build()}
         */
        public Builder connectTimeoutMs(long ms) { this.connectTimeoutMs = ms; return this; }

        /**
         * How long each request waits for its reply, the write included; unset, 10&nbsp;s.
         * Expiry is {@code RtspException(TIMEOUT)}.
         *
         * @param ms the deadline, positive; checked at {@link #build()}
         */
        public Builder requestTimeoutMs(long ms) { this.requestTimeoutMs = ms; return this; }

        /**
         * The {@code User-Agent} header value; unset, {@code tst-rtp/0.1}. A value holding a
         * control byte (a CR or LF that would start another header) is refused at connect
         * with {@code RtspException(PROTOCOL)}, before anything is written.
         */
        public Builder userAgent(String ua) {
            this.userAgent = Objects.requireNonNull(ua, "userAgent"); return this;
        }

        /**
         * @throws IllegalArgumentException for an interval or timeout of 0 or below, or an
         *     interval together with {@code keepalive(false)}
         */
        public RtspClientConfig build() {
            positive(keepaliveIntervalMs, "keepaliveIntervalMs");
            positive(connectTimeoutMs, "connectTimeoutMs");
            positive(requestTimeoutMs, "requestTimeoutMs");
            if (keepaliveIntervalMs != null && !keepalive) {
                throw new IllegalArgumentException(
                    "keepaliveIntervalMs is set but keepalive(false) turns the pings off");
            }
            return new RtspClientConfig(this);
        }

        private static void positive(Long v, String name) {
            if (v != null && v <= 0) {
                throw new IllegalArgumentException(name + " must be positive, got " + v);
            }
        }
    }
}
