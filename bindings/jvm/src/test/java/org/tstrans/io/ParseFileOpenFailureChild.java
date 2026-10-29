package org.tstrans.io;

import java.nio.file.Files;
import java.nio.file.Path;

/**
 * Child JVM for {@code ParseFileTest}: started with a native-library override
 * that cannot load, it calls {@link Io#parseFile(Path)} and reports the thrown
 * type and how many of its own file descriptors still point at the input.
 */
public final class ParseFileOpenFailureChild {
    private ParseFileOpenFailureChild() {}

    public static void main(String[] args) throws Exception {
        Path input = Path.of(args[0]).toRealPath();
        try {
            Io.parseFile(input).close();
            System.out.println("THROWN=none");
        } catch (Throwable t) {
            System.out.println("THROWN=" + t.getClass().getName());
        }
        long open = 0;
        try (var fds = Files.list(Path.of("/proc/self/fd"))) {
            for (Path fd : (Iterable<Path>) fds::iterator) {
                try {
                    if (Files.readSymbolicLink(fd).equals(input)) open++;
                } catch (java.io.IOException ignored) {
                    // the descriptor closed while listing (e.g. the listing's own)
                }
            }
        }
        System.out.println("OPEN=" + open);
    }
}
