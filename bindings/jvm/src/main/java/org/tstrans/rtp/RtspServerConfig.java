package org.tstrans.rtp;

import java.util.Optional;

/**
 * Configuration for {@link RtspServer#start(RtspServerConfig)}. Mirrors tst-py
 * {@code tstrans.rtp.RtspServerConfig} (a frozen dataclass). Build via
 * {@link #of(String)} (defaults + bind addr) or {@link #builder()}.
 *
 * <p>{@code tlsCert}/{@code tlsKey} are PEM file paths read by the native server at
 * {@link RtspServer#start}; bad paths throw {@link org.tstrans.RtspException} of kind
 * {@code TLS} from {@code start()}. {@link Builder#build} ENFORCES that they are set
 * together (both or neither) and that the bind address carries an explicit
 * {@code rtsps://} scheme. This is no longer the only guard against that
 * misconfiguration — tst-rtp's {@code RtspServer::start()} now refuses a plaintext
 * bind carrying TLS paths too (kind {@code TLS}) — but the build()-time check stays
 * because it throws earlier, with a clearer error, before any native call
 * (mirrors tst-py).
 */
public final class RtspServerConfig {
    private final String bindAddr;
    private final Object auth;            // BasicAuth | DigestAuth | null
    private final int maxSessions;
    private final long sessionTimeoutSecs;
    private final int fanoutCapacity;
    private final long gracefulShutdownDrainMs;
    private final String tlsCert;   // nullable; PEM cert-chain FILE PATH
    private final String tlsKey;    // nullable; PEM private-key FILE PATH
    private final boolean acceptUnregisteredPublishers;

    private RtspServerConfig(Builder b) {
        this.bindAddr = b.bindAddr;
        this.auth = b.auth;
        this.maxSessions = b.maxSessions;
        this.sessionTimeoutSecs = b.sessionTimeoutSecs;
        this.fanoutCapacity = b.fanoutCapacity;
        this.gracefulShutdownDrainMs = b.gracefulShutdownDrainMs;
        this.tlsCert = b.tlsCert;
        this.tlsKey = b.tlsKey;
        this.acceptUnregisteredPublishers = b.acceptUnregisteredPublishers;
    }

    /** Config with the default field set, bound to {@code bindAddr}. */
    public static RtspServerConfig of(String bindAddr) {
        return builder().bindAddr(bindAddr).build();
    }

    public static Builder builder() { return new Builder(); }

    public String bindAddr() { return bindAddr; }

    /** @return the auth credential ({@link BasicAuth} or {@link DigestAuth}), if set. */
    public Optional<Object> auth() { return Optional.ofNullable(auth); }

    public int maxSessions() { return maxSessions; }
    public long sessionTimeoutSecs() { return sessionTimeoutSecs; }
    public int fanoutCapacity() { return fanoutCapacity; }
    public long gracefulShutdownDrainMs() { return gracefulShutdownDrainMs; }

    /** PEM certificate-chain file path for an {@code rtsps://} bind, if set. */
    public Optional<String> tlsCert() { return Optional.ofNullable(tlsCert); }

    /** PEM private-key file path for an {@code rtsps://} bind, if set. */
    public Optional<String> tlsKey() { return Optional.ofNullable(tlsKey); }

    /**
     * Whether an ANNOUNCE to an unregistered path creates a publish mount on demand,
     * handed out by {@link RtspServer#nextPublisher(long)}. Default {@code false}
     * (such an ANNOUNCE answers {@code 404}).
     */
    public boolean acceptUnregisteredPublishers() { return acceptUnregisteredPublishers; }

