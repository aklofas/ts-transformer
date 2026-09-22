package org.tstrans;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import org.junit.jupiter.api.Test;

class CodecErrorModelTest {
    @Test
    void kindHasAllFifteenRustVariants() {
        // Mirrors tst_core::codec::CodecParseError. A Rust-side rename or
        // addition is caught here. Task B3.2 ADDED the three nal-framing
        // converter errors that used to fold into the ENGINE_ERROR wildcard;
        // nothing retires, so B3.6 leaves this at fifteen.
        CodecParseException.Kind[] ks = CodecParseException.Kind.values();
        assertEquals(15, ks.length);
        for (String n : new String[] {
                "TRUNCATED_RBSP", "INVALID_GOLOMB", "RESERVED_VALUE", "UNSUPPORTED_PROFILE",
                "DANGLING_SPS_REFERENCE", "DANGLING_VPS_REFERENCE", "ENGINE_ERROR",
                "INVALID_LEB128", "BAD_SYNC_WORD", "TRUNCATED", "FORBIDDEN",
                "UNSUPPORTED_FREE_FORMAT",
                "INVALID_LENGTH_SIZE", "NAL_LENGTH_OVERFLOW", "BUFFER_TOO_SMALL"}) {
            CodecParseException.Kind.valueOf(n); // throws if missing
        }
    }

    @Test
    void truncatedRbspCarriesOffsetAndNeededBits() {
        CodecParseException e = new CodecParseException(
                CodecParseException.Kind.TRUNCATED_RBSP, "h264", "truncated",
                /* offsetBits */ 80, /* neededBits */ 5, /* field */ null,
                /* value */ null, /* profileIdc */ null, /* spsId */ null,
                /* vpsId */ null, /* offsetBytes */ null, /* expected */ null,
                /* found */ null, /* needed */ null, /* had */ null, /* layer */ null);
        assertTrue(e instanceof BindingException, "must extend BindingException");
        assertEquals(CodecParseException.Kind.TRUNCATED_RBSP, e.kind());
        assertEquals("h264", e.codec());
        assertEquals("truncated", e.getMessage());
        assertEquals(Integer.valueOf(80), e.offsetBits());
        assertEquals(Integer.valueOf(5), e.neededBits());
        assertNull(e.value());
        assertNull(e.field());
    }

    @Test
    void reservedValueCarriesFieldAndValue() {
        CodecParseException e = new CodecParseException(
                CodecParseException.Kind.RESERVED_VALUE, "h265", "reserved",
                /* offsetBits */ null, /* neededBits */ null,
                /* field */ "chroma_format_idc", /* value */ 4,
                /* profileIdc */ null, /* spsId */ null, /* vpsId */ null,
                /* offsetBytes */ null, /* expected */ null, /* found */ null,
                /* needed */ null, /* had */ null, /* layer */ null);
        assertEquals(CodecParseException.Kind.RESERVED_VALUE, e.kind());
        assertEquals("chroma_format_idc", e.field());
        assertEquals(Integer.valueOf(4), e.value());
        assertNull(e.offsetBits());
    }
}
