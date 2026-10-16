package org.tstrans.rtp;

import org.tstrans.RtpException;

/**
 * The 0.8 name of {@link Transport}, the raw RTP sending transport.
 *
 * @deprecated use {@link Transport} for the raw transport, or
 *     {@link org.tstrans.shells.Sender#open(String)} for the TS shell over
 *     {@code rtp://}; removed in 0.10.0.
 */
@Deprecated(since = "0.9.0", forRemoval = true)
public final class Sender extends Transport {
    private Sender(long handle) { super(handle); }

    /**
     * @deprecated use {@link Transport#fromUrl(String)}.
     */
    @Deprecated(since = "0.9.0", forRemoval = true)
    public static Sender fromUrl(String url) throws RtpException {
        return fromUrl(url, DEFAULT_PKT_SIZE, null);
    }

    /**
     * @deprecated use {@link Transport#fromUrl(String, int, Long)}.
     */
    @Deprecated(since = "0.9.0", forRemoval = true)
    public static Sender fromUrl(String url, int pktSize, Long ssrc) throws RtpException {
        return new Sender(openHandle(url, pktSize, ssrc));
    }
}
