package org.tstrans;

import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.zip.CRC32;

/**
 * Loads {@code libtstjni} by extracting the platform-matched native library
 * from the JAR's {@code native/<triple>/} resources to a stable,
 * content-addressed temp directory and {@code System.load}-ing it. Same
 * pattern as JNA / sqlite-jdbc / LWJGL.
 *
 * <p>The fat JAR ships {@code native/<triple>/libtstjni.<ext>} for every
 * Tier 1 platform; {@link #resourcePath} resolves the running host to that
 * path. The library filename is always {@code libtstjni.<ext>} on every
 * platform (Windows's cargo {@code tstjni.dll} is renamed to
 * {@code libtstjni.dll} at packaging time); {@code System.load} takes an
 * absolute path so the on-disk name need not match the module's internal name.
 *
 * <h2>Extraction layout</h2>
 * <p>The library is extracted to
 * {@code <java.io.tmpdir>/tstrans-native-<hash>/libtstjni.<ext>}, where
 * {@code <hash>} is a CRC32 hex digest of the resource bytes. If the file
 * already exists with the same hash (e.g. from a previous JVM run with the
 * same JAR), it is reused without re-extraction. On load, stale sibling
 * {@code tstrans-native-*} directories (from older JAR versions) are
 * swept best-effort. On Windows, a currently-loaded DLL is locked by the OS
 * and cannot be deleted; the deletion error is silently swallowed and the
 * stale directory is left for cleanup on a future run after the JVM exits.
 *
 * <h2>Developer / debug override</h2>
 * <p>Set the JVM system property {@code tstrans.native.lib} to an absolute
 * path to bypass JAR extraction entirely:
 * <pre>{@code
 *   java -Dtstrans.native.lib=/path/to/target/debug/libtstjni.so ...
 * }</pre>
 * The property is checked before any JAR resource lookup or temp-file
 * extraction. This is intended for local debug builds — point at a freshly
 * compiled {@code libtstjni.so} without repackaging the JAR.
 */
public final class NativeLoader {
    private static volatile boolean loaded = false;

    private NativeLoader() {}

    public static synchronized void load() {
        if (loaded) {
            return;
        }

        // 1. Developer override: -Dtstrans.native.lib=/abs/path/to/libtstjni.so
        String override = overridePath();
        if (override != null) {
            System.load(override);
            nVerifyKinds();
            loaded = true;
            return;
        }

        // 2. Extract from JAR to a content-addressed stable directory.
        String osName = System.getProperty("os.name", "");
        String resource = resourcePath(osName, System.getProperty("os.arch", ""));
        String ext = libExtension(osName);
        try (InputStream in = NativeLoader.class.getResourceAsStream(resource)) {
            if (in == null) {
                throw new UnsatisfiedLinkError("native library not found on classpath: " + resource);
            }
            byte[] bytes = in.readAllBytes();
            Path target = extractToStableDir(bytes, ext,
                    Path.of(System.getProperty("java.io.tmpdir")));
            System.load(target.toAbsolutePath().toString());
            nVerifyKinds();
            loaded = true;
        } catch (IOException e) {
            throw new UnsatisfiedLinkError(
                    "failed to extract native library " + resource + ": " + e.getMessage());
        }
    }

    /**
     * Returns the value of the {@code tstrans.native.lib} system property, or
     * {@code null} if the property is absent or empty.
     *
     * <p>Package-private for unit testing.
     */
    static String overridePath() {
        String v = System.getProperty("tstrans.native.lib");
        return (v != null && !v.isEmpty()) ? v : null;
    }

    /**
     * Resolve every error kind the native library can raise against the
     * {@code *Exception.Kind} enums of THIS JAR. Throws
     * {@link IllegalStateException} naming the missing member when the JAR
     * and {@code libtstjni} were built from different sources — at load,
     * not at the first failing call.
     */
    private static native void nVerifyKinds();

