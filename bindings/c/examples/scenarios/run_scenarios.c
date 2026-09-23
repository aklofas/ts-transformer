/*
 * run_scenarios.c — C adapter for the cross-binding scenario harness (WS-5).
 *
 * WHY THIS FILE EXISTS
 * --------------------
 * The ts-transformer project has three binding layers: Rust (the core),
 * Python (tst-py / PyO3), and C (tst-c / cbindgen). WS-5 of the
 * test-architecture overhaul adds a cross-binding scenario harness that
 * proves all three bindings produce identical, deterministic output from the
 * same committed input artifacts. This file is the C adapter — the third
 * leg of that harness.
 *
 * For each scenario in the committed `scenarios.toml` manifest, this program:
 *   1. Reads the corresponding input artifact.
 *   2. Feeds it through the C ABI (tst_demuxer_* offline demuxer, ABI minor 8).
 *   3. Normalises the result to the binding-neutral golden envelope
 *      (schema_version, lossy, core[], extensions).
 *   4. Compares field-by-field against the committed `golden.json`.
 *   5. Prints a per-scenario pass/fail line; exits non-zero if any fail.
 *
 * The normalisation rules mirror the Rust adapter in
 * `crates/tst-integration/tests/rust_scenarios.rs` and the Python adapter in
 * `bindings/python/tests/test_scenarios.py` exactly.
 *
 * HOW TO BUILD AND RUN
 * --------------------
 * From the workspace root (ts-transformer/):
 *
 *   SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 cargo build -p tst-c
 *   gcc -I bindings/c/include -L target/debug -Wall -Werror \
 *       -o /tmp/run_scenarios \
 *       bindings/c/examples/scenarios/run_scenarios.c -ltstrans
 *   LD_LIBRARY_PATH=target/debug /tmp/run_scenarios \
 *       crates/tst-integration/tests/fixtures/scenarios
 *
 * Or use the companion shell script:
 *   bash bindings/c/examples/scenarios/run_scenarios.sh
 *
 * Default scenarios dir (when no argv[1] given):
 *   ../../../tst-integration/tests/fixtures/scenarios  (relative to CWD).
 *   This works when invoked from within bindings/c/ or from the workspace
 *   root; adjust with argv[1] if your working directory differs.
 *
 * DESIGN DECISIONS
 * ----------------
 *
 * SHA-256 (no external dep):
 *   The C standard library has no SHA-256. mbedTLS *is* statically linked
 *   into libtstrans.so but its symbols are not exported (only tst_* symbols
 *   are exported). Rather than pull in libcrypto or require an extra -I for
 *   mbedtls headers, this file embeds a compact public-domain SHA-256
 *   implementation (~80 lines). It produces the same digest as Rust's sha2
 *   crate — verified against the committed golden's payload_sha256 field.
 *
 * TOML parsing (hand-rolled minimal parser):
 *   scenarios.toml is a simple fixed-shape manifest. Pulling in a TOML
 *   library into a C teaching example would be heavyweight and inappropriate.
 *   The parser here handles exactly the shape needed:
 *     [[scenario]]
 *     id = "..."
 *     kind = "..."
 *     input = "..."
 *     golden = "..."
 *   It ignores all other keys. It does NOT handle TOML string escapes beyond
 *   basic quoted strings — the manifest uses only plain ASCII identifiers and
 *   file paths, so this is safe. Document any future non-ASCII paths before
 *   adding them to scenarios.toml.
 *
 * JSON parsing (hand-rolled minimal parser):
 *   The golden.json files have a known fixed shape. Rather than link a JSON
 *   library, this adapter implements a targeted extractor for the fields it
 *   needs. For golden comparison, it compares the normalised fields
 *   individually (program, pid, stream_type, pts, key, payload_sha256, set)
 *   rather than serialising its own output to a JSON string and doing a
 *   string compare — this is more robust to key ordering.
 *
 * Roundtrip scenarios (re-mux in C):
 *   The offline muxer surface (tst_mux_config_new, tst_muxer_open, push_video/
 *   audio/klv, pull, close) is ALWAYS available — ABI minor 9 un-gated it, so
 *   there is no TST_HAS_SRT guard and no hash-only fallback. The roundtrip
 *   runner dispatches on the scenario id; there are TWO roundtrip recipes,
 *   each re-muxed in C and compared for FULL byte-identity (memcmp) + sha256:
 *     - video-roundtrip: add_program(1, 0x1000) + add_video_stream(0x1011,
 *       H264); push one synthetic H.264 IDR at pts=0 (key_frame).
 *     - audio-klv-roundtrip: same program + video, plus add_audio_stream(
 *       0x1021, AAC) + add_klv_stream(0x1031, SYNCHRONOUS_METADATA,
 *       carries_pts=true); push H.264 IDR + one synthetic ADTS frame + raw
 *       ST 0601 LS (muxer auto-wraps the AU cell), all at pts=0.
 *   Each recipe drains with a 1316-byte pull loop, then asserts the C-produced
 *   bytes are byte-for-byte equal to the committed output.ts AND that their
 *   sha256 equals golden.extensions.output_sha256. This proves C reproduces
 *   the mux output exactly, not merely that it can hash a committed file.
 *
 * Strict-rejection scenario:
 *   Feed 8192 × 0xFF through tst_demuxer_feed (default config, no strict
 *   mode). The all-0xFF input has no 0x47 MPEG-TS sync bytes; after scanning
 *   SYNC_SEARCH_WINDOW (188 × 32 = 6016) bytes without finding a sync byte
 *   the demuxer returns an Unrecoverable error (negative tst_e code). Any
 *   negative code maps to the umbrella public code "STRICT_REJECTION" —
 *   the same umbrella mapping the Rust and Python adapters use. Also verifies
 *   that tst_demuxer_close(NULL) is safe (null-safe contract).
 */

#include "tstrans.h"
#include <arpa/inet.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

/* ── Compact SHA-256 implementation (public domain) ─────────────────────────
 *
 * WHY: The C standard library has no SHA-256. libtstrans exports only tst_*
 * symbols (mbedTLS is linked but its symbols are hidden). Rather than depend
 * on libssl/libcrypto, a compact (~80 LOC) public-domain implementation is
 * embedded here. Produces the same output as Rust's sha2::Sha256.
 *
 * Source: adapted from the public-domain "FIPS 180-4 reference implementation"
 * pattern used widely in embedded and test code.
 */

/* SHA-256 initial hash values (first 32 bits of the fractional parts of the
 * square roots of the first 8 primes — FIPS 180-4 §5.3.3). */
static const uint32_t SHA256_H0[8] = {
    0x6a09e667u, 0xbb67ae85u, 0x3c6ef372u, 0xa54ff53au,
    0x510e527fu, 0x9b05688cu, 0x1f83d9abu, 0x5be0cd19u,
};

/* SHA-256 round constants (first 32 bits of the fractional parts of the cube
 * roots of the first 64 primes — FIPS 180-4 §4.2.2). */
static const uint32_t SHA256_K[64] = {
    0x428a2f98u, 0x71374491u, 0xb5c0fbcfu, 0xe9b5dba5u,
    0x3956c25bu, 0x59f111f1u, 0x923f82a4u, 0xab1c5ed5u,
    0xd807aa98u, 0x12835b01u, 0x243185beu, 0x550c7dc3u,
    0x72be5d74u, 0x80deb1feu, 0x9bdc06a7u, 0xc19bf174u,
    0xe49b69c1u, 0xefbe4786u, 0x0fc19dc6u, 0x240ca1ccu,
    0x2de92c6fu, 0x4a7484aau, 0x5cb0a9dcu, 0x76f988dau,
    0x983e5152u, 0xa831c66du, 0xb00327c8u, 0xbf597fc7u,
    0xc6e00bf3u, 0xd5a79147u, 0x06ca6351u, 0x14292967u,
    0x27b70a85u, 0x2e1b2138u, 0x4d2c6dfcu, 0x53380d13u,
    0x650a7354u, 0x766a0abbu, 0x81c2c92eu, 0x92722c85u,
    0xa2bfe8a1u, 0xa81a664bu, 0xc24b8b70u, 0xc76c51a3u,
    0xd192e819u, 0xd6990624u, 0xf40e3585u, 0x106aa070u,
    0x19a4c116u, 0x1e376c08u, 0x2748774cu, 0x34b0bcb5u,
    0x391c0cb3u, 0x4ed8aa4au, 0x5b9cca4fu, 0x682e6ff3u,
    0x748f82eeu, 0x78a5636fu, 0x84c87814u, 0x8cc70208u,
    0x90befffau, 0xa4506cebu, 0xbef9a3f7u, 0xc67178f2u,
};

#define SHA256_ROTR(x, n) (((x) >> (n)) | ((x) << (32 - (n))))
#define SHA256_CH(x, y, z) (((x) & (y)) ^ (~(x) & (z)))
#define SHA256_MAJ(x, y, z) (((x) & (y)) ^ ((x) & (z)) ^ ((y) & (z)))
#define SHA256_EP0(x) (SHA256_ROTR(x,  2) ^ SHA256_ROTR(x, 13) ^ SHA256_ROTR(x, 22))
#define SHA256_EP1(x) (SHA256_ROTR(x,  6) ^ SHA256_ROTR(x, 11) ^ SHA256_ROTR(x, 25))
#define SHA256_SIG0(x) (SHA256_ROTR(x,  7) ^ SHA256_ROTR(x, 18) ^ ((x) >>  3))
#define SHA256_SIG1(x) (SHA256_ROTR(x, 17) ^ SHA256_ROTR(x, 19) ^ ((x) >> 10))

typedef struct {
    uint32_t state[8];  /* current hash state (H0..H7) */
    uint64_t bitlen;    /* total bits processed so far */
    uint8_t  data[64];  /* current 512-bit (64-byte) message block */
    size_t   datalen;   /* bytes buffered in data[] */
} sha256_ctx_t;

static void sha256_transform(sha256_ctx_t *ctx, const uint8_t block[64]) {
    /* Expand 16-word block to 64-word schedule (FIPS 180-4 §6.2.2 step 1). */
    uint32_t w[64];
    for (int i = 0; i < 16; i++) {
        w[i] = ((uint32_t)block[i*4  ] << 24)
             | ((uint32_t)block[i*4+1] << 16)
             | ((uint32_t)block[i*4+2] <<  8)
             | ((uint32_t)block[i*4+3]);
    }
    for (int i = 16; i < 64; i++) {
        w[i] = SHA256_SIG1(w[i-2]) + w[i-7] + SHA256_SIG0(w[i-15]) + w[i-16];
    }
    /* Working variables (step 2). */
    uint32_t a = ctx->state[0], b = ctx->state[1],
             c = ctx->state[2], d = ctx->state[3],
             e = ctx->state[4], f = ctx->state[5],
             g = ctx->state[6], h = ctx->state[7];
    /* 64 compression rounds (step 3). */
    for (int i = 0; i < 64; i++) {
        uint32_t t1 = h + SHA256_EP1(e) + SHA256_CH(e,f,g) + SHA256_K[i] + w[i];
        uint32_t t2 = SHA256_EP0(a) + SHA256_MAJ(a,b,c);
        h = g; g = f; f = e; e = d + t1;
        d = c; c = b; b = a; a = t1 + t2;
    }
    /* Add compressed chunk to current hash (step 4). */
    ctx->state[0] += a; ctx->state[1] += b;
    ctx->state[2] += c; ctx->state[3] += d;
    ctx->state[4] += e; ctx->state[5] += f;
    ctx->state[6] += g; ctx->state[7] += h;
}

static void sha256_init(sha256_ctx_t *ctx) {
    memcpy(ctx->state, SHA256_H0, sizeof(SHA256_H0));
    ctx->bitlen = 0;
    ctx->datalen = 0;
}

static void sha256_update(sha256_ctx_t *ctx, const uint8_t *data, size_t len) {
    for (size_t i = 0; i < len; i++) {
        ctx->data[ctx->datalen++] = data[i];
        if (ctx->datalen == 64) {
            sha256_transform(ctx, ctx->data);
            ctx->bitlen += 512;
            ctx->datalen = 0;
        }
    }
}

/* Produce the 32-byte digest into `out`. */
static void sha256_final(sha256_ctx_t *ctx, uint8_t out[32]) {
    size_t i = ctx->datalen;
    /* Pad the remaining data (FIPS 180-4 §5.1.1). */
    if (ctx->datalen < 56) {
        ctx->data[i++] = 0x80;
        while (i < 56) ctx->data[i++] = 0x00;
    } else {
        ctx->data[i++] = 0x80;
        while (i < 64) ctx->data[i++] = 0x00;
        sha256_transform(ctx, ctx->data);
        memset(ctx->data, 0, 56);
    }
    /* Append the original message length in bits as a 64-bit big-endian. */
    ctx->bitlen += (uint64_t)ctx->datalen * 8;
    for (int j = 7; j >= 0; j--) {
        ctx->data[56 + (7 - j)] = (uint8_t)(ctx->bitlen >> (j * 8));
    }
    sha256_transform(ctx, ctx->data);
    /* Produce output in big-endian word order. */
    for (int k = 0; k < 8; k++) {
        out[k*4  ] = (uint8_t)(ctx->state[k] >> 24);
        out[k*4+1] = (uint8_t)(ctx->state[k] >> 16);
        out[k*4+2] = (uint8_t)(ctx->state[k] >>  8);
        out[k*4+3] = (uint8_t)(ctx->state[k]);
    }
}

/* Hash `len` bytes at `data`; write lowercase hex digest into `hex_out`
 * (must be ≥ 65 bytes: 64 hex chars + NUL terminator). */
static void sha256_hex(const uint8_t *data, size_t len, char hex_out[65]) {
    sha256_ctx_t ctx;
    sha256_init(&ctx);
    sha256_update(&ctx, data, len);
    uint8_t digest[32];
    sha256_final(&ctx, digest);
    for (int i = 0; i < 32; i++) {
        sprintf(hex_out + i * 2, "%02x", (unsigned)digest[i]);
    }
    hex_out[64] = '\0';
}

/* ── File I/O helpers ────────────────────────────────────────────────────────*/

/* Read the entire contents of `path` into a heap-allocated buffer.
 * Sets *out_len to the number of bytes read. Returns NULL on error. */
