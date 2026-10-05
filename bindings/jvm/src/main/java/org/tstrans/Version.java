package org.tstrans;

/**
 * Version information for the tstrans JVM binding.
 *
 * <p>Exposes {@link #versionString()}, the one native method that proves
 * the JNI pipeline is loaded and returns the Rust workspace crate version.
 */
public final class Version {
    private Version() {}

    static {
        NativeLoader.load();
    }

    /** @return the native (Rust) workspace crate version, e.g. {@code "0.1.0"}. */
    public static native String versionString();
}
