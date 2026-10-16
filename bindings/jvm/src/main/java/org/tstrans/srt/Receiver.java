package org.tstrans.srt;

import org.tstrans.BindingException;
import org.tstrans.SrtException;

/**
 * The 0.8 name of {@link org.tstrans.shells.Receiver} over SRT.
 *
 * @deprecated use {@link org.tstrans.shells.Receiver#open(String)}; removed in 0.10.0.
 */
@Deprecated(since = "0.9.0", forRemoval = true)
public final class Receiver extends org.tstrans.shells.Receiver {
    private Receiver(long handle) { super(handle); }

    /**
     * @deprecated use {@link org.tstrans.shells.Receiver#open(String)}.
     */
    @Deprecated(since = "0.9.0", forRemoval = true)
    public static Receiver fromUrl(String url) throws SrtException {
        ShellAdapters.requireSrt(url, "Receiver");
        try {
            return new Receiver(openHandle(url));
        } catch (BindingException e) {
            throw ShellAdapters.srt(e, url);
        }
    }
}