static uint8_t *read_file(const char *path, size_t *out_len) {
    FILE *f = fopen(path, "rb");
    if (!f) {
        fprintf(stderr, "ERROR: cannot open '%s'\n", path);
        return NULL;
    }
    if (fseek(f, 0, SEEK_END) != 0) {
        fprintf(stderr, "ERROR: fseek failed on '%s'\n", path);
        fclose(f);
        return NULL;
    }
    long size = ftell(f);
    if (size < 0) {
        fprintf(stderr, "ERROR: ftell failed on '%s'\n", path);
        fclose(f);
        return NULL;
    }
    rewind(f);
    uint8_t *buf = (uint8_t *)malloc((size_t)size + 1);
    if (!buf) {
        fprintf(stderr, "ERROR: malloc failed (%ld bytes)\n", size);
        fclose(f);
        return NULL;
    }
    size_t n = fread(buf, 1, (size_t)size, f);
    fclose(f);
    if ((long)n != size) {
        fprintf(stderr, "ERROR: short read on '%s' (%zu of %ld bytes)\n",
                path, n, size);
        free(buf);
        return NULL;
    }
    buf[size] = '\0'; /* NUL-terminate for text parsing convenience */
    *out_len = n;
    return buf;
}

/* Build an absolute path from a directory and a relative path.
 * Result is malloc'd; caller must free. */
static char *path_join(const char *dir, const char *rel) {
    size_t dlen = strlen(dir);
    size_t rlen = strlen(rel);
    char *out = (char *)malloc(dlen + 1 + rlen + 1);
    if (!out) return NULL;
    memcpy(out, dir, dlen);
    out[dlen] = '/';
    memcpy(out + dlen + 1, rel, rlen);
    out[dlen + 1 + rlen] = '\0';
    return out;
}

/* ── Minimal TOML parser ─────────────────────────────────────────────────────
 *
 * WHY hand-rolled: scenarios.toml is a fixed-shape manifest with only simple
 * quoted-string values. Pulling a TOML library into a C teaching example
 * would be heavyweight. This parser handles exactly the needed shape:
 *
 *   [[scenario]]
 *   id = "..."
 *   kind = "..."
 *   input = "..."
 *   golden = "..."
 *   features = [...]    <-- ignored (array not needed)
 *   tier = "..."        <-- ignored
 *   schema_version = N  <-- ignored
 *
 * Limitations (all safe for the current manifest):
 *   - Reads only the four string keys listed above; ignores all others.
 *   - Does NOT handle TOML string escapes (no backslash processing).
 *   - Does NOT handle multi-line strings.
 *   - Silently accepts malformed lines (skips them).
 *
 * If scenarios.toml ever gains non-ASCII paths or escape sequences, extend
 * this parser or switch to a real TOML library before shipping.
 */

#define MAX_SCENARIOS 32
#define MAX_STR_LEN   256

typedef struct {
    char id[MAX_STR_LEN];
    char kind[MAX_STR_LEN];
    char input[MAX_STR_LEN];
    char golden[MAX_STR_LEN];
} scenario_entry_t;

/* Extract the value from a line like `  key = "value"` or `key = "value"`.
 * Copies the quoted content into `out` (max `out_size` bytes including NUL).
 * Returns 1 on success, 0 if the key doesn't match or line isn't a match. */
static int toml_extract_string(const char *line, const char *key,
                               char *out, size_t out_size) {
    /* Skip leading whitespace. */
    while (*line == ' ' || *line == '\t') line++;
    size_t klen = strlen(key);
    if (strncmp(line, key, klen) != 0) return 0;
    line += klen;
    /* Skip whitespace + '=' + whitespace. */
    while (*line == ' ' || *line == '\t') line++;
    if (*line != '=') return 0;
    line++;
    while (*line == ' ' || *line == '\t') line++;
    if (*line != '"') return 0;
    line++;
    /* Copy up to the closing '"'. */
    size_t i = 0;
    while (*line && *line != '"' && i < out_size - 1) {
        out[i++] = *line++;
    }
    out[i] = '\0';
    return 1;
}

/* Parse `scenarios.toml` content (NUL-terminated) into `entries`.
 * Returns the number of scenarios found (up to MAX_SCENARIOS). */
static int parse_scenarios_toml(const char *text, scenario_entry_t *entries) {
    int count = 0;
    int in_scenario = 0;  /* 1 once we've seen [[scenario]] */

    /* Walk line by line. */
    const char *p = text;
    while (*p) {
        /* Find end of current line. */
        const char *eol = p;
        while (*eol && *eol != '\n') eol++;

        /* Copy the line into a temporary buffer for parsing. */
        size_t llen = (size_t)(eol - p);
        if (llen >= MAX_STR_LEN) {
            /* Skip overlong lines (shouldn't happen in a well-formed manifest). */
            p = (*eol == '\n') ? eol + 1 : eol;
            continue;
        }
        char line[MAX_STR_LEN];
        memcpy(line, p, llen);
        line[llen] = '\0';

        /* Strip trailing '\r' (Windows CRLF). */
        if (llen > 0 && line[llen - 1] == '\r') {
            line[llen - 1] = '\0';
        }

        /* Detect a [[scenario]] section header — start a new entry. */
        {
            const char *t = line;
            while (*t == ' ' || *t == '\t') t++;
            if (strncmp(t, "[[scenario]]", 12) == 0) {
                if (count < MAX_SCENARIOS) {
                    memset(&entries[count], 0, sizeof(entries[count]));
                    count++;
                    in_scenario = 1;
                }
                goto next_line;
            }
        }

        /* Parse key-value pairs inside the current [[scenario]] block. */
        if (in_scenario && count > 0) {
            scenario_entry_t *e = &entries[count - 1];
            /* Try each of the four keys we care about. */
            if (toml_extract_string(line, "id",     e->id,     MAX_STR_LEN)) goto next_line;
            if (toml_extract_string(line, "kind",   e->kind,   MAX_STR_LEN)) goto next_line;
            if (toml_extract_string(line, "input",  e->input,  MAX_STR_LEN)) goto next_line;
            if (toml_extract_string(line, "golden", e->golden, MAX_STR_LEN)) goto next_line;
            /* Any other key (features, tier, schema_version) is silently ignored. */
        }

next_line:
        p = (*eol == '\n') ? eol + 1 : eol;
        /* Stop at the real NUL terminator. */
        if (*p == '\0') break;
    }
    return count;
}

/* ── Minimal JSON extractor ──────────────────────────────────────────────────
 *
 * WHY hand-rolled: golden.json has a fixed known shape. Rather than link a
 * JSON library, this extracts targeted fields. For a golden like:
 *
 *   {"schema_version":0,"lossy":false,"core":[...],"extensions":null}
 *
 * We extract:
 *   - core[] array: each object's "event", "program", "pid", "stream_type",
 *     "pts", "key", "payload_sha256", "set", "code" fields as strings.
 *   - extensions.output_sha256: the sha256 digest string.
 *
 * Approach: scan for `"key":"value"` / `"key":value` patterns. This is
 * order-independent and handles the fixed field set safely. It does NOT
 * handle nested objects, arrays of arrays, or TOML-style escapes beyond
 * `\"` in string values — all safe for the current golden shape.
 */

/* Extract a JSON string value for `key` in `json` (NUL-terminated).
 * `start` is the offset to begin searching from (for scanning arrays).
 * Returns the offset just past the value, or 0 on failure. Copies the
 * string content into `out` (truncated to out_size-1 chars + NUL).
 *
 * Handles optional whitespace between the key colon and the opening quote
 * — the serde_json pretty-printer emits `"key": "value"` (with a space)
 * while the compact form emits `"key":"value"` (no space). Both are valid
 * JSON and both appear in the committed golden files. */
static size_t json_extract_string(const char *json, size_t start,
                                  const char *key,
                                  char *out, size_t out_size) {
    /* Search for `"key":` (key name in quotes followed by colon). */
    char key_pat[MAX_STR_LEN + 4];
    snprintf(key_pat, sizeof(key_pat), "\"%s\":", key);
    const char *found = strstr(json + start, key_pat);
    if (!found) {
        out[0] = '\0';
        return 0;
    }
    const char *p = found + strlen(key_pat);
    /* Skip optional whitespace between ':' and the opening '"'. */
    while (*p == ' ' || *p == '\t' || *p == '\n' || *p == '\r') p++;
    if (*p != '"') {
        /* Value is not a string (e.g. null, number, boolean). */
        out[0] = '\0';
        return 0;
    }
    p++; /* skip opening '"' */
    size_t i = 0;
    while (*p && *p != '"' && i < out_size - 1) {
        if (*p == '\\' && *(p + 1) == '"') {
            /* Handle escaped quote. */
            out[i++] = '"';
            p += 2;
        } else {
            out[i++] = *p++;
        }
    }
    out[i] = '\0';
    /* Return offset past the closing '"'. */
    return (size_t)((p - json) + 1);
}

/* Extract a JSON integer (int64) for `key`. Returns 1 on success.
 * Handles optional whitespace between ':' and the digit sequence. */
static int json_extract_int64(const char *json, size_t start,
                              const char *key, int64_t *out) {
    char pat[MAX_STR_LEN + 4];
    snprintf(pat, sizeof(pat), "\"%s\":", key);
    const char *found = strstr(json + start, pat);
    if (!found) return 0;
    const char *vstart = found + strlen(pat);
    while (*vstart == ' ' || *vstart == '\t' || *vstart == '\n' || *vstart == '\r') vstart++;
    char *endp;
    *out = (int64_t)strtoll(vstart, &endp, 10);
    return (endp != vstart);
}

/* Extract a JSON boolean for `key`. Returns 1 on success.
 * Handles optional whitespace between ':' and the value. */
static int json_extract_bool(const char *json, size_t start,
                             const char *key, int *out) {
    char pat[MAX_STR_LEN + 4];
    snprintf(pat, sizeof(pat), "\"%s\":", key);
    const char *found = strstr(json + start, pat);
    if (!found) return 0;
    const char *vstart = found + strlen(pat);
    while (*vstart == ' ' || *vstart == '\t' || *vstart == '\n' || *vstart == '\r') vstart++;
    if (strncmp(vstart, "true", 4) == 0)  { *out = 1; return 1; }
    if (strncmp(vstart, "false", 5) == 0) { *out = 0; return 1; }
    return 0;
}

/* ── pid → stream_type map ───────────────────────────────────────────────────
 *
 * WHY two-pass: the golden's stream_type is the raw PMT byte (e.g. "0x1b"),
 * not a codec enum. Like the Rust and Python normalisers, we build a
 * pid → stream_type_byte map from ProgramMap events first, then emit media
 * events using that map. This makes the normalisation binding-neutral.
 */

#define MAX_PIDS 64

typedef struct {
    uint16_t pid;
    uint8_t  stream_type_byte;
} pid_st_entry_t;

typedef struct {
    pid_st_entry_t entries[MAX_PIDS];
    int count;
} pid_st_map_t;

static void pid_st_map_insert(pid_st_map_t *m, uint16_t pid, uint8_t st) {
    /* Only insert if not already present (first ProgramMap wins). */
    for (int i = 0; i < m->count; i++) {
        if (m->entries[i].pid == pid) return;
    }
    if (m->count < MAX_PIDS) {
        m->entries[m->count].pid = pid;
        m->entries[m->count].stream_type_byte = st;
        m->count++;
    }
}

static uint8_t pid_st_map_lookup(const pid_st_map_t *m, uint16_t pid, uint8_t fallback) {
    for (int i = 0; i < m->count; i++) {
        if (m->entries[i].pid == pid) return m->entries[i].stream_type_byte;
    }
    return fallback;
}

/* ── Core event list ─────────────────────────────────────────────────────────
 *
 * A simple growable array of normalised CoreEvent structs, matching the golden
 * envelope's "core" array. Each event carries just the fields that the
 * normalised golden checks.
 */

typedef enum {
    CE_VIDEO = 0,
    CE_AUDIO,
    CE_KLV,
    CE_SUBTITLE,
    CE_UNKNOWN,
    CE_ERROR,
} core_event_kind_t;

typedef struct {
    core_event_kind_t kind;
    /* Common fields */
    uint16_t program;
    uint16_t pid;
    char     stream_type[16];  /* "0x1b" etc. */
    /* Video / Audio */
    int64_t  pts;
    int      key;              /* 1=true, 0=false */
    char     payload_sha256[65];
    /* KLV */
    char     set[32];          /* "st0601" or "unknown" */
    /* Subtitle */
    char     codec[32];        /* "dvb_subtitle" | "dvb_teletext" | "webvtt" | "cea708_standalone" */
    /* Error */
    char     code[64];         /* "STRICT_REJECTION" etc. */
} core_event_t;

#define MAX_CORE_EVENTS 256

typedef struct {
    core_event_t events[MAX_CORE_EVENTS];
    int count;
} core_event_list_t;

static int core_event_list_push(core_event_list_t *list, const core_event_t *ev) {
    if (list->count >= MAX_CORE_EVENTS) {
        fprintf(stderr, "WARNING: core event list full (>%d events)\n", MAX_CORE_EVENTS);
        return 0;
    }
    list->events[list->count++] = *ev;
    return 1;
}

/* ── MISB ST 0601 UL prefix (first 13 bytes) ────────────────────────────────
 *
 * Used to identify the KLV set from the raw payload bytes — binding-neutral
 * (the same 16-byte Universal Label is visible to C, Python, and Rust adapters).
 * Returns "st0601" if the payload starts with this prefix, "unknown" otherwise.
 */
static const uint8_t ST0601_UL_PREFIX[13] = {
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01,
    0x0E, 0x01, 0x03, 0x01, 0x01,
};

static const char *klv_set_from_ul(const uint8_t *payload, size_t payload_len) {
    /* The ST 0601 set is identified by the first 13 bytes of its Universal
     * Label key. Bytes 14-16 carry the version and are NOT matched here —
     * only the stable 13-byte prefix matters for set identification. */
    if (payload_len >= sizeof(ST0601_UL_PREFIX)
        && memcmp(payload, ST0601_UL_PREFIX, sizeof(ST0601_UL_PREFIX)) == 0) {
        return "st0601";
    }
    return "unknown";
}

/* ── Subtitle codec → binding-neutral string tag ────────────────────────────
 *
 * Maps the C `tst_subtitle_codec` enum (on tst_event_t.u.sample.codec when
 * stream_kind == TST_STREAM_KIND_SUBTITLE) to the binding-neutral string used
 * in the golden's `subtitle.codec` field. Mirrors the Rust `subtitle_codec_tag()`
 * table and the Python `_SUBTITLE_CODEC_TAG` dict EXACTLY:
 *   DVB_SUBTITLING    → "dvb_subtitle"
 *   DVB_TELETEXT      → "dvb_teletext"
 *   WEB_VTT_IN_TS     → "webvtt"
 *   CEA708_STANDALONE → "cea708_standalone"
 */
