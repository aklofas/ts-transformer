package org.tstrans.klv;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.lang.reflect.Array;
import java.lang.reflect.RecordComponent;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.List;
import java.util.stream.Stream;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.MethodSource;

/** A record's list fields are snapshots: the caller's list is neither aliased nor writable through the record. */
class RecordListOwnershipTest {

    static Stream<Class<? extends Record>> records() {
        return Stream.of(PayloadList.class, RvtAoi.class, RvtLs.class, RvtPoi.class,
            SdccFlpField.class, SecurityLs.class, UasDatalinkLs.class, VmtiLs.class,
            VTargetPack.class);
    }

    @ParameterizedTest
    @MethodSource("records")
    void recordListsDoNotAliasTheCallersList(Class<? extends Record> type) throws Exception {
        RecordComponent[] comps = type.getRecordComponents();
        Class<?>[] types = new Class<?>[comps.length];
        Object[] args = new Object[comps.length];
        List<List<Object>> supplied = new ArrayList<>();
        for (int i = 0; i < comps.length; i++) {
            types[i] = comps[i].getType();
            if (types[i] == List.class) {
                List<Object> l = new ArrayList<>();
                l.add(null);
                supplied.add(l);
                args[i] = l;
            } else if (types[i].isPrimitive()) {
                args[i] = Array.get(Array.newInstance(types[i], 1), 0);
            } else if (types[i] == ByteBuffer.class) {
                args[i] = ByteBuffer.allocate(16);
            }
        }
        var ctor = type.getDeclaredConstructor(types);
        Record r = ctor.newInstance(args);
        supplied.forEach(List::clear);
        for (RecordComponent c : comps) {
            if (c.getType() != List.class) continue;
            @SuppressWarnings("unchecked")
            List<Object> got = (List<Object>) c.getAccessor().invoke(r);
            assertEquals(1, got.size(), type.getSimpleName() + "." + c.getName());
            assertThrows(UnsupportedOperationException.class, () -> got.add(null));
        }
    }
}
