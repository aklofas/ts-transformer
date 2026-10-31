package org.tstrans.rtp;

import java.util.Objects;

/**
 * One username and password a client may present for a mount path. The password has no
 * getter and {@link #toString()} redacts it.
 */
public final class MountCredential {
    private final String username;
    private final String password;

    private MountCredential(String username, String password) {
        this.username = username;
        this.password = password;
    }

    /**
     * A credential for {@code username} with {@code password}. A rule that holds an empty
     * username is refused when it is installed.
     */
    public static MountCredential of(String username, String password) {
        return new MountCredential(Objects.requireNonNull(username, "username"),
            Objects.requireNonNull(password, "password"));
    }

    /** The username. */
    public String username() { return username; }

    /** Package-private: the password crosses JNI once, when a rule is installed. */
    String password() { return password; }

    @Override public String toString() {
        return "MountCredential(username=" + username + ", password=<redacted>)";
    }
}
