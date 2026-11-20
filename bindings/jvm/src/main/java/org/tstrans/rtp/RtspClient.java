package org.tstrans.rtp;

import java.util.Objects;
import org.tstrans.NativeLoader;
import org.tstrans.RtspException;

/**
 * Static facade for RTSP client connections. {@link #connect(RtspClientConfig)}
 * runs the full OPTIONS / DESCRIBE / SETUP / PLAY exchange against the server and
 * returns a live {@link RtspSession} in PLAY state; {@link #push} publishes instead
 * (ANNOUNCE / SETUP / RECORD) and returns an {@link RtspPushSession}. Mirrors tst-py
 * {@code tstrans.rtp.RtspClient}.
 *
 * <p><b>{@code rtsps://} is supported.</b> {@code RtspClientConfig.tlsRootCertsPem},
 * when set, is parsed as a PEM bundle and used as the custom trust anchors for
 * server-certificate verification (for private-CA deployments); when unset, the
 * connection verifies against the platform's native trust roots. A malformed PEM
 * bundle, a certificate rejected as a trust anchor, or an empty bundle all surface
 * {@link RtspException} of kind {@code TLS} before any connect I/O begins.
 *
 * <p>{@code transportPref} and {@code rtspVersion} override the URL's
 * {@code ?transport=} / {@code ?rtsp_version=} when they are not at their defaults.
 */
public final class RtspClient {
    static { NativeLoader.load(); }

    private RtspClient() {}

    /**
     * Connect and drive the control-plane to PLAY (MPEG-TS media).
     *
     * @param config the connection configuration
     * @return a live session in PLAY state
     * @throws RtspException on any control-plane failure (URL parse → {@code PROTOCOL};
     *     TLS setup/handshake/verification failure → {@code TLS}; refused/timeout →
     *     {@code IO}/{@code TIMEOUT}; 401/404 → {@code AUTH_REQUIRED}/{@code NOT_FOUND}; …)
     */
    public static RtspSession connect(RtspClientConfig config) throws RtspException {
        ClientAuth auth = ClientAuth.of(config);
        byte[] tlsRoots = config.tlsRootCertsPem().orElse(null);
        long h = nConnect(effectiveUrl(config), auth.user(), auth.password(), auth.digestOnly(),
            config.keepalive(), tlsRoots, config.keepaliveIntervalMs().orElse(0),
            config.connectTimeoutMs().orElse(0), config.requestTimeoutMs().orElse(0),
            config.userAgent().orElse(null));
        if (h == 0) {
            throw new RtspException(RtspException.Kind.PROTOCOL,
                "nConnect returned 0 without throwing");
        }
        return new RtspSession(h);
    }

    /**
     * Connect and drive the control-plane to PLAY for an H.264 media stream.
     *
     * <p>Twin of {@link #connect(RtspClientConfig)}, but uses
     * {@code setup_h264_auto} instead of {@code setup_mp2t_auto}. The resulting
     * session stashes the negotiated {@code H264DepayConfig} (payload type and
     * out-of-band SPS/PPS from the SDP {@code a=fmtp:} line) so that
     * {@link RtspSession#intoH264Receiver()} can configure the depacketizer
     * without the caller needing to inspect the SDP manually.
     *
     * <p><b>pause() / play() are unavailable after intoH264Receiver().</b>
     * Calling {@link RtspSession#intoH264Receiver()} consumes the session wrapper
     * — control-plane methods ({@link RtspSession#pause()} / {@link RtspSession#play()})
     * throw {@link IllegalStateException} afterward. This differs from the
     * {@link #connect(RtspClientConfig)} path, where {@link RtspSession#intoDemuxReceiver()}
     * leaves those methods open, and from the Python binding (which keeps the session
     * wrapper usable after {@code session.into_h264_receiver()}).
     *
     * @param config the connection configuration
     * @return a live session in PLAY state, with H.264 depacketizer config stashed
     * @throws RtspException {@code MOUNT} when the SDP has no H.264 media or more
     *     than one H.264 media (use {@link #connect(RtspClientConfig)} for MP2T
     *     streams); {@code UNSUPPORTED_TRANSPORT} for packetization mode 2; other
     *     control-plane failures as in {@link #connect(RtspClientConfig)}
     */
    public static RtspSession connectH264(RtspClientConfig config) throws RtspException {
        ClientAuth auth = ClientAuth.of(config);
        byte[] tlsRoots = config.tlsRootCertsPem().orElse(null);
        long h = nConnectH264(effectiveUrl(config), auth.user(), auth.password(), auth.digestOnly(),
            config.keepalive(), tlsRoots, config.keepaliveIntervalMs().orElse(0),
            config.connectTimeoutMs().orElse(0), config.requestTimeoutMs().orElse(0),
            config.userAgent().orElse(null));
        if (h == 0) {
            throw new RtspException(RtspException.Kind.PROTOCOL,
                "nConnectH264 returned 0 without throwing");
        }
        return new RtspSession(h);
    }

