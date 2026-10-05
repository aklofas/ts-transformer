# Decode video frames in-memory with PyAV (Python)

> **When to use this:** You're demuxing a `.ts` with `tstrans` (for KLV, timing, or
> transmux) and also want the decoded frames — without re-opening the file in OpenCV
> or `av.open()` and time-aligning two separate readers. Optionally you want to
> process the elementary stream (filter NAL units, decode only keyframes) before the
> decoder sees it.

> **Note:** This is one of the few **Python** recipes — it integrates
> [PyAV](https://pyav.org) (the FFmpeg Python binding: `pip install av`), which is
> *not* a `tstrans` dependency. Most of the cookbook is Rust.

> **Related:**
> - [languages/python.md](/docs/languages/python.md) — the `tstrans` Python surface
> - [guides/mpegts-demux.md](/docs/guides/mpegts-demux.md) — raw-first `Video` events + opt-in `.parse()`
> - [Reconstitute Annex B parameter sets for decoder replay](/docs/cookbook/codecs/reconstitute-annex-b.md) — Annex-B reconstruction (Rust)

Under the raw-first model, `DemuxEvent.Video.raw` is the exact encoded Annex-B access
unit. Feed those bytes to a PyAV decoder `CodecContext` — no container, no file — and
you get decoded frames from the **same demux pass** that yields your KLV, already
PTS-aligned. That replaces the common two-reader setup (`tstrans` for metadata +
OpenCV/`av.open()` for frames, then align by timestamp).

Real captures need two things a naive "feed every AU" loop misses, so the pattern below
handles both:

1. **Start at the first keyframe.** Captures often begin mid-GOP — the first AUs are
   P-slices that reference SPS/PPS the decoder hasn't seen, so feeding them gives
   `non-existing PPS 0` errors. Skip until the first `ev.random_access_indicator` (the
   IDR carries the in-band SPS/PPS), exactly as OpenCV does internally.
2. **Carry the PES PTS.** H.264 may use B-frames (decode order ≠ display order). Put
   each AU's `ev.pts` on the packet so PyAV emits frames in **display order** with
   correct `frame.pts`.

## Decode straight from `ev.raw`

```python
from fractions import Fraction
import av
from tstrans import io as tio, klv
from tstrans.mpegts import DemuxEvent, VideoCodec

dec = av.codec.CodecContext.create("h264", "r")   # "hevc"=H.265, "av1"=AV1, "vvc"=H.266 (exp.)
TB = Fraction(1, 90000)                            # MPEG-TS 90 kHz clock
started = False

for ev in tio.parse_file("input.ts"):
    if isinstance(ev, DemuxEvent.Video):
        if not started:
            if not ev.random_access_indicator:     # skip pre-keyframe AUs (mid-GOP start)
                continue
            started = True
        pkt = av.Packet(bytes(ev.raw))             # ev.raw IS the encoded Annex-B AU
        pkt.pts = ev.pts.raw                        # 90 kHz PES PTS -> correct display order
        pkt.time_base = TB
        for frame in dec.decode(pkt):
            img = frame.to_ndarray(format="bgr24")  # HxWx3 uint8, OpenCV-compatible
            t_ms = frame.pts * 1000 / 90000          # true presentation timestamp (ms)
    elif isinstance(ev, DemuxEvent.Metadata):
        ls = klv.decode_uas_datalink(ev.payload)     # PTS-aligned to the frames above

for frame in dec.decode(None):                       # flush the decoder reorder buffer at EOF
    img = frame.to_ndarray(format="bgr24")
```

## Decode only a time window (gapless)

`tstrans` has no seek — you always demux forward from the start of the file. For a long
capture that's wasteful if you only want a short window: a naive loop decodes the entire
leading portion just to reach it. The fix (battle-tested on real captures) is to **buffer
the current GOP** — reset the buffer on every keyframe — and only start *decoding* once
you reach the window, replaying the buffered GOP first. The window then begins from a real
IDR (gapless, no `non-existing PPS`) without decoding everything before it.

```python
from fractions import Fraction
import av
from tstrans import io as tio
from tstrans.mpegts import DemuxEvent, VideoCodec

_PYAV = {VideoCodec.H264: "h264", VideoCodec.H265: "hevc", VideoCodec.H266: "vvc"}  # H.26x Annex-B
TB = Fraction(1, 90000)

def decode_window(path, start_s, dur_s):
    """Yield (t_ms, frame) for frames in [start_s, start_s+dur_s].

    Decoding begins at the keyframe at/before the window, so the slice is gapless.
    `t_ms` is stream-relative (ms from the first video AU). AV1 (OBU carriage, not
    Annex-B) is out of scope here — see the straight-decode section above.
    """
    start_ms, end_ms = start_s * 1000.0, (start_s + dur_s) * 1000.0
    first = dec = None
    gop, decoding = [], False

    def emit(frame):
        if frame.pts is None:
            return None
        rel = (frame.pts - first) * 1000.0 / 90000.0
        if rel < start_ms:
            return None
        if rel > end_ms:
            return "stop"
        return rel

    def decode_au(ev):
        pkt = av.Packet(bytes(ev.raw))
        pkt.pts = ev.pts.raw
        pkt.time_base = TB
        return dec.decode(pkt)

    for ev in tio.parse_file(path):
        if not isinstance(ev, DemuxEvent.Video):
            continue
        if first is None:
            first = ev.pts.raw
        if dec is None:
            dec = av.codec.CodecContext.create(_PYAV[ev.codec], "r")
        rel = (ev.pts.raw - first) * 1000.0 / 90000.0

        if not decoding:
            if ev.random_access_indicator:      # new GOP — restart the buffer here
                gop = [ev]
            elif gop:
                gop.append(ev)
            if rel >= start_ms and gop:         # reached the window: replay its GOP first
                decoding = True
                for buffered in gop:
                    for frame in decode_au(buffered):
                        r = emit(frame)
                        if r == "stop":
                            return
                        if r is not None:
                            yield r, frame
                gop = None
            continue

        if rel > end_ms + 2000:                 # slack for B-frame reorder, then stop demuxing
            break
        for frame in decode_au(ev):
            r = emit(frame)
            if r == "stop":
                return
            if r is not None:
                yield r, frame

    if dec is not None and decoding:            # flush the reorder buffer at the tail
        for frame in dec.decode(None):
            r = emit(frame)
            if r == "stop":
                return
            if r is not None:
                yield r, frame

# e.g. the 10 s starting at t=30 s, as grayscale ndarrays:
for t_ms, frame in decode_window("input.ts", start_s=30, dur_s=10):
    gray = frame.to_ndarray(format="gray")      # HxW uint8 luma plane
```

## Process the elementary stream first (parsed NAL units)

To filter or modify the stream before decoding, use the opt-in parse (`ev.parse()`),
manipulate the NAL list, then reconstruct Annex-B. `parse()` strips each NAL's 1-byte
header (H.264), so re-add it when rebuilding:

```python
def nal_to_annexb_h264(n):
    hdr = ((n.ref_idc & 0x3) << 5) | (n.nal_type & 0x1f)   # the byte parse() stripped
    return b"\x00\x00\x00\x01" + bytes([hdr]) + bytes(n.payload)

def au_from_nals(nals):
    return b"".join(nal_to_annexb_h264(n) for n in nals)

dec = av.codec.CodecContext.create("h264", "r")
started = False
for ev in tio.parse_file("input.ts"):
    if not isinstance(ev, DemuxEvent.Video):
        continue
    if not started:
        if not ev.random_access_indicator:
            continue
        started = True
    nals = ev.parse()
    nals = [n for n in nals if n.nal_type != 6]      # e.g. drop SEI (type 6) before decode
    pkt = av.Packet(au_from_nals(nals))
    pkt.pts = ev.pts.raw
    pkt.time_base = Fraction(1, 90000)
    for frame in dec.decode(pkt):
        img = frame.to_ndarray(format="bgr24")
```

**Keyframe-only decode** — gate on the random-access flag (each IDR carries in-band
SPS/PPS, so it decodes standalone):

```python
for ev in tio.parse_file("input.ts"):
    if isinstance(ev, DemuxEvent.Video) and ev.random_access_indicator:
        pkt = av.Packet(bytes(ev.raw))
        pkt.pts = ev.pts.raw
        pkt.time_base = Fraction(1, 90000)
        for frame in dec.decode(pkt):
            ...   # one frame per keyframe
```

NAL-type cheatsheet (H.264): `1` = non-IDR slice, `5` = IDR, `6` = SEI, `7` = SPS,
`8` = PPS. (H.265 / H.266 use a 2-byte NAL header — reconstruct both bytes from
`nal_type` + `layer_id` + `temporal_id_plus1`.)

## Gotchas

- **Start at the first keyframe.** Feeding pre-IDR P-slices errors with `non-existing
  PPS`. The `started` guard above skips to the first `random_access_indicator`.
- **Flush at EOF.** The decoder buffers (B-frame reorder) — flush with
  `dec.decode(None)` to get the tail frames.
- **Use `frame.pts`, not loop position.** Setting `pkt.pts = ev.pts.raw` makes PyAV
  emit frames in display order with a correct `frame.pts` (90 kHz). Pair frames to
  metadata by PTS, not by iteration index. (`Pts90khz` exposes `.raw` / `.ms` / `.seconds`.)
- **`ev.raw` vs `ev.parse()`.** For a straight decode, feed `ev.raw` (cheapest, exact).
  Use the parsed path only when you need to inspect / modify NAL units first — the
  reconstructed bytes decode identically but are not byte-identical to `ev.raw`
  (start-code widths can differ).
- **Windowed decode needs the GOP, not just the window.** To render a time slice without
  decoding everything before it, buffer AUs from each keyframe and replay that GOP when you
  reach the window (see *Decode only a time window*) — feeding the decoder mid-GOP gives
  garbage or `non-existing PPS`.
- **Codec.** Match `ev.codec`: `h264` / `hevc` (H.265) / `av1`; H.266 (`vvc`) decode is
  experimental in current FFmpeg.

This recipe is Python-only (PyAV); there is no Rust `tst-examples` program for it. See
[languages/python.md](/docs/languages/python.md) for the full `tstrans` Python surface.
