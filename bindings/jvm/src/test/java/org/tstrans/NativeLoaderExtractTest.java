package org.tstrans;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.attribute.FileTime;
import java.util.ArrayList;
import java.util.List;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.io.TempDir;

/**
 * Unit tests for the content-addressed extraction helpers and dev-override
 * property. All tests are Linux-runnable; the Windows-locked-DLL path (the
 * atomic move fails, the existing target is verified by content and reused)
 * is not exercised directly.
 */
class NativeLoaderExtractTest {

    /** The per-user cache root the loader uses under {@code tmp}. */
    private static Path userRoot(Path tmp) {
        return tmp.resolve("tstrans-native-" + NativeLoader.userSegment(System.getProperty("user.name")));
    }

    /** Pre-creates a trusted (owner-only) root and the hash directory for {@code bytes}. */
    private static Path hashDir(Path tmp, byte[] bytes) throws IOException {
        Path root = userRoot(tmp);
        if (tmp.getFileSystem().supportedFileAttributeViews().contains("posix")) {
            Files.createDirectories(root, java.nio.file.attribute.PosixFilePermissions
                .asFileAttribute(java.nio.file.attribute.PosixFilePermissions.fromString("rwx------")));
        } else {
            Files.createDirectories(root);
        }
        return Files.createDirectories(root.resolve(NativeLoader.contentHash(bytes)));
    }

    // -----------------------------------------------------------------------
    // contentHash
    // -----------------------------------------------------------------------

    @Test
    void contentHashIsDeterministic() {
        byte[] bytes = new byte[]{1, 2, 3, 4, 5};
        assertEquals(NativeLoader.contentHash(bytes), NativeLoader.contentHash(bytes),
                "same bytes must produce the same hash on repeated calls");
    }

    @Test
    void contentHashDiffersForDifferentInput() {
        assertNotEquals(
                NativeLoader.contentHash(new byte[]{1, 2, 3}),
                NativeLoader.contentHash(new byte[]{4, 5, 6}),
                "different bytes must produce different hashes");
    }

    @Test
    void contentHashHexFormat() {
        String hash = NativeLoader.contentHash(new byte[]{0});
        assertNotNull(hash);
        assertFalse(hash.isEmpty());
        assertTrue(hash.matches("[0-9a-f]+"), "hash must be lowercase hex, got: " + hash);
    }

    // -----------------------------------------------------------------------
    // extractToStableDir
    // -----------------------------------------------------------------------

    @Test
    void extractToStableDirCreatesExpectedPath(@TempDir Path tmp) throws IOException {
        byte[] bytes = new byte[]{10, 20, 30};
        String hash = NativeLoader.contentHash(bytes);
        Path target = NativeLoader.extractToStableDir(bytes, "so", tmp);

        // Must sit in tstrans-native-<user>/<hash>/libtstjni.so under the given base
        assertEquals("libtstjni.so", target.getFileName().toString());
        assertEquals(hash, target.getParent().getFileName().toString());
        assertEquals(userRoot(tmp), target.getParent().getParent());
        assertTrue(Files.exists(target));
        assertArrayEquals(bytes, Files.readAllBytes(target));
    }

    @Test
    void extractToStableDirReusesAVerifiedFileOnSecondCall(@TempDir Path tmp) throws IOException {
        byte[] bytes = new byte[]{11, 22, 33, 44};
        Path target = NativeLoader.extractToStableDir(bytes, "so", tmp);

        // An intact cached file must not be rewritten; an old mtime would change if it were.
        FileTime old = FileTime.fromMillis(1_000_000_000_000L);
        Files.setLastModifiedTime(target, old);

        Path target2 = NativeLoader.extractToStableDir(bytes, "so", tmp);

        assertEquals(target, target2, "second call must return the same path");
        assertEquals(old, Files.getLastModifiedTime(target2),
                "an intact cached file must be reused, not rewritten");
    }

    // -----------------------------------------------------------------------
    // Shared cache: staging, verified reuse, no deletion
    // -----------------------------------------------------------------------

