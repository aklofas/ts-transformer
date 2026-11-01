package org.tstrans;

import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.FileAlreadyExistsException;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.attribute.BasicFileAttributes;
import java.nio.file.attribute.PosixFilePermission;
import java.nio.file.attribute.PosixFilePermissions;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.Set;

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
 * {@code <java.io.tmpdir>/tstrans-native-<user>/<hash>/libtstjni.<ext>}, where
 * {@code <user>} is the {@code user.name} property reduced to
 * {@code [A-Za-z0-9._-]} and {@code <hash>} is the SHA-256 hex digest of the
 * resource bytes. {@code java.io.tmpdir} is shared on multi-user Linux, so the
 * per-user root is trusted only when it is a real directory (not a symbolic
 * link), owned by the current user, and (POSIX) accessible by the owner alone
 * ({@code rwx------}); it is created that way when absent. When the root exists
 * but fails any of those checks, or cannot be created, the loader does not
 * touch it: it extracts into a fresh random owner-only directory
 * {@code <java.io.tmpdir>/tstrans-native-<random>/} without reusing anything,
 * and logs a warning naming the reason. Ownership of the root, the
 * {@code <hash>} directory and a reused library is verified on every file
 * system with an "owner" attribute view, against the identity the process
 * creates files as (the owner of a probe file {@code tstrans-id-*.tmp} created
 * and removed under the temp directory; a crash can leave a zero-byte one), not
 * against {@code user.name}; a store without that view is never trusted. The
 * mode check is POSIX-only. The owner check refuses a planted root, directory
 * or file, but it is not an access control: on Windows no ACL is set, so when
 * the temp directory's ACL lets other accounts write to files in it, one of
 * them can still modify the library in place, even between verification and
 * {@code System.load}. Use a private {@code java.io.tmpdir} there; an ACL on the
 * cache root is a follow-up. A squatted root makes every launch copy the library into a new
 * private directory, and nothing removes those copies.
 *
 * <p>Each JVM writes its own uniquely named owner-only staging file and
 * publishes it with an atomic move, so JVMs starting together never share a
 * partial file. An existing target is reused only when it is a regular file
 * (not a symbolic link) owned by the current user whose size and SHA-256 match the resource, so a file
 * truncated by a crash is replaced. The loader never deletes a
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

    private static final System.Logger LOG = System.getLogger(NativeLoader.class.getName());
    private static final Set<PosixFilePermission> OWNER_DIR =
            PosixFilePermissions.fromString("rwx------");
    private static final Set<PosixFilePermission> OWNER_FILE =
            PosixFilePermissions.fromString("rw-------");

    /**
     * Extracts {@code bytes} to
     * {@code <tmpBase>/tstrans-native-<user>/<hash>/libtstjni.<ext>}, reusing an
     * existing file only when it is a regular file whose size and SHA-256
     * match. When the per-user root cannot be trusted, extracts into a private
     * random directory instead and reuses nothing. Deletes nothing but its own
     * staging file.
     *
     * <p>Package-private for unit testing.
     */
    static Path extractToStableDir(byte[] bytes, String ext, Path tmpBase) throws IOException {
        boolean posix = tmpBase.getFileSystem().supportedFileAttributeViews().contains("posix");
        Path root = tmpBase.resolve("tstrans-native-" + userSegment(System.getProperty("user.name")));
        Path targetDir = null;
        String reason = trustedRoot(root, tmpBase, posix);
        if (reason == null) {
            Path dir = root.resolve(contentHash(bytes));
            try {
                if (posix) {
                    Files.createDirectories(dir, PosixFilePermissions.asFileAttribute(OWNER_DIR));
                } else {
                    Files.createDirectories(dir);
                }
                if (isOwnedDirectory(dir, tmpBase)) {
                    targetDir = dir;
                } else {
                    reason = dir + " is not a directory owned by the current user";
                }
            } catch (IOException e) {
                reason = dir + " could not be created as a directory (" + e + ")";
            }
        }
        if (targetDir == null) {
            LOG.log(System.Logger.Level.WARNING,
                    "not using {0}: {1}; extracting the native library into a private directory",
                    root, reason);
            targetDir = posix
                    ? Files.createTempDirectory(tmpBase, "tstrans-native-",
                            PosixFilePermissions.asFileAttribute(OWNER_DIR))
                    : Files.createTempDirectory(tmpBase, "tstrans-native-");
        }
        Path target = targetDir.resolve("libtstjni." + ext);
        if (matches(target, bytes, tmpBase)) {
            return target;
        }
        // A per-call staging name keeps concurrent JVMs off each other's partial
        // writes; the atomic move means target is either absent or complete.
        // createTempFile is owner-only on POSIX, and the library stays that way.
        Path staging = Files.createTempFile(targetDir, "libtstjni." + ext + ".", ".part");
        try {
            Files.write(staging, bytes);
            publish(staging, target, bytes, tmpBase);
        } finally {
            Files.deleteIfExists(staging);
        }
        return target;
    }

    /**
     * Creates {@code root} owner-only if absent; otherwise accepts it only when
     * it is a real directory, owned by this user and (POSIX) owner-only. Never
     * modifies an existing directory. Returns {@code null} when the root is
     * trusted, else the reason it is not.
     */
    private static String trustedRoot(Path root, Path tmpBase, boolean posix) {
        try {
            if (posix) {
                Files.createDirectory(root, PosixFilePermissions.asFileAttribute(OWNER_DIR));
            } else {
                Files.createDirectory(root);
            }
            return null;
        } catch (FileAlreadyExistsException exists) {
            // Fall through to the checks on what is already there.
        } catch (IOException | UnsupportedOperationException e) {
            return "it could not be created (" + e + ")";
        }
        try {
            if (!isOwnedDirectory(root, tmpBase)) {
                return "it is not a real directory owned by the current user";
            }
            if (posix && !Files.getPosixFilePermissions(root, LinkOption.NOFOLLOW_LINKS)
                    .equals(OWNER_DIR)) {
                return "it is not accessible by its owner alone";
            }
            return null;
        } catch (IOException | UnsupportedOperationException e) {
            return "it could not be inspected (" + e + ")";
        }
    }

    private static boolean isOwnedDirectory(Path dir, Path tmpBase) {
        try {
            return Files.readAttributes(dir, BasicFileAttributes.class, LinkOption.NOFOLLOW_LINKS)
                            .isDirectory()
                    && currentUserOwns(dir, tmpBase);
        } catch (IOException e) {
            return false;
        }
    }

    /**
     * True when {@code path} is owned by the identity this process creates files
     * as. That identity is read from the file system (the owner of a probe file
     * {@code tstrans-id-*.tmp} created and removed under {@code tmpBase}; a crash
     * can leave a zero-byte one behind), not from {@code user.name}, which is
     * {@code ?} or unset when the uid has no passwd entry and can be overridden
     * on the command line. A store without an "owner" attribute view cannot be
     * verified and is never trusted.
     */
    static boolean currentUserOwns(Path path, Path tmpBase) {
        if (!path.getFileSystem().supportedFileAttributeViews().contains("owner")) {
            return false;
        }
        try {
            Path probe = Files.createTempFile(tmpBase, "tstrans-id-", ".tmp");
            try {
                return Files.getOwner(path, LinkOption.NOFOLLOW_LINKS)
                        .equals(Files.getOwner(probe, LinkOption.NOFOLLOW_LINKS));
            } finally {
                Files.deleteIfExists(probe);
            }
        } catch (IOException | UnsupportedOperationException e) {
            return false;
        }
    }

    /**
     * {@code user.name} reduced to {@code [A-Za-z0-9._-]}. A name that needed
     * replacements also gets a short hash of the raw name, so two different
     * names cannot map to the same directory.
     */
    static String userSegment(String user) {
        String raw = user == null ? "" : user;
        String clean = raw.replaceAll("[^A-Za-z0-9._-]", "_");
        if (clean.isEmpty()) {
            clean = "_";
        }
        if (clean.equals(raw)) {
            return clean;
        }
        return clean + "-" + contentHash(raw.getBytes(StandardCharsets.UTF_8)).substring(0, 8);
    }

    /**
     * Moves {@code staging} onto {@code target}, or accepts a complete copy that
     * another JVM published first.
     *
     * <p>On Windows two replace-renames onto the same name race: one fails with
     * ACCESS_DENIED while the other is still superseding the old file, and for
     * that instant the target reads as absent or locked. That is not a reason to
     * fail the load, so a failed move is retried briefly until it succeeds or an
     * intact copy (published by the other JVM, loaded and locked or not) is in
     * place.
     */
    private static void publish(Path staging, Path target, byte[] bytes, Path tmpBase)
            throws IOException {
        IOException last = null;
        for (int attempt = 0; attempt < 20; attempt++) {
            if (attempt > 0) {
                try {
                    Thread.sleep(50);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    last.addSuppressed(e);
                    throw last;
                }
            }
            try {
                Files.move(staging, target, StandardCopyOption.ATOMIC_MOVE,
                        StandardCopyOption.REPLACE_EXISTING);
                return;
            } catch (IOException e) {
                if (matches(target, bytes, tmpBase)) {
                    return;
                }
                last = e;
            }
        }
        throw last;
    }

    /**
     * True only for a regular, non-symlink file owned by this user with this
     * content. False for any unreadable target, so the caller falls through to
     * extraction.
     */
    private static boolean matches(Path file, byte[] bytes, Path tmpBase) {
        try {
            return Files.readAttributes(file, BasicFileAttributes.class, LinkOption.NOFOLLOW_LINKS)
                            .isRegularFile()
                    && currentUserOwns(file, tmpBase)
                    && Files.size(file) == bytes.length
                    && contentHash(Files.readAllBytes(file)).equals(contentHash(bytes));
        } catch (IOException e) {
            return false;
        }
    }

    /**
     * Returns the SHA-256 hex digest of {@code bytes}: 64 lowercase hex
     * characters, deterministic.
     *
     * <p>Package-private for unit testing.
     */
    static String contentHash(byte[] bytes) {
        try {
            return java.util.HexFormat.of().formatHex(
                    MessageDigest.getInstance("SHA-256").digest(bytes));
        } catch (NoSuchAlgorithmException e) {
            throw new IllegalStateException(e); // every JRE ships SHA-256
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
