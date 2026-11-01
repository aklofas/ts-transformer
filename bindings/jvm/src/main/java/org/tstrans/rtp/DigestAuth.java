package org.tstrans.rtp;

import java.util.Objects;
import java.util.Optional;

/**
 * HTTP Digest auth credentials per RFC 7616 (MD5 + SHA-256) / RFC 2617 (legacy
 * MD5). Mirrors tst-py {@code tstrans.rtp.DigestAuth}: same secret-handling story
 * as {@link BasicAuth} — password held in memory, never publicly re-exposed,
 * redacted in {@link #toString()}.
 *
 * <p>As a client credential it never falls back to Basic: a server that offers
 * no Digest challenge fails the request with an {@code RtspException} and the
 * password is never sent in Basic form. Use {@link BasicAuth} to accept either
 * scheme.
 */
public final class DigestAuth {
    private final String user;
    private final String password;   // no public accessor; read by RtspClient.connect
    private final DigestAlgorithm algorithm;
    private final String realm;      // nullable

    public DigestAuth(String user, String password) {
        this(user, password, DigestAlgorithm.MD5, null);
    }
    public DigestAuth(String user, String password, DigestAlgorithm algorithm) {
        this(user, password, algorithm, null);
    }
    public DigestAuth(String user, String password, DigestAlgorithm algorithm, String realm) {
        this.user = Objects.requireNonNull(user, "user");
        this.password = Objects.requireNonNull(password, "password");
        this.algorithm = Objects.requireNonNull(algorithm, "algorithm");
        this.realm = realm;
    }

    public String user() { return user; }

    /**
     * The caller-selected digest algorithm. <b>Informational:</b> tst-rtp's
     * challenge handler picks the actual algorithm from the server's
     * {@code WWW-Authenticate} header at connect time, so this value is NOT
     * forwarded to the native connect (it's retained for caller introspection,
     * matching tst-py).
     */
    public DigestAlgorithm algorithm() { return algorithm; }
    public Optional<String> realm() { return Optional.ofNullable(realm); }

    /** Package-private: handed to the native connect, never exposed publicly. */
    String password() { return password; }

    @Override public String toString() {
        return "DigestAuth(user=" + user + ", password=<redacted>, algorithm="
            + algorithm + ", realm=" + realm + ")";
    }
}