static const char *subtitle_codec_tag(int codec) {
    switch (codec) {
        case TST_SUBTITLE_CODEC_DVB_SUBTITLING:    return "dvb_subtitle";
        case TST_SUBTITLE_CODEC_DVB_TELETEXT:      return "dvb_teletext";
        case TST_SUBTITLE_CODEC_WEB_VTT_IN_TS:     return "webvtt";
        case TST_SUBTITLE_CODEC_CEA708_STANDALONE: return "cea708_standalone";
        default:                                   return "unknown";
    }
}

/* ── NonConformant issue_code → stable public string code ────────────────────
 *
 * Maps the C `tst_nonconformant_code` enum (on tst_event_t.u.nonconformant.
 * issue_code) to the stable public string code emitted in the golden's
 * `error.code` field. The strings are exactly the `TST_NONCONFORMANT_CODE_*`
 * constant base names (minus the `TST_NONCONFORMANT_CODE_` prefix), matching
 * the Rust `nonconformant_issue_code()` and Python `NonConformantKind.name`
 * outputs byte-for-byte.
 *
 * Returns NULL for an unrecognised code so the caller can fail loudly rather
 * than silently emit a wrong/empty code — an honest cross-binding gap signal.
 */
static const char *nonconformant_code_str(int code) {
    switch (code) {
        case TST_NONCONFORMANT_CODE_STREAM_TYPE_MISMATCH_SYNC_ON_ASYNC_PID:
            return "STREAM_TYPE_MISMATCH_SYNC_ON_ASYNC_PID";
        case TST_NONCONFORMANT_CODE_STREAM_TYPE_MISMATCH_ASYNC_ON_SYNC_PID:
            return "STREAM_TYPE_MISMATCH_ASYNC_ON_SYNC_PID";
        case TST_NONCONFORMANT_CODE_MISSING_METADATA_DESCRIPTOR: return "MISSING_METADATA_DESCRIPTOR";
        case TST_NONCONFORMANT_CODE_PCR_ANOMALY:                 return "PCR_ANOMALY";
        case TST_NONCONFORMANT_CODE_PSI_CHECKSUM_MISMATCH:       return "PSI_CHECKSUM_MISMATCH";
        case TST_NONCONFORMANT_CODE_PUSI_MID_PES:                return "PUSI_MID_PES";
        case TST_NONCONFORMANT_CODE_PID_REUSED_ACROSS_PROGRAMS:  return "PID_REUSED_ACROSS_PROGRAMS";
        case TST_NONCONFORMANT_CODE_SUBTITLE_MISSING_DESCRIPTOR: return "SUBTITLE_MISSING_DESCRIPTOR";
        case TST_NONCONFORMANT_CODE_SUBTITLE_DESCRIPTOR_AMBIGUOUS: return "SUBTITLE_DESCRIPTOR_AMBIGUOUS";
        case TST_NONCONFORMANT_CODE_SUBTITLE_DESCRIPTOR_MALFORMED: return "SUBTITLE_DESCRIPTOR_MALFORMED";
        case TST_NONCONFORMANT_CODE_AV1_REGISTRATION_MALFORMED:  return "AV1_REGISTRATION_MALFORMED";
        case TST_NONCONFORMANT_CODE_AV1_OBU_MISSING_SIZE_FIELD:  return "AV1_OBU_MISSING_SIZE_FIELD";
        case TST_NONCONFORMANT_CODE_AV1_TILE_LIST_NOT_ALLOWED:   return "AV1_TILE_LIST_NOT_ALLOWED";
        case TST_NONCONFORMANT_CODE_PSI_OVERLONG_SECTION:        return "PSI_OVERLONG_SECTION";
        case TST_NONCONFORMANT_CODE_TRANSPORT_ERROR_PACKET:      return "TRANSPORT_ERROR_PACKET";
        case TST_NONCONFORMANT_CODE_PSI_CC_DISCONTINUITY:        return "PSI_CC_DISCONTINUITY";
        case TST_NONCONFORMANT_CODE_MULTI_CELL_AU:               return "MULTI_CELL_AU";
        case TST_NONCONFORMANT_CODE_PSI_MULTI_SECTION_UNSUPPORTED: return "PSI_MULTI_SECTION_UNSUPPORTED";
        case TST_NONCONFORMANT_CODE_OTHER:                       return "OTHER";
        case TST_NONCONFORMANT_CODE_MALFORMED_PES:               return "MALFORMED_PES";
        case TST_NONCONFORMANT_CODE_DVB_SUB_DATA_IDENTIFIER:     return "DVB_SUB_DATA_IDENTIFIER";
        case TST_NONCONFORMANT_CODE_PTS_ANOMALY:                 return "PTS_ANOMALY";
        case TST_NONCONFORMANT_CODE_MISSING_REQUIRED_PTS:        return "MISSING_REQUIRED_PTS";
        case TST_NONCONFORMANT_CODE_PES_HEADER_MALFORMED:        return "PES_HEADER_MALFORMED";
        case TST_NONCONFORMANT_CODE_SUBTITLE_ALIGNMENT_MISSING:  return "SUBTITLE_ALIGNMENT_MISSING";
        case TST_NONCONFORMANT_CODE_PCR_MALFORMED:               return "PCR_MALFORMED";
        case TST_NONCONFORMANT_CODE_NAL_HEADER:                  return "NAL_HEADER";
        case TST_NONCONFORMANT_CODE_AV1_OBU_HEADER:              return "AV1_OBU_HEADER";
        case TST_NONCONFORMANT_CODE_AC3_SYNC_MISSING:            return "AC3_SYNC_MISSING";
        case TST_NONCONFORMANT_CODE_LATM_FRAMING:                return "LATM_FRAMING";
        case TST_NONCONFORMANT_CODE_AV1_WRONG_STREAM_ID:         return "AV1_WRONG_STREAM_ID";
        case TST_NONCONFORMANT_CODE_AV1_MISSING_TS_OBU_FRAMING:  return "AV1_MISSING_TS_OBU_FRAMING";
        case TST_NONCONFORMANT_CODE_CFI_TOLERATED:               return "CFI_TOLERATED";
        default:                                                 return NULL;
    }
}

/* ── Demux scenario runner ───────────────────────────────────────────────────
 *
 * Two-pass approach (same as Rust/Python):
 *   Pass 1: Feed all bytes, flush, drain ALL events into a temporary
 *           buffer, building the pid→stream_type map from ProgramMap events.
 *   Pass 2: Walk the buffered events and emit CoreEvents for media events.
 *
 * Why two passes: a ProgramMap event may appear after a Sample event when
 * scanning the ring buffer order; the Rust normaliser has the same
 * observation and drains all events before the final mapping pass.
 */

/* Temporary buffer for raw events so we can do two passes. */
#define MAX_RAW_EVENTS 512

typedef struct {
    tst_event_t event;

    /* We must copy payload data because it's borrowed and only valid
     * until the next tst_demuxer_next_event / tst_demuxer_close call.
     * For this adapter we need:
     *   - Sample: NAL payloads (for sha256 computation)
     *   - Metadata: KLV payload (for UL prefix detection)
     *   - ProgramMap: stream_info entries (for pid→stream_type map)
     *
     * We avoid heap allocation inside the event buffer by storing
     * pre-computed data here: for Samples we store the sha256 of the
     * concatenated NAL/OBU payloads; for Metadata we store the first
     * 32 bytes of payload (enough for the 16-byte UL + BER length);
     * for ProgramMap we store the stream_count entries inline (up to
     * MAX_STREAMS_PER_PMT per event).
     */

    /* Sample: pre-computed sha256 of concatenated NAL/OBU payloads. */
    char sample_payload_sha256[65];

    /* Metadata: copy of first 32 bytes of KLV payload (UL is 16 B). */
    uint8_t metadata_payload_prefix[32];
    size_t  metadata_payload_prefix_len;

    /* ProgramMap: inline copy of stream_info entries. */
#define MAX_STREAMS_PER_PMT 32
    tst_stream_info_t pmt_streams[MAX_STREAMS_PER_PMT];
    size_t pmt_stream_count;
} raw_event_t;

/* Compute the sha256 of a SAMPLE event's payload (video OR audio).
 *
 * MUST be called in Pass 1, while the borrowed arena pointers
 * (nals[i].payload / obus[i].payload / sample.payload) are still valid —
 * i.e. before the next tst_demuxer_next_event or tst_demuxer_close call.
 * Pass 2 reads the precomputed digest; it must NEVER touch the arena
 * pointers after the demuxer is closed (arena-lifetime contract).
 *
 * Video — WHY concatenation: the Rust normaliser's video_payload_bytes()
 * concatenates all NAL unit payloads (RBSP bytes, Annex-B start codes already
 * stripped) and sha256s the result. The C ABI exposes the same RBSP bytes on
 * nals[i].payload. For AV1 it uses obus[i].payload. We mirror that exactly.
 *
 * Audio — the Rust normaliser hashes the raw `frames` bytes from
 * SamplePayload::Audio; the C ABI surfaces the same raw frame bytes on
 * sample.payload/payload_len (post-PES stripping).
 */
static void compute_sample_sha256(const tst_event_t *ev, char out_hex[65]) {
    sha256_ctx_t ctx;
    sha256_init(&ctx);
    if (ev->u.sample.stream_kind == TST_STREAM_KIND_VIDEO) {
        if (ev->u.sample.nal_count > 0) {
            /* H.264 / H.265 / H.266 — hash RBSP payload bytes from each NAL.
             * The NAL header byte(s) are NOT included — `tst_nal_t.payload`
             * is the raw RBSP content after start-code and header stripping,
             * same as what the Rust normaliser reads from NalUnit { payload }. */
            for (size_t i = 0; i < ev->u.sample.nal_count; i++) {
                sha256_update(&ctx,
                    ev->u.sample.nals[i].payload,
                    ev->u.sample.nals[i].payload_len);
            }
        } else if (ev->u.sample.obu_count > 0) {
            /* AV1 — hash OBU payloads (header, extension, LEB128 size stripped). */
            for (size_t i = 0; i < ev->u.sample.obu_count; i++) {
                sha256_update(&ctx,
                    ev->u.sample.obus[i].payload,
                    ev->u.sample.obus[i].payload_len);
            }
        } else {
            /* Fallback: raw sample payload bytes (should not be reached for
             * well-formed H.264/H.265/H.266/AV1 streams). */
            sha256_update(&ctx, ev->u.sample.payload, ev->u.sample.payload_len);
        }
    } else if (ev->u.sample.stream_kind == TST_STREAM_KIND_AUDIO) {
        /* Audio — hash the raw frame bytes (same bytes the Rust normaliser
         * hashes from SamplePayload::Audio's `frames`). */
        sha256_update(&ctx, ev->u.sample.payload, ev->u.sample.payload_len);
    }
    uint8_t digest[32];
    sha256_final(&ctx, digest);
    for (int k = 0; k < 32; k++) {
        sprintf(out_hex + k * 2, "%02x", (unsigned)digest[k]);
    }
    out_hex[64] = '\0';
}

