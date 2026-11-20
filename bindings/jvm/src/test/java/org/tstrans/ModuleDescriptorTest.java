package org.tstrans;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.lang.module.ModuleDescriptor;
import java.lang.module.ModuleFinder;
import java.nio.file.Path;
import java.util.Set;
import java.util.TreeSet;
import java.util.stream.Collectors;
import org.junit.jupiter.api.Test;

class ModuleDescriptorTest {

    /**
     * Every package of org.tstrans but {@code org.tstrans.internal} is public API: a named-module
     * consumer ({@code requires org.tstrans;}) must reach each one, and must not reach the
     * internal one. The classpath test run never enforces the descriptor, so this reads the
     * compiled one.
     */
    @Test
    void everyPackageButTheInternalOneIsExported() throws Exception {
        Path classes =
                Path.of(Version.class.getProtectionDomain().getCodeSource().getLocation().toURI());
        ModuleDescriptor d = ModuleFinder.of(classes).find("org.tstrans").orElseThrow().descriptor();
        Set<String> exported =
                d.exports().stream()
                        .filter(e -> !e.isQualified())
                        .map(ModuleDescriptor.Exports::source)
                        .collect(Collectors.toCollection(TreeSet::new));
        Set<String> packages = new TreeSet<>(d.packages());
        assertTrue(packages.remove("org.tstrans.internal"), "org.tstrans.internal is missing");
        assertEquals(packages, exported);
        assertTrue(
                exported.containsAll(
                        Set.of("org.tstrans.shells", "org.tstrans.hls", "org.tstrans.webrtc")));
    }

    /** The close-time end-reason carrier lives in the non-exported package, not on the enum. */
    @Test
    void endReasonHasNoPublicNestedClass() {
        for (Class<?> c : org.tstrans.shells.EndReason.class.getDeclaredClasses()) {
            assertFalse(java.lang.reflect.Modifier.isPublic(c.getModifiers()), c.getName());
        }
        assertEquals("org.tstrans.internal",
                org.tstrans.internal.EndReasonSnapshot.class.getPackageName());
    }
}
