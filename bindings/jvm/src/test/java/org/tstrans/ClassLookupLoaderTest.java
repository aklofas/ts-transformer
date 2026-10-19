package org.tstrans;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.tstrans.codec.Codec;
import org.tstrans.codec.H264Sps;

/**
 * The native side resolves its classes by name with {@code FindClass}, which
 * uses the loader of the class that declared the native. The thread's context
 * class loader must not take part: here it refuses every load, and the
 * returned object graph must still build.
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
}