/* Run a demux scenario: feed input bytes, collect events, normalise. */
static int run_demux(const char *scenarios_dir_path,
                     const scenario_entry_t *entry,
                     core_event_list_t *out_events) {
    /* Read the input artifact. */
    char *input_path = path_join(scenarios_dir_path, entry->input);
    if (!input_path) return -1;

    size_t input_len = 0;
    uint8_t *input_data = read_file(input_path, &input_len);
    free(input_path);
    if (!input_data) return -1;

    /* Open a default-config offline demuxer. */
    struct TstDemuxer *demuxer = tst_demuxer_open();
    if (!demuxer) {
        fprintf(stderr, "ERROR: tst_demuxer_open() failed: %s\n",
                tst_get_last_error_str());
        free(input_data);
        return -1;
    }

    /* Feed all input bytes in a single call.
     * Returns negative (TST_E_* code) on error (e.g. Unrecoverable sync loss). */
    int feed_rc = tst_demuxer_feed(demuxer, input_data, input_len);
    free(input_data);

    if (feed_rc < 0) {
        /* Feed error: map to STRICT_REJECTION umbrella code.
         * This mirrors the Rust/Python adapters which catch DemuxError from
         * feed() and return [{"event":"error","code":"STRICT_REJECTION"}]. */
        tst_demuxer_close(demuxer);
        core_event_t err_ev;
        memset(&err_ev, 0, sizeof(err_ev));
        err_ev.kind = CE_ERROR;
        strncpy(err_ev.code, "STRICT_REJECTION", sizeof(err_ev.code) - 1);
        core_event_list_push(out_events, &err_ev);
        return 0; /* Error correctly mapped — not a program failure */
    }

    /* Flush to materialise any PES boundary event at end-of-input. */
    tst_demuxer_flush(demuxer);

    /* ─── Pass 1: drain all events into a temporary buffer ─────────────────
     *
     * We must copy payload data here because event pointers are only valid
     * until the next tst_demuxer_next_event or tst_demuxer_close call. We
     * pre-compute sha256 of NAL payloads while the data is still borrowed,
     * and copy the first 32 bytes of KLV payload for UL detection.
     * ProgramMap stream_info entries are copied inline (stream_type field).
     */
    static raw_event_t raw_events[MAX_RAW_EVENTS];
    int raw_count = 0;

    tst_event_t ev;
    memset(&ev, 0, sizeof(ev));
    while (1) {
        int rc = tst_demuxer_next_event(demuxer, &ev);
        if (rc == TST_E_NOT_AVAILABLE) break; /* drained */
        if (rc != 0) {
            fprintf(stderr, "WARNING: tst_demuxer_next_event rc=%d\n", rc);
            break;
        }
        if (raw_count >= MAX_RAW_EVENTS) {
            fprintf(stderr, "WARNING: raw event buffer full (>%d)\n", MAX_RAW_EVENTS);
            break;
        }

        raw_event_t *re = &raw_events[raw_count];
        memset(re, 0, sizeof(*re));
        re->event = ev; /* struct copy — copies scalars; pointers borrow the arena */

        /* Per-kind payload capture while pointers are still valid. */
        switch (ev.kind) {
            case TST_EVENT_KIND_SAMPLE:
                /* Compute sha256 of the sample payload (video NAL/OBU bytes or
                 * audio frame bytes) NOW, before the next next_event call — and
                 * crucially before tst_demuxer_close — invalidates the borrowed
                 * arena pointers. Pass 2 reads only this precomputed digest. */
                if (ev.u.sample.stream_kind == TST_STREAM_KIND_VIDEO
                    || ev.u.sample.stream_kind == TST_STREAM_KIND_AUDIO) {
                    compute_sample_sha256(&ev, re->sample_payload_sha256);
                }
                break;

            case TST_EVENT_KIND_METADATA:
                /* Copy the first 32 bytes of KLV payload for UL prefix
                 * detection. 16-byte UL + 1-byte BER length = 17 bytes minimum;
                 * 32 bytes is ample headroom. */
                {
                    size_t copy_len = ev.u.metadata.payload_len;
                    if (copy_len > sizeof(re->metadata_payload_prefix)) {
                        copy_len = sizeof(re->metadata_payload_prefix);
                    }
                    if (ev.u.metadata.payload && copy_len > 0) {
                        memcpy(re->metadata_payload_prefix,
                               ev.u.metadata.payload, copy_len);
                    }
                    re->metadata_payload_prefix_len = copy_len;
                }
                break;

            case TST_EVENT_KIND_PROGRAM_MAP:
                /* Copy stream_info entries for pid→stream_type mapping. */
                {
                    size_t sc = ev.u.program_map.stream_count;
                    if (sc > MAX_STREAMS_PER_PMT) sc = MAX_STREAMS_PER_PMT;
                    if (ev.u.program_map.streams && sc > 0) {
                        memcpy(re->pmt_streams, ev.u.program_map.streams,
                               sc * sizeof(tst_stream_info_t));
                    }
                    re->pmt_stream_count = sc;
                }
                break;

            default:
                break;
        }

        raw_count++;
    }

    tst_demuxer_close(demuxer);

    /* ─── Pass 1b: build pid → stream_type map from ProgramMap events ──── */
    pid_st_map_t st_map;
    memset(&st_map, 0, sizeof(st_map));
    for (int i = 0; i < raw_count; i++) {
        const raw_event_t *re = &raw_events[i];
        if (re->event.kind != TST_EVENT_KIND_PROGRAM_MAP) continue;
        for (size_t j = 0; j < re->pmt_stream_count; j++) {
            pid_st_map_insert(&st_map,
                re->pmt_streams[j].pid,
                re->pmt_streams[j].stream_type);
        }
    }

    /* ─── Pass 2: emit CoreEvents for media events ─────────────────────── */
    for (int i = 0; i < raw_count; i++) {
        const raw_event_t *re = &raw_events[i];
        const tst_event_t *e = &re->event;

        switch (e->kind) {
            case TST_EVENT_KIND_PROGRAM_MAP:
                /* Skip — topology event, not in the media golden. */
                break;

            case TST_EVENT_KIND_SAMPLE: {
                int sk = e->u.sample.stream_kind;
                core_event_t cev;
                memset(&cev, 0, sizeof(cev));
                cev.program = e->u.sample.program_number;
                cev.pid     = e->u.sample.pid;

                if (sk == TST_STREAM_KIND_VIDEO) {
                    /* Video fallback stream_type: H.264 = 0x1B, H.265 = 0x24,
                     * H.266 = 0x33, AV1 = 0x06. These match the Rust fallback
                     * table in video_codec_pmt_byte(). */
                    uint8_t fallback;
                    switch (e->u.sample.codec) {
                        case TST_VIDEO_CODEC_H264: fallback = 0x1B; break;
                        case TST_VIDEO_CODEC_H265: fallback = 0x24; break;
                        case TST_VIDEO_CODEC_H266: fallback = 0x33; break;
                        case TST_VIDEO_CODEC_AV1:  fallback = 0x06; break;
                        default:                   fallback = 0x1B; break;
                    }
                    uint8_t st = pid_st_map_lookup(&st_map, e->u.sample.pid, fallback);
                    snprintf(cev.stream_type, sizeof(cev.stream_type), "0x%02x", st);
                    cev.kind = CE_VIDEO;
                    cev.pts  = e->u.sample.pts;
                    cev.key  = e->u.sample.random_access_indicator ? 1 : 0;
                    strncpy(cev.payload_sha256, re->sample_payload_sha256,
                            sizeof(cev.payload_sha256) - 1);
                    core_event_list_push(out_events, &cev);

                } else if (sk == TST_STREAM_KIND_AUDIO) {
                    /* Audio fallback stream_type: MP2 = 0x03, AAC = 0x0F,
                     * AAC-LATM = 0x11, AC-3 = 0x81. */
                    uint8_t fallback;
                    switch (e->u.sample.codec) {
                        case TST_AUDIO_CODEC_MP2:      fallback = 0x03; break;
                        case TST_AUDIO_CODEC_AAC:      fallback = 0x0F; break;
                        case TST_AUDIO_CODEC_AAC_LATM: fallback = 0x11; break;
                        case TST_AUDIO_CODEC_AC3:      fallback = 0x81; break;
                        default:                       fallback = 0x03; break;
                    }
                    uint8_t st = pid_st_map_lookup(&st_map, e->u.sample.pid, fallback);
                    snprintf(cev.stream_type, sizeof(cev.stream_type), "0x%02x", st);
                    cev.kind = CE_AUDIO;
                    cev.pts  = e->u.sample.pts;
                    /* Read the digest precomputed in Pass 1. The arena pointers
                     * (e->u.sample.payload) are already freed by tst_demuxer_close;
                     * we must NEVER dereference them here. */
                    strncpy(cev.payload_sha256, re->sample_payload_sha256,
                            sizeof(cev.payload_sha256) - 1);
                    core_event_list_push(out_events, &cev);

                } else if (sk == TST_STREAM_KIND_SUBTITLE) {
                    /* Subtitle: project to {event:"subtitle", program, pid,
                     * stream_type, codec}. All subtitle codecs carry PMT
                     * stream_type 0x06; the binding-neutral codec tag comes
                     * from the tst_subtitle_codec enum on the sample's `codec`
                     * field (same table as Rust subtitle_codec_tag() / Python
                     * _SUBTITLE_CODEC_TAG). */
                    uint8_t st = pid_st_map_lookup(&st_map, e->u.sample.pid, 0x06);
                    snprintf(cev.stream_type, sizeof(cev.stream_type), "0x%02x", st);
                    cev.kind = CE_SUBTITLE;
                    strncpy(cev.codec, subtitle_codec_tag(e->u.sample.codec),
                            sizeof(cev.codec) - 1);
                    core_event_list_push(out_events, &cev);

                } else if (sk == TST_STREAM_KIND_UNKNOWN) {
                    /* Unknown stream: emit with just the pid. */
                    cev.kind = CE_UNKNOWN;
                    core_event_list_push(out_events, &cev);

                }
                break;
            }

            case TST_EVENT_KIND_METADATA: {
                /* KLV metadata event. stream_type fallback:
                 *   KlvSync (metadata_kind == KLV_SYNC_AU_CELL) → 0x15
                 *   KlvAsync (metadata_kind == KLV_ASYNC)        → 0x06
                 * The C ABI exposes metadata_kind on the metadata sub-struct. */
                uint8_t fallback;
                if (e->u.metadata.metadata_kind == TST_METADATA_KIND_KLV_SYNC_AU_CELL) {
                    fallback = 0x15;
                } else {
                    fallback = 0x06; /* KlvAsync + Unknown both map to 0x06 */
                }
                uint8_t st = pid_st_map_lookup(&st_map, e->u.metadata.pid, fallback);

                core_event_t cev;
                memset(&cev, 0, sizeof(cev));
                cev.kind    = CE_KLV;
                cev.program = e->u.metadata.program_number;
                cev.pid     = e->u.metadata.pid;
                snprintf(cev.stream_type, sizeof(cev.stream_type), "0x%02x", st);
                strncpy(cev.set,
                    klv_set_from_ul(re->metadata_payload_prefix,
                                    re->metadata_payload_prefix_len),
                    sizeof(cev.set) - 1);
                core_event_list_push(out_events, &cev);
                break;
            }

            case TST_EVENT_KIND_NON_CONFORMANT: {
                /* Lenient-mode diagnostic — surfaced inline as an error event
                 * with the specific stable code, in queue order alongside media
                 * events (mirrors the Rust normaliser which maps NonConformant
                 * to CoreEvent::Error, and the Python adapter). The conformant
                 * Muxer emits zero NonConformant events, so clean demux
                 * scenarios are unaffected. */
                const char *code = nonconformant_code_str(e->u.nonconformant.issue_code);
                if (!code) {
                    fprintf(stderr,
                        "ERROR: unrecognised NonConformant issue_code %d; this C "
                        "adapter must be updated to map it to a stable code.\n",
                        e->u.nonconformant.issue_code);
                    return -1;
                }
                core_event_t cev;
                memset(&cev, 0, sizeof(cev));
                cev.kind = CE_ERROR;
                strncpy(cev.code, code, sizeof(cev.code) - 1);
                core_event_list_push(out_events, &cev);
                break;
            }

            case TST_EVENT_KIND_DISCONTINUITY:
            case TST_EVENT_KIND_RECONNECT_DISCONTINUITY:
                /* Diagnostics — skip from the media golden (same as Rust/Python). */
                break;

            default:
                fprintf(stderr, "WARNING: unknown event kind %d\n", e->kind);
                break;
        }
    }

    return 0;
}

/* Synthetic H.264 IDR AU — MUST match tst-integration's synthetic_h264_idr()
 * (crates/tst-integration/src/scenarios/mod.rs):
 * 4-byte Annex-B start code + IDR NAL header (0x65) + 15 bytes 0xA5 ^ i. */
static size_t synth_h264_idr(uint8_t out[20]) {
    out[0] = 0x00; out[1] = 0x00; out[2] = 0x00; out[3] = 0x01;
    out[4] = 0x65;
    for (uint8_t i = 0; i < 15; i++) out[5 + i] = (uint8_t)(0xA5 ^ i);
    return 20;
}

/* Synthetic ADTS frame — MUST match tst-integration's synthetic_adts_frame()
 * (crates/tst-integration/src/scenarios/mod.rs):
 * 7-byte ADTS header (MPEG-2 ID, no CRC, AAC-LC, sample_rate_index=4=44100 Hz,
 * channel_config=2 stereo, frame_length=15) + 8 deterministic payload bytes.
 *
 * `frame_length` is a 13-bit field split across header bytes 3/4/5 (bits
 * 12..11 in byte 3, bits 10..3 in byte 4, bits 2..0 in byte 5) — see the
 * per-byte layout comments below. */
static size_t synth_adts_frame(uint8_t out[15]) {
    const uint32_t total_len = 15;       /* aac_frame_length (13-bit) = full frame size */
    const uint8_t sample_rate_index = 4; /* 44100 Hz */
    const uint8_t channel_config = 2;    /* stereo */
    /* byte0: syncword bits 15..8 (all 1). */
    out[0] = 0xFF;
    /* byte1: syncword bits 7..4 | ID(1)=MPEG-2 | layer(2)=00 | protection_absent(1)=1. */
    out[1] = 0xF1; /* 0b1111_0001 */
    /* byte2: profile(2)=01 AAC-LC | sampling_freq_index(4) | private(1)=0 |
     *        channel_config bit 2. */
    out[2] = (uint8_t)((1 << 6) | ((sample_rate_index & 0xF) << 2)
                       | ((channel_config >> 2) & 1));
    /* byte3: channel_config bits 1..0 | orig/copy(1)=0 | home(1)=0 |
     *        copyright_id(1)=0 | copyright_start(1)=0 | frame_length bits 12..11. */
    out[3] = (uint8_t)(((channel_config & 0x3) << 6) | ((total_len >> 11) & 0x3));
    /* byte4: frame_length bits 10..3. */
    out[4] = (uint8_t)((total_len >> 3) & 0xFF);
    /* byte5: frame_length bits 2..0 | buffer_fullness bits 10..6 (all 1 = VBR). */
    out[5] = (uint8_t)(((total_len & 0x7) << 5) | 0x1F);
    /* byte6: buffer_fullness bits 5..0 (all 1) | num_raw_data_blocks(2)=0. */
    out[6] = (uint8_t)(0x3F << 2);
    static const uint8_t body[8] = {0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7};
    memcpy(out + 7, body, 8);
    return 15;
}

/* Minimal ST 0601 LS — MUST match tst-integration's minimal_st0601_ls()
 * (crates/tst-integration/src/scenarios/mod.rs):
 * 16-byte MISB ST 0601 UAS Datalink LS UL + BER short-form length 0. */
static size_t synth_minimal_st0601_ls(uint8_t out[17]) {
    static const uint8_t ls[17] = {
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01,
        0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00, /* UL bytes 1-16 */
        0x00,                                            /* BER length = 0 */
    };
    memcpy(out, ls, sizeof(ls));
    return sizeof(ls);
}

/* Drain all buffered TS packets from `mux` with a 1316-byte (7×188) pull loop,
 * matching the Rust/Python drain_mux. Appends to *produced (realloc-grown).
 * Returns 0 on success, -1 on OOM. On the -1 path *produced may hold the
 * partial allocation (it is NOT freed or nulled here) — the caller is still
 * responsible for free()ing *produced. */
static int drain_muxer(struct tst_muxer_t *mux,
                       uint8_t **produced, size_t *produced_len, size_t *produced_cap) {
    uint8_t pull_buf[1316];
    for (;;) {
        size_t n = tst_muxer_pull(mux, pull_buf, sizeof(pull_buf));
        if (n == 0) break;
        if (*produced_len + n > *produced_cap) {
            size_t new_cap = (*produced_cap == 0) ? 65536 : *produced_cap * 2;
            while (new_cap < *produced_len + n) new_cap *= 2;
            uint8_t *grown = realloc(*produced, new_cap);
            if (!grown) return -1;
            *produced = grown; *produced_cap = new_cap;
        }
        memcpy(*produced + *produced_len, pull_buf, n);
        *produced_len += n;
    }
    return 0;
}

/* Re-mux the `video-roundtrip` recipe in C (single H.264 IDR at pts=0).
 * Returns malloc'd bytes via *out / *out_len, or NULL on failure. */
