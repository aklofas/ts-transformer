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

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/**
 * Unit tests for the content-addressed extraction helpers and dev-override
 * property. All tests are Linux-runnable; the
 * Windows-locked-DLL path (where {@link Files#delete} swallows
 * {@link IOException} on an in-use DLL) is documented but not exercised on CI.
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
    void extractToStableDirReusesFileOnSecondCall(@TempDir Path tmp) throws IOException {
        byte[] bytes = new byte[]{11, 22, 33, 44};

        // First extraction
        Path target = NativeLoader.extractToStableDir(bytes, "so", tmp);
        assertTrue(Files.exists(target));

        // Plant a sentinel so we can detect a re-write
        byte[] sentinel = new byte[]{(byte) 0xDE, (byte) 0xAD};
        Files.write(target, sentinel);

        // Second extraction with same bytes
        Path target2 = NativeLoader.extractToStableDir(bytes, "so", tmp);

        assertEquals(target, target2, "second call must return the same path");
        assertArrayEquals(sentinel, Files.readAllBytes(target2),
                "file must NOT be overwritten on second call — sentinel should survive");
    }

    // -----------------------------------------------------------------------
    // sweepStale
    // -----------------------------------------------------------------------

    @Test
    void sweepStaleRemovesOldDirs(@TempDir Path tmp) throws IOException {
        // Plant a stale dir with a dummy file inside
        Path staleDir = tmp.resolve("tstrans-native-0000dead");
        Files.createDirectories(staleDir);
        Files.write(staleDir.resolve("libtstjni.so"), new byte[]{1});

        // Plant another stale dir (no files)
        Path emptyStale = tmp.resolve("tstrans-native-cafebabe");
        Files.createDirectories(emptyStale);

        // Plant an unrelated dir that must not be touched
        Path unrelated = tmp.resolve("something-else");
        Files.createDirectories(unrelated);

        NativeLoader.sweepStale(tmp, "tstrans-native-current");

        assertFalse(Files.exists(staleDir), "stale dir with file should be deleted");
        assertFalse(Files.exists(emptyStale), "empty stale dir should be deleted");
        assertTrue(Files.exists(unrelated), "unrelated dir must be untouched");
    }

    @Test
    void sweepStalePreservesCurrentDir(@TempDir Path tmp) throws IOException {
        // Current dir must survive the sweep
        Path currentDir = tmp.resolve("tstrans-native-aabbccdd");
        Files.createDirectories(currentDir);
        Files.write(currentDir.resolve("libtstjni.so"), new byte[]{2});

        // Stale dir must be removed
        Path staleDir = tmp.resolve("tstrans-native-11223344");
        Files.createDirectories(staleDir);
        Files.write(staleDir.resolve("libtstjni.so"), new byte[]{3});

        NativeLoader.sweepStale(tmp, "tstrans-native-aabbccdd");

        assertTrue(Files.exists(currentDir), "current dir must survive");
        assertTrue(Files.exists(currentDir.resolve("libtstjni.so")), "current lib must survive");
        assertFalse(Files.exists(staleDir), "stale dir must be swept");
    }

    @Test
    void sweepStaleIsNoOpWhenNoStalePresent(@TempDir Path tmp) throws IOException {
        // No tstrans-native-* dirs at all → no error, no change
        Path unrelated = tmp.resolve("irrelevant");
        Files.createDirectories(unrelated);

        NativeLoader.sweepStale(tmp, "tstrans-native-current");

        assertTrue(Files.exists(unrelated)); // unchanged
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
    // Atomic write — partial-file recovery
    // -----------------------------------------------------------------------

    /**
     * Regression guard for the crash-mid-write scenario fixed by the atomic
     * write path.
     *
     * <p>Before the fix, {@code Files.write(target, bytes)} wrote directly to
     * the stable path. A JVM crash mid-write left a <em>partial</em>
     * {@code target}. On the next run the {@code Files.exists(target)} fast-
     * path reused it, and {@code System.load} threw an
     * {@link UnsatisfiedLinkError} with no recovery.
     *
     * <p>After the fix, writes land on {@code libtstjni.<ext>.part} first and
     * are promoted to {@code target} via {@code ATOMIC_MOVE}. A crash between
     * the write and the move therefore leaves only {@code .part} partial, never
     * {@code target}.  This test simulates that exact crash: plants a truncated
     * {@code .part} file (as if the JVM died after {@code Files.write(part)}
     * but before {@code Files.move}), then calls {@link NativeLoader#extractToStableDir}
     * and asserts that the complete library is extracted to {@code target}.
     */
    @Test
    void partialDotPartFileFromCrashedWriteIsOverwritten(@TempDir Path tmp) throws IOException {
        byte[] fullBytes = new byte[]{10, 20, 30, 40, 50};

        // Compute the stable-dir path so we can pre-plant the .part file.
        String hash = NativeLoader.contentHash(fullBytes);
        Path targetDir = tmp.resolve("tstrans-native-" + hash);
        Files.createDirectories(targetDir);

        // Simulate crash: .part written but move never completed → target absent.
        Path partFile = targetDir.resolve("libtstjni.so.part");
        Files.write(partFile, new byte[]{(byte) 0xDE, (byte) 0xAD}); // truncated

        // target does not exist → extraction must overwrite the stale .part and
        // produce a complete, loadable target.
        Path result = NativeLoader.extractToStableDir(fullBytes, "so", tmp);

        assertEquals(targetDir.resolve("libtstjni.so"), result,
                "must return the content-addressed target path");
        assertArrayEquals(fullBytes, Files.readAllBytes(result),
                "target must contain the full library bytes; stale .part must not block it");
        assertFalse(Files.exists(partFile),
                ".part staging file must be gone after successful atomic promotion");
    }
}
