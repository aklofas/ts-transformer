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
 * {@code <hash>} is a CRC32 hex digest of the resource bytes. Each JVM writes
 * its own uniquely named staging file in that directory and publishes it with
 * an atomic move, so JVMs starting together never share a partial file. An
 * existing target is reused only when its size and CRC32 match the resource,
 * so a file truncated by a crash is replaced. The loader never deletes a
 * {@code tstrans-native-*} entry: another JVM may be about to load it, and
 * Windows locks a loaded DLL. Directories from older JAR versions are left for
 * the operating system's temp cleaner.
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
     * {@code <tmpBase>/tstrans-native-<hash>/libtstjni.<ext>}, reusing an
     * existing file only when its size and CRC32 match. Deletes nothing but
     * its own staging file.
     *
     * <p>Package-private for unit testing.
     */
    static Path extractToStableDir(byte[] bytes, String ext, Path tmpBase) throws IOException {
        Path targetDir = tmpBase.resolve("tstrans-native-" + contentHash(bytes));
        Path target = targetDir.resolve("libtstjni." + ext);
        if (matches(target, bytes)) {
            return target;
        }
        Files.createDirectories(targetDir);
        // A per-call staging name keeps concurrent JVMs off each other's partial
        // writes; the atomic move means target is either absent or complete.
        Path staging = Files.createTempFile(targetDir, "libtstjni." + ext + ".", ".part");
        try {
            Files.write(staging, bytes);
            try {
                Files.move(staging, target, StandardCopyOption.ATOMIC_MOVE,
                        StandardCopyOption.REPLACE_EXISTING);
            } catch (IOException e) {
                // Another JVM published first, or (Windows) the target is a loaded,
                // locked DLL: an intact copy is just as good.
                if (matches(target, bytes)) {
                    return target;
                }
                throw e;
            }
        } finally {
            Files.deleteIfExists(staging);
        }
        return target;
    }

    private static boolean matches(Path file, byte[] bytes) throws IOException {
        return Files.isRegularFile(file)
                && Files.size(file) == bytes.length
                && contentHash(Files.readAllBytes(file)).equals(contentHash(bytes));
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