static uint8_t *remux_video_roundtrip(size_t *out_len) {
    struct tst_mux_config_t *cfg = tst_mux_config_new();
    if (!cfg) { fprintf(stderr, "ERROR: tst_mux_config_new failed\n"); return NULL; }
    tst_program_handle_t prog = tst_mux_config_add_program(cfg, 1, 0x1000);
    if (prog == TST_INVALID_PROGRAM_HANDLE) {
        fprintf(stderr, "ERROR: add_program failed\n"); tst_mux_config_free(cfg); return NULL;
    }
    tst_video_stream_handle_t vstream =
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TST_VIDEO_CODEC_H264);
    if (vstream == TST_INVALID_STREAM_HANDLE) {
        fprintf(stderr, "ERROR: add_video_stream failed\n"); tst_mux_config_free(cfg); return NULL;
    }
    struct tst_muxer_t *mux = tst_muxer_open(cfg);
    tst_mux_config_free(cfg);
    if (!mux) { fprintf(stderr, "ERROR: tst_muxer_open failed\n"); return NULL; }

    uint8_t idr[20];
    size_t idr_len = synth_h264_idr(idr);
    if (tst_muxer_push_video(mux, idr, idr_len, /*pts=*/0, /*key_frame=*/true) != 0) {
        fprintf(stderr, "ERROR: tst_muxer_push_video failed\n"); tst_muxer_close(mux); return NULL;
    }

    uint8_t *produced = NULL; size_t produced_len = 0, produced_cap = 0;
    if (drain_muxer(mux, &produced, &produced_len, &produced_cap) != 0) {
        fprintf(stderr, "ERROR: OOM draining mux\n"); free(produced); tst_muxer_close(mux); return NULL;
    }
    tst_muxer_close(mux);
    *out_len = produced_len;
    return produced;
}

/* Re-mux the `audio-klv-roundtrip` recipe in C — H.264 video + AAC audio +
 * SYNCHRONOUS KLV, all at pts=0. Mirrors audio_klv_roundtrip_ts_bytes() in
 * tst-integration byte-for-byte: program_number=1, pmt_pid=0x1000; video
 * pid=0x1011 H264; audio pid=0x1021 AAC; KLV pid=0x1031 SYNCHRONOUS_METADATA
 * (carries_pts=true). Callers pass RAW ST 0601 LS bytes — the muxer auto-wraps
 * the 5-byte AU cell header. Returns malloc'd bytes, or NULL on failure. */
static uint8_t *remux_audio_klv_roundtrip(size_t *out_len) {
    struct tst_mux_config_t *cfg = tst_mux_config_new();
    if (!cfg) { fprintf(stderr, "ERROR: tst_mux_config_new failed\n"); return NULL; }
    tst_program_handle_t prog = tst_mux_config_add_program(cfg, 1, 0x1000);
    if (prog == TST_INVALID_PROGRAM_HANDLE) {
        fprintf(stderr, "ERROR: add_program failed\n"); tst_mux_config_free(cfg); return NULL;
    }
    tst_video_stream_handle_t vstream =
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TST_VIDEO_CODEC_H264);
    if (vstream == TST_INVALID_STREAM_HANDLE) {
        fprintf(stderr, "ERROR: add_video_stream failed\n"); tst_mux_config_free(cfg); return NULL;
    }
    tst_audio_stream_handle_t astream =
        tst_mux_config_add_audio_stream(cfg, prog, 0x1021, TST_AUDIO_CODEC_AAC);
    if (astream == TST_INVALID_STREAM_HANDLE) {
        fprintf(stderr, "ERROR: add_audio_stream failed\n"); tst_mux_config_free(cfg); return NULL;
    }
    /* SynchronousMetadata requires carries_pts = true. */
    tst_klv_stream_handle_t kstream = tst_mux_config_add_klv_stream(
        cfg, prog, 0x1031, TST_KLV_STREAM_TYPE_SYNCHRONOUS_METADATA, /*carries_pts=*/true);
    if (kstream == TST_INVALID_STREAM_HANDLE) {
        fprintf(stderr, "ERROR: add_klv_stream failed\n"); tst_mux_config_free(cfg); return NULL;
    }
    struct tst_muxer_t *mux = tst_muxer_open(cfg);
    tst_mux_config_free(cfg);
    if (!mux) { fprintf(stderr, "ERROR: tst_muxer_open failed\n"); return NULL; }

    /* PTS 0 throughout — the committed output_sha256 is locked to this output. */
    uint8_t idr[20];   size_t idr_len = synth_h264_idr(idr);
    uint8_t adts[15];  size_t adts_len = synth_adts_frame(adts);
    uint8_t ls[17];    size_t ls_len = synth_minimal_st0601_ls(ls);

    if (tst_muxer_push_video(mux, idr, idr_len, /*pts=*/0, /*key_frame=*/true) != 0) {
        fprintf(stderr, "ERROR: push_video failed\n"); tst_muxer_close(mux); return NULL;
    }
    if (tst_muxer_push_audio(mux, adts, adts_len, /*pts=*/0) != 0) {
        fprintf(stderr, "ERROR: push_audio failed\n"); tst_muxer_close(mux); return NULL;
    }
    /* Pass raw KLV LS bytes — muxer auto-wraps in the AU cell header. */
    if (tst_muxer_push_klv(mux, ls, ls_len, /*pts=*/0) != 0) {
        fprintf(stderr, "ERROR: push_klv failed\n"); tst_muxer_close(mux); return NULL;
    }

    uint8_t *produced = NULL; size_t produced_len = 0, produced_cap = 0;
    if (drain_muxer(mux, &produced, &produced_len, &produced_cap) != 0) {
        fprintf(stderr, "ERROR: OOM draining mux\n"); free(produced); tst_muxer_close(mux); return NULL;
    }
    tst_muxer_close(mux);
    *out_len = produced_len;
    return produced;
}

/* Re-mux the `video-dts-roundtrip` recipe in C — single H.264 IDR with
 * distinct PTS (9000) and DTS (6000) via tst_muxer_push_video_to_with_dts.
 * Mirrors video_dts_roundtrip_ts_bytes() in tst-integration byte-for-byte:
 * program_number=1, pmt_pid=0x1000, video pid=0x1011 H264.
 * The video-stream handle is the value returned by tst_mux_config_add_video_stream
 * (stable across the config→open boundary per ABI contract).
 * Returns malloc'd bytes via *out_len, or NULL on failure. */
static uint8_t *remux_video_dts_roundtrip(size_t *out_len) {
    struct tst_mux_config_t *cfg = tst_mux_config_new();
    if (!cfg) { fprintf(stderr, "ERROR: tst_mux_config_new failed\n"); return NULL; }
    tst_program_handle_t prog = tst_mux_config_add_program(cfg, 1, 0x1000);
    if (prog == TST_INVALID_PROGRAM_HANDLE) {
        fprintf(stderr, "ERROR: add_program failed\n"); tst_mux_config_free(cfg); return NULL;
    }
    /* The returned vstream handle is stable across config→open (ABI §video-handles). */
    tst_video_stream_handle_t vstream =
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TST_VIDEO_CODEC_H264);
    if (vstream == TST_INVALID_STREAM_HANDLE) {
        fprintf(stderr, "ERROR: add_video_stream failed\n"); tst_mux_config_free(cfg); return NULL;
    }
    struct tst_muxer_t *mux = tst_muxer_open(cfg);
    tst_mux_config_free(cfg);
    if (!mux) { fprintf(stderr, "ERROR: tst_muxer_open failed\n"); return NULL; }

    uint8_t idr[20];
    size_t idr_len = synth_h264_idr(idr);
    /* PTS=9000, DTS=6000 ticks (90 kHz) — fixed so the golden is stable. */
    if (tst_muxer_push_video_to_with_dts(mux, vstream, idr, idr_len,
                                         /*pts=*/9000, /*dts=*/6000,
                                         /*key_frame=*/true) != 0) {
        fprintf(stderr, "ERROR: tst_muxer_push_video_to_with_dts failed\n");
        tst_muxer_close(mux); return NULL;
    }

    uint8_t *produced = NULL; size_t produced_len = 0, produced_cap = 0;
    if (drain_muxer(mux, &produced, &produced_len, &produced_cap) != 0) {
        fprintf(stderr, "ERROR: OOM draining mux\n"); free(produced); tst_muxer_close(mux); return NULL;
    }
    tst_muxer_close(mux);
    *out_len = produced_len;
    return produced;
}

/* Re-mux the `video-misp-roundtrip` recipe in C — single H.264 IDR with a
 * spliced ST 0604 MISP microsecond timestamp via tst_muxer_push_video_misp_to.
 * Mirrors video_misp_roundtrip_ts_bytes() in tst-integration byte-for-byte:
 * program_number=1, pmt_pid=0x1000, video pid=0x1011 H264.
 * MispTimestamp: kind=0 (Micro), time_status=0x1F, value=0x0005F5E100000001.
 * PTS=9000 ticks (90 kHz) — fixed so the golden is stable.
 * Returns malloc'd bytes via *out_len, or NULL on failure. */
static uint8_t *remux_video_misp_roundtrip(size_t *out_len) {
    struct tst_mux_config_t *cfg = tst_mux_config_new();
    if (!cfg) { fprintf(stderr, "ERROR: tst_mux_config_new failed\n"); return NULL; }
    tst_program_handle_t prog = tst_mux_config_add_program(cfg, 1, 0x1000);
    if (prog == TST_INVALID_PROGRAM_HANDLE) {
        fprintf(stderr, "ERROR: add_program failed\n"); tst_mux_config_free(cfg); return NULL;
    }
    tst_video_stream_handle_t vstream =
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TST_VIDEO_CODEC_H264);
    if (vstream == TST_INVALID_STREAM_HANDLE) {
        fprintf(stderr, "ERROR: add_video_stream failed\n"); tst_mux_config_free(cfg); return NULL;
    }
    struct tst_muxer_t *mux = tst_muxer_open(cfg);
    tst_mux_config_free(cfg);
    if (!mux) { fprintf(stderr, "ERROR: tst_muxer_open failed\n"); return NULL; }

    uint8_t idr[20];
    size_t idr_len = synth_h264_idr(idr);
    /* MispTimestamp::micros(0x0005_F5E1_0000_0001, 0x1F) — kind=0, status=0x1F */
    if (tst_muxer_push_video_misp_to(mux, vstream, idr, idr_len,
                                     /*pts=*/9000,
                                     /*key_frame=*/true,
                                     /*misp_kind=*/0,
                                     /*time_status=*/0x1F,
                                     /*value=*/UINT64_C(0x0005F5E100000001)) != 0) {
        fprintf(stderr, "ERROR: tst_muxer_push_video_misp_to failed\n");
        tst_muxer_close(mux); return NULL;
    }

    uint8_t *produced = NULL; size_t produced_len = 0, produced_cap = 0;
    if (drain_muxer(mux, &produced, &produced_len, &produced_cap) != 0) {
        fprintf(stderr, "ERROR: OOM draining mux\n"); free(produced); tst_muxer_close(mux); return NULL;
    }
    tst_muxer_close(mux);
    *out_len = produced_len;
    return produced;
}

/* ── Roundtrip scenario runner ── */
static int run_roundtrip(const char *scenarios_dir_path,
                         const scenario_entry_t *entry,
                         const char *golden_json,
                         core_event_list_t *out_events) {
    (void)out_events; /* roundtrip carries no media events — core: [] */

    /* Extract expected sha256 from golden.extensions.output_sha256. */
    char expected_sha256[65];
    size_t ext_off = json_extract_string(golden_json, 0,
        "output_sha256", expected_sha256, sizeof(expected_sha256));
    if (ext_off == 0 || expected_sha256[0] == '\0') {
        fprintf(stderr, "ERROR: cannot extract extensions.output_sha256 from golden\n");
        return -1;
    }

    /* Read the committed artifact (entry->input IS output.ts for roundtrip). */
    char *artifact_path = path_join(scenarios_dir_path, entry->input);
    if (!artifact_path) return -1;
    size_t committed_len = 0;
    uint8_t *committed = read_file(artifact_path, &committed_len);
    free(artifact_path);
    if (!committed) return -1;

    /* ── Re-mux the recipe in C and compare to the committed bytes. ──────────
     *
     * Dispatch on the scenario id so each roundtrip re-runs its own
     * single-source-of-truth mux recipe (matching the Rust run_roundtrip and
     * Python _run_roundtrip dispatch). The offline tst_muxer_* surface is
     * un-gated at ABI minor 9, so this path needs no TST_HAS_SRT guard. */
    size_t produced_len = 0;
    uint8_t *produced = NULL;
    if (strcmp(entry->id, "video-roundtrip") == 0) {
        produced = remux_video_roundtrip(&produced_len);
    } else if (strcmp(entry->id, "audio-klv-roundtrip") == 0) {
        produced = remux_audio_klv_roundtrip(&produced_len);
    } else if (strcmp(entry->id, "video-dts-roundtrip") == 0) {
        produced = remux_video_dts_roundtrip(&produced_len);
    } else if (strcmp(entry->id, "video-misp-roundtrip") == 0) {
        produced = remux_video_misp_roundtrip(&produced_len);
    } else {
        fprintf(stderr, "ERROR: unknown roundtrip scenario id '%s'\n", entry->id);
        free(committed);
        return -1;
    }
    if (!produced) { free(committed); return -1; }

    /* Byte-for-byte parity with the committed artifact. A mismatch here is a
     * real cross-binding finding — do NOT loosen to a hash-only check. */
    if (produced_len != committed_len || memcmp(produced, committed, produced_len) != 0) {
        fprintf(stderr,
            "FAIL [%s]: C-produced bytes differ from committed output.ts "
            "(produced %zu bytes, committed %zu bytes)\n",
            entry->id, produced_len, committed_len);
        free(produced); free(committed); return -1;
    }

    /* sha256 parity with the golden. */
    char digest_hex[65];
    sha256_hex(produced, produced_len, digest_hex);
    free(committed);
    if (strcmp(digest_hex, expected_sha256) != 0) {
        fprintf(stderr, "FAIL [%s]: sha256 mismatch\n  computed : %s\n  expected : %s\n",
            entry->id, digest_hex, expected_sha256);
        free(produced); return -1;
    }

    /* video-misp-roundtrip: demux the produced TS, extract the MISP timestamp
     * from the first video AU, and assert it matches the golden's committed
     * misp_kind/misp_time_status/misp_value fields. */
    if (strcmp(entry->id, "video-misp-roundtrip") == 0) {
        /* Read expected MISP fields from golden extensions. */
        int64_t golden_kind_i64 = 0, golden_status_i64 = 0, golden_value_i64 = 0;
        if (!json_extract_int64(golden_json, 0, "misp_kind", &golden_kind_i64)
            || !json_extract_int64(golden_json, 0, "misp_time_status", &golden_status_i64)
            || !json_extract_int64(golden_json, 0, "misp_value", &golden_value_i64)) {
            fprintf(stderr, "ERROR [%s]: cannot read misp fields from golden\n", entry->id);
            free(produced); return -1;
        }
        uint8_t golden_kind   = (uint8_t)golden_kind_i64;
        uint8_t golden_status = (uint8_t)golden_status_i64;
        uint64_t golden_value = (uint64_t)golden_value_i64;

        /* Demux the produced TS to get the video AU. */
        struct TstDemuxer *demuxer = tst_demuxer_open();
        if (!demuxer) {
            fprintf(stderr, "ERROR [%s]: tst_demuxer_open failed\n", entry->id);
            free(produced); return -1;
        }
        int feed_rc = tst_demuxer_feed(demuxer, produced, produced_len);
        if (feed_rc < 0) {
            fprintf(stderr, "ERROR [%s]: tst_demuxer_feed failed rc=%d\n", entry->id, feed_rc);
            tst_demuxer_close(demuxer); free(produced); return -1;
        }
        tst_demuxer_flush(demuxer);

        int misp_found = 0;
        tst_event_t ev;
        memset(&ev, 0, sizeof(ev));
        while (1) {
            int rc = tst_demuxer_next_event(demuxer, &ev);
            if (rc == TST_E_NOT_AVAILABLE) break;
            if (rc != 0) { break; }
            if (ev.kind == TST_EVENT_KIND_SAMPLE
                && ev.u.sample.stream_kind == TST_STREAM_KIND_VIDEO
                && ev.u.sample.payload && ev.u.sample.payload_len > 0) {
                /* Extract MISP timestamp while the borrowed pointer is still valid. */
                uint8_t out_kind = 0, out_status = 0;
                uint64_t out_value = 0;
                int misp_rc = tst_misp_time_extract(ev.u.sample.payload,
                                                    ev.u.sample.payload_len,
                                                    TST_VIDEO_CODEC_H264,
                                                    &out_kind, &out_status, &out_value);
                if (misp_rc != 0) {
                    fprintf(stderr, "FAIL [%s]: tst_misp_time_extract rc=%d — MISP SEI not found\n",
                        entry->id, misp_rc);
                    tst_demuxer_close(demuxer); free(produced); return -1;
                }
                if (out_kind != golden_kind || out_status != golden_status
                    || out_value != golden_value) {
                    fprintf(stderr,
                        "FAIL [%s]: MISP extract mismatch — "
                        "got kind=%u status=%u value=%llu, "
                        "expected kind=%u status=%u value=%llu\n",
                        entry->id,
                        (unsigned)out_kind, (unsigned)out_status,
                        (unsigned long long)out_value,
                        (unsigned)golden_kind, (unsigned)golden_status,
                        (unsigned long long)golden_value);
                    tst_demuxer_close(demuxer); free(produced); return -1;
                }
                misp_found = 1;
                break;
            }
        }
        tst_demuxer_close(demuxer);
        if (!misp_found) {
            fprintf(stderr, "FAIL [%s]: no video AU found after demux\n", entry->id);
            free(produced); return -1;
        }
    }

    free(produced);
    return 0;
}

