package org.tstrans.rtp;

import org.tstrans.BindingException;
import org.tstrans.MuxException;
import org.tstrans.RtpException;
import org.tstrans.mpegts.MuxerConfig;

/**
 * The 0.8 name of {@link org.tstrans.shells.MuxSender} over RTP.
 *
 * @deprecated use {@link org.tstrans.shells.MuxSender#open(String, MuxerConfig)}; removed in 0.10.0.
 */
@Deprecated(since = "0.9.0", forRemoval = true)
public final class MuxSender extends org.tstrans.shells.MuxSender {
    private MuxSender(long handle) { super(handle); }

    /** Default UDP datagram size (RTP header + TS payload). */
    public static final int DEFAULT_PKT_SIZE = 1316;

    private static String withPktSize(String url, int pktSize) {
        return url + (url.contains("?") ? "&" : "?") + "pkt_size=" + pktSize;
    }

    /**
     * @deprecated use {@link org.tstrans.shells.MuxSender#open(String, MuxerConfig)}.
     */
    @Deprecated(since = "0.9.0", forRemoval = true)
    public static MuxSender fromUrl(String url, MuxerConfig programConfig) throws RtpException, MuxException {
        ShellAdapters.requireRtp(url, "MuxSender");
        try {
            return new MuxSender(openHandle(url, programConfig));
        } catch (MuxException e) {
            throw e;
        } catch (BindingException e) {
            throw ShellAdapters.rtp(e, url);
        }
    }

    /**
     * @deprecated use {@code open(url + "?pkt_size=" + pktSize, programConfig)} on {@link org.tstrans.shells.MuxSender#open(String, MuxerConfig)}.
     */
    @Deprecated(since = "0.9.0", forRemoval = true)
    public static MuxSender fromUrl(String url, MuxerConfig programConfig, int pktSize) throws RtpException, MuxException {
        if (pktSize < 0) throw new IllegalArgumentException("pktSize must be >= 0: " + pktSize);
        ShellAdapters.requireRtp(url, "MuxSender");
        try {
            return new MuxSender(openHandle(withPktSize(url, pktSize), programConfig));
        } catch (MuxException e) {
            throw e;
        } catch (BindingException e) {
            throw ShellAdapters.rtp(e, url);
        }
    }
}