    @Override public String toString() {
        return "RtspServerConfig(bindAddr=" + bindAddr
            + ", auth=" + (auth != null ? "<auth>" : "None")
            + ", maxSessions=" + maxSessions
            + ", sessionTimeoutSecs=" + sessionTimeoutSecs
            + ", fanoutCapacity=" + fanoutCapacity
            + ", gracefulShutdownDrainMs=" + gracefulShutdownDrainMs
            + ", tlsCert=" + (tlsCert != null ? tlsCert : "None")
            + ", tlsKey=" + (tlsKey != null ? tlsKey : "None")
            + ", acceptUnregisteredPublishers=" + acceptUnregisteredPublishers + ")";
    }

    /** Builder for {@link RtspServerConfig}. Defaults match tst-py. */
    public static final class Builder {
        private String bindAddr = "0.0.0.0:8554";
        private Object auth;
        private int maxSessions = 100;
        private long sessionTimeoutSecs = 60;
        private int fanoutCapacity = 256;
        private long gracefulShutdownDrainMs = 2000;
        private String tlsCert;
        private String tlsKey;
        private boolean acceptUnregisteredPublishers;

        private Builder() {}

        public Builder bindAddr(String v) {
            this.bindAddr = java.util.Objects.requireNonNull(v, "bindAddr");
            return this;
        }

        /** @param auth a {@link BasicAuth}, {@link DigestAuth}, or {@code null}. */
        public Builder auth(Object auth) {
            if (auth != null && !(auth instanceof BasicAuth) && !(auth instanceof DigestAuth)) {
                throw new IllegalArgumentException("auth must be BasicAuth, DigestAuth, or null");
            }
            this.auth = auth;
            return this;
        }

        public Builder maxSessions(int v) { this.maxSessions = v; return this; }
        public Builder sessionTimeoutSecs(long v) { this.sessionTimeoutSecs = v; return this; }
        public Builder fanoutCapacity(int v) { this.fanoutCapacity = v; return this; }
        public Builder gracefulShutdownDrainMs(long v) { this.gracefulShutdownDrainMs = v; return this; }

        /** PEM certificate-chain file path ({@code rtsps://} binds). Set with {@link #tlsKey}. */
        public Builder tlsCert(String pemPath) { this.tlsCert = pemPath; return this; }

        /** PEM private-key file path ({@code rtsps://} binds). Set with {@link #tlsCert}. */
        public Builder tlsKey(String pemPath) { this.tlsKey = pemPath; return this; }

        /**
         * Let an ANNOUNCE to an unregistered path create a publish mount on demand
         * (default {@code false}). With it on, anyone who can reach the port, and pass
         * {@link #auth} when it is set, can create mounts. Up to 64 handles wait for
         * {@link RtspServer#nextPublisher(long)} and the server holds up to 256
         * on-demand mounts; an ANNOUNCE past either bound answers {@code 503}.
         */
        public Builder acceptUnregisteredPublishers(boolean v) {
            this.acceptUnregisteredPublishers = v;
            return this;
        }

        public RtspServerConfig build() {
            if (maxSessions <= 0)
                throw new IllegalArgumentException("maxSessions must be > 0; got " + maxSessions);
            if (sessionTimeoutSecs <= 0)
                throw new IllegalArgumentException(
                    "sessionTimeoutSecs must be > 0; got " + sessionTimeoutSecs);
            if (fanoutCapacity <= 0)
                throw new IllegalArgumentException(
                    "fanoutCapacity must be > 0; got " + fanoutCapacity);
            if (gracefulShutdownDrainMs < 0)
                throw new IllegalArgumentException(
                    "gracefulShutdownDrainMs must be >= 0; got " + gracefulShutdownDrainMs);
            if ((tlsCert == null) != (tlsKey == null)) {
                throw new IllegalArgumentException(
                    "tlsCert and tlsKey must be set together (both or neither)");
            }
            if (tlsCert != null && !bindAddr.startsWith("rtsps://")) {
                throw new IllegalArgumentException(
                    "tlsCert/tlsKey require an explicit rtsps:// bind address"
                        + " (got \"" + bindAddr + "\")");
            }
            return new RtspServerConfig(this);
        }
    }
}