/* ── Binding contract (strict-rejection) runner ──────────────────────────────
 *
 * Feed 8192 × 0xFF through tst_demuxer_feed with a default-config demuxer.
 * The all-0xFF input has no 0x47 MPEG-TS sync byte. After scanning
 * SYNC_SEARCH_WINDOW (188 × 32 = 6016 bytes) without a sync byte, the demuxer
 * returns a negative (Unrecoverable) error code. Any negative return from feed
 * maps to the umbrella public code "STRICT_REJECTION" — the same mapping used
 * by the Rust and Python adapters.
 *
 * Also verifies two idempotence/safety contracts:
 *   1. tst_demuxer_close(NULL) is safe (null-pointer guard documented in header).
 *   2. tst_demuxer_close on an error-state demuxer does not crash.
 *
 * The remaining binding contracts are dispatched on the scenario id:
 *
 *   malformed-psi-strict / exception-kind-stability
 *     Open a StrictMode::Full offline demuxer (tst_demux_config_set_strict_mode
 *     + tst_demuxer_open_with_config) and feed the PAT(valid)+PMT(bad-CRC)
 *     TS. Under Full strict mode the PsiChecksumMismatch escalates to a
 *     DemuxError that tst_demuxer_feed surfaces as a negative code → mapped to
 *     the umbrella "STRICT_REJECTION", identical to the Rust/Python adapters.
 *
 *   drop-idempotence  (THE REAL C DOUBLE-FREE TEETH)
 *     Open a demuxer, feed the minimal valid TS, flush twice, then close once,
 *     null the caller's pointer, and call tst_demuxer_close(NULL) again. The
 *     second close exercises the ABI's documented null-pointer guard — the
 *     idiomatic double-close-safe pattern (no double-free, no crash). A literal
 *     same-pointer double-close is undefined behavior and is deliberately NOT
 *     performed (see the per-function header for the full ABI-contract note).
 *     Emits the DOUBLE_CLOSE_OK sentinel.
 *
 *   forged-handle  (THE REAL C HANDLE-VALIDATION TEETH)
 *     Build a valid single-video muxer (one stream → handle 0), then call
 *     tst_muxer_push_video_to() with the FORGED handle value read from
 *     input.bin (0x100, one bit past the canonical 0xFF mask). The ABI must
 *     REJECT it with TST_E_INVALID_USAGE (carrying MuxError::InvalidStreamHandle)
 *     rather than dereferencing it. Emits the INVALID_HANDLE sentinel.
 */

/* strict-rejection: feed 8192 × 0xFF to a default (lenient) demuxer; the
 * unrecoverable sync-loss surfaces as a negative feed code → STRICT_REJECTION.
 * Also exercises the null-safe and error-state close contracts. */
static int contract_strict_rejection(const char *scenarios_dir_path,
                                     const scenario_entry_t *entry,
                                     core_event_list_t *out_events) {
    char *input_path = path_join(scenarios_dir_path, entry->input);
    if (!input_path) return -1;
    size_t input_len = 0;
    uint8_t *input_data = read_file(input_path, &input_len);
    free(input_path);
    if (!input_data) return -1;

    /* Verify null-safe close BEFORE any demuxer is open — confirming the
     * null-pointer guard works independent of the error-path test below. */
    tst_demuxer_close(NULL); /* must not crash */

    struct TstDemuxer *demuxer = tst_demuxer_open();
    if (!demuxer) {
        fprintf(stderr, "ERROR: tst_demuxer_open() failed: %s\n", tst_get_last_error_str());
        free(input_data);
        return -1;
    }

    int feed_rc = tst_demuxer_feed(demuxer, input_data, input_len);
    free(input_data);

    if (feed_rc >= 0) {
        fprintf(stderr,
            "FAIL [%s]: expected tst_demuxer_feed to return a negative error "
            "code on garbage input, got %d\n", entry->id, feed_rc);
        tst_demuxer_close(demuxer);
        return -1;
    }

    fprintf(stdout, "  [strict-rejection] tst_demuxer_feed returned %d — "
            "maps to STRICT_REJECTION\n", feed_rc);

    /* Close the error-state demuxer. Must not crash. */
    tst_demuxer_close(demuxer);

    core_event_t err_ev;
    memset(&err_ev, 0, sizeof(err_ev));
    err_ev.kind = CE_ERROR;
    strncpy(err_ev.code, "STRICT_REJECTION", sizeof(err_ev.code) - 1);
    core_event_list_push(out_events, &err_ev);
    return 0;
}

/* malformed-psi-strict / exception-kind-stability: feed PAT(valid)+PMT(bad-CRC)
 * to a StrictMode::Full demuxer; assert the rejection surfaces as the stable
 * STRICT_REJECTION umbrella code (same as the Rust/Python strict-PSI path). */
static int contract_strict_psi(const char *scenarios_dir_path,
                               const scenario_entry_t *entry,
                               core_event_list_t *out_events) {
    char *input_path = path_join(scenarios_dir_path, entry->input);
    if (!input_path) return -1;
    size_t input_len = 0;
    uint8_t *input_data = read_file(input_path, &input_len);
    free(input_path);
    if (!input_data) return -1;

    /* Build a StrictMode::Full config so PsiChecksumMismatch → StrictRejection. */
    struct tst_demux_config_t *cfg = tst_demux_config_new();
    if (!cfg) {
        fprintf(stderr, "ERROR: tst_demux_config_new() failed: %s\n", tst_get_last_error_str());
        free(input_data);
        return -1;
    }
    if (tst_demux_config_set_strict_mode(cfg, TST_STRICT_MODE_FULL) != 0) {
        fprintf(stderr, "ERROR: tst_demux_config_set_strict_mode failed\n");
        tst_demux_config_free(cfg); free(input_data); return -1;
    }
    struct TstDemuxer *demuxer = tst_demuxer_open_with_config(cfg);
    tst_demux_config_free(cfg); /* config is read at open time */
    if (!demuxer) {
        fprintf(stderr, "ERROR: tst_demuxer_open_with_config() failed: %s\n",
                tst_get_last_error_str());
        free(input_data);
        return -1;
    }

    int feed_rc = tst_demuxer_feed(demuxer, input_data, input_len);
    free(input_data);

    if (feed_rc >= 0) {
        fprintf(stderr,
            "FAIL [%s]: expected StrictMode::Full feed to reject the corrupted-PMT "
            "input with a negative code, got %d\n", entry->id, feed_rc);
        tst_demuxer_close(demuxer);
        return -1;
    }

    fprintf(stdout, "  [%s] strict-mode tst_demuxer_feed returned %d — "
            "maps to STRICT_REJECTION\n", entry->id, feed_rc);
    tst_demuxer_close(demuxer);

    core_event_t err_ev;
    memset(&err_ev, 0, sizeof(err_ev));
    err_ev.kind = CE_ERROR;
    strncpy(err_ev.code, "STRICT_REJECTION", sizeof(err_ev.code) - 1);
    core_event_list_push(out_events, &err_ev);
    return 0;
}

/* drop-idempotence — THE REAL C DOUBLE-CLOSE TEETH.
 *
 * Exercises the demuxer lifecycle's idempotence guarantees through the C ABI:
 *
 *   1. tst_demuxer_flush() called TWICE — the second flush must be a safe
 *      no-op (the header documents flush as idempotent).
 *   2. The documented "double close" idiom: tst_demuxer_close() once to
 *      consume + free the handle, then NULL the caller's pointer, then
 *      tst_demuxer_close(NULL) again — the second close hits the ABI's
 *      null-pointer guard and is a safe no-op (no double-free, no crash).
 *
 * IMPORTANT — honest ABI contract: tst_demuxer_close() consumes the underlying
 * `Box` via the raw pointer; the header AND the Rust impl explicitly document
 * that "passing the same non-null pointer twice is undefined behavior
 * (use-after-free on the consumed Box)". There is no handle-validity registry,
 * so a literal close(p); close(p) on the SAME non-null pointer is UB (it
 * deadlocks the allocator in practice — observed during Task 13). The honest,
 * ABI-sanctioned double-close-safe idiom is therefore "close then null then
 * close(NULL)" — which is exactly what a careful caller does and what this
 * contract asserts. A true double-free GUARD (validity-tracking close) would be
 * a tst-c source change, out of scope here; the null-after-close idiom is the
 * real guarantee the ABI actually offers. This mirrors how the Rust/Python
 * adapters express "double close" via flush-twice + drop rather than passing a
 * freed handle twice.
 *
 * A fresh demuxer opened afterwards still works. Emits the DOUBLE_CLOSE_OK
 * sentinel. */
static int contract_drop_idempotence(const char *scenarios_dir_path,
                                     const scenario_entry_t *entry,
                                     core_event_list_t *out_events) {
    char *input_path = path_join(scenarios_dir_path, entry->input);
    if (!input_path) return -1;
    size_t input_len = 0;
    uint8_t *input_data = read_file(input_path, &input_len);
    free(input_path);
    if (!input_data) return -1;

    struct TstDemuxer *demuxer = tst_demuxer_open();
    if (!demuxer) {
        fprintf(stderr, "ERROR: tst_demuxer_open() failed: %s\n", tst_get_last_error_str());
        free(input_data);
        return -1;
    }
    if (tst_demuxer_feed(demuxer, input_data, input_len) < 0) {
        fprintf(stderr, "FAIL [%s]: minimal valid TS should feed cleanly\n", entry->id);
        tst_demuxer_close(demuxer); free(input_data); return -1;
    }
    free(input_data);

    /* Flush twice — both must succeed: per the ABI, flush() on a valid OPEN
     * demuxer returns 0 (it only errors on a null/closed handle), and the
     * second flush is an idempotent no-op (the handle is not closed until
     * below). Surface a non-zero rc explicitly rather than letting this
     * lifecycle contract emit DOUBLE_CLOSE_OK while flush is silently failing. */
    int fr1 = tst_demuxer_flush(demuxer);
    int fr2 = tst_demuxer_flush(demuxer);
    if (fr1 < 0 || fr2 < 0) {
        fprintf(stderr, "FAIL [%s]: tst_demuxer_flush() on an open demuxer should "
                "succeed (idempotent); got rc=%d,%d\n", entry->id, fr1, fr2);
        tst_demuxer_close(demuxer); return -1;
    }

    /* THE TEETH: close once (consumes + frees the handle), null the caller's
     * pointer, then close again on the now-NULL pointer. The second close hits
     * the ABI's documented null-guard → safe no-op, no double-free. This is the
     * ABI-sanctioned double-close-safe idiom (see the function-header note). */
    tst_demuxer_close(demuxer);
    demuxer = NULL;
    tst_demuxer_close(demuxer); /* close(NULL) — guarded no-op, must be safe */

    /* A fresh instance still works after the prior was finalised + closed. */
    struct TstDemuxer *fresh = tst_demuxer_open();
    if (!fresh) {
        fprintf(stderr, "FAIL [%s]: fresh demuxer open failed after double-close\n", entry->id);
        return -1;
    }
    if (tst_demuxer_flush(fresh) < 0) {
        fprintf(stderr, "FAIL [%s]: tst_demuxer_flush() on a fresh demuxer should succeed\n", entry->id);
        tst_demuxer_close(fresh); return -1;
    }
    tst_demuxer_close(fresh);

    fprintf(stdout, "  [drop-idempotence] flush() x2 + close()/null/close(NULL) "
            "idiom — second close was a safe no-op (no double-free)\n");

    core_event_t ev;
    memset(&ev, 0, sizeof(ev));
    ev.kind = CE_ERROR;
    strncpy(ev.code, "DOUBLE_CLOSE_OK", sizeof(ev.code) - 1);
    core_event_list_push(out_events, &ev);
    return 0;
}

/* forged-handle — THE REAL C HANDLE-VALIDATION TEETH.
 *
 * Read the forged handle value (4-byte LE u32 = 0x100) from input.bin, build a
 * valid single-video muxer (one stream → real handle 0), and pass the FORGED
 * handle to tst_muxer_push_video_to(). The ABI must REJECT it with
 * TST_E_INVALID_USAGE (carrying MuxError::InvalidStreamHandle) rather than
 * dereferencing it / crashing. Emits the INVALID_HANDLE sentinel. */
