/*
 * mux_with_audio_klv_subtitles.c — Four-stream MPEG-TS mux from C:
 *   H.264 video + AAC-ADTS audio + ST 0601 KLV + DVB subtitles.
 *
 * This is the FIRST C example that exercises all four user-visible
 * stream-handle types in one program:
 *   - TstVideoStreamHandle    (mux_dual_camera.c already covered this)
 *   - TstAudioStreamHandle    (NEW in C — was only in Rust before)
 *   - TstKlvStreamHandle      (mux_dual_camera.c already covered this)
 *   - TstSubtitleStreamHandle (NEW in C — was only in Rust before)
 *
 * Mirrors two Rust examples:
 *   - examples/muxing/mux_audio_video_klv.rs       (video + audio + KLV)
 *   - examples/muxing/mux_with_webvtt_subtitles.rs (video + subtitle)
 * combined into one program so a C integrator can see the full surface in
 * a single file.
 *
 * Build (from the ts-transformer workspace root):
 *   SRT_FORCE_VENDORED=1 cargo build -p tst-c
 *   cc -I bindings/c/include \
 *      -L target/debug \
 *      -Wall -Werror \
 *      -o /tmp/mux_with_audio_klv_subtitles \
 *      bindings/c/examples/muxing/mux_with_audio_klv_subtitles.c \
 *      -ltstrans
 *
 * Run:
 *   LD_LIBRARY_PATH=target/debug /tmp/mux_with_audio_klv_subtitles
 *
 * Output:
 *   /tmp/mux_4streams.ts
 *
 * Verify:
 *   ffprobe -show_streams /tmp/mux_4streams.ts
 *     # Expect 4 streams: codec_type=video, audio, data (KLV), subtitle.
 *   tsp -I file /tmp/mux_4streams.ts -P analyze
 *     # TSDuck's PSI walker — PMT should enumerate all 4 PIDs with the
 *     # right stream_type bytes and descriptors.
 *
 * API choice — file-output muxer vs SRT-wrapping sender:
 *   This example uses the standalone `tst_muxer_t` (file-output path)
 *   matching mux_dual_camera.c's choice. The SRT-wrapping sibling is
 *   `tst_mux_sender_t` (tst_mux_sender_send_audio_to / send_subtitle_to);
 *   it's available too but adds network setup that isn't relevant to
 *   "show the 4-stream handle surface." Output goes to a plain file.
 */

#include "tstrans.h"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ── Synthetic payload constants ──────────────────────────────────────── */

/*
 * Synthetic H.264 NAL — 4-byte Annex-B start code (0x00 0x00 0x00 0x01)
 * followed by nal_unit_type 0x65 (IDR slice in non-IDR-frame nomenclature:
 * the low 5 bits are 0x05, with nal_ref_idc=3 in the high two bits → 0x65).
 *
 * WHY 0x65 here vs 0x67 (SPS) in mux_dual_camera.c?
 *   Both are "stub bytes that satisfy the muxer's structural roundtrip" —
 *   neither produces a decodable bitstream. 0x65 is the convention used
 *   by the Rust C-ABI tests at bindings/c/tests/audio_subtitle.rs
 *   (NAL_IDR). The muxer does NOT inspect NAL bytes to derive RAI; the
 *   RAI bit is driven solely by the `key_frame` arg to push_video_to.
 *   See mux_synthetic_srt.c for the full lesson on the "no auto-RAI from
 *   NAL contents" contract.
 */
static const uint8_t NAL_H264[8] = {
    0x00, 0x00, 0x00, 0x01, 0x65, 0xBB, 0xBB, 0xBB,
};

/*
 * Synthetic AAC-ADTS frame — 7-byte ADTS header + 9 bytes zero-filled
 * raw_data_block. Mirrors SYNTHETIC_ADTS in tests/audio_subtitle.rs.
 *
 * WHY exactly 16 bytes (not 8 from the plan's smaller stub)?
 *   The ADTS header's `aac_frame_length` field (bits 30-42, spans bytes
 *   3-5) must match the total frame size including header. The byte
 *   sequence below has frame_length encoded as 16 — match it by passing
 *   sizeof(AAC_ADTS_FRAME) = 16. Using a length-mismatched stub causes
 *   the muxer's ADTS framer to either reject the frame or split it in
 *   unintended ways.
 *
 * Header byte breakdown (ISO/IEC 13818-7 §6.2):
 *   0xFF 0xF1 — 12-bit sync (0xFFF) + MPEG-4 + Layer 00 + no_CRC=1
 *   0x50     — profile (AAC LC) + sample_rate_idx 4 (44.1 kHz)
 *   0x80     — channel_config 1 (mono) + remaining MSBs zero
 *   0x02 0x1F 0xFC — frame_length=16 (bits) + buffer_fullness + num_frames-1
 *   0x00 .. — 9 bytes of payload (zeroed; not a decodable AAC block,
 *             but the muxer only needs the framing to be self-consistent
 *             for PES emission to succeed).
 *
 * ffprobe will still report `codec_name=aac` and `codec_type=audio` on
 * this stream because that comes from the PMT's stream_type byte (0x0F
 * for ADTS AAC), not from parsing the audio payload.
 */
