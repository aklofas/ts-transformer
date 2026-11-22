package org.tstrans.rtp;

import java.util.Optional;
import java.util.OptionalInt;
import org.tstrans.Redact;

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
    private final int maxReaders;   // -1: unset
    private final long sessionTimeoutSecs;
    private final int fanoutCapacity;
    private final long gracefulShutdownDrainMs;
    private final String tlsCert;   // nullable; PEM cert-chain FILE PATH
    private final String tlsKey;    // nullable; PEM private-key FILE PATH
    private final boolean acceptUnregisteredPublishers;
    private final boolean endPublishMountOnPublisherExit;
    private final long publishIdleTimeoutMs;
    private final MountRule defaultMountRule;  // nullable
    private final Boolean allowUdpMedia;       // nullable: decided by the bind's scheme
    private final boolean acceptDigestWithoutQop;

    private RtspServerConfig(Builder b) {
        this.bindAddr = b.bindAddr;
        this.auth = b.auth;
        this.maxSessions = b.maxSessions;
        this.maxReaders = b.maxReaders == null ? -1 : b.maxReaders;
        this.sessionTimeoutSecs = b.sessionTimeoutSecs;
        this.fanoutCapacity = b.fanoutCapacity;
        this.gracefulShutdownDrainMs = b.gracefulShutdownDrainMs;
        this.tlsCert = b.tlsCert;
        this.tlsKey = b.tlsKey;
        this.acceptUnregisteredPublishers = b.acceptUnregisteredPublishers;
        this.endPublishMountOnPublisherExit = b.endPublishMountOnPublisherExit;
        this.publishIdleTimeoutMs = b.publishIdleTimeoutMs;
        this.defaultMountRule = b.defaultMountRule;
        this.allowUdpMedia = b.allowUdpMedia;
        this.acceptDigestWithoutQop = b.acceptDigestWithoutQop;
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

    /**
     * The cap on reader sessions at once, if set (see {@link Builder#maxReaders(int)});
     * empty (the default) means no cap besides {@link #maxSessions()}.
     */
    public OptionalInt maxReaders() {
        return maxReaders < 0 ? OptionalInt.empty() : OptionalInt.of(maxReaders);
    }
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

    /**
     * Server-wide default for publish mounts: whether a mount's receiver ends
     * ({@code END_OF_STREAM}) when a publisher that reached RECORD leaves; a publisher that
     * never recorded does not end it. Default {@code false} (the mount idles for the next
     * publisher). To accept a publisher after the mount ended, remove the mount and add it
     * again; an ended on-demand mount stays in the table, and counts toward the on-demand cap,
     * until it is removed. {@link PublishMountConfig} overrides it per mount;
     * on-demand mounts always follow it.
     */
    public boolean endPublishMountOnPublisherExit() { return endPublishMountOnPublisherExit; }

    /**
     * Milliseconds without media after which a recording publisher's session is ended,
     * timed from RECORD or its last RTP packet; {@code 0} (the default) means unset, so
     * only {@link #sessionTimeoutSecs()} governs. A publisher that has not reached RECORD
     * is never ended by it. A value longer than the session timeout still matters: a
     * publisher that keeps its session alive with RTSP requests while sending no media is
     * ended by this timeout, not the session timeout.
     */
    public long publishIdleTimeoutMs() { return publishIdleTimeoutMs; }

    /**
     * The rule for mount paths no {@link MountRules} entry names, if set. Without one the
     * default is {@link MountPolicy#inherit()} for both directions.
     */
    public Optional<MountRule> defaultMountRule() { return Optional.ofNullable(defaultMountRule); }

    /**
     * Whether sessions may carry media over UDP, if set: a reader's or a publisher's SETUP
     * with a transport that is not TCP-interleaved, and multicast mounts. A server that does
     * not allow it answers such a SETUP {@code 461 Unsupported Transport}. Empty (the
     * default) means yes on an {@code rtsp://} bind and NO on an {@code rtsps://} bind, so
     * that an {@code rtsps://} server sends and takes media only inside the TLS connection.
     */
    public Optional<Boolean> allowUdpMedia() { return Optional.ofNullable(allowUdpMedia); }

    /**
     * Whether a Digest answer that carries no {@code qop} is accepted (the RFC 2069 form
     * GStreamer 1.24 sends). Default {@code false}. See {@link
     * Builder#acceptDigestWithoutQop(boolean)}.
     */
    public boolean acceptDigestWithoutQop() { return acceptDigestWithoutQop; }

    @Override public String toString() {
        return "RtspServerConfig(bindAddr=" + Redact.url(bindAddr)
            + ", auth=" + (auth != null ? "<auth>" : "None")
            + ", maxSessions=" + maxSessions
            + ", maxReaders=" + (maxReaders < 0 ? "None" : Integer.toString(maxReaders))
            + ", sessionTimeoutSecs=" + sessionTimeoutSecs
            + ", fanoutCapacity=" + fanoutCapacity
            + ", gracefulShutdownDrainMs=" + gracefulShutdownDrainMs
            + ", tlsCert=" + (tlsCert != null ? tlsCert : "None")
            + ", tlsKey=" + (tlsKey != null ? tlsKey : "None")
            + ", acceptUnregisteredPublishers=" + acceptUnregisteredPublishers
            + ", endPublishMountOnPublisherExit=" + endPublishMountOnPublisherExit
            + ", publishIdleTimeoutMs=" + publishIdleTimeoutMs
            + ", defaultMountRule=" + (defaultMountRule != null ? defaultMountRule : "None")
            + ", allowUdpMedia=" + (allowUdpMedia != null ? allowUdpMedia : "None")
            + ", acceptDigestWithoutQop=" + acceptDigestWithoutQop + ")";
    }

    /** Builder for {@link RtspServerConfig}. Defaults match tst-py. */
    public static final class Builder {
        private String bindAddr = "0.0.0.0:8554";
        private Object auth;
        private int maxSessions = 100;
        private Integer maxReaders;   // null: unset
        private long sessionTimeoutSecs = 60;
        private int fanoutCapacity = 256;
        private long gracefulShutdownDrainMs = 2000;
        private String tlsCert;
        private String tlsKey;
        private boolean acceptUnregisteredPublishers;
        private boolean endPublishMountOnPublisherExit;
        private long publishIdleTimeoutMs;
        private MountRule defaultMountRule;
        private Boolean allowUdpMedia;
        private boolean acceptDigestWithoutQop;

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

        /**
         * Cap on reader sessions at once. A reader holds a slot from its SETUP to the end of
         * its role: its TEARDOWN, a refusal on its own mount or the end of its connection; a
         * reader set up again keeps the one slot it holds. Readers of a multicast mount count;
         * publishers do not. A reader's SETUP over the cap is answered {@code 453 Not Enough
         * Bandwidth} and its connection is closed after the reply (this library's client
         * reports it as {@code RtspException} of kind {@code PROTOCOL}).
         *
         * <p>Unset (the default): no cap besides {@link #maxSessions(int)}. {@code 0}: no
         * readers at all, an ingest-only server. With {@code n} below {@code maxSessions} the
         * difference can never be held by readers, so readers cannot use up every connection
         * of a server that also takes publishers; a cap at or above {@code maxSessions}
         * protects nothing. A connection without a session still holds one of the {@code
         * maxSessions} for up to 30&nbsp;s, so a publisher's connection is not promised.
         *
         * <p><b>Stability: Provisional.</b>
         *
         * @param v the cap, {@code 0} or more ({@link #build()} refuses a negative one)
         */
        public Builder maxReaders(int v) { this.maxReaders = v; return this; }
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
         * on-demand mounts; an ANNOUNCE past either bound answers {@code 503}. A
         * connected elementary-track publisher can hold about 46 MB at worst, about
         * 4.6 GB at the default {@code maxSessions} of 100, so set {@link #auth} and
         * size {@code maxSessions} before enabling this on a reachable port.
         */
        public Builder acceptUnregisteredPublishers(boolean v) {
            this.acceptUnregisteredPublishers = v;
            return this;
        }

        /**
         * Server-wide default: end a publish mount's receiver when a publisher that
         * reached RECORD leaves, and refuse later publishers on that mount. Default
         * {@code false}. See {@link RtspServerConfig#endPublishMountOnPublisherExit()}.
         */
        public Builder endPublishMountOnPublisherExit(boolean v) {
            this.endPublishMountOnPublisherExit = v;
            return this;
        }

        /**
         * End a recording publisher's session after this many milliseconds without media.
         * {@code 0} (the default) leaves it unset. See {@link
         * RtspServerConfig#publishIdleTimeoutMs()}.
         */
        public Builder publishIdleTimeoutMs(long v) {
            this.publishIdleTimeoutMs = v;
            return this;
        }

        /**
         * The rule for mount paths no {@link MountRules} entry names, in force from the
         * first request (default: {@link MountPolicy#inherit()} for both directions).
         */
        public Builder defaultMountRule(MountRule rule) {
            this.defaultMountRule = rule;
            return this;
        }

        /**
         * Allow or refuse media over UDP: a reader's or a publisher's SETUP with a transport
         * that is not TCP-interleaved (refused with {@code 461 Unsupported Transport}), and
         * multicast mounts. Unset, it is allowed on an {@code rtsp://} bind and refused on an
         * {@code rtsps://} bind. {@code allowUdpMedia(true)} on an {@code rtsps://} server
         * restores the behaviour of 0.10.0, in which the media travels unencrypted; {@code
         * allowUdpMedia(false)} makes a plain server TCP-only.
         *
         * <p><b>Stability: Provisional.</b>
         */
        public Builder allowUdpMedia(boolean v) {
            this.allowUdpMedia = v;
            return this;
        }

        /**
         * Accept a Digest answer that carries no {@code qop}: the RFC 2069 form, which
         * GStreamer 1.24's {@code rtspsrc} and {@code rtspclientsink} send. Applies to both
         * Digest schemes, the server's own credential and every mount rule's; the challenge
         * still offers {@code qop="auth"}, and an answer with {@code qop} is checked as
         * before, its nonce count included. Default {@code false}, so such an answer is
         * refused as a downgrade.
         *
         * <p>What is given up, only for a client that sends no {@code qop} and only with this
         * on: replay protection inside one connection. Such an answer has no nonce count, so
         * a captured {@code Authorization} value for one method and URI can be sent again on
         * the same connection while it lives. The nonce is still 128 random bits per TCP
         * connection and is never accepted on another.
         *
         * <p>GStreamer 1.24 authenticates with Basic, or with Digest MD5 under this option,
         * never with Digest SHA-256: it answers a SHA-256 challenge with an MD5 response.
         *
         * <p><b>Stability: Provisional.</b>
         */
        public Builder acceptDigestWithoutQop(boolean v) {
            this.acceptDigestWithoutQop = v;
            return this;
        }

        public RtspServerConfig build() {
            if (maxSessions <= 0)
                throw new IllegalArgumentException("maxSessions must be > 0; got " + maxSessions);
            if (maxReaders != null && maxReaders < 0)
                throw new IllegalArgumentException("maxReaders must be >= 0; got " + maxReaders);
            if (sessionTimeoutSecs <= 0)
                throw new IllegalArgumentException(
                    "sessionTimeoutSecs must be > 0; got " + sessionTimeoutSecs);
            if (fanoutCapacity <= 0)
                throw new IllegalArgumentException(
                    "fanoutCapacity must be > 0; got " + fanoutCapacity);
            if (publishIdleTimeoutMs < 0)
                throw new IllegalArgumentException(
                    "publishIdleTimeoutMs must be >= 0; got " + publishIdleTimeoutMs);
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
                        + " (got \"" + Redact.url(bindAddr) + "\")");
            }
            return new RtspServerConfig(this);
        }
    }
}