static int contract_forged_handle(const char *scenarios_dir_path,
                                  const scenario_entry_t *entry,
                                  core_event_list_t *out_events) {
    char *input_path = path_join(scenarios_dir_path, entry->input);
    if (!input_path) return -1;
    size_t input_len = 0;
    uint8_t *input_data = read_file(input_path, &input_len);
    free(input_path);
    if (!input_data) return -1;

    /* The committed artifact is the forged handle value as 4 little-endian
     * bytes — keeps the cross-binding input single-sourced. */
    if (input_len != 4) {
        fprintf(stderr, "FAIL [%s]: forged-handle input must be a 4-byte LE u32, got %zu\n",
                entry->id, input_len);
        free(input_data);
        return -1;
    }
    uint32_t forged = (uint32_t)input_data[0]
                    | ((uint32_t)input_data[1] << 8)
                    | ((uint32_t)input_data[2] << 16)
                    | ((uint32_t)input_data[3] << 24);
    free(input_data);
    if (forged != 0x100u) {
        fprintf(stderr, "FAIL [%s]: forged-handle artifact value drifted: got %#x, expected 0x100\n",
                entry->id, forged);
        return -1;
    }

    /* Build a valid muxer with exactly one video stream (real handle 0). */
    struct tst_mux_config_t *cfg = tst_mux_config_new();
    if (!cfg) { fprintf(stderr, "ERROR: tst_mux_config_new failed\n"); return -1; }
    tst_program_handle_t prog = tst_mux_config_add_program(cfg, 1, 0x1000);
    if (prog == TST_INVALID_PROGRAM_HANDLE) {
        fprintf(stderr, "ERROR: add_program failed\n"); tst_mux_config_free(cfg); return -1;
    }
    tst_video_stream_handle_t vstream =
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TST_VIDEO_CODEC_H264);
    if (vstream == TST_INVALID_STREAM_HANDLE) {
        fprintf(stderr, "ERROR: add_video_stream failed\n"); tst_mux_config_free(cfg); return -1;
    }
    struct tst_muxer_t *mux = tst_muxer_open(cfg);
    tst_mux_config_free(cfg);
    if (!mux) { fprintf(stderr, "ERROR: tst_muxer_open failed\n"); return -1; }

    /* THE TEETH: pass the forged handle (0x100) to a real fan-out call. The
     * only valid handle is 0; 0x100 is out of range, so the ABI must reject it
     * with TST_E_INVALID_USAGE rather than dereferencing it. */
    uint8_t idr[20];
    size_t idr_len = synth_h264_idr(idr);
    int rc = tst_muxer_push_video_to(mux, (tst_video_stream_handle_t)forged,
                                     idr, idr_len, /*pts=*/0, /*key_frame=*/true);
    tst_muxer_close(mux);

    if (rc != TST_E_INVALID_USAGE) {
        fprintf(stderr,
            "FAIL [%s]: forged handle %#x was NOT rejected with TST_E_INVALID_USAGE "
            "(got rc=%d) — the ABI must reject a forged handle, not deref it\n",
            entry->id, forged, rc);
        return -1;
    }

    fprintf(stdout, "  [forged-handle] tst_muxer_push_video_to(forged=%#x) rejected "
            "with TST_E_INVALID_USAGE — maps to INVALID_HANDLE\n", forged);

    core_event_t ev;
    memset(&ev, 0, sizeof(ev));
    ev.kind = CE_ERROR;
    strncpy(ev.code, "INVALID_HANDLE", sizeof(ev.code) - 1);
    core_event_list_push(out_events, &ev);
    return 0;
}

/* cancelled-recv-kind - one cancelled plain SRT receive at the C ABI: the
 * observed code must be TST_E_CLOSED (-7), which is the golden's
 * extensions.c_code and maps to its core code "CLOSED".
 *
 * The golden's extensions.detail ("cancelled from another thread") is the
 * BINDING-LAYER detail the Rust/Python/JVM legs assert. The C ABI does NOT
 * carry it on this path by design: a receive that ends Closed/EndOfStream goes
 * through `record_recv_closed`, which relabels from the shared cancel flag and
 * writes its own message ("receiver was cancelled or closed by caller") so a
 * caller close and a peer EOS can be told apart. Per the C ABI error-mapping
 * contract (docs/reference/binding-authors.md) the message is a debug aid and
 * not part of the stable contract - the CODE is. So this leg asserts the code
 * and only prints the message.
 *
 * The listener open blocks in accept, so it and the parked recv run on one
 * worker thread; the idle caller and the cancel run here. Every wait is a 10 s
 * FAILURE bound; the worker is detached, never joined after a failed verdict. */
typedef struct {
    uint16_t port;
    _Atomic(tst_receiver_t *) rx;
    _Atomic int rc;
    _Atomic int done;
    char detail[256];
} cancelled_recv_state_t;

/* Ask the OS for an ephemeral UDP port, then release it — the same shape as
 * the repo's other loopback harnesses (`bindings/python/tests/_builders/ports.py`,
 * `TestSupport.freeUdpPort()` in the JVM tests), which document and accept the
 * same tiny TOCTOU window between release and rebind. Two reasons it stays
 * that way here: the C ABI cannot hand the port back (`tst_receiver_open_listener`
 * BLOCKS in accept, so there is no handle to query until a peer has already
 * connected — and the peer needs the port), and a fixed band would collide
 * with the Rust tests that own one (`tests/receiving/cancel_first.rs`, band
 * 32_000). If the window is ever lost, `open_listener` fails, the worker
 * publishes `done` with its own message, and the poll loop below reports
 * "listener open failed: ..." at once — a loud, fast failure, never a hang or
 * a silent pass. */
static uint16_t free_udp_port(void) {
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    a.sin_port = 0;
    if (fd < 0 || bind(fd, (struct sockaddr *)&a, sizeof a) != 0) {
        perror("free_udp_port");
        exit(2);
    }
    socklen_t len = sizeof a;
    if (getsockname(fd, (struct sockaddr *)&a, &len) != 0) {
        perror("getsockname");
        exit(2);
    }
    uint16_t port = ntohs(a.sin_port);
    close(fd);
    return port;
}

static void *cancelled_recv_worker(void *arg) {
    cancelled_recv_state_t *st = arg;
    char url[64];
    snprintf(url, sizeof url, "srt://:%u", (unsigned)st->port);
    /* blocks until the caller connects */
    tst_receiver_t *rx = tst_receiver_open_listener(url);
    if (!rx) {
        snprintf(st->detail, sizeof st->detail, "open_listener failed: %s",
                 tst_get_last_error_str());
        atomic_store(&st->rc, 1);
        atomic_store(&st->done, 1);
        return NULL;
    }
    atomic_store(&st->rx, rx);
    uint8_t pkt[188];
    int rc = tst_receiver_recv_packet(rx, pkt);
    /* thread-local last error: read it HERE, on the calling thread */
    snprintf(st->detail, sizeof st->detail, "%s", tst_get_last_error_str());
    atomic_store(&st->rc, rc);
    atomic_store(&st->done, 1);
    return NULL;
}

static int contract_cancelled_recv_kind(const char *scenarios_dir_path,
                                        const scenario_entry_t *entry,
                                        core_event_list_t *out_events) {
    (void)scenarios_dir_path;
    /* HEAP, not stack. The worker below is DETACHED and keeps writing into
     * this state (its `detail` buffer and three atomics) until its
     * open_listener / recv_packet returns. Several failure paths here return
     * while that worker is still parked, so the state must outlive this
     * frame: on those paths it is deliberately LEAKED - the process is about
     * to report a failed scenario and exit, and a one-off leak is strictly
     * better than the use-after-free a stack local would give. It is freed
     * only once `done` proves the worker has finished writing. */
    cancelled_recv_state_t *st = calloc(1, sizeof *st);
    if (!st) {
        perror("calloc");
        return -1;
    }
    st->port = free_udp_port();
    pthread_t th;
    if (pthread_create(&th, NULL, cancelled_recv_worker, st) != 0) {
        perror("pthread_create");
        free(st); /* no worker started - nothing can reference it */
        return -1;
    }
    pthread_detach(th);

    char caller[64];
    snprintf(caller, sizeof caller, "srt://127.0.0.1:%u?mode=caller", (unsigned)st->port);
    tst_raw_sender_t *peer = NULL;
    for (int i = 0; i < 100 && !peer; i++) { /* <= 5 s: the listener may not be bound yet */
        peer = tst_raw_sender_open(caller, NULL);
        if (!peer) {
            usleep(50 * 1000);
        }
    }
    if (!peer) {
        fprintf(stderr, "FAIL [%s]: idle peer could not connect: %s\n", entry->id,
                tst_get_last_error_str());
        /* The worker is parked in open_listener's accept with nothing to
         * accept, and the C ABI exposes no listener cancel handle, so it
         * cannot be woken from here. Leak `st` and let the failing run end.
         * (Only reachable when SRT loopback is broken outright.) */
        return -1;
    }

    /* Watch `done` as well as `rx`: an open_listener failure sets `done`
     * without ever publishing an `rx`, and polling `rx` alone would stall the
     * full 10 s and then report the wrong reason. */
    tst_receiver_t *rx = NULL;
    for (int i = 0; i < 200; i++) { /* <= 10 s */
        rx = atomic_load(&st->rx);
        if (rx || atomic_load(&st->done)) {
            break;
        }
        usleep(50 * 1000);
    }
    if (!rx) {
        if (atomic_load(&st->done)) {
            /* The worker has finished writing, so the state is ours again. */
            fprintf(stderr, "FAIL [%s]: listener open failed: %s\n", entry->id, st->detail);
            tst_raw_sender_close(peer);
            free(st);
        } else {
            fprintf(stderr, "FAIL [%s]: listener never accepted within 10 s\n", entry->id);
            tst_raw_sender_close(peer);
            /* worker still parked - leak (see the allocation comment) */
        }
        return -1;
    }
    usleep(300 * 1000); /* let recv_packet park in srt_recv */
    if (tst_receiver_cancel(rx) != 0) {
        fprintf(stderr, "FAIL [%s]: tst_receiver_cancel: %s\n", entry->id,
                tst_get_last_error_str());
        tst_raw_sender_close(peer);
        /* Close the receiver so the parked worker returns instead of sitting
         * in libsrt at process exit (where atexit(srt_cleanup) would join it). */
        tst_receiver_close(rx);
        return -1; /* worker may still be mid-write - leak */
    }
    for (int i = 0; i < 200 && !atomic_load(&st->done); i++) { /* <= 10 s */
        usleep(50 * 1000);
    }
    tst_raw_sender_close(peer);
    if (!atomic_load(&st->done)) {
        fprintf(stderr, "FAIL [%s]: cancel did not end the parked recv within 10 s\n",
                entry->id);
        tst_receiver_close(rx); /* same reason as above */
        return -1;              /* worker still parked - leak */
    }
    tst_receiver_close(rx);

    /* `done` is set: the worker is finished and the state is ours. Snapshot
     * what the verdict needs, then free. */
    int rc = atomic_load(&st->rc);
    char detail[sizeof st->detail];
    snprintf(detail, sizeof detail, "%s", st->detail);
    free(st);

    if (rc != TST_E_CLOSED) {
        fprintf(stderr, "FAIL [%s]: expected TST_E_CLOSED (%d) after cancel, got %d (%s)\n",
                entry->id, (int)TST_E_CLOSED, rc, detail);
        return -1;
    }
    fprintf(stdout,
            "  [cancelled-recv-kind] recv_packet after cancel -> TST_E_CLOSED (last error: "
            "\"%s\") - maps to CLOSED\n",
            detail);
    core_event_t ev;
    memset(&ev, 0, sizeof ev);
    ev.kind = CE_ERROR;
    strncpy(ev.code, "CLOSED", sizeof(ev.code) - 1);
    core_event_list_push(out_events, &ev);
    return 0;
}

static int run_binding_contract(const char *scenarios_dir_path,
                                const scenario_entry_t *entry,
                                core_event_list_t *out_events) {
    if (strcmp(entry->id, "strict-rejection") == 0) {
        return contract_strict_rejection(scenarios_dir_path, entry, out_events);
    }
    if (strcmp(entry->id, "malformed-psi-strict") == 0
        || strcmp(entry->id, "exception-kind-stability") == 0) {
        return contract_strict_psi(scenarios_dir_path, entry, out_events);
    }
    if (strcmp(entry->id, "drop-idempotence") == 0) {
        return contract_drop_idempotence(scenarios_dir_path, entry, out_events);
    }
    if (strcmp(entry->id, "forged-handle") == 0) {
        return contract_forged_handle(scenarios_dir_path, entry, out_events);
    }
    if (strcmp(entry->id, "cancelled-recv-kind") == 0) {
        return contract_cancelled_recv_kind(scenarios_dir_path, entry, out_events);
    }
    fprintf(stderr, "ERROR: unknown binding_contract scenario: %s\n", entry->id);
    return -1;
}

/* ── Golden comparison ───────────────────────────────────────────────────────
 *
 * For each scenario we compare observed CoreEvents against the committed
 * golden.json field-by-field. The golden's "core" array is parsed from JSON
 * and checked against the normalised C-adapter output.
 *
 * For the demux scenario, this means comparing:
 *   video: program, pid, stream_type, pts, key, payload_sha256
 *   klv:   program, pid, stream_type, set
 *   error: code
 */

/* Parse and count the golden's "core" events; fill `out_events` up to
 * `max_events` entries. Returns the count. */
