package org.tstrans.hls;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;

class HlsValueTypesTest {
    @Test
    void hlsModeOrdinalsAreTheNativeContract() {
        assertEquals(0, HlsMode.LIVE.ordinal());
        assertEquals(1, HlsMode.EVENT.ordinal());
        assertEquals(2, HlsMode.VOD.ordinal());
        assertEquals(3, HlsMode.values().length);
    }

    @Test
    void publisherStatsOptionalFieldsUseMinusOneAsAbsent() {
        var absent = new PublisherStats(2, 376, -1, -1);
        assertTrue(absent.currentSegmentAgeUs().isEmpty());
        assertTrue(absent.lastSegmentDurationUs().isEmpty());
        var present = new PublisherStats(2, 376, 1500, 4000000);
        assertEquals(1500, present.currentSegmentAgeUs().getAsLong());
        assertEquals(4000000, present.lastSegmentDurationUs().getAsLong());
    }

    @Test
    void publisherInterfaceMirrorsTheRustTrait() throws Exception {
        // close() is AutoCloseable's quiet counterpart to finish(), not a trait
        // method — filtered out here exactly as the mirror rail filters it.
        var names = java.util.Arrays.stream(Publisher.class.getDeclaredMethods())
            .map(java.lang.reflect.Method::getName)
            .filter(name -> !name.equals("close"))
            .sorted().toList();
        assertEquals(java.util.List.of("cutSegment", "cutSegmentWithDuration", "finish", "pushTs", "stats"), names);
    }
}
