package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;

import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import org.junit.jupiter.api.Test;

class AccessControlTest {

    @Test
    void docExampleUserAndResource() {
        AccessControl ac = AccessControl.parse("#!::u=admin,r=bluesbrothers1_hi");
        assertEquals(Optional.of("admin"), ac.user());
        assertEquals(Optional.of("bluesbrothers1_hi"), ac.resource());
        assertEquals(Optional.empty(), ac.host());
        assertEquals(Optional.empty(), ac.session());
        assertEquals(AccessControl.Kind.STREAM, ac.kind());
        assertEquals("stream", ac.kindRaw());
        assertEquals(AccessControl.Mode.REQUEST, ac.mode());
        assertEquals("request", ac.modeRaw());
    }

    @Test
    void docExampleFilePublish() {
        AccessControl ac = AccessControl.parse("#!::u=johnny,t=file,m=publish,r=results.csv");
        assertEquals(AccessControl.Kind.FILE, ac.kind());
        assertEquals(AccessControl.Mode.PUBLISH, ac.mode());
        assertEquals(Optional.of("results.csv"), ac.resource());
    }

    @Test
    void customKeysAndOtherValues() {
        AccessControl ac = AccessControl.parse(
                "#!::user_tier=gold,t=telemetry,m=bidirectional,h=cam.example,s=abc");
        assertEquals(Optional.of("gold"), ac.get("user_tier"));
        assertEquals(AccessControl.Kind.OTHER, ac.kind());
        assertEquals("telemetry", ac.kindRaw());
        assertEquals(AccessControl.Mode.BIDIRECTIONAL, ac.mode());
        assertEquals(Optional.of("cam.example"), ac.host());
        assertEquals(Optional.of("abc"), ac.session());
        assertEquals(Optional.empty(), ac.get("u"));
    }

    @Test
    void allKeepsInputOrderAndIsUnmodifiable() {
        AccessControl ac = AccessControl.parse("#!::u=a,user_x=1,r=b");
        assertEquals(List.of("u", "user_x", "r"), new ArrayList<>(ac.all().keySet()));
        assertThrows(UnsupportedOperationException.class, () -> ac.all().put("k", "v"));
    }

    @Test
    void rejectsWhatIsNotTheForm() {
        IllegalArgumentException e =
                assertThrows(IllegalArgumentException.class, () -> AccessControl.parse("publish:amber"));
        assertTrue(e.getMessage().contains("not an access-control"), e.getMessage());
        e = assertThrows(IllegalArgumentException.class, () -> AccessControl.parse("#!{a:b}"));
        assertTrue(e.getMessage().contains("not supported"), e.getMessage());
        e = assertThrows(IllegalArgumentException.class, () -> AccessControl.parse("#!::u=a,u=b"));
        assertTrue(e.getMessage().contains("duplicate key"), e.getMessage());
    }
}
