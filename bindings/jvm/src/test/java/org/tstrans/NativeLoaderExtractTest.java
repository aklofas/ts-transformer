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
        // CRC32 of a single zero byte is a well-known value; at minimum ensure it
        // is a non-empty lowercase hex string.
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

        // Must sit in tstrans-native-<hash>/libtstjni.so under the given base
        assertEquals("libtstjni.so", target.getFileName().toString());
        assertEquals("tstrans-native-" + hash, target.getParent().getFileName().toString());
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
        Path dir = tmp.resolve("tstrans-native-" + NativeLoader.contentHash(bytes));
        // The fixed name every JVM used before; a directory makes Files.write on it fail.
        Files.createDirectories(dir.resolve("libtstjni.so.part"));
        Path target = NativeLoader.extractToStableDir(bytes, "so", tmp);
        assertArrayEquals(bytes, Files.readAllBytes(target));
    }

    @Test
    void extractionLeavesOtherVersionsCachesAlone(@TempDir Path tmp) throws IOException {
        Path other = tmp.resolve("tstrans-native-deadbeef");
        Files.createDirectories(other);
        Files.write(other.resolve("libtstjni.so"), new byte[] {9});
        NativeLoader.extractToStableDir(new byte[] {1, 2, 3}, "so", tmp);
        assertTrue(Files.exists(other.resolve("libtstjni.so")),
            "a different version's cached library must survive another version's load");
    }

    @Test
    void reuseReplacesACorruptCachedLibrary(@TempDir Path tmp) throws IOException {
        byte[] bytes = {5, 6, 7, 8, 9};
        Path dir = tmp.resolve("tstrans-native-" + NativeLoader.contentHash(bytes));
        Files.createDirectories(dir);
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
    void publishedLibraryIsReadableByOtherUsers(@TempDir Path tmp) throws IOException {
        org.junit.jupiter.api.Assumptions.assumeTrue(
            tmp.getFileSystem().supportedFileAttributeViews().contains("posix"));
        Path target = NativeLoader.extractToStableDir(new byte[] {1, 1, 2, 3}, "so", tmp);
        var perms = Files.getPosixFilePermissions(target);
        assertTrue(perms.contains(java.nio.file.attribute.PosixFilePermission.GROUP_READ));
        assertTrue(perms.contains(java.nio.file.attribute.PosixFilePermission.OTHERS_READ));
    }

    @Test
    void unreadableCachedLibraryFallsThroughToExtraction(@TempDir Path tmp) throws IOException {
        org.junit.jupiter.api.Assumptions.assumeTrue(
            tmp.getFileSystem().supportedFileAttributeViews().contains("posix"));
        byte[] bytes = {8, 6, 7, 5, 3, 0, 9};
        Path dir = tmp.resolve("tstrans-native-" + NativeLoader.contentHash(bytes));
        Files.createDirectories(dir);
        Path planted = dir.resolve("libtstjni.so");
        Files.write(planted, bytes);
        Files.setPosixFilePermissions(planted, java.util.Set.of());
        org.junit.jupiter.api.Assumptions.assumeFalse(Files.isReadable(planted), "running as root");
        Path target = NativeLoader.extractToStableDir(bytes, "so", tmp);
        assertArrayEquals(bytes, Files.readAllBytes(target));
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
        Path targetDir = tmp.resolve("tstrans-native-" + NativeLoader.contentHash(fullBytes));
        Files.createDirectories(targetDir);
        Files.write(targetDir.resolve("libtstjni.so.part"), new byte[]{(byte) 0xDE, (byte) 0xAD});

        Path result = NativeLoader.extractToStableDir(fullBytes, "so", tmp);

        assertEquals(targetDir.resolve("libtstjni.so"), result);
        assertArrayEquals(fullBytes, Files.readAllBytes(result));
    }
}
