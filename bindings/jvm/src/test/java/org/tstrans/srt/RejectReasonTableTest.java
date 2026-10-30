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
        assertEquals(21, rust.size());
        assertEquals(rust, java);
    }
}
