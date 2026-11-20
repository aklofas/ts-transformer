package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.Map;
import java.util.stream.Collectors;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.NativeHandle;

/** {@code AuthScheme.code()} is package-private, so its code pin lives beside it (native table: rtp/mount_rules.rs). */
class AuthSchemeCodeTest {
    @Test
    void schemeCodesMatchTheNativeDecodeTable() {
        assertEquals(
            Map.of("BASIC", 0, "DIGEST_MD5", 1, "DIGEST_SHA256", 2),
            Arrays.stream(AuthScheme.values()).collect(Collectors.toMap(Enum::name, AuthScheme::code)));
    }

    /**
     * An unknown scheme code throws {@code IllegalArgumentException} naming the code, before the
     * rules are looked up; the rules stay open and take a known scheme afterwards.
     */
    @Test
    @Timeout(30)
    void anUnknownSchemeCodeIsAnIllegalArgument() throws Exception {
        assumeTrue(isLinux(), "RTSP server gated to Linux (real sockets + tokio runtime)");
        try (RtspServer server = RtspServer.start(RtspServerConfig.builder().bindAddr("127.0.0.1:0")
                .gracefulShutdownDrainMs(50).build());
             MountRules rules = server.mountRules()) {
            Method peek = NativeHandle.class.getDeclaredMethod("peekHandle");
            peek.setAccessible(true);
            Method set = MountRules.class.getDeclaredMethod("nSetChallenge", long.class, int.class, String.class);
            set.setAccessible(true);
            long handle = (long) peek.invoke(rules);
            InvocationTargetException e = assertThrows(InvocationTargetException.class,
                () -> set.invoke(null, handle, 99, "lab"));
            IllegalArgumentException iae = assertInstanceOf(IllegalArgumentException.class, e.getCause());
            assertEquals("unknown AuthScheme code 99", iae.getMessage());
            assertFalse(rules.isClosed());
            rules.setChallenge(AuthScheme.DIGEST_SHA256, "lab");
        }
    }
}
