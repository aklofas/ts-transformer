package org.tstrans.rtp;

import org.tstrans.NativeLoader;
import org.tstrans.RtspException;

/**
 * Static facade for RTSP client connections. {@link #connect(RtspClientConfig)}
 * runs the full OPTIONS / DESCRIBE / SETUP / PLAY exchange against the server and
 * returns a live {@link RtspSession} in PLAY state. Mirrors tst-py
 * {@code tstrans.rtp.RtspClient}.
 *
 * <p><b>{@code rtsps://} is supported.</b> {@code RtspClientConfig.tlsRootCertsPem},
 * when set, is parsed as a PEM bundle and used as the custom trust anchors for
 * server-certificate verification (for private-CA deployments); when unset, the
 * connection verifies against the platform's native trust roots. A malformed PEM
 * bundle, a certificate rejected as a trust anchor, or an empty bundle all surface
 * {@link RtspException} of kind {@code TLS} before any connect I/O begins.
 *
 * <p><b>Pass-through config fields.</b> {@code transportPref} and {@code rtspVersion}
 * are likewise informational/pass-through: the underlying tst-rtp connect derives
 * the transport (from a {@code ?transport=udp|tcp} URL query) and the version (from
 * the {@code rtsp://} vs {@code rtsps://} scheme) from the URL, not from these
 * fields. They round-trip through the config unchanged.
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
        String authUser = null, authPassword = null;
        Object a = config.auth().orElse(null);
        if (a instanceof BasicAuth b) {
            authUser = b.user(); authPassword = b.password();
        } else if (a instanceof DigestAuth d) {
            authUser = d.user(); authPassword = d.password();
        }
        byte[] tlsRoots = config.tlsRootCertsPem().orElse(null);
        long h = nConnect(config.url(), authUser, authPassword, config.keepalive(), tlsRoots);
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
        String authUser = null, authPassword = null;
        Object a = config.auth().orElse(null);
        if (a instanceof BasicAuth b) {
            authUser = b.user(); authPassword = b.password();
        } else if (a instanceof DigestAuth d) {
            authUser = d.user(); authPassword = d.password();
        }
        byte[] tlsRoots = config.tlsRootCertsPem().orElse(null);
        long h = nConnectH264(config.url(), authUser, authPassword, config.keepalive(), tlsRoots);
        if (h == 0) {
            throw new RtspException(RtspException.Kind.PROTOCOL,
                "nConnectH264 returned 0 without throwing");
        }
        return new RtspSession(h);
    }

    // authUser/authPassword null when no auth. The algorithm is NOT passed: tst-rtp's
    // challenge handler picks it from the server's WWW-Authenticate header (matches
    // tst-py, which captures DigestAuth.algorithm for introspection only).
    // tlsRootCertsPem null means platform native trust roots.
    private static native long nConnect(String url, String authUser, String authPassword,
        boolean keepalive, byte[] tlsRootCertsPem) throws RtspException;

    private static native long nConnectH264(String url, String authUser, String authPassword,
        boolean keepalive, byte[] tlsRootCertsPem) throws RtspException;
}
