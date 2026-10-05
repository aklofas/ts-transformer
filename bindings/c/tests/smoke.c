/*
 * Compiled and run from tests/smoke.rs. Exercises every C handle type
 * except live-socket connects (those need a Listener pair which is
 * easier to set up from Rust — see receiving/live_pair.rs).
 */

#include "tstrans.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

int main(void) {
    /* Version macros are visible. */
    printf("tstrans version: %d.%d.%d\n",
           TST_VERSION_MAJOR, TST_VERSION_MINOR, TST_VERSION_PATCH);

    /* Muxer roundtrip — single program. */
    tst_mux_config_t* cfg = tst_mux_config_new();
    if (!cfg) { fprintf(stderr, "mux_config_new failed\n"); return 1; }
    tst_program_handle_t p1 = tst_mux_config_add_program(cfg, 1, 0x1000);
    if (p1 == TST_INVALID_PROGRAM_HANDLE) {
        fprintf(stderr, "add_program failed\n"); return 2;
    }
    tst_video_stream_handle_t hv = tst_mux_config_add_video_stream(
        cfg, p1, 0x1011, TST_VIDEO_CODEC_H264);
    if (hv == TST_INVALID_STREAM_HANDLE) { fprintf(stderr, "add_video_stream failed\n"); return 3; }
    tst_klv_stream_handle_t hk = tst_mux_config_add_klv_stream(
        cfg, p1, 0x1031, TST_KLV_STREAM_TYPE_PRIVATE_DATA, false);
    if (hk == TST_INVALID_STREAM_HANDLE) { fprintf(stderr, "add_klv_stream failed\n"); return 4; }

    tst_muxer_t* mux = tst_muxer_open(cfg);
    if (!mux) { fprintf(stderr, "muxer_open failed: %s\n", tst_get_last_error_str()); return 5; }

    uint8_t nal[] = { 0, 0, 0, 1, 0x65, 0xAA, 0xAA, 0xAA, 0xAA };
    if (tst_muxer_push_video_to(mux, hv, nal, sizeof(nal), 0, true) != 0) {
        fprintf(stderr, "push_video_to failed: %s\n", tst_get_last_error_str());
        return 6;
    }
    uint8_t out[4096];
    size_t n = tst_muxer_pull(mux, out, sizeof(out));
    if (n == 0 || out[0] != 0x47) {
        fprintf(stderr, "pull returned %zu bytes, first=0x%02x\n", n, out[0]);
        return 7;
    }
    tst_muxer_close(mux);
    tst_mux_config_free(cfg);

    /* All NULL closes are no-ops. */
    tst_muxer_close(NULL);
    tst_mux_sender_close(NULL);
    tst_managed_mux_sender_close(NULL);
    tst_sender_close(NULL);
    tst_managed_sender_close(NULL);
    tst_raw_sender_close(NULL);
    tst_managed_raw_sender_close(NULL);

    /* Configs free cleanly. */
    tst_sender_config_free(tst_sender_config_new());
    tst_raw_sender_config_free(tst_raw_sender_config_new());
    tst_reconnect_policy_free(tst_reconnect_policy_new());

    /* Open of a sender against an invalid URL yields NULL with last-error set. */
    tst_mux_config_t* mc = tst_mux_config_new();
    tst_program_handle_t mc_prog = tst_mux_config_add_program(mc, 1, 0x1000);
    tst_video_stream_handle_t mc_hv = tst_mux_config_add_video_stream(
        mc, mc_prog, 0x1011, TST_VIDEO_CODEC_H264);
    tst_klv_stream_handle_t mc_hk = tst_mux_config_add_klv_stream(
        mc, mc_prog, 0x1031, TST_KLV_STREAM_TYPE_PRIVATE_DATA, false);
    (void)mc_hv; (void)mc_hk;
    tst_mux_sender_t* s = tst_mux_sender_open("not-a-url", mc);
    if (s != NULL) { fprintf(stderr, "expected NULL on invalid url\n"); return 8; }
    int code = tst_get_last_error();
    if (code == 0) { fprintf(stderr, "expected non-zero last-error\n"); return 9; }
    printf("invalid-url last-error: %d (%s)\n", code, tst_get_last_error_str());
    tst_mux_config_free(mc);

    /* Multi-stream muxer: two video PIDs + one KLV PID, each with a handle. */
    tst_mux_config_t* mcfg = tst_mux_config_new();
    if (!mcfg) return 10;
    tst_program_handle_t mp = tst_mux_config_add_program(mcfg, 1, 0x1000);
    tst_video_stream_handle_t h_eo =
        tst_mux_config_add_video_stream(mcfg, mp, 0x1011, TST_VIDEO_CODEC_H264);
    tst_video_stream_handle_t h_ir =
        tst_mux_config_add_video_stream(mcfg, mp, 0x1012, TST_VIDEO_CODEC_H264);
    tst_klv_stream_handle_t h_klv =
        tst_mux_config_add_klv_stream(mcfg, mp, 0x1031, TST_KLV_STREAM_TYPE_PRIVATE_DATA, false);
    if (h_eo == TST_INVALID_STREAM_HANDLE) return 11;
    if (h_ir == TST_INVALID_STREAM_HANDLE) return 12;
    if (h_klv == TST_INVALID_STREAM_HANDLE) return 13;
    /* packed handle: VideoStreamHandle::pack(0,0)=0, pack(0,1)=1 */
    if (h_eo != 0 || h_ir != 1) {
        fprintf(stderr, "video handles not 0,1: got %u,%u\n", h_eo, h_ir);
        return 14;
    }
    if (h_klv != 0) { fprintf(stderr, "klv handle not 0: got %u\n", h_klv); return 15; }

    tst_muxer_t* mmux = tst_muxer_open(mcfg);
    tst_mux_config_free(mcfg);
    if (!mmux) return 16;

    if (tst_muxer_push_video_to(mmux, h_eo, nal, sizeof(nal), 0, true) != 0) return 17;
    if (tst_muxer_push_video_to(mmux, h_ir, nal, sizeof(nal), 1000, true) != 0) return 18;
    uint8_t klv[] = {
        0x06, 0x0e, 0x2b, 0x34, 0x02, 0x0b, 0x01, 0x01,
        0x0e, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
    };
    if (tst_muxer_push_klv_to(mmux, h_klv, klv, sizeof(klv), 2000) != 0) return 19;

    /* Drain and confirm aligned TS bytes. */
    size_t mn = tst_muxer_pull(mmux, out, sizeof(out));
    if (mn == 0 || (mn % 188) != 0 || out[0] != 0x47) {
        fprintf(stderr, "multi-stream pull: n=%zu first=0x%02x\n", mn, out[0]);
        return 20;
    }

    /* Out-of-range handle is rejected with TST_E_INVALID_USAGE. */
    int rc = tst_muxer_push_video_to(mmux, 99, nal, sizeof(nal), 3000, true);
    if (rc != TST_E_INVALID_USAGE) {
        fprintf(stderr, "expected TST_E_INVALID_USAGE, got %d\n", rc);
        return 21;
    }

    tst_muxer_close(mmux);

    /* Multi-program config: verify add_program returns sequential ordinals,
     * stream-add routes to the right program, build succeeds. */
    {
        tst_mux_config_t* mpcfg = tst_mux_config_new();
        if (!mpcfg) return 22;

        tst_program_handle_t pp1 = tst_mux_config_add_program(mpcfg, 1, 0x1000);
        tst_program_handle_t pp2 = tst_mux_config_add_program(mpcfg, 2, 0x1100);
        if (pp1 != 0) {
            fprintf(stderr, "program 1 handle not 0: got %u\n", pp1);
            return 23;
        }
        if (pp2 != 1) {
            fprintf(stderr, "program 2 handle not 1: got %u\n", pp2);
            return 24;
        }

        /* VideoStreamHandle::pack(0,0)=0 for program 1 stream 0 */
        tst_video_stream_handle_t vp1 = tst_mux_config_add_video_stream(
            mpcfg, pp1, 0x1011, TST_VIDEO_CODEC_H264);
        /* VideoStreamHandle::pack(1,0)=0x10 for program 2 stream 0 */
        tst_video_stream_handle_t vp2 = tst_mux_config_add_video_stream(
            mpcfg, pp2, 0x1111, TST_VIDEO_CODEC_H265);
        if (vp1 == TST_INVALID_STREAM_HANDLE || vp2 == TST_INVALID_STREAM_HANDLE) {
            fprintf(stderr, "video stream add failed\n"); return 25;
        }
        /* Handles from different programs must be distinct. */
        if (vp1 == vp2) {
            fprintf(stderr, "expected distinct handles; got %u == %u\n", vp1, vp2);
            return 26;
        }

        tst_mux_config_add_klv_stream(mpcfg, pp1, 0x1031, TST_KLV_STREAM_TYPE_PRIVATE_DATA, false);
        tst_mux_config_add_klv_stream(mpcfg, pp2, 0x1131, TST_KLV_STREAM_TYPE_PRIVATE_DATA, false);

        tst_muxer_t* mpmux = tst_muxer_open(mpcfg);
        if (!mpmux) {
            fprintf(stderr, "multi-program muxer_open failed: %s\n", tst_get_last_error_str());
            tst_mux_config_free(mpcfg);
            return 27;
        }

        tst_mux_config_free(mpcfg);
        tst_muxer_close(mpmux);
        fprintf(stderr, "multi-program config OK\n");
    }

    /* Stats accessor smoke. Confirms tst_muxer_get_stats / reset_stats
     * link, return 0 on success, populate per_stream_count from the
     * eager-on-construction config, and reject null pointers with a
     * non-zero return. Live round-trip is covered by the Rust
     * integration test bindings/c/tests/stats.rs. */
    {
        tst_mux_config_t* scfg = tst_mux_config_new();
        tst_program_handle_t sp = tst_mux_config_add_program(scfg, 1, 0x1000);
        tst_mux_config_add_video_stream(scfg, sp, 0x0100, TST_VIDEO_CODEC_H264);
        tst_mux_config_add_klv_stream(scfg, sp, 0x0101, TST_KLV_STREAM_TYPE_PRIVATE_DATA, false);
        tst_muxer_t* sm = tst_muxer_open(scfg);
        if (!sm) { fprintf(stderr, "stats: mux open failed: %s\n", tst_get_last_error_str()); return 28; }

        tst_muxer_stats_t st;
        int src = tst_muxer_get_stats(sm, &st);
        if (src != 0) { fprintf(stderr, "stats: get_stats failed: %d\n", src); return 29; }
        /* One video + one KLV = 2 streams; per_stream_count mirrors the
         * stream count declared in the config at construction time. */
        if (st.per_stream_count != 2) {
            fprintf(stderr, "stats: expected 2 streams, got %u\n", st.per_stream_count);
            return 30;
        }

        src = tst_muxer_reset_stats(sm);
        if (src != 0) { fprintf(stderr, "stats: reset_stats failed: %d\n", src); return 31; }

        /* Null-pointer paths must return non-zero — the C API treats null
         * handles and null out-pointers as caller bugs and reports them
         * immediately rather than crashing. */
        if (tst_muxer_get_stats(NULL, &st) == 0) {
            fprintf(stderr, "stats: null muxer should fail\n");
            return 32;
        }
        if (tst_muxer_get_stats(sm, NULL) == 0) {
            fprintf(stderr, "stats: null out should fail\n");
            return 33;
        }

        tst_muxer_close(sm);
        tst_mux_config_free(scfg);
        fprintf(stderr, "stats smoke OK\n");
    }

    /* Codec-specific per-stream stats accessor smoke. Confirms
     * tst_muxer_get_stream_codec_stats links and reports TST_E_NOT_FOUND
     * for a PID that has never been observed on the handle. Live
     * round-trip with populated codec arms is covered by Rust-side
     * integration tests; this block only verifies the C link surface
     * and the not-found error code. */
    {
        tst_mux_config_t* ccfg = tst_mux_config_new();
        tst_program_handle_t cp = tst_mux_config_add_program(ccfg, 1, 0x1000);
        tst_mux_config_add_video_stream(ccfg, cp, 0x0100, TST_VIDEO_CODEC_H264);
        tst_muxer_t* cm = tst_muxer_open(ccfg);
        if (!cm) { fprintf(stderr, "codec_stats: mux open failed: %s\n", tst_get_last_error_str()); return 34; }

        tst_stream_codec_stats_t cs;
        int crc = tst_muxer_get_stream_codec_stats(cm, 0x9999, &cs);
        if (crc != TST_E_NOT_FOUND) {
            fprintf(stderr, "codec_stats: expected TST_E_NOT_FOUND (-14) for unseen PID, got %d\n", crc);
            return 35;
        }

        tst_muxer_close(cm);
        tst_mux_config_free(ccfg);
        fprintf(stderr, "codec_stats smoke: pid-not-seen returns NOT_FOUND OK\n");
    }

    printf("smoke OK\n");
    return 0;
}
