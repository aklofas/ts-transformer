package org.tstrans.rtp;

import java.util.List;
import java.util.Objects;

/**
 * What the server does with one direction of traffic (publish or read) on a mount path.
 * Build one with {@link #inherit()}, {@link #open()}, {@link #require(MountCredential...)}
 * or {@link #refuse(RefuseStatus)}.
 */
public final class MountPolicy {
    static final int KIND_INHERIT = 0;
    static final int KIND_OPEN = 1;
    static final int KIND_REQUIRE = 2;
    static final int KIND_REFUSE = 3;

    private static final MountPolicy INHERIT = new MountPolicy(KIND_INHERIT, null, List.of());
    private static final MountPolicy OPEN = new MountPolicy(KIND_OPEN, null, List.of());

    private final int kind;
    private final RefuseStatus status;
    private final List<MountCredential> credentials;

    private MountPolicy(int kind, RefuseStatus status, List<MountCredential> credentials) {
        this.kind = kind;
        this.status = status;
        this.credentials = credentials;
    }

    /**
     * Defer to the next level: a path rule defers to the table default, and the table
     * default defers to the server-wide credential.
     */
    public static MountPolicy inherit() { return INHERIT; }

    /** Allow without credentials. */
    public static MountPolicy open() { return OPEN; }

    /**
     * Allow a client that presents one of {@code credentials}.
     *
     * @throws IllegalArgumentException if no credential is given
     */
    public static MountPolicy require(MountCredential... credentials) {
        Objects.requireNonNull(credentials, "credentials");
        if (credentials.length == 0) {
            throw new IllegalArgumentException("require needs at least one credential");
        }
        for (MountCredential c : credentials) Objects.requireNonNull(c, "credential");
        return new MountPolicy(KIND_REQUIRE, null, List.of(credentials));
    }

    /** Refuse with {@code status}. */
    public static MountPolicy refuse(RefuseStatus status) {
        return new MountPolicy(KIND_REFUSE, Objects.requireNonNull(status, "status"), List.of());
    }

    int kind() { return kind; }

    int statusCode() { return status == null ? 0 : status.code(); }

    String[] usernames() {
        return credentials.stream().map(MountCredential::username).toArray(String[]::new);
    }

    String[] passwords() {
        return credentials.stream().map(MountCredential::password).toArray(String[]::new);
    }

    @Override public String toString() {
        return switch (kind) {
            case KIND_INHERIT -> "Inherit";
            case KIND_OPEN -> "Open";
            case KIND_REQUIRE -> "Require" + credentials;
            default -> "Refuse(" + status + ")";
        };
    }
}
