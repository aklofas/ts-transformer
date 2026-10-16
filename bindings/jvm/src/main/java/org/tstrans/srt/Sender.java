package org.tstrans.srt;

import org.tstrans.BindingException;
import org.tstrans.SrtException;

/**
 * The 0.8 name of {@link org.tstrans.shells.Sender} over SRT.
 *
 * @deprecated use {@link org.tstrans.shells.Sender#open(String)}; removed in 0.10.0.
 */
@Deprecated(since = "0.9.0", forRemoval = true)
public final class Sender extends org.tstrans.shells.Sender {
    private Sender(long handle) { super(handle); }

    /**
     * @deprecated use {@link org.tstrans.shells.Sender#open(String)}.
     */
    @Deprecated(since = "0.9.0", forRemoval = true)
    public static Sender fromUrl(String url) throws SrtException {
        ShellAdapters.requireSrt(url, "Sender");
        try {
            return new Sender(openHandle(url));
        } catch (BindingException e) {
            throw ShellAdapters.srt(e, url);
        }
    }
}
