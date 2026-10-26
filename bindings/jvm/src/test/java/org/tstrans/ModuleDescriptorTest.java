package org.tstrans;

import static org.junit.jupiter.api.Assertions.assertEquals;
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
     * Every package of org.tstrans is public API: a named-module consumer ({@code requires
     * org.tstrans;}) must reach each one. The classpath test run never enforces the descriptor, so
     * this reads the compiled one.
     */
    @Test
    void everyPackageIsExported() throws Exception {
        Path classes =
                Path.of(Version.class.getProtectionDomain().getCodeSource().getLocation().toURI());
        ModuleDescriptor d = ModuleFinder.of(classes).find("org.tstrans").orElseThrow().descriptor();
        Set<String> exported =
                d.exports().stream()
                        .filter(e -> !e.isQualified())
                        .map(ModuleDescriptor.Exports::source)
                        .collect(Collectors.toCollection(TreeSet::new));
        assertEquals(new TreeSet<>(d.packages()), exported);
        assertTrue(
                exported.containsAll(
                        Set.of("org.tstrans.shells", "org.tstrans.hls", "org.tstrans.webrtc")));
    }
}
