package org.tstrans.rtp;

import java.util.Objects;
import java.util.Optional;

/**
 * HTTP Basic auth credentials per RFC 7617. Sent only after a 401 challenge.
 * Mirrors tst-py {@code tstrans.rtp.BasicAuth}: the password is held in memory
 * and never re-exposed through the public surface (only {@link #user()} and
 * {@link #realm()} are readable); {@link #toString()} redacts it. As a client
 * credential it answers Digest when the server offers it and Basic otherwise;
 * use {@link DigestAuth} to refuse Basic.
 */
public final class BasicAuth {
    private final String user;
    private final String password;   // no public accessor; read by RtspClient.connect
    private final String realm;      // nullable

    public BasicAuth(String user, String password) { this(user, password, null); }

    public BasicAuth(String user, String password, String realm) {
        this.user = Objects.requireNonNull(user, "user");
        this.password = Objects.requireNonNull(password, "password");
        this.realm = realm;
    }

    public String user() { return user; }

    public Optional<String> realm() { return Optional.ofNullable(realm); }

    /** Package-private: handed to the native connect, never exposed publicly. */
    String password() { return password; }

    @Override public String toString() {
        return "BasicAuth(user=" + user + ", password=<redacted>, realm=" + realm + ")";
    }
}