    /** Another JVM's staging path must not block or corrupt this JVM's extraction. */
    @Test
    void extractionIgnoresAForeignStagingFile(@TempDir Path tmp) throws IOException {
        byte[] bytes = {1, 2, 3, 4};
        Path dir = hashDir(tmp, bytes);
        // The fixed name every JVM used before; a directory makes Files.write on it fail.
        Files.createDirectories(dir.resolve("libtstjni.so.part"));
        Path target = NativeLoader.extractToStableDir(bytes, "so", tmp);
        assertArrayEquals(bytes, Files.readAllBytes(target));
    }

    @Test
    void extractionLeavesOtherVersionsCachesAlone(@TempDir Path tmp) throws IOException {
        Path other = hashDir(tmp, new byte[] {42}).resolveSibling("deadbeef");
        Files.createDirectories(other);
        Files.write(other.resolve("libtstjni.so"), new byte[] {9});
        NativeLoader.extractToStableDir(new byte[] {1, 2, 3}, "so", tmp);
        assertTrue(Files.exists(other.resolve("libtstjni.so")),
            "a different version's cached library must survive another version's load");
    }

    @Test
    void reuseReplacesACorruptCachedLibrary(@TempDir Path tmp) throws IOException {
        byte[] bytes = {5, 6, 7, 8, 9};
        Path dir = hashDir(tmp, bytes);
        Files.write(dir.resolve("libtstjni.so"), new byte[] {5, 6}); // truncated by a crash
        Path target = NativeLoader.extractToStableDir(bytes, "so", tmp);
        assertArrayEquals(bytes, Files.readAllBytes(target));
    }

    @Test
    void extractionLeavesNoStagingFileBehind(@TempDir Path tmp) throws IOException {
        Path target = NativeLoader.extractToStableDir(new byte[] {4, 3, 2, 1}, "so", tmp);
        try (var s = Files.list(target.getParent())) {
            assertEquals(List.of(target.getFileName().toString()),
                s.map(p -> p.getFileName().toString()).toList());
        }
    }

    @Test
    @Timeout(60)
    void concurrentExtractionsAllSucceed(@TempDir Path tmp) throws Exception {
        byte[] bytes = new byte[256 * 1024];
        new java.util.Random(7).nextBytes(bytes);
        for (int round = 0; round < 25; round++) {
            Path base = Files.createDirectory(tmp.resolve("r" + round));
            int n = 16;
            var barrier = new java.util.concurrent.CyclicBarrier(n);
            var pool = java.util.concurrent.Executors.newFixedThreadPool(n);
            try {
                List<java.util.concurrent.Future<Path>> fs = new ArrayList<>();
                for (int i = 0; i < n; i++) {
                    fs.add(pool.submit(() -> { barrier.await(); return NativeLoader.extractToStableDir(bytes, "so", base); }));
                }
                for (var f : fs) assertArrayEquals(bytes, Files.readAllBytes(f.get()));
            } finally {
                pool.shutdownNow();
            }
        }
    }

    /** The extracting JVM must not schedule the shared file for deletion at its exit. */
    @Test
    @Timeout(60)
    void cachedLibrarySurvivesTheExtractingJvmsExit(@TempDir Path tmp) throws Exception {
        String java = Path.of(System.getProperty("java.home"), "bin", "java").toString();
        TestSupport.ChildResult child = TestSupport.runChild(
            new ProcessBuilder(List.of(java, "-cp", System.getProperty("java.class.path"),
                NativeLoaderExtractChild.class.getName(), tmp.toString())),
            tmp.resolve("child.log"), 30_000);
        String out = child.output();
        assertEquals(0, child.exitValue(), out);
        Path target = Path.of(out.strip().lines().reduce((a, b) -> b).orElseThrow());
        assertTrue(Files.exists(target), "deleteOnExit removed the shared cached library: " + out);
    }

