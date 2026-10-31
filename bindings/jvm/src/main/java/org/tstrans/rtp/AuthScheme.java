package org.tstrans.rtp;

/** The challenge a server sends a client that must authenticate for a mount path. */
public enum AuthScheme {
    /** HTTP Basic. */
    BASIC(0),
    /** HTTP Digest with MD5. */
    DIGEST_MD5(1),
    /** HTTP Digest with SHA-256. */
    DIGEST_SHA256(2);

    private final int code;

    AuthScheme(int code) { this.code = code; }

    /** The scheme code the native layer takes: 0 Basic, 1 Digest MD5, 2 Digest SHA-256. */
    int code() { return code; }
}
