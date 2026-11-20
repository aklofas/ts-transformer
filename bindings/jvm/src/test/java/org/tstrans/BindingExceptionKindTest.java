package org.tstrans;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.List;
import java.util.Set;
import java.util.TreeSet;
import java.util.stream.Stream;
import org.junit.jupiter.api.Test;
import org.tstrans.shells.Sender;

/** {@link BindingException#kind()} reads the concrete class's {@code Kind} through the base. */
class BindingExceptionKindTest {

    private static final List<Class<? extends BindingException>> SUBCLASSES = List.of(
        CodecParseException.class, DemuxException.class, HlsException.class,
        KlvDecodeException.class, KlvEncodeException.class, MuxException.class,
        RistException.class, RtpException.class, RtspException.class, SrtException.class,
        TcpException.class, UdpException.class, WebrtcException.class);

    @Test
    void aGenericCatchReadsTheKind() {
        // A bad srt:// URL, caught as the base type.
        BindingException e = assertThrows(BindingException.class,
            () -> Sender.open("srt://127.0.0.1:9000?latency=not-a-number").close());
        assertSame(SrtException.class, e.getClass());
        assertEquals("CONFIG_INVALID", e.kind().name());
        assertSame(((SrtException) e).kind(), e.kind());
        // Another class through the same base: an rtp:// URL naming a host where an IP
        // literal is required.
        BindingException r = assertThrows(BindingException.class,
            () -> Sender.open("rtp://media.example:5004").close());
        assertSame(RtpException.class, r.getClass());
        assertEquals("HOST_NOT_LITERAL", r.kind().name());
        assertSame(((RtpException) r).kind(), r.kind());
    }

    @Test
    void eachSubclassOverridesKindWithItsOwnKind() throws Exception {
        Method base = BindingException.class.getMethod("kind");
        assertEquals(Enum.class, base.getReturnType());
        assertTrue(Modifier.isAbstract(base.getModifiers()));
        for (Class<? extends BindingException> c : SUBCLASSES) {
            assertTrue(Modifier.isFinal(c.getModifiers()), c + " is final");
            Class<?> kind = Class.forName(c.getName() + "$Kind");
            assertTrue(kind.isEnum(), kind + " is an enum");
            Method m = c.getMethod("kind");
            assertEquals(c, m.getDeclaringClass(), c + " declares kind()");
            assertEquals(kind, m.getReturnType(), c + ".kind() returns its own Kind");
        }
        // The 13 above are every exception class beside the base.
        Path dir = Paths.get(BindingException.class.getResource("BindingException.class").toURI()).getParent();
        Set<String> found = new TreeSet<>();
        try (Stream<Path> files = Files.list(dir)) {
            files.map(f -> f.getFileName().toString())
                .filter(n -> n.endsWith("Exception.class") && !n.contains("$"))
                .map(n -> n.substring(0, n.length() - ".class".length()))
                .filter(n -> !n.equals("BindingException"))
                .forEach(found::add);
        }
        Set<String> listed = new TreeSet<>();
        for (Class<?> c : SUBCLASSES) listed.add(c.getSimpleName());
        assertEquals(listed, found);
    }
}