    @Test
    void unreadableCachedLibraryFallsThroughToExtraction(@TempDir Path tmp) throws IOException {
        org.junit.jupiter.api.Assumptions.assumeTrue(
            tmp.getFileSystem().supportedFileAttributeViews().contains("posix"));
        byte[] bytes = {8, 6, 7, 5, 3, 0, 9};
        Path dir = hashDir(tmp, bytes);
        Path planted = dir.resolve("libtstjni.so");
        Files.write(planted, bytes);
        Files.setPosixFilePermissions(planted, java.util.Set.of());
        org.junit.jupiter.api.Assumptions.assumeFalse(Files.isReadable(planted), "running as root");
        Path target = NativeLoader.extractToStableDir(bytes, "so", tmp);
        assertArrayEquals(bytes, Files.readAllBytes(target));
    }

    // -----------------------------------------------------------------------
    // Per-user owner-only cache root
    // -----------------------------------------------------------------------

    private static final java.util.Set<java.nio.file.attribute.PosixFilePermission> OWNER_ONLY =
        java.nio.file.attribute.PosixFilePermissions.fromString("rwx------");

    private static void assumePosix(Path p) {
        org.junit.jupiter.api.Assumptions.assumeTrue(
            p.getFileSystem().supportedFileAttributeViews().contains("posix"));
    }

    /** The per-user root name for a plain account name; skips on an unusual one. */
    private static String userRootName() {
        String user = System.getProperty("user.name");
        org.junit.jupiter.api.Assumptions.assumeTrue(user.matches("[A-Za-z0-9._-]+"), user);
        return "tstrans-native-" + user;
    }

    @Test
    void extractionCreatesAnOwnerOnlyPerUserRoot(@TempDir Path tmp) throws IOException {
        assumePosix(tmp);
        String rootName = userRootName();
        Path target = NativeLoader.extractToStableDir(new byte[] {1, 2, 3, 9}, "so", tmp);
        Path root = tmp.resolve(rootName);
        assertTrue(target.startsWith(root), target + " not under " + root);
        assertEquals(OWNER_ONLY, Files.getPosixFilePermissions(root));
        assertEquals(java.nio.file.attribute.PosixFilePermissions.fromString("rw-------"),
            Files.getPosixFilePermissions(target));
    }

    private static void assertPrivateFallback(Path tmp, Path untrusted, byte[] bytes) throws IOException {
        Path target = NativeLoader.extractToStableDir(bytes, "so", tmp);
        assertFalse(target.startsWith(untrusted), "used the untrusted root: " + target);
        Path dir = target.getParent();
        assertEquals(tmp, dir.getParent(), "fallback must sit directly under the base");
        assertTrue(dir.getFileName().toString().startsWith("tstrans-native-"));
        assertNotEquals(untrusted.getFileName(), dir.getFileName());
        assertEquals(OWNER_ONLY, Files.getPosixFilePermissions(dir));
        assertArrayEquals(bytes, Files.readAllBytes(target));
    }

    @Test
    void aPermissivePerUserRootIsNeverUsed(@TempDir Path tmp) throws IOException {
        assumePosix(tmp);
        Path root = Files.createDirectory(tmp.resolve(userRootName()));
        var open = java.nio.file.attribute.PosixFilePermissions.fromString("rwxr-xr-x");
        Files.setPosixFilePermissions(root, open);
        assertPrivateFallback(tmp, root, new byte[] {4, 4, 2});
        assertEquals(open, Files.getPosixFilePermissions(root));
        try (var s = Files.list(root)) {
            assertEquals(0, s.count(), "the permissive directory must stay untouched");
        }
    }

    @Test
    void aSymlinkedPerUserRootIsNeverUsed(@TempDir Path tmp) throws IOException {
        assumePosix(tmp);
        Path elsewhere = Files.createDirectory(tmp.resolve("elsewhere"),
            java.nio.file.attribute.PosixFilePermissions.asFileAttribute(OWNER_ONLY));
        Path link = Files.createSymbolicLink(tmp.resolve(userRootName()), elsewhere);
        assertPrivateFallback(tmp, link, new byte[] {7, 7, 1});
        try (var s = Files.list(elsewhere)) {
            assertEquals(0, s.count(), "the link target must stay untouched");
        }
    }