static const uint8_t AAC_ADTS_FRAME[16] = {
    0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
};

/*
 * Minimal 17-byte ST 0601 KLV blob — same shape as mux_dual_camera.c.
 *   - 16 bytes: UAS Datalink LS Universal Label
 *   -  1 byte:  BER short-form length = 0 (empty Local Set)
 * This is the smallest spec-conformant ST 0601 KLV envelope. The muxer
 * frames it into one KLV PES packet on PID 0x1031.
 */
static const uint8_t KLV_BLOB[17] = {
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01,
    0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
    0x00, /* length = 0 */
};

/*
 * Synthetic DVB-subtitling PES payload — ETSI EN 300 743 §7.2 layout:
 *   0x20         — data_identifier (subtitle stream marker)
 *   0x00         — subtitle_stream_id
 *   0x0F         — sync_byte for first segment
 *   0x10         — segment_type 0x10 = page_composition_segment
 *   0x00 0x01    — page_id = 1 (matches composition_page_id in PMT)
 *   0x00 0x06    — segment_length = 6 (body bytes that follow)
 *   0x3C         — page_timeout = 60 seconds
 *   0x00         — page_version_number=0 + page_state=0
 *   0x00 0x00 0x00 0x00 — single region: id=0, h-pos=0, v-pos=0 (pad)
 *   0xFF         — end_of_PES_data_field_marker (sync_byte 0xFF closes)
 *
 * This is a spec-shape-valid (if minimal) page_composition_segment.
 * Renderers will see "blank page" — what matters here is that the muxer
 * frames it correctly as a subtitle PES on PID 0x1041 with PMT
 * stream_type=0x06 + auto-emitted subtitling_descriptor.
 */
static const uint8_t DVB_SUB_SEGMENT[15] = {
    0x20, 0x00,
    0x0F, 0x10, 0x00, 0x01,
    0x00, 0x06,
    0x3C, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xFF,
};

