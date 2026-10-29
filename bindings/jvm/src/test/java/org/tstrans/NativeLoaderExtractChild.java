package org.tstrans;

import java.nio.file.Path;

/** Child JVM for the cache-survives-exit test: extracts once and prints the path. */
public final class NativeLoaderExtractChild {
    private NativeLoaderExtractChild() {}

    public static void main(String[] args) throws Exception {
        Path target = NativeLoader.extractToStableDir(new byte[] {7, 7, 7}, "so", Path.of(args[0]));
        System.out.println(target.toAbsolutePath());
        System.exit(0);
    }
}
