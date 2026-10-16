package org.tstrans.rtp;

import org.tstrans.RtpException;

/**
 * The 0.8 name of {@link RecvTransport}, the raw RTP receiving transport.
 *
 * @deprecated use {@link RecvTransport} for the raw transport, or
 *     {@link org.tstrans.shells.Receiver#open(String)} for the TS shell over
 *     {@code rtp://}; removed in 0.10.0.
 */
@Deprecated(since = "0.9.0", forRemoval = true)
public final class Receiver extends RecvTransport {
    private Receiver(long handle) { super(handle); }

    /**
     * @deprecated use {@link RecvTransport#fromUrl(String)}.
     */
    @Deprecated(since = "0.9.0", forRemoval = true)
    public static Receiver fromUrl(String url) throws RtpException {
        return new Receiver(openHandle(url));
    }
}
