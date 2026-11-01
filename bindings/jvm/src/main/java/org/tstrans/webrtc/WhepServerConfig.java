package org.tstrans.webrtc;

import java.util.Objects;
import org.tstrans.Redact;

/**
 * How a {@link WhepServer} binds. The {@code whep://} or {@code wheps://} URL
 * carries every setting: the bind address and port, and the query keys
 * {@code media_port}, {@code advertise}, {@code max_viewers}, {@code token},
 * {@code cors_origin}, {@code cert} and {@code key}. A record, so settings
 * that are not URL keys can be added without changing {@link WhepServer#bind}.
 *
 * @param bindUrl the {@code whep://ip:port} or {@code wheps://ip:port?cert=…&key=…}
 *     URL; the host is a literal IP address
 */
public record WhepServerConfig(String bindUrl) {
    public WhepServerConfig {
        Objects.requireNonNull(bindUrl, "bindUrl");
    }

    @Override public String toString() {
        return "WhepServerConfig[bindUrl=" + Redact.url(bindUrl) + "]";
    }
}