int main(void) {
    /*
     * ── Step 1: Build the mux config ─────────────────────────────────────
     *
     * One program containing four streams of different types.
     */
    tst_mux_config_t *cfg = tst_mux_config_new();
    if (!cfg) {
        fprintf(stderr, "tst_mux_config_new: out of memory\n");
        return 1;
    }

    /*
     * Register program 1 (PMT PID 0x1000). Standard single-program layout
     * — see mux_dual_camera.c for the WHY-this-PID rationale.
     */
    tst_program_handle_t prog =
        tst_mux_config_add_program(cfg, /*program_number=*/1, /*pmt_pid=*/0x1000);
    if (prog == TST_INVALID_PROGRAM_HANDLE) {
        fprintf(stderr, "add_program failed: %s\n", tst_get_last_error_str());
        tst_mux_config_free(cfg);
        return 2;
    }

    /*
     * Add video (H.264) on PID 0x1011. The first video stream becomes the
     * default PCR source for the program — we leave that default in
     * place (no explicit set_pcr_pid call). PMT stream_type derives from
     * the codec choice: TST_VIDEO_CODEC_H264 → 0x1B.
     */
    tst_video_stream_handle_t h_video =
        tst_mux_config_add_video_stream(cfg, prog, /*pid=*/0x1011,
                                         TST_VIDEO_CODEC_H264);

    /*
     * Add audio (AAC-ADTS) on PID 0x1021.
     *
     * WHY TST_AUDIO_CODEC_AAC (and not _AAC_LATM / _MP2)?
     *   The synthetic payload above is ADTS-framed AAC (0xFFF sync);
     *   _AAC matches and drives PMT stream_type=0x0F. _AAC_LATM expects
     *   different framing (stream_type 0x11); _MP2 expects MP2 frame
     *   sync + a different header — both would surface as malformed-
     *   frame errors inside the muxer's framer.
     *
     * Use the `_with_language` variant if you want ffprobe to report
     * `language=eng` for this stream (auto-emits ISO 639 descriptor).
     */
    tst_audio_stream_handle_t h_audio =
        tst_mux_config_add_audio_stream(cfg, prog, /*pid=*/0x1021,
                                         TST_AUDIO_CODEC_AAC);

    /*
     * Add KLV (async, no PTS in PES) on PID 0x1031. Same rationale as
     * mux_dual_camera.c — PRIVATE_DATA / carries_pts=false avoids AU
     * cell wrapping. Use SYNCHRONOUS_METADATA when downstream demuxers
     * want PTS-paired KLV-to-video alignment.
     */
    tst_klv_stream_handle_t h_klv =
        tst_mux_config_add_klv_stream(cfg, prog, /*pid=*/0x1031,
                                       TST_KLV_STREAM_TYPE_PRIVATE_DATA,
                                       /*carries_pts=*/false);

    /*
     * Add DVB subtitles on PID 0x1041.
     *
     * WHY dvb_subtitling vs the other 3 subtitle constructors?
     *   DVB-sub carries the most-structured parameters (language +
     *   subtitling_type + composition_page_id + ancillary_page_id) so
     *   it's the most instructive of the four. WebVTT is typical for
     *   HLS; teletext is European broadcast; CEA-708 is NTSC captions.
     *
     * Parameter notes:
     *   - language: ISO 639-2 lowercase ("eng" = English).
     *   - subtitling_type 0x10 = "DVB sub, no AR signalling" per ETSI
     *     EN 300 468 Table 26 (safe HD/SD-agnostic default).
     *   - composition_page_id=1: MUST match the page_id encoded in the
     *     DVB-sub segment bytes (above at offset 4-5 = 0x00 0x01).
     *   - ancillary_page_id=0: not using an ancillary fallback page.
     */
    const uint8_t language[3] = { 'e', 'n', 'g' };
    tst_subtitle_stream_handle_t h_sub =
        tst_mux_config_add_subtitle_stream_dvb_subtitling(
            cfg, prog, /*pid=*/0x1041,
            language,
            /*subtitling_type=*/0x10,
            /*composition_page_id=*/1,
            /*ancillary_page_id=*/0);

    /* Validate all four handles before proceeding. */
    if (h_video == TST_INVALID_STREAM_HANDLE ||
        h_audio == TST_INVALID_STREAM_HANDLE ||
        h_klv == TST_INVALID_STREAM_HANDLE ||
        h_sub == TST_INVALID_STREAM_HANDLE) {
        fprintf(stderr, "stream-add failed: %s\n", tst_get_last_error_str());
        tst_mux_config_free(cfg);
        return 3;
    }

    /*
     * ── Step 2: Open the muxer ───────────────────────────────────────────
     *
     * Same shape as mux_dual_camera.c — config is consumed (copied) by
     * tst_muxer_open; free it immediately afterwards.
     */
    tst_muxer_t *m = tst_muxer_open(cfg);
    tst_mux_config_free(cfg);
    cfg = NULL;
    if (!m) {
        fprintf(stderr, "tst_muxer_open failed: %s\n", tst_get_last_error_str());
        return 4;
    }

    /*
     * ── Step 3: Open the output file ─────────────────────────────────────
     */
    FILE *out = fopen("/tmp/mux_4streams.ts", "wb");
    if (!out) {
        perror("fopen /tmp/mux_4streams.ts");
        tst_muxer_close(m);
        return 4;
    }

    /*
     * ── Step 4: Push 30 frames + drain after each ────────────────────────
     *
     * WHY 3000 PTS ticks per frame across ALL FOUR streams?
     *   MPEG-TS PTS is a 90 kHz clock (90,000 ticks/sec). At 30 fps that
     *   gives 3,000 ticks per video frame. Real-world audio (typically
     *   23.2 ms per AAC frame at 44.1 kHz = ~2087 ticks) and subtitles
     *   (display-duration-driven) would pace independently of video, but
     *   for a structural-roundtrip teaching example synchronizing all
     *   four to the video clock is significantly easier to read and
     *   still produces a valid TS file. The Rust mirror examples make
     *   the same simplification.
     *
     * WHY the drain inside the loop?
     *   tst_muxer_pull has an internal buffer (~10,000 packets ≈ 1.88 MB
     *   by default). For a 30-frame run we won't overflow, but the
     *   canonical pattern is push-then-drain so the in-memory queue
     *   stays bounded on longer runs (e.g., a 2-hour file recording).
     *   `pull` returns 0 when no aligned data is ready — that's the
     *   normal "buffer not yet full" signal, not an error.
     *
     * WHY 188 * 64 for the drain buffer?
     *   64 TS packets = 12,032 bytes — generous enough that one pull
     *   call drains the typical 10-20-packet burst that follows a push.
     */
    uint8_t buf[188 * 64];

    for (int i = 0; i < 30; i++) {
        int64_t pts = (int64_t)i * 3000;
        bool key = (i == 0); /* keyframe only on the first frame */

        /*
         * Push video. The `key_frame` arg drives the TS adaptation-field
         * RAI bit — the muxer does NOT auto-detect from NAL bytes
         * (see mux_synthetic_srt.c for the full lesson).
         */
        if (tst_muxer_push_video_to(m, h_video, NAL_H264, sizeof(NAL_H264),
                                     pts, key) != 0) {
            fprintf(stderr, "video push[%d] failed: %s\n", i,
                    tst_get_last_error_str());
            goto fail;
        }

        /*
         * Push audio. Same PTS as video — see the pacing rationale above.
         */
        if (tst_muxer_push_audio_to(m, h_audio, AAC_ADTS_FRAME,
                                     sizeof(AAC_ADTS_FRAME), pts) != 0) {
            fprintf(stderr, "audio push[%d] failed: %s\n", i,
                    tst_get_last_error_str());
            goto fail;
        }

        /*
         * Push KLV. PTS is supplied but ignored by the PES emitter for
         * carries_pts=false streams; still passed because it informs
         * the muxer's internal PSI/PCR scheduling.
         */
        if (tst_muxer_push_klv_to(m, h_klv, KLV_BLOB, sizeof(KLV_BLOB),
                                   pts) != 0) {
            fprintf(stderr, "klv push[%d] failed: %s\n", i,
                    tst_get_last_error_str());
            goto fail;
        }

        /*
         * Push subtitle. DVB-sub PES carries PTS — renderers display the
         * page at that presentation time. In a real subtitle pipeline
         * you'd push only on state changes (page composition updates),
         * not every video frame; pushing every frame here is wasteful
         * but legal and keeps the loop shape symmetric.
         */
        if (tst_muxer_push_subtitle_to(m, h_sub, DVB_SUB_SEGMENT,
                                        sizeof(DVB_SUB_SEGMENT), pts) != 0) {
            fprintf(stderr, "subtitle push[%d] failed: %s\n", i,
                    tst_get_last_error_str());
            goto fail;
        }

        /* Drain all ready TS packets into the output file. */
        size_t n;
        while ((n = tst_muxer_pull(m, buf, sizeof(buf))) > 0) {
            /* Structural sanity: every pull is a whole-packet multiple. */
            if ((n % 188) != 0 || buf[0] != 0x47) {
                fprintf(stderr,
                        "alignment check failed at frame %d: "
                        "n=%zu first=0x%02x\n",
                        i, n, (unsigned)buf[0]);
                goto fail;
            }
            fwrite(buf, 1, n, out);
        }
    }

    /*
     * ── Step 5: Close + final drain + verify hint ────────────────────────
     *
     * One last drain after the loop in case the internal buffer holds
     * a final partial burst (PSI re-emit, trailing PES packets, etc.).
     * The muxer doesn't have an explicit "flush" entry — pull-until-empty
     * is the convention. tst_muxer_close releases all internal state.
     */
    size_t n;
    while ((n = tst_muxer_pull(m, buf, sizeof(buf))) > 0) {
        if ((n % 188) != 0 || buf[0] != 0x47) {
            fprintf(stderr,
                    "final-drain alignment failed: n=%zu first=0x%02x\n",
                    n, (unsigned)buf[0]);
            goto fail;
        }
        fwrite(buf, 1, n, out);
    }

    fclose(out);
    tst_muxer_close(m);

    printf("Wrote /tmp/mux_4streams.ts\n");
    printf("Try: ffprobe -show_streams /tmp/mux_4streams.ts\n");
    printf("  Expected: 4 streams — video (h264), audio (aac), data (KLV), subtitle (dvb_subtitle).\n");
    return 0;

fail:
    /*
     * Cleanup on error: close file first (partial output is fine —
     * fclose flushes whatever was already written), then close the
     * muxer to release internal state. cfg was already freed before
     * the loop.
     */
    fclose(out);
    tst_muxer_close(m);
    return 5;
}