static int parse_golden_core(const char *json,
                             core_event_t *out_events, int max_events) {
    int count = 0;
    /* Find "core":[  */
    const char *core_start = strstr(json, "\"core\":");
    if (!core_start) return 0;
    core_start = strchr(core_start, '[');
    if (!core_start) return 0;
    core_start++; /* skip '[' */

    const char *p = core_start;
    /* Walk each {...} object in the array. */
    while (*p && count < max_events) {
        /* Skip whitespace and commas between objects. */
        while (*p == ' ' || *p == '\n' || *p == '\r' || *p == '\t' || *p == ',') p++;
        if (*p == ']') break; /* end of array */
        if (*p != '{') { p++; continue; }

        /* Find the matching '}'. */
        const char *obj_start = p;
        int depth = 0;
        while (*p) {
            if (*p == '{') depth++;
            else if (*p == '}') { depth--; if (depth == 0) { p++; break; } }
            p++;
        }
        size_t obj_len = (size_t)(p - obj_start);
        /* Copy the object to a NUL-terminated buffer. */
        char obj_buf[512];
        if (obj_len >= sizeof(obj_buf)) {
            fprintf(stderr, "WARNING: golden core object too large, skipping\n");
            continue;
        }
        memcpy(obj_buf, obj_start, obj_len);
        obj_buf[obj_len] = '\0';

        /* Extract the "event" tag. */
        core_event_t ev;
        memset(&ev, 0, sizeof(ev));
        char event_tag[32];
        json_extract_string(obj_buf, 0, "event", event_tag, sizeof(event_tag));

        if (strcmp(event_tag, "video") == 0) {
            ev.kind = CE_VIDEO;
            int64_t prog64 = 0, pid64 = 0;
            json_extract_int64(obj_buf, 0, "program", &prog64);
            json_extract_int64(obj_buf, 0, "pid",     &pid64);
            ev.program = (uint16_t)prog64;
            ev.pid     = (uint16_t)pid64;
            json_extract_string(obj_buf, 0, "stream_type",
                                ev.stream_type, sizeof(ev.stream_type));
            json_extract_int64(obj_buf, 0, "pts", &ev.pts);
            int key_val = 0;
            json_extract_bool(obj_buf, 0, "key", &key_val);
            ev.key = key_val;
            json_extract_string(obj_buf, 0, "payload_sha256",
                                ev.payload_sha256, sizeof(ev.payload_sha256));

        } else if (strcmp(event_tag, "audio") == 0) {
            ev.kind = CE_AUDIO;
            int64_t prog64 = 0, pid64 = 0;
            json_extract_int64(obj_buf, 0, "program", &prog64);
            json_extract_int64(obj_buf, 0, "pid",     &pid64);
            ev.program = (uint16_t)prog64;
            ev.pid     = (uint16_t)pid64;
            json_extract_string(obj_buf, 0, "stream_type",
                                ev.stream_type, sizeof(ev.stream_type));
            json_extract_int64(obj_buf, 0, "pts", &ev.pts);
            json_extract_string(obj_buf, 0, "payload_sha256",
                                ev.payload_sha256, sizeof(ev.payload_sha256));

        } else if (strcmp(event_tag, "klv") == 0) {
            ev.kind = CE_KLV;
            int64_t prog64 = 0, pid64 = 0;
            json_extract_int64(obj_buf, 0, "program", &prog64);
            json_extract_int64(obj_buf, 0, "pid",     &pid64);
            ev.program = (uint16_t)prog64;
            ev.pid     = (uint16_t)pid64;
            json_extract_string(obj_buf, 0, "stream_type",
                                ev.stream_type, sizeof(ev.stream_type));
            json_extract_string(obj_buf, 0, "set", ev.set, sizeof(ev.set));

        } else if (strcmp(event_tag, "subtitle") == 0) {
            ev.kind = CE_SUBTITLE;
            int64_t prog64 = 0, pid64 = 0;
            json_extract_int64(obj_buf, 0, "program", &prog64);
            json_extract_int64(obj_buf, 0, "pid",     &pid64);
            ev.program = (uint16_t)prog64;
            ev.pid     = (uint16_t)pid64;
            json_extract_string(obj_buf, 0, "stream_type",
                                ev.stream_type, sizeof(ev.stream_type));
            json_extract_string(obj_buf, 0, "codec", ev.codec, sizeof(ev.codec));

        } else if (strcmp(event_tag, "unknown") == 0) {
            ev.kind = CE_UNKNOWN;
            int64_t pid64 = 0;
            json_extract_int64(obj_buf, 0, "pid", &pid64);
            ev.pid = (uint16_t)pid64;

        } else if (strcmp(event_tag, "error") == 0) {
            ev.kind = CE_ERROR;
            json_extract_string(obj_buf, 0, "code", ev.code, sizeof(ev.code));

        } else {
            fprintf(stderr,
                "ERROR: golden contains unrecognised event tag '%s'; "
                "this C adapter must be updated to handle it.\n", event_tag);
            /* Fail the comparison rather than silently skipping. */
            ev.kind = CE_ERROR;
            strncpy(ev.code, "UNKNOWN_EVENT_TAG", sizeof(ev.code) - 1);
        }

        out_events[count++] = ev;
    }
    return count;
}

/* Compare two core event lists field-by-field.
 * Returns 1 if identical, 0 if different (and prints a diff). */
static int compare_core_events(const char *scenario_id,
                               const core_event_t *observed, int obs_count,
                               const core_event_t *expected, int exp_count) {
    if (obs_count != exp_count) {
        fprintf(stderr, "FAIL [%s]: core event count mismatch: "
                "observed=%d expected=%d\n",
                scenario_id, obs_count, exp_count);
        return 0;
    }

    int ok = 1;
    for (int i = 0; i < obs_count; i++) {
        const core_event_t *o = &observed[i];
        const core_event_t *e = &expected[i];

        if (o->kind != e->kind) {
            fprintf(stderr, "FAIL [%s] event[%d]: kind mismatch "
                    "(observed=%d expected=%d)\n",
                    scenario_id, i, o->kind, e->kind);
            ok = 0;
            continue;
        }

        switch (o->kind) {
            case CE_VIDEO:
                if (o->program != e->program) {
                    fprintf(stderr, "FAIL [%s] video[%d]: program %u != %u\n",
                            scenario_id, i, o->program, e->program); ok = 0;
                }
                if (o->pid != e->pid) {
                    fprintf(stderr, "FAIL [%s] video[%d]: pid %u != %u\n",
                            scenario_id, i, o->pid, e->pid); ok = 0;
                }
                if (strcmp(o->stream_type, e->stream_type) != 0) {
                    fprintf(stderr, "FAIL [%s] video[%d]: stream_type '%s' != '%s'\n",
                            scenario_id, i, o->stream_type, e->stream_type); ok = 0;
                }
                if (o->pts != e->pts) {
                    fprintf(stderr, "FAIL [%s] video[%d]: pts %" PRId64 " != %" PRId64 "\n",
                            scenario_id, i, o->pts, e->pts); ok = 0;
                }
                if (o->key != e->key) {
                    fprintf(stderr, "FAIL [%s] video[%d]: key %d != %d\n",
                            scenario_id, i, o->key, e->key); ok = 0;
                }
                if (strcmp(o->payload_sha256, e->payload_sha256) != 0) {
                    fprintf(stderr, "FAIL [%s] video[%d]: payload_sha256\n"
                            "  observed : %s\n  expected : %s\n",
                            scenario_id, i, o->payload_sha256, e->payload_sha256); ok = 0;
                }
                break;

            case CE_AUDIO:
                if (o->program != e->program) {
                    fprintf(stderr, "FAIL [%s] audio[%d]: program %u != %u\n",
                            scenario_id, i, o->program, e->program); ok = 0;
                }
                if (o->pid != e->pid) {
                    fprintf(stderr, "FAIL [%s] audio[%d]: pid %u != %u\n",
                            scenario_id, i, o->pid, e->pid); ok = 0;
                }
                if (strcmp(o->stream_type, e->stream_type) != 0) {
                    fprintf(stderr, "FAIL [%s] audio[%d]: stream_type '%s' != '%s'\n",
                            scenario_id, i, o->stream_type, e->stream_type); ok = 0;
                }
                if (o->pts != e->pts) {
                    fprintf(stderr, "FAIL [%s] audio[%d]: pts %" PRId64 " != %" PRId64 "\n",
                            scenario_id, i, o->pts, e->pts); ok = 0;
                }
                if (strcmp(o->payload_sha256, e->payload_sha256) != 0) {
                    fprintf(stderr, "FAIL [%s] audio[%d]: payload_sha256\n"
                            "  observed : %s\n  expected : %s\n",
                            scenario_id, i, o->payload_sha256, e->payload_sha256); ok = 0;
                }
                break;

            case CE_KLV:
                if (o->program != e->program) {
                    fprintf(stderr, "FAIL [%s] klv[%d]: program %u != %u\n",
                            scenario_id, i, o->program, e->program); ok = 0;
                }
                if (o->pid != e->pid) {
                    fprintf(stderr, "FAIL [%s] klv[%d]: pid %u != %u\n",
                            scenario_id, i, o->pid, e->pid); ok = 0;
                }
                if (strcmp(o->stream_type, e->stream_type) != 0) {
                    fprintf(stderr, "FAIL [%s] klv[%d]: stream_type '%s' != '%s'\n",
                            scenario_id, i, o->stream_type, e->stream_type); ok = 0;
                }
                if (strcmp(o->set, e->set) != 0) {
                    fprintf(stderr, "FAIL [%s] klv[%d]: set '%s' != '%s'\n",
                            scenario_id, i, o->set, e->set); ok = 0;
                }
                break;

            case CE_SUBTITLE:
                if (o->program != e->program) {
                    fprintf(stderr, "FAIL [%s] subtitle[%d]: program %u != %u\n",
                            scenario_id, i, o->program, e->program); ok = 0;
                }
                if (o->pid != e->pid) {
                    fprintf(stderr, "FAIL [%s] subtitle[%d]: pid %u != %u\n",
                            scenario_id, i, o->pid, e->pid); ok = 0;
                }
                if (strcmp(o->stream_type, e->stream_type) != 0) {
                    fprintf(stderr, "FAIL [%s] subtitle[%d]: stream_type '%s' != '%s'\n",
                            scenario_id, i, o->stream_type, e->stream_type); ok = 0;
                }
                if (strcmp(o->codec, e->codec) != 0) {
                    fprintf(stderr, "FAIL [%s] subtitle[%d]: codec '%s' != '%s'\n",
                            scenario_id, i, o->codec, e->codec); ok = 0;
                }
                break;

            case CE_UNKNOWN:
                if (o->pid != e->pid) {
                    fprintf(stderr, "FAIL [%s] unknown[%d]: pid %u != %u\n",
                            scenario_id, i, o->pid, e->pid); ok = 0;
                }
                break;

            case CE_ERROR:
                if (strcmp(o->code, e->code) != 0) {
                    fprintf(stderr, "FAIL [%s] error[%d]: code '%s' != '%s'\n",
                            scenario_id, i, o->code, e->code); ok = 0;
                }
                break;

            default:
                /* Defensive: an out-of-range kind must fail loudly rather than
                 * compare equal. Unreachable today (unknown golden tags are
                 * rejected in parse_golden_core), but a silent pass on a future
                 * kind would be a parity hole. */
                fprintf(stderr, "FAIL [%s] event[%d]: unhandled core event "
                        "kind %d\n", scenario_id, i, o->kind);
                ok = 0;
                break;
        }
    }
    return ok;
}

/* ── Main ────────────────────────────────────────────────────────────────────*/

int main(int argc, char **argv) {
    /* The scenarios directory can be passed as argv[1].
     *
     * Default (no argv[1]): "../../../tst-integration/tests/fixtures/scenarios"
     * relative to the current working directory. This works correctly when:
     *   - CWD = ts-transformer/ workspace root (the most common invocation)
     *   - CWD = ts-transformer/bindings/c/ (e.g. after `cd` for the build step)
     * Adjust with argv[1] for any other CWD. The companion run_scenarios.sh sets
     * argv[1] to an absolute path derived from the script's own location.
     */
    const char *scenarios_dir_path = (argc >= 2)
        ? argv[1]
        : "crates/tst-integration/tests/fixtures/scenarios";

    fprintf(stdout, "scenarios dir: %s\n", scenarios_dir_path);

    /* Read scenarios.toml. */
    char *manifest_path = path_join(scenarios_dir_path, "scenarios.toml");
    if (!manifest_path) { fprintf(stderr, "ERROR: OOM\n"); return 1; }

    size_t manifest_len = 0;
    uint8_t *manifest_data = read_file(manifest_path, &manifest_len);
    free(manifest_path);
    if (!manifest_data) return 1;

    /* Parse the manifest. */
    static scenario_entry_t entries[MAX_SCENARIOS];
    int scenario_count = parse_scenarios_toml((const char *)manifest_data, entries);
    free(manifest_data);

    if (scenario_count == 0) {
        fprintf(stderr, "ERROR: no scenarios found in scenarios.toml\n");
        return 1;
    }
    fprintf(stdout, "found %d scenario(s)\n\n", scenario_count);

    /* Run each scenario and compare against its golden. */
    int failures = 0;
    for (int s = 0; s < scenario_count; s++) {
        const scenario_entry_t *entry = &entries[s];
        fprintf(stdout, "--- scenario '%s' (kind=%s)\n", entry->id, entry->kind);

        /* Read golden.json. */
        char *golden_path = path_join(scenarios_dir_path, entry->golden);
        if (!golden_path) { fprintf(stderr, "ERROR: OOM\n"); failures++; continue; }

        size_t golden_len = 0;
        uint8_t *golden_data = read_file(golden_path, &golden_len);
        free(golden_path);
        if (!golden_data) { failures++; continue; }
        const char *golden_json = (const char *)golden_data;

        /* Run the scenario. */
        core_event_list_t observed;
        memset(&observed, 0, sizeof(observed));

        int run_rc = 0;
        if (strcmp(entry->kind, "demux") == 0) {
            run_rc = run_demux(scenarios_dir_path, entry, &observed);
        } else if (strcmp(entry->kind, "roundtrip") == 0) {
            run_rc = run_roundtrip(scenarios_dir_path, entry, golden_json, &observed);
        } else if (strcmp(entry->kind, "binding_contract") == 0) {
            run_rc = run_binding_contract(scenarios_dir_path, entry, &observed);
        } else {
            fprintf(stderr, "ERROR: unknown scenario kind '%s'\n", entry->kind);
            run_rc = -1;
        }

        if (run_rc != 0) {
            fprintf(stderr, "FAIL [%s]: scenario runner returned error\n", entry->id);
            free(golden_data);
            failures++;
            continue;
        }

        /* Parse golden's core events and compare. */
        /* Roundtrip scenario: core is empty — comparison trivially passes if
         * run_roundtrip() returned 0 (which already verified the sha256). */
        static core_event_t golden_core_events[MAX_CORE_EVENTS];
        int golden_event_count = parse_golden_core(golden_json,
            golden_core_events, MAX_CORE_EVENTS);
        free(golden_data);

        int match = compare_core_events(entry->id,
            observed.events, observed.count,
            golden_core_events, golden_event_count);

        if (match) {
            fprintf(stdout, "PASS [%s]\n", entry->id);
        } else {
            failures++;
        }
        fprintf(stdout, "\n");
    }

    /* Summary. */
    fprintf(stdout, "=== %d scenario(s): %d passed, %d failed ===\n",
            scenario_count,
            scenario_count - failures,
            failures);

    return failures > 0 ? 1 : 0;
}
