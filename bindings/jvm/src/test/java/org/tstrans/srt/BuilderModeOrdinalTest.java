package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.Map;
import java.util.stream.Collectors;
import org.junit.jupiter.api.Test;
import org.tstrans.SrtException;
import org.tstrans.TestSupport;

/** {@code Builder.Mode} is package-private, so its code pin lives beside it (native table: srt/lowlevel.rs). */
class BuilderModeOrdinalTest {
    @Test
    void modeCodesMatchTheNativeDecodeTable() {
        assertEquals(
            Map.of("URL_CHOICE", 0, "CALLER", 1, "LISTENER", 2, "RENDEZVOUS", 3),
            Arrays.stream(Builder.Mode.values()).collect(Collectors.toMap(Enum::name, Builder.Mode::code)));
    }

    /** An unknown mode code is refused before any socket work; it is not read as {@code URL_CHOICE}. */
    @Test
    void anUnknownModeCodeIsConfigInvalidOnConnectAndListen() throws Exception {
        String connectUrl = "srt://127.0.0.1:" + TestSupport.freeUdpPort() + "?connect_timeout=500";
        String listenUrl = "srt://127.0.0.1:" + TestSupport.freeUdpPort() + "?mode=listener";
        for (String name : new String[] {"nConnect", "nListen"}) {
            Method m = Builder.class.getDeclaredMethod(name, String.class, int.class,
                Integer.class, String.class, String.class, String.class,
                Integer.class, Integer.class, Integer.class, Integer.class,
                Integer.class, Integer.class, Long.class, Integer.class, Integer.class);
            m.setAccessible(true);
            String url = name.equals("nConnect") ? connectUrl : listenUrl;
            InvocationTargetException e = assertThrows(InvocationTargetException.class,
                () -> m.invoke(null, url, 99, null, null, null, null, null, null, null, null, null, null, null, null, null),
                name);
            SrtException s = assertInstanceOf(SrtException.class, e.getCause(), name);
            assertEquals(SrtException.Kind.CONFIG_INVALID, s.kind(), name + ": " + s.getMessage());
            assertTrue(s.getMessage().contains("unknown Builder.Mode code 99"), name + ": " + s.getMessage());
        }
    }
}
