package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;

import java.lang.reflect.Field;
import java.lang.reflect.Modifier;
import java.util.Arrays;
import java.util.Set;
import java.util.TreeSet;
import org.junit.jupiter.api.Test;

/** The Java {@link RejectReason} constants match the native typed predefined reasons. */
class RejectReasonTableTest {

    @Test
    void constantsMatchTheRustTable() throws Exception {
        Set<String> java = new TreeSet<>();
        for (Field f : RejectReason.class.getDeclaredFields()) {
            int m = f.getModifiers();
            if (f.getType() == RejectReason.class && Modifier.isPublic(m) && Modifier.isStatic(m)) {
                RejectReason r = (RejectReason) f.get(null);
                assertEquals(f.getName(), r.name(), "constant name");
                java.add(r.code() + " " + r.name());
            }
        }
        Set<String> rust = new TreeSet<>(Arrays.asList(RejectReason.nTable()));
        assertEquals(39, rust.size());
        assertEquals(rust, java);
    }

    @Test
    void everyRustCodeHasAName() {
        // libsrt's 18 internal codes, as a failed connect reports them.
        for (int code = 0; code <= 17; code++) {
            RejectReason r = RejectReason.ofCode(code);
            assertNotEquals("OTHER", r.name(), "code " + code);
            assertSame(r, RejectReason.ofCode(code));
        }
        assertSame(RejectReason.BAD_SECRET, RejectReason.ofCode(10));
        assertEquals("RDV_COOKIE", RejectReason.ofCode(9).name());
        assertEquals("MESSAGE_API", RejectReason.ofCode(12).name());
        // A rule still may not send one.
        assertThrows(IllegalArgumentException.class, () -> ListenRule.reject(RejectReason.BAD_SECRET));
        assertEquals("OTHER", RejectReason.ofCode(999).name());
    }
}