    /**
     * Extracts {@code bytes} to
     * {@code <tmpBase>/tstrans-native-<hash>/libtstjni.<ext>}, reusing the
     * file without re-writing if it already exists (same content hash = same
     * JAR build). Stale sibling {@code tstrans-native-*} directories are
     * swept best-effort before extraction.
     *
     * <p>Package-private for unit testing.
     */
    static Path extractToStableDir(byte[] bytes, String ext, Path tmpBase) throws IOException {
        String hash = contentHash(bytes);
        String dirName = "tstrans-native-" + hash;
        Path targetDir = tmpBase.resolve(dirName);
        Path target = targetDir.resolve("libtstjni." + ext);

        // Sweep stale dirs from previous JAR versions before (re)using ours.
        sweepStale(tmpBase, dirName);

        if (!Files.exists(target)) {
            Files.createDirectories(targetDir);
            // Write to a staging file first, then promote atomically.  If the
            // JVM crashes between write and move only the .part file is partial;
            // target either does not exist (next run re-extracts) or already
            // contains a complete write (from a racing JVM that won the move).
            // REPLACE_EXISTING lets a racing loser overwrite with identical bytes.
            Path part = targetDir.resolve("libtstjni." + ext + ".part");
            Files.write(part, bytes);
            Files.move(part, target, StandardCopyOption.ATOMIC_MOVE,
                    StandardCopyOption.REPLACE_EXISTING);
        }
        target.toFile().deleteOnExit();
        return target;
    }

    /**
     * Returns the CRC32 hex digest of {@code bytes}. Deterministic: same
     * bytes always produce the same 8-character lowercase hex string.
     *
     * <p>Package-private for unit testing.
     */
    static String contentHash(byte[] bytes) {
        CRC32 crc = new CRC32();
        crc.update(bytes);
        return String.format("%08x", crc.getValue());
    }

    /**
     * Best-effort sweep of {@code <tmpBase>/tstrans-native-*} directories
     * other than {@code keepDirName}. Files inside each stale directory are
     * deleted first, then the directory itself. On Windows, a currently-loaded
     * DLL is locked by the OS; the resulting {@link IOException} is silently
     * swallowed and the stale directory is left for cleanup on the next run.
     *
     * <p>Package-private for unit testing.
     */
    static void sweepStale(Path tmpBase, String keepDirName) {
        try (var stream = Files.list(tmpBase)) {
            stream.filter(p -> {
                String n = p.getFileName().toString();
                return n.startsWith("tstrans-native-")
                        && !n.equals(keepDirName)
                        && Files.isDirectory(p);
            }).forEach(staleDir -> {
                try {
                    try (var files = Files.list(staleDir)) {
                        files.forEach(f -> {
                            try {
                                Files.delete(f);
                            } catch (IOException ignored) {
                                // Windows: DLL locked by the OS → leave it.
                            }
                        });
                    }
                    Files.delete(staleDir);
                } catch (IOException ignored) {
                    // Directory not empty (locked file on Windows) → leave it.
                }
            });
        } catch (IOException ignored) {
            // tmpBase not listable (unusual) → skip sweep entirely.
        }
    }

    /**
     * JAR-internal resource path for the given host. Pure function of
     * {@code os.name}/{@code os.arch} so it is testable on any host.
     */
    static String resourcePath(String osName, String osArch) {
        return "/native/" + triple(osName, osArch) + "/libtstjni." + libExtension(osName);
    }

    /** Normalize {@code os.name}/{@code os.arch} to the JAR triple (e.g. {@code linux-x86_64}). */
    static String triple(String osName, String osArch) {
        String os = osName.toLowerCase();
        String arch = osArch.toLowerCase();
        String o;
        if (os.contains("win")) {
            o = "windows";
        } else if (os.contains("mac") || os.contains("darwin")) {
            o = "macos";
        } else {
            o = "linux";
        }
        String a = (arch.equals("amd64") || arch.equals("x86_64")) ? "x86_64"
                : (arch.equals("aarch64") || arch.equals("arm64")) ? "aarch64"
                : arch;
        return o + "-" + a;
    }

    static String libExtension(String osName) {
        String os = osName.toLowerCase();
        if (os.contains("win")) {
            return "dll";
        }
        if (os.contains("mac") || os.contains("darwin")) {
            return "dylib";
        }
        return "so";
    }
}