    @Test
    void aSymlinkedCacheEntryIsNotReused(@TempDir Path tmp) throws IOException {
        assumePosix(tmp);
        byte[] bytes = {3, 1, 4, 1, 5};
        Path first = NativeLoader.extractToStableDir(bytes, "so", tmp);
        Path real = tmp.resolve("real.so");
        Files.write(real, bytes);
        Files.delete(first);
        Files.createSymbolicLink(first, real);
        Path target = NativeLoader.extractToStableDir(bytes, "so", tmp);
        assertEquals(first, target);
        assertTrue(Files.isRegularFile(target, java.nio.file.LinkOption.NOFOLLOW_LINKS),
            "the symlinked entry must have been replaced by a regular file");
        assertArrayEquals(bytes, Files.readAllBytes(target));
    }

    @Test
    void contentHashIsSha256Hex() {
        String hash = NativeLoader.contentHash("abc".getBytes(java.nio.charset.StandardCharsets.US_ASCII));
        assertEquals("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", hash);
    }

    @Test
    void contentHashSeparatesEqualCrc32Inputs() {
        // Birthday search over distinct 6-byte inputs: ~200k samples give a few CRC32 collisions.
        java.util.Map<Long, byte[]> seen = new java.util.HashMap<>();
        byte[] a = null;
        byte[] b = null;
        for (int i = 0; b == null; i++) {
            byte[] in = {(byte) i, (byte) (i >>> 8), (byte) (i >>> 16), (byte) (i >>> 24),
                (byte) (i * 31), (byte) (i * 131 >>> 3)};
            java.util.zip.CRC32 crc = new java.util.zip.CRC32();
            crc.update(in);
            byte[] prev = seen.put(crc.getValue(), in);
            if (prev != null && !java.util.Arrays.equals(prev, in)) {
                a = prev;
                b = in;
            }
        }
        assertNotEquals(NativeLoader.contentHash(a), NativeLoader.contentHash(b));
        assertTrue(NativeLoader.contentHash(a).matches("[0-9a-f]{64}"));
    }

    // -----------------------------------------------------------------------
    // Dev override property
    // -----------------------------------------------------------------------

    @Test
    void overridePathIsNullWhenPropertyAbsent() {
        System.clearProperty("tstrans.native.lib");
        assertNull(NativeLoader.overridePath(),
                "overridePath() must return null when property is not set");
    }

    @Test
    void overridePathReturnsPropertyValue() {
        System.setProperty("tstrans.native.lib", "/some/path/libtstjni.so");
        try {
            assertEquals("/some/path/libtstjni.so", NativeLoader.overridePath());
        } finally {
            System.clearProperty("tstrans.native.lib");
        }
    }

    @Test
    void overridePathTreatsEmptyAsAbsent() {
        System.setProperty("tstrans.native.lib", "");
        try {
            assertNull(NativeLoader.overridePath(),
                    "empty property must be treated as absent (null)");
        } finally {
            System.clearProperty("tstrans.native.lib");
        }
    }

    // -----------------------------------------------------------------------
    // Atomic write — crashed staging file
    // -----------------------------------------------------------------------

    /**
     * A truncated staging file left by a crashed JVM belongs to nobody now: it
     * must neither block extraction nor be touched by the loader.
     */
    @Test
    void extractionSucceedsBesideACrashedStagingFile(@TempDir Path tmp) throws IOException {
        byte[] fullBytes = new byte[]{10, 20, 30, 40, 50};
        Path targetDir = hashDir(tmp, fullBytes);
        Files.write(targetDir.resolve("libtstjni.so.part"), new byte[]{(byte) 0xDE, (byte) 0xAD});

        Path result = NativeLoader.extractToStableDir(fullBytes, "so", tmp);

        assertEquals(targetDir.resolve("libtstjni.so"), result);
        assertArrayEquals(fullBytes, Files.readAllBytes(result));
    }
}
