package org.tstrans.rtp;

import java.util.Optional;

/**
 * Per-mount options for {@link RtspServer#addPublishMount(String, PublishMountConfig)}.
 * An option left unset follows the server-wide default in {@link RtspServerConfig}.
 * Immutable; build with {@link #builder()}.
 */
public final class PublishMountConfig {
    private final Boolean endOnPublisherExit;  // null = inherit

    private PublishMountConfig(Builder b) {
        this.endOnPublisherExit = b.endOnPublisherExit;
    }

    /** Options that inherit every server-wide default. */
    public static PublishMountConfig defaults() { return builder().build(); }

    public static Builder builder() { return new Builder(); }

    /**
     * Whether the mount's receiver ends ({@code END_OF_STREAM}) when a publisher
     * that reached RECORD leaves; empty when the mount follows {@link
     * RtspServerConfig#endPublishMountOnPublisherExit()}.
     */
    public Optional<Boolean> endOnPublisherExit() { return Optional.ofNullable(endOnPublisherExit); }

    @Override public String toString() {
        return "PublishMountConfig(endOnPublisherExit="
            + (endOnPublisherExit != null ? endOnPublisherExit : "inherit") + ")";
    }

    /** Builder for {@link PublishMountConfig}. */
    public static final class Builder {
        private Boolean endOnPublisherExit;

        private Builder() {}

        /**
         * End the mount's receiver when a publisher that reached RECORD leaves
         * ({@code true}), or keep it open for the next publisher ({@code false}).
         */
        public Builder endOnPublisherExit(boolean v) {
            this.endOnPublisherExit = v;
            return this;
        }

        public PublishMountConfig build() { return new PublishMountConfig(this); }
    }
}
