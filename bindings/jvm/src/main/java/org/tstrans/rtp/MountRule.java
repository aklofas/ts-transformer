package org.tstrans.rtp;

import java.util.Objects;

/** The publish and read policies for one mount path, or for the table default. */
public final class MountRule {
    private final MountPolicy publish;
    private final MountPolicy read;

    private MountRule(MountPolicy publish, MountPolicy read) {
        this.publish = publish;
        this.read = read;
    }

    /** A rule with separate policies for clients that publish (ANNOUNCE/RECORD) and read. */
    public static MountRule of(MountPolicy publish, MountPolicy read) {
        return new MountRule(Objects.requireNonNull(publish, "publish"),
            Objects.requireNonNull(read, "read"));
    }

    /** A rule that applies {@code policy} to both directions. */
    public static MountRule both(MountPolicy policy) {
        Objects.requireNonNull(policy, "policy");
        return new MountRule(policy, policy);
    }

    /** The policy for a client that publishes to the path. */
    public MountPolicy publish() { return publish; }

    /** The policy for a client that reads the path. */
    public MountPolicy read() { return read; }

    @Override public String toString() {
        return "MountRule(publish=" + publish + ", read=" + read + ")";
    }
}
