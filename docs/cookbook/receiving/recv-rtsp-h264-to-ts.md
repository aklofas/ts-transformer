# Ingest H.264 from an RTSP camera and remux to MPEG-TS

> **When to use this:** You have an IP camera (ONVIF / generic RTSP) that
> exposes a bare H.264 elementary stream over RTSP (no MPEG-TS wrapper), and
> you want to re-mux the incoming access units into standard MPEG-TS for
> downstream consumers (SRT sender, file archive, TSDuck inspection, etc.).

> **Related:**
> - [`/docs/languages/python.md#h264-over-rtp-ingest-rfc-6184`](/docs/languages/python.md#h264-over-rtp-ingest-rfc-6184) — full Python API reference
> - [`examples/receiving/recv_rtsp_h264.rs`](/examples/receiving/recv_rtsp_h264.rs) — Rust runnable twin with full commentary
> - [Pair sync-KLV with video AUs by nearest PTS](/docs/cookbook/pairing/pair-klv-by-pts.md) — once you have AUs, pairing with KLV
> - [Receive MPEG-TS over UDP](/docs/cookbook/receiving/udp.md) — for cameras that expose MPEG-TS-over-RTP (use `RtspClient.connect()` → `into_demux_receiver()` for that shape)

Most surveillance cameras and gimbal sensor pods use one of two RTSP shapes:

1. **MPEG-TS-over-RTP (PT=33)** — the whole transport stream (video + KLV + audio) rides one RTP flow. Use `RtspClient.connect(config)` → `session.into_demux_receiver()` for this shape.
2. **H.264 elementary stream** — bare H.264 NAL units ride an RTP flow with a dynamic payload type (commonly PT=96). This recipe covers that shape.

---

## Python (primary)

```python
from tstrans.rtp import (
    RtspClient,
    RtspClientConfig,
    DigestAuth,
    H264DepayConfig,
    ParameterSetInjection,
)
from tstrans.mpegts import (
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    VideoCodec,
    Pts90khz,
)
import sys

# ── Config ────────────────────────────────────────────────────────────────
cfg = RtspClientConfig(
    "rtsp://cam.local/h264",
    auth=DigestAuth("admin", "secret"),   # BasicAuth | DigestAuth | None
)

# ── Optional: customise the depacketizer ─────────────────────────────────
# Most callers can leave this as None — connect_h264 fills in the negotiated
# payload type and any sprop-parameter-sets NALUs from the SDP automatically.
# Set ParameterSetInjection.NONE only when the camera reliably sends in-band
# SPS/PPS before every IDR and the redundant pre-injection is unwanted.
depay_cfg = None   # or H264DepayConfig(parameter_set_injection=ParameterSetInjection.BEFORE_IDR)

# ── Connect ───────────────────────────────────────────────────────────────
with RtspClient.connect_h264(cfg) as session:
    # into_h264_receiver() keeps the session wrapper open — session.pause()
    # and session.play() still work while the receiver is running.
    with session.into_h264_receiver() as rx:

        # ── Muxer ─────────────────────────────────────────────────────
        # Build a one-program MPEG-TS mux: program 1, PMT on PID 0x100,
        # H.264 video on PID 0x101. Add more streams (.add_video / .add_klv /
        # .add_audio) to the same builder if needed.
        prog = (
            MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x100)
            .add_video(0x101, VideoCodec.H264)
            .build()
        )
        mux = Muxer(MuxerConfigBuilder().add_program(prog).build())
        buf = bytearray(1316)          # 7 TS packets per pull chunk

        for au in rx:                  # recv_au(), GIL released while blocking
            # au.pts is a 90 kHz decode-order timestamp — same clock as
            # MPEG-TS PTS/PCR, no rescaling needed.
            #
            # B-frame caveat: if the encoder uses B-frames (PTS ≠ DTS),
            # au.pts reflects decode order. Low-latency live cameras almost
            # never use B-frames — PTS == DTS is the safe assumption.
            # For B-frame content use mux.push_video_to_with_dts() and
            # supply DTS from external encoder metadata.
            mux.push_video(
                au.annexb,
                pts=Pts90khz.from_raw(au.pts),
                key_frame=au.key_frame,
            )

            # KLV pairing slot: if this camera also sends telemetry on a
            # separate UDP feed or a second RTSP m-line, push it here with
            # the same pts. See the pair-klv-by-pts recipe for the pairing pattern.
            # mux.push_klv(klv_bytes, pts=Pts90khz.from_raw(au.pts), metadata_service_id=0x00)

            while (n := mux.pull(buf)) > 0:
                sys.stdout.buffer.write(buf[:n])   # or: srt_sender.send_bytes(bytes(buf[:n]))

        # ── Final stats ───────────────────────────────────────────────
        ds = rx.depay_stats()
        print(
            f"done: {ds.aus_emitted} AUs, {ds.aus_dropped} dropped, "
            f"{ds.seq_gaps} seq gaps",
            file=sys.stderr,
        )
```

---

## Key points

### `sprop-parameter-sets` are handled automatically

The SDP that the camera sends during DESCRIBE often contains the encoder's
SPS and PPS out-of-band in the `a=fmtp:N sprop-parameter-sets=<base64>,...`
attribute. `connect_h264` decodes these from base64 and stores them in the
`H264DepayConfig` stashed inside the session.

With `ParameterSetInjection.BEFORE_IDR` (the default), the depacketizer
prepends the stored SPS/PPS before every IDR frame — giving a decoder a
clean self-contained start point even when the camera only sends them in the
SDP and not in-band. Set `ParameterSetInjection.NONE` only when the camera
reliably sends SPS/PPS in-band before every IDR and the redundant prepend
is genuinely unwanted.

### Loss behavior and the `depay_stats` counters

RFC 6184 RTP loss is detected via sequence-number gaps. A gap poisons the
currently-accumulating access unit and the access unit the gap-carrying
packet joins. Poisoned AUs are silently dropped — no partial frames surface
from `recv_au()`. Use the stats counters to monitor loss rate:

```python
ds = rx.depay_stats()
# aus_emitted     — successfully reassembled AUs
# aus_dropped     — AUs discarded due to loss, oversize, or NALU errors
# aus_dropped_oversize — subset of dropped: exceeded max_au_bytes (8 MiB default)
# seq_gaps        — RTP sequence-number discontinuities observed
# parameter_set_updates — times the depacketizer's cached SPS/PPS were refreshed
```

The `rtp_stats()` counter tracks lower-level packet discard:

```python
rs = rx.rtp_stats()
# malformed_packets — datagrams with invalid RTP header, wrong PT, or empty payload
```

### Forcing TCP-interleaved (NAT / firewall)

When UDP is blocked (NAT, strict firewall), append `?transport=tcp` to the
RTSP URL. The session negotiates RFC 7826 §14 TCP interleaving and the
pump thread drains the TCP stream, forwarding RTP frames over an mpsc
channel to the receiver (on this H.264 path the interleaved RTCP frames are
drained and discarded):

```python
cfg = RtspClientConfig("rtsp://cam.local/h264?transport=tcp")
```

### Packetization mode 2 is not supported

Mode 2 (interleaved — STAP-B / MTAP16 / MTAP24 / FU-B with DON) is
rejected at SETUP time with `RtspError(UNSUPPORTED_TRANSPORT)`. Modes 0
(single-NAL) and 1 (non-interleaved FU-A / STAP-A) work with all cameras
tested. Almost all production cameras use mode 1.

---

## Rust twin

The full Rust example with per-step commentary is at
[`examples/receiving/recv_rtsp_h264.rs`](/examples/receiving/recv_rtsp_h264.rs).
Compile-only in CI (no live camera); run locally:

```bash
cargo run -p tst-examples --example recv_rtsp_h264 -- rtsp://cam.local/h264
```