    /**
     * Publish to the server: connect, ANNOUNCE {@code description} as an SDP body at the
     * config's URL, SETUP each of its tracks for recording interleaved on the control
     * connection ({@code RTP/AVP/TCP}, {@code mode=record}), and RECORD. Returns the
     * recording session that sends the media.
     *
     * <p>The config is read as by {@link #connect(RtspClientConfig)}: the URL, the
     * credential, {@code rtsps://} trust roots and keepalive pings. Publishing over UDP is
     * not implemented: {@link TransportPref#UDP} (or {@code ?transport=udp}) is refused.
     * RTSP 2.0 has no ANNOUNCE: {@link RtspVersion#V2_0} is refused. Both are refused
     * before the request concerned is written. On any failure the connection is closed;
     * with a session open, TEARDOWN is written first.
     *
     * @param config the connection configuration
     * @param description what the session announces
     * @return the recording session
     * @throws RtspException {@code UNSUPPORTED_TRANSPORT} for a UDP transport preference
     *     ({@link TransportPref#UDP} or {@code ?transport=udp}, refused before any SETUP
     *     is written) and for a SETUP reply whose {@code Transport} header does not
     *     parse; {@code PROTOCOL} for RTSP 2.0 (no ANNOUNCE written), a URL that does not
     *     parse, a description that lacks a parameter set, a SETUP reply the client
     *     cannot use (no {@code Transport} header, a transport that is not interleaved,
     *     no session, another session, or a channel pair it cannot take), and a refused
     *     status other than {@code 404} and {@code 401} (a server that does not take the
     *     codec answers {@code 415}); {@code NOT_FOUND} for a {@code 404}; for a
     *     {@code 401}, which the client answers once with the configured credential:
     *     {@code PROTOCOL} when it carries no {@code WWW-Authenticate} header,
     *     {@code AUTH_FAILED} when no credential is configured or the retry is answered
     *     {@code 401} again, and {@code AUTH_REQUIRED} when the challenge offers no scheme
     *     the credential may answer; {@code IO}, {@code TLS} and {@code TIMEOUT} for the
     *     connection, its TLS handshake and a request left unanswered
     * @throws NullPointerException if an argument is {@code null}
     */
    public static RtspPushSession push(RtspClientConfig config, PushDescription description)
            throws RtspException {
        Objects.requireNonNull(config, "config");
        Objects.requireNonNull(description, "description");
        ClientAuth auth = ClientAuth.of(config);
        byte[] tlsRoots = config.tlsRootCertsPem().orElse(null);
        long h = nPush(effectiveUrl(config), auth.user(), auth.password(), auth.digestOnly(),
            config.keepalive(), tlsRoots, config.keepaliveIntervalMs().orElse(0),
            config.connectTimeoutMs().orElse(0), config.requestTimeoutMs().orElse(0),
            config.userAgent().orElse(null), description.kind(), description.klv(),
            description.parameterSets());
        if (h == 0) {
            throw new RtspException(RtspException.Kind.PROTOCOL, "nPush returned 0 without throwing");
        }
        return new RtspPushSession(h);
    }

    /**
     * The credential fields the native factories take: {@code null} user and password
     * without a credential; {@code digestOnly} for a {@link DigestAuth}, which refuses a
     * challenge that offers no Digest.
     */
    private record ClientAuth(String user, String password, boolean digestOnly) {
        static ClientAuth of(RtspClientConfig config) {
            Object a = config.auth().orElse(null);
            if (a instanceof BasicAuth b) return new ClientAuth(b.user(), b.password(), false);
            if (a instanceof DigestAuth d) return new ClientAuth(d.user(), d.password(), true);
            return new ClientAuth(null, null, false);
        }
    }

    /**
     * The URL the native connect sees: {@code config.url()} with a
     * non-default {@code transportPref} / {@code rtspVersion} appended as the
     * {@code ?transport=} / {@code ?rtsp_version=} query keys. The URL parser
     * lets the last occurrence of a key win, so a non-default config value
     * overrides the URL's own; the defaults ({@code AUTO}, {@code V1_0})
     * leave the URL in charge, as the C builder and the Python binding do.
     */
    static String effectiveUrl(RtspClientConfig config) {
        StringBuilder u = new StringBuilder(config.url());
        char sep = config.url().indexOf('?') >= 0 ? '&' : '?';
        if (config.transportPref() != TransportPref.AUTO) {
            u.append(sep).append("transport=")
                .append(config.transportPref() == TransportPref.TCP ? "tcp" : "udp");
            sep = '&';
        }
        if (config.rtspVersion() != RtspVersion.V1_0) {
            u.append(sep).append("rtsp_version=2.0");
        }
        return u.toString();
    }

    // authUser/authPassword null when no auth. The algorithm is NOT passed: tst-rtp's
    // challenge handler picks it from the server's WWW-Authenticate header (matches
    // tst-py, which captures DigestAuth.algorithm for introspection only).
    // digestOnly true (a DigestAuth credential) refuses a challenge that offers no Digest.
    // tlsRootCertsPem null means platform native trust roots. The three millisecond
    // values are 0 and userAgent null where the config leaves them unset.
    private static native long nConnect(String url, String authUser, String authPassword,
        boolean digestOnly, boolean keepalive, byte[] tlsRootCertsPem, long keepaliveIntervalMs,
        long connectTimeoutMs, long requestTimeoutMs, String userAgent) throws RtspException;

    private static native long nConnectH264(String url, String authUser, String authPassword,
        boolean digestOnly, boolean keepalive, byte[] tlsRootCertsPem, long keepaliveIntervalMs,
        long connectTimeoutMs, long requestTimeoutMs, String userAgent) throws RtspException;

    // kind is a PushDescription.KIND_* code; parameterSets as PushDescription keeps them.
    private static native long nPush(String url, String authUser, String authPassword,
        boolean digestOnly, boolean keepalive, byte[] tlsRootCertsPem, long keepaliveIntervalMs,
        long connectTimeoutMs, long requestTimeoutMs, String userAgent, int kind, boolean klv,
        byte[][] parameterSets) throws RtspException;
}
