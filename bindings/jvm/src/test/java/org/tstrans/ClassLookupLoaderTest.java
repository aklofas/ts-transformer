package org.tstrans;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.tstrans.codec.Codec;
import org.tstrans.codec.H264Sps;

/**
 * The native side resolves its classes by name with {@code FindClass}, which
 * uses the loader of the class that declared the native. The thread's context
 * class loader must not take part: here it refuses every load, and the
 * returned object graph must still build.
 *
 * <p>jni also fills its own per-type class caches (Throwable, String, the
 * exception types) on first use through the context loader; a refusing loader
 * there used to kill the JVM. The library warms those caches when it loads, so
 * a typed exception thrown on such a thread arrives as an exception. That case
 * runs in a fresh JVM, because in this one the caches may already be warm.
 */
class ClassLookupLoaderTest {
    // Real 1080p High@4.0 SPS, the same bytes as H264Test.
    private static final byte[] SPS_1080P_HIGH40 = TestSupport.unsigned(
            100, 16, 40, 172, 184, 15, 0, 68, 252, 184, 11, 80, 16, 16, 20, 0,
            3, 0, 4, 0, 0, 3, 0, 240, 16);

    /** A context loader that refuses every class. */
    private static final ClassLoader REFUSING = new ClassLoader(null) {
        @Override
        protected Class<?> loadClass(String name, boolean resolve) {
            throw new SecurityException("context loader refuses " + name);
        }
    };

    private static Throwable onRefusingThread(Runnable body) throws InterruptedException {
        AtomicReference<Throwable> failure = new AtomicReference<>();
        Thread t = new Thread(() -> {
            try {
                body.run();
            } catch (Throwable e) {
                failure.set(e);
            }
        });
        t.setContextClassLoader(REFUSING);
        t.start();
        t.join(30_000);
        assertTrue(!t.isAlive(), "lookup thread did not finish");
        return failure.get();
    }

    @Test
    void objectGraphBuildsWhenTheContextLoaderRefuses() throws InterruptedException {
        AtomicReference<H264Sps> sps = new AtomicReference<>();
        Throwable failure = onRefusingThread(() -> {
            try {
                sps.set(Codec.parseH264Sps(SPS_1080P_HIGH40));
            } catch (CodecParseException e) {
                throw new AssertionError(e);
            }
        });
        assertNull(failure);
        assertEquals(1920L, sps.get().width());
        assertEquals(1080L, sps.get().height());
    }

    @Test
    void typedExceptionArrivesInAFreshJvmWhoseFirstThreadRefuses() throws Exception {
        String java = System.getProperty("java.home") + File.separator + "bin" + File.separator + "java";
        // The child writes to a file, not a pipe, so its output can never
        // block it while this side waits.
        Path log = Files.createTempFile("tstrans-fresh-jvm", ".log");
        try {
            Process p = new ProcessBuilder(java, "-Xcheck:jni", "-cp", System.getProperty("java.class.path"),
                            FreshJvm.class.getName())
                    .redirectErrorStream(true)
                    .redirectOutput(log.toFile())
                    .start();
            boolean finished = p.waitFor(60, TimeUnit.SECONDS);
            if (!finished) {
                // Never leave the child running on the runner.
                p.destroyForcibly().waitFor(10, TimeUnit.SECONDS);
            }
            String out = new String(Files.readAllBytes(log), StandardCharsets.UTF_8);
            assertTrue(finished, "child JVM did not finish: " + out);
            assertEquals(0, p.exitValue(), out);
            assertTrue(out.contains("TYPED TRUNCATED_RBSP"), out);
        } finally {
            Files.deleteIfExists(log);
        }
    }

    /**
     * The child JVM: its first native call, and so the library load itself,
     * happens on a thread whose context loader refuses every class.
     */
    public static final class FreshJvm {
        public static void main(String[] args) throws Exception {
            Throwable failure = onRefusingThread(() -> {
                try {
                    Codec.parseH264Sps(new byte[0]);
                    System.out.println("NO EXCEPTION");
                } catch (CodecParseException e) {
                    System.out.println("TYPED " + e.kind());
                }
            });
            if (failure != null) {
                failure.printStackTrace(System.out);
                System.exit(1);
            }
        }
    }
}
