"""Python adapter for the cross-binding scenario harness (WS-5).

For each scenario in ``crates/tst-integration/tests/fixtures/scenarios/
scenarios.toml`` this test:

1. Feeds the committed input artifact through ``tstrans``.
2. Normalises the result into the binding-neutral golden envelope dict.
3. Asserts ``observed == committed_golden`` (full equality, order-sensitive).

Normalisation rules mirror the Rust adapter at
``crates/tst-integration/tests/rust_scenarios.rs`` exactly:

Video
  ``payload_sha256`` = sha256 of the concatenated RBSP bytes from every
  ``NalUnit.payload`` (H.264/H.265/H.266) or ``Obu.payload`` (AV1) obtained
  via the opt-in ``DemuxEvent.Video.parse()``.  This is the same operation
  as the Rust normaliser's ``video_payload_bytes()``.

stream_type
  The raw PMT stream_type byte from the first ``DemuxEvent.ProgramMap``
  that lists the PID, formatted as lowercase hex ``"0x1b"``.  Built into
  a ``pid → stream_type_str`` map before emitting media events — same
  approach as the Rust normaliser.

KLV set identity
  Detected from the first 13 bytes of ``DemuxEvent.Metadata.payload`` using the
  MISB ST 0601 UAS Datalink LS UL prefix.  Returns ``"st0601"`` or
  ``"unknown"``.

Subtitle
  ``DemuxEvent.Subtitle`` projects to ``{event:"subtitle", program, pid,
  stream_type, codec}`` where ``codec`` ∈ {"dvb_subtitle","dvb_teletext",
  "webvtt","cea708_standalone"}.  The mapping from the Python
  ``SubtitleCodec`` enum is exactly the Rust ``subtitle_codec_tag()`` table
  (all subtitle codecs carry PMT stream_type 0x06).

NonConformant diagnostics
  Under the DEFAULT (lenient) ``DemuxerConfig`` the demuxer does NOT raise —
  it surfaces ``DemuxEvent.NonConformant`` diagnostic events inline and
  continues.  Each one projects to ``{event:"error", code:"<STABLE CODE>"}``
  where the stable code is the ``NonConformantKind`` enum member NAME
  (e.g. ``PES_HEADER_MALFORMED``, ``MISSING_REQUIRED_PTS``).  These NAMEs are
  exactly the strings the Rust ``nonconformant_issue_code()`` returns, which
  in turn match the ``TST_NONCONFORMANT_CODE_*`` C constant base names.  The
  events are emitted in demuxer queue order, interleaved with media events —
  reproducing the golden's ordered sequence byte-for-byte.

Error mapping
  Any ``DemuxError`` raised by ``Demuxer.feed`` or ``Demuxer.flush`` maps
  to the umbrella public code ``"STRICT_REJECTION"`` regardless of the
  specific Python ``DemuxErrorKind``.  This is the binding-neutral umbrella
  code used by all adapters (see ``demux_error_code()`` in
  ``crates/tst-integration/src/scenarios/mod.rs``).

Roundtrip
  The Python muxer reproduces the exact same TS bytes as the Rust generator
  (verified byte-identical against the committed ``output.ts`` artifact AND
  sha256-equal to ``extensions.output_sha256``).  This proves the Python
  binding's muxer is deterministic and produces the same artefact.  Covers
  both ``video-roundtrip`` (video-only) and ``audio-klv-roundtrip``
  (video + AAC audio + synchronous KLV).

Lifecycle binding contracts
  ``drop-idempotence`` exercises the Python demuxer lifecycle (flush twice +
  ``del`` + a fresh instance that still works) and emits the
  ``DOUBLE_CLOSE_OK`` sentinel.  ``forged-handle`` is a C-ABI trust-boundary
  contract with no Python equivalent (PyO3 has no raw opaque handle), so the
  Python adapter does a STRUCTURAL-only assertion (the committed golden
  parses with ``code:"INVALID_HANDLE"`` + ``contract:"forged_handle"``) and
  defers the runtime teeth to the C adapter (Task 13) — mirroring how the
  Rust adapter scoped the raw-pointer-deref portion.

Unknown golden event tags
  If the committed golden contains an ``event`` tag that this normaliser
  does not recognise, the test fails loudly — never silently skips.
"""

from __future__ import annotations

import hashlib
import json
import sys
from pathlib import Path
from typing import Any

import pytest

from tstrans.exceptions import DemuxError
from tstrans.mpegts import (
    AudioCodec,
    DemuxEvent,
    Demuxer,
    DemuxerConfig,
    KlvStreamType,
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    NonConformantKind,
    Pts90khz,
    StrictMode,
    SubtitleCodec,
    VideoCodec,
)

from conftest import _TOML_AVAILABLE, require_scenario, scenarios_dir

# ── Helpers ───────────────────────────────────────────────────────────────────

_ST0601_UL_PREFIX: bytes = bytes([
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01,
    0x0E, 0x01, 0x03, 0x01, 0x01,
])


def _sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _klv_set_from_ul(payload: bytes) -> str:
    """Detect the MISB KLV set from the first 13 bytes of the raw payload.

    Returns ``"st0601"`` for the ST 0601 UAS Datalink LS UL, else
    ``"unknown"``.  Mirrors ``klv_set_from_ul()`` in the Rust normaliser.
    """
    if (
        len(payload) >= len(_ST0601_UL_PREFIX)
        and payload[: len(_ST0601_UL_PREFIX)] == _ST0601_UL_PREFIX
    ):
        return "st0601"
    return "unknown"


def _video_payload_bytes(ev: DemuxEvent.Video) -> bytes:  # type: ignore[name-defined]
    """Concatenate raw payload bytes from all NAL units / OBUs.

    H.264/H.265/H.266: concatenate ``NalUnit.payload`` (RBSP, Annex-B
    start codes already stripped by the demuxer — same bytes the Rust
    normaliser reads from ``NalUnit { payload, .. }``).
    AV1: concatenate ``Obu.payload``.

    Mirrors ``video_payload_bytes()`` in the Rust normaliser.
    """
    out = bytearray()
    for item in ev.parse():
        out.extend(bytes(item.payload))
    return bytes(out)


# Map the Python `SubtitleCodec` enum to the binding-neutral string tag used in
# the golden's `subtitle.codec` field.  Mirrors the Rust `subtitle_codec_tag()`
# table exactly (read both): DvbSubtitling→"dvb_subtitle",
# DvbTeletext→"dvb_teletext", WebVttInTs→"webvtt",
# Cea708Standalone→"cea708_standalone".
_SUBTITLE_CODEC_TAG: dict[SubtitleCodec, str] = {
    SubtitleCodec.DVB_SUBTITLING: "dvb_subtitle",
    SubtitleCodec.DVB_TELETEXT: "dvb_teletext",
    SubtitleCodec.WEBVTT_IN_TS: "webvtt",
    SubtitleCodec.CEA708_STANDALONE: "cea708_standalone",
}


def _nonconformant_code(ev: DemuxEvent.NonConformant) -> str:  # type: ignore[name-defined]
    """Map a NonConformant diagnostic event to its stable public string code.

    The Python `NonConformantKind` enum member NAME is an exact match to the
    Rust `nonconformant_issue_code()` stable code (and the
    `TST_NONCONFORMANT_CODE_*` C-constant base name) for EVERY member EXCEPT
    `STREAM_TYPE_MISMATCH` — that is the one PyO3-collapsed case, handled by the
    loud branch below.  Example of the exact path:
    `NonConformantKind.PES_HEADER_MALFORMED` → `"PES_HEADER_MALFORMED"`.
    """
    if ev.kind is NonConformantKind.STREAM_TYPE_MISMATCH:
        # PyO3 collapses Rust's two distinct stable codes
        # (STREAM_TYPE_MISMATCH_SYNC_ON_ASYNC_PID /
        # STREAM_TYPE_MISMATCH_ASYNC_ON_SYNC_PID) into this single member, so
        # the Python binding cannot derive which split code applies.  No current
        # golden exercises it; reproducing a split-variant golden requires a
        # tst-py binding change (re-expose the two codes), NOT an adapter-test
        # change — fail loud rather than emit the wrong "STREAM_TYPE_MISMATCH".
        pytest.fail(
            "NonConformantKind.STREAM_TYPE_MISMATCH is PyO3-collapsed: the "
            "Python binding folds Rust's two stable codes "
            "STREAM_TYPE_MISMATCH_SYNC_ON_ASYNC_PID / "
            "STREAM_TYPE_MISMATCH_ASYNC_ON_SYNC_PID into one member, so this "
            "adapter cannot derive the split code. Reproducing a split-variant "
            "golden requires a tst-py binding change (re-expose both codes), "
            "not an adapter-test change."
        )
    return ev.kind.name


# ── Normaliser ────────────────────────────────────────────────────────────────

def _demux_to_core_events(ts_bytes: bytes) -> list[dict[str, Any]]:
    """Feed *ts_bytes* through the tstrans Demuxer and return normalised
    ``CoreEvent``-shaped dicts.

    Rules (matching the Rust normaliser):
    - ``ProgramMap`` → build the ``pid → stream_type_hex`` map; skip from output.
    - ``Video``      → ``{"event":"video", ...}``
    - ``Audio``      → ``{"event":"audio", ...}``
    - ``Subtitle``   → ``{"event":"subtitle", ...}``
    - ``Klv``        → ``{"event":"klv", ...}``
    - ``UnknownSample`` → ``{"event":"unknown", "pid": ...}``
    - ``NonConformant`` → ``{"event":"error","code":"<STABLE CODE>"}`` (surfaced
      inline, in queue order, NOT skipped — mirrors the Rust normaliser which
      maps NonConformant to ``CoreEvent::Error``).
    - ``Discontinuity`` / ``ReconnectDiscontinuity`` → skipped (diagnostics).
    - ``DemuxError`` from ``feed`` → ``[{"event":"error","code":"STRICT_REJECTION"}]``.
    """
    demuxer = Demuxer()

    try:
        demuxer.feed(ts_bytes)
    except DemuxError:
        # Any DemuxError maps to the umbrella STRICT_REJECTION code — matches
        # ``demux_error_code()`` in the Rust normaliser which unconditionally
        # returns "STRICT_REJECTION" for every DemuxError variant.
        return [{"event": "error", "code": "STRICT_REJECTION"}]

    demuxer.flush()

    # Collect all raw events so we can build the stream_type map from
    # ProgramMap events before emitting media events — same two-pass
    # approach as the Rust normaliser.
    raw_events = list(demuxer)

    # pid → "0x1b"-style stream_type string, derived from ProgramMap events.
    stream_type_by_pid: dict[int, str] = {}
    for ev in raw_events:
        if isinstance(ev, DemuxEvent.ProgramMap):
            for prog in ev.programs:
                for si in prog.streams:
                    stream_type_by_pid[si.pid] = f"0x{si.stream_type:02x}"

    def stream_type_hex(pid: int, fallback_byte: int) -> str:
        return stream_type_by_pid.get(pid, f"0x{fallback_byte:02x}")

    events: list[dict[str, Any]] = []
    for ev in raw_events:
        if isinstance(ev, DemuxEvent.ProgramMap):
            pass  # topology — skip from output

        elif isinstance(ev, DemuxEvent.Video):
            raw = _video_payload_bytes(ev)
            events.append({
                "event": "video",
                "program": ev.stream.program_number,
                "pid": ev.stream.pid,
                "stream_type": stream_type_hex(ev.stream.pid, 0x1B),
                "pts": ev.pts.raw,
                "key": ev.random_access_indicator,
                "payload_sha256": _sha256_hex(raw),
            })

        elif isinstance(ev, DemuxEvent.Audio):
            # Raw-first: the Rust normaliser hashes the raw ``frames`` bytes
            # from SamplePayload::Audio (sha256_hex(&frames)). ``ev.raw`` is
            # exactly those bytes, so hash them directly.
            raw = bytes(ev.raw)
            events.append({
                "event": "audio",
                "program": ev.stream.program_number,
                "pid": ev.stream.pid,
                "stream_type": stream_type_hex(ev.stream.pid, 0x03),
                "pts": ev.pts.raw,
                "payload_sha256": _sha256_hex(raw),
            })

        elif isinstance(ev, DemuxEvent.Subtitle):
            # All subtitle codecs carry PMT stream_type 0x06; the binding-
            # neutral codec tag comes from the SubtitleCodec enum (same table
            # as the Rust normaliser's subtitle_codec_tag()).
            events.append({
                "event": "subtitle",
                "program": ev.stream.program_number,
                "pid": ev.stream.pid,
                "stream_type": stream_type_hex(ev.stream.pid, 0x06),
                "codec": _SUBTITLE_CODEC_TAG[ev.codec],
            })

        elif isinstance(ev, DemuxEvent.Metadata):
            events.append({
                "event": "klv",
                "program": ev.stream.program_number,
                "pid": ev.stream.pid,
                "stream_type": stream_type_hex(ev.stream.pid, 0x06),
                "set": _klv_set_from_ul(ev.payload),
            })

        elif isinstance(ev, DemuxEvent.UnknownSample):
            events.append({"event": "unknown", "pid": ev.stream.pid})

        elif isinstance(ev, DemuxEvent.NonConformant):
            # Lenient-mode diagnostic — surfaced inline as an error event with
            # the specific stable code, in queue order alongside media events.
            # The conformant Muxer emits zero NonConformant events, so the clean
            # demux scenarios are unaffected.
            events.append({
                "event": "error",
                "code": _nonconformant_code(ev),
            })

        elif isinstance(ev, (
            DemuxEvent.Discontinuity,
            DemuxEvent.ReconnectDiscontinuity,
        )):
            pass  # diagnostic — skip from output

        # No 'else' fall-through — if a new DemuxEvent subclass is added and
        # the normaliser doesn't handle it, the type-coverage gap becomes
        # visible via test failure on the golden comparison.

    return events


# ── Per-kind runners ──────────────────────────────────────────────────────────

def _run_demux(scenario_id: str, input_path: Path) -> list[dict[str, Any]]:
    """Run a ``kind=demux`` scenario and return the normalised core event list."""
    return _demux_to_core_events(input_path.read_bytes())


def _synthetic_h264_idr() -> bytes:
    """Reproduce the Rust generator's ``synthetic_h264_idr()``.

    4-byte Annex-B start code + 0x65 (IDR, nal_ref_idc=3) +
    15 deterministic filler bytes (0xA5 ^ i for i in 0..15).
    """
    hdr = bytes([0x00, 0x00, 0x00, 0x01, 0x65])
    body = bytes(0xA5 ^ i for i in range(15))
    return hdr + body


def _video_roundtrip_ts_bytes() -> bytes:
    """Python mirror of ``video_roundtrip_ts_bytes()`` in the Rust integration crate.

    Reproduces the same muxer recipe byte-for-byte:
      - program_number=1, pmt_pid=0x1000
      - video pid=0x1011, VideoCodec.H264
      - push_video(synthetic_h264_idr(), pts=0, key_frame=True)

    The Python Muxer is deterministic when given the same config and input
    sequence — verified byte-identical against the committed output.ts artifact.
    """
    prog = (
        MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x1000)
        .add_video(pid=0x1011, codec=VideoCodec.H264)
        .build()
    )
    cfg = MuxerConfigBuilder().add_program(prog).build()
    mux = Muxer(cfg)
    mux.push_video(_synthetic_h264_idr(), pts=Pts90khz.from_raw(0), key_frame=True)
    out = bytearray()
    buf = bytearray(1316)  # 7 × 188
    while True:
        n = mux.pull(buf)
        if n == 0:
            break
        out.extend(buf[:n])
    return bytes(out)


def _synthetic_adts_frame() -> bytes:
    """Reproduce the Rust generator's ``synthetic_adts_frame()`` byte-for-byte.

    7-byte ADTS header (MPEG-2 ID, no CRC, AAC-LC, sample_rate_index=4 →
    44100 Hz, channel_config=2 stereo, frame_length=15) + 8 deterministic
    payload bytes.
    """
    total_len = 15  # 7-byte header + 8 payload bytes
    sample_rate_index = 4  # 44100 Hz
    channel_config = 2  # stereo
    h = bytearray(7)
    h[0] = 0xFF
    h[1] = 0b1111_0001  # ID=MPEG-2, layer=00, protection_absent=1
    h[2] = (1 << 6) | ((sample_rate_index & 0xF) << 2) | ((channel_config >> 2) & 1)
    h[3] = ((channel_config & 0b11) << 6) | ((total_len >> 11) & 0b11)
    h[4] = (total_len >> 3) & 0xFF
    h[5] = ((total_len & 0b111) << 5) | 0b1_1111
    h[6] = 0b11_1111 << 2
    return bytes(h) + bytes([0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7])


def _minimal_st0601_ls() -> bytes:
    """Reproduce the Rust generator's ``minimal_st0601_ls()`` byte-for-byte.

    16-byte MISB ST 0601 UAS Datalink LS UL + BER short-form length 0.
    """
    return bytes([
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01,
        0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,  # UL bytes 1-16
        0x00,  # BER short-form length = 0
    ])


def _video_dts_roundtrip_ts_bytes() -> bytes:
    """Reproduce the ``video-dts-roundtrip`` mux recipe byte-for-byte.

    Single H.264 video stream (program 1, pmt_pid 0x1000, video pid 0x1011);
    one IDR pushed via ``push_video_to_with_dts`` with distinct PTS 9000 /
    DTS 6000 (forces ``PTS_DTS_flags='11'``). The committed ``output_sha256``
    is shared with the Rust/C/JVM adapters.
    """
    prog = (
        MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x1000)
        .add_video(pid=0x1011, codec=VideoCodec.H264)
        .build()
    )
    cfg = MuxerConfigBuilder().add_program(prog).build()
    mux = Muxer(cfg)
    handle = mux.video_stream_handle(0)
    assert handle is not None
    mux.push_video_to_with_dts(
        handle,
        _synthetic_h264_idr(),
        pts=Pts90khz.from_raw(9000),
        dts=Pts90khz.from_raw(6000),
        key_frame=True,
    )
    out = bytearray()
    buf = bytearray(1316)  # 7 × 188
    while True:
        n = mux.pull(buf)
        if n == 0:
            break
        out.extend(buf[:n])
    return bytes(out)


def _video_misp_roundtrip_ts_bytes() -> bytes:
    """Reproduce the ``video-misp-roundtrip`` mux recipe byte-for-byte.

    Single H.264 video stream (program 1, pmt_pid 0x1000, video pid 0x1011);
    one IDR pushed via ``push_video_misp_to`` with
    ``MispTimestamp.micros(0x0005_F5E1_0000_0001, 0x1F)`` and PTS 9000 ticks.
    The committed ``output_sha256`` is shared with the Rust/C/JVM adapters.
    """
    from tstrans.codec import MispTimestamp

    prog = (
        MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x1000)
        .add_video(pid=0x1011, codec=VideoCodec.H264)
        .build()
    )
    cfg = MuxerConfigBuilder().add_program(prog).build()
    mux = Muxer(cfg)
    handle = mux.video_stream_handle(0)
    assert handle is not None
    misp = MispTimestamp.micros(0x0005_F5E1_0000_0001, 0x1F)
    mux.push_video_misp_to(
        handle,
        _synthetic_h264_idr(),
        pts=Pts90khz.from_raw(9000),
        key_frame=True,
        misp=misp,
    )
    out = bytearray()
    buf = bytearray(1316)  # 7 × 188
    while True:
        n = mux.pull(buf)
        if n == 0:
            break
        out.extend(buf[:n])
    return bytes(out)


def _audio_klv_roundtrip_ts_bytes() -> bytes:
    """Python mirror of ``audio_klv_roundtrip_ts_bytes()`` in the Rust crate.

    Reproduces the same muxer recipe byte-for-byte:
      - program_number=1, pmt_pid=0x1000
      - video pid=0x1011 H.264; audio pid=0x1021 AAC;
        KLV pid=0x1031 SYNCHRONOUS_METADATA (carries_pts=True)
      - PTS 0 throughout
      - push_video(synthetic_h264_idr(), pts=0, key=True)
      - push_audio(synthetic_adts_frame(), pts=0)
      - push_klv(minimal_st0601_ls(), pts=0)  # muxer auto-wraps AU cell header

    The committed output_sha256 golden is locked to this exact mux output.
    """
    prog = (
        MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x1000)
        .add_video(pid=0x1011, codec=VideoCodec.H264)
        .add_audio(0x1021, AudioCodec.AAC)
        # SynchronousMetadata requires carries_pts=True.
        .add_klv(0x1031, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )
    cfg = MuxerConfigBuilder().add_program(prog).build()
    mux = Muxer(cfg)
    pts = Pts90khz.from_raw(0)
    mux.push_video(_synthetic_h264_idr(), pts=pts, key_frame=True)
    mux.push_audio(_synthetic_adts_frame(), pts=pts)
    # Pass raw KLV LS bytes — muxer auto-wraps in the AU cell header.
    mux.push_klv(_minimal_st0601_ls(), pts=pts)
    out = bytearray()
    buf = bytearray(1316)  # 7 × 188
    while True:
        n = mux.pull(buf)
        if n == 0:
            break
        out.extend(buf[:n])
    return bytes(out)


def _run_roundtrip(
    scenario_id: str,
    input_path: Path,
    golden_extensions: dict[str, Any],
) -> list[dict[str, Any]]:
    """Run a ``kind=roundtrip`` scenario.

    Reproduces the mux recipe via the Python muxer, asserts byte-identity
    against the committed ``output.ts``, and asserts the sha256 matches
    ``extensions.output_sha256`` from the golden.

    Returns ``[]`` — roundtrip scenarios carry no media events.
    """
    if scenario_id == "video-roundtrip":
        fresh = _video_roundtrip_ts_bytes()
    elif scenario_id == "audio-klv-roundtrip":
        fresh = _audio_klv_roundtrip_ts_bytes()
    elif scenario_id == "video-dts-roundtrip":
        fresh = _video_dts_roundtrip_ts_bytes()
    elif scenario_id == "video-misp-roundtrip":
        fresh = _video_misp_roundtrip_ts_bytes()
    else:
        pytest.fail(f"unknown roundtrip scenario id: {scenario_id!r}")

    committed = input_path.read_bytes()  # output.ts IS the input for roundtrip
    assert fresh == committed, (
        f"[{scenario_id}] Python muxer output differs from committed output.ts "
        f"({len(fresh)} bytes vs {len(committed)} bytes)"
    )

    expected_sha256 = golden_extensions.get("output_sha256", "")
    actual_sha256 = _sha256_hex(fresh)
    assert actual_sha256 == expected_sha256, (
        f"[{scenario_id}] sha256 mismatch: got {actual_sha256!r}, "
        f"expected {expected_sha256!r}"
    )

    # video-misp-roundtrip: demux the produced TS, extract the MISP timestamp,
    # and assert it matches the golden's committed misp_* fields.
    if scenario_id == "video-misp-roundtrip":
        from tstrans.codec import MispTimestamp, MispTimeKind, extract_misp_timestamp

        d = Demuxer(DemuxerConfig())
        d.feed(fresh)
        d.flush()
        video_events = [e for e in d if isinstance(e, DemuxEvent.Video)]
        assert video_events, f"[{scenario_id}] no video event after demux"

        ts_out = extract_misp_timestamp(video_events[0].raw, VideoCodec.H264)
        assert ts_out is not None, f"[{scenario_id}] extract_misp_timestamp returned None"

        golden_kind = golden_extensions.get("misp_kind")
        golden_status = golden_extensions.get("misp_time_status")
        golden_value = golden_extensions.get("misp_value")

        expected_kind = MispTimeKind.MICRO if golden_kind == 0 else MispTimeKind.NANO
        assert ts_out.kind == expected_kind, (
            f"[{scenario_id}] misp kind mismatch: got {ts_out.kind!r}, "
            f"expected kind={golden_kind}"
        )
        assert ts_out.time_status == golden_status, (
            f"[{scenario_id}] misp time_status mismatch: "
            f"got {ts_out.time_status}, expected {golden_status}"
        )
        assert ts_out.value == golden_value, (
            f"[{scenario_id}] misp value mismatch: "
            f"got {ts_out.value}, expected {golden_value}"
        )

    return []


def _run_strict_rejection(scenario_id: str, input_bytes: bytes) -> list[dict[str, Any]]:
    """Feed garbage bytes (no 0x47 sync) to the default-config demuxer; assert a
    DemuxError is raised and maps to the umbrella ``STRICT_REJECTION`` code.

    Also asserts drop idempotence: the demuxer can be destroyed after an error
    without crashing.  Shared by ``strict-rejection`` (8192 × 0xFF garbage).
    """
    demuxer = Demuxer()
    raised = False
    try:
        demuxer.feed(input_bytes)
        # If feed somehow doesn't raise (extremely unlikely for 8192 × 0xFF
        # which has no sync byte), flush and drain to give the demuxer a chance
        # to surface the error.
        demuxer.flush()
        list(demuxer)
    except DemuxError:
        raised = True

    # Idempotent drop: destroy the demuxer after an error — must not panic.
    del demuxer

    if not raised:
        pytest.fail(
            f"[{scenario_id}] expected DemuxError on garbage input, got none"
        )

    return [{"event": "error", "code": "STRICT_REJECTION"}]


def _run_strict_psi_rejection(
    scenario_id: str, input_bytes: bytes
) -> list[dict[str, Any]]:
    """Feed a TS with a valid PAT + a PMT with a corrupted CRC to a
    ``StrictMode.FULL`` demuxer; assert the ``PsiChecksumMismatch`` escalates to
    a DemuxError that maps to the umbrella ``STRICT_REJECTION`` code.

    Shared by ``malformed-psi-strict`` and ``exception-kind-stability`` — both
    carry the identical malformed-PSI input and both must surface the SAME
    stable public code regardless of the binding (exception-kind stability).
    The default (lenient) demuxer would NOT raise here — it would emit a
    NonConformant event instead — so FULL strict mode is required, mirroring the
    Rust adapter's ``Demuxer::with_config(DemuxerConfig::builder().strict(StrictMode::Full).build())``.
    """
    demuxer = Demuxer(DemuxerConfig(strict_mode=StrictMode.FULL))
    raised = False
    try:
        demuxer.feed(input_bytes)
        demuxer.flush()
        list(demuxer)
    except DemuxError:
        raised = True
    del demuxer
    if not raised:
        pytest.fail(
            f"[{scenario_id}] expected DemuxError (PsiChecksumMismatch under "
            f"StrictMode.FULL) on corrupted-PMT-CRC input, got none"
        )
    return [{"event": "error", "code": "STRICT_REJECTION"}]


def _run_drop_idempotence(
    scenario_id: str, input_bytes: bytes
) -> list[dict[str, Any]]:
    """Exercise the Python demuxer lifecycle and assert double-close safety.

    The native ``tstrans.mpegts.Demuxer`` has no explicit ``close()`` method
    and is not a context manager — its lifecycle is governed by ``flush()`` (the
    explicit end-of-stream finaliser) and Python GC / ``del``.  "Double close"
    is therefore expressed exactly as the Rust adapter does it: feed the minimal
    valid TS, ``flush()`` TWICE (the second flush must be a safe no-op), then
    ``del`` the instance, then construct a FRESH demuxer that still works.  None
    of this may raise or crash.  On success the contract emits the sentinel
    ``DOUBLE_CLOSE_OK``.
    """
    demuxer = Demuxer()
    demuxer.feed(input_bytes)
    demuxer.flush()
    demuxer.flush()  # second "close" — must be a safe no-op, no raise.
    list(demuxer)  # drain — must not crash after a double-flush.
    del demuxer  # explicit drop — must not crash.

    # A fresh instance still works after the prior was finalised + dropped.
    fresh = Demuxer()
    fresh.feed(input_bytes)
    fresh.flush()
    list(fresh)
    del fresh

    return [{"event": "error", "code": "DOUBLE_CLOSE_OK"}]


def _run_forged_handle(
    scenario_id: str, input_bytes: bytes
) -> list[dict[str, Any]]:
    """Forged-handle trust-boundary contract — STRUCTURAL-only on the Python side.

    ``forged-handle`` is fundamentally a C-ABI contract: a forged opaque
    ``tst_*`` handle must be rejected, not dereferenced.  The PyO3 binding has no
    raw opaque-handle concept — stream handles never cross the Python boundary as
    integers a caller can forge — so there is no runtime guard to exercise here.

    The Python adapter therefore performs a STRUCTURAL assertion only: it
    confirms the committed input artifact is the expected 4-byte LE forged value
    and emits the ``INVALID_HANDLE`` sentinel that the committed golden carries.
    The runtime teeth (a forged opaque pointer must not be dereferenced) are
    deferred to the C adapter (Task 13) — mirroring exactly how the Rust adapter
    scoped the raw-pointer-deref portion as a C-adapter concern.
    """
    # The committed artifact is the forged handle value as 4 little-endian bytes
    # (FORGED_HANDLE_RAW = 0x100 — one bit past the canonical 0xFF mask). Confirm
    # the cross-binding input is single-sourced and exactly what we assert on.
    if len(input_bytes) != 4:
        pytest.fail(
            f"[{scenario_id}] forged-handle input must be a 4-byte LE u32, "
            f"got {len(input_bytes)} bytes"
        )
    forged = int.from_bytes(input_bytes, "little")
    if forged != 0x100:
        pytest.fail(
            f"[{scenario_id}] forged-handle artifact value drifted: "
            f"got {forged:#x}, expected 0x100"
        )

    return [{"event": "error", "code": "INVALID_HANDLE"}]


def _run_cancelled_recv_kind(scenario_id: str) -> list[dict[str, Any]]:
    """One cancelled plain SRT receive; the observed kind name is the golden's
    code (Arc 2 spec 3.3 / 6).

    Listener-mode ``Receiver`` opened on a daemon thread (``from_url`` blocks in
    the one-shot accept), caller-mode ``Sender`` that never sends, the receive
    parked on a daemon thread, and the cancel fired from the test thread.  Every
    wait is a 10 s FAILURE bound, never a duration assertion; a rescue frame
    from the still-connected peer unparks the daemon thread if the cancel failed,
    so it never sits in a native read at interpreter exit.
    """
    import socket
    import threading
    import time

    import tstrans
    from tstrans.exceptions import SrtError

    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    listener_url = f"srt://:{port}?mode=listener"
    caller_url = f"srt://127.0.0.1:{port}?mode=caller"

    box: list[Any] = []
    acc_err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            box.append(tstrans.srt.Receiver.from_url(listener_url))
        except BaseException as exc:  # noqa: BLE001
            acc_err.append(exc)

    acc = threading.Thread(target=accept_worker, daemon=True, name="accept")
    acc.start()
    time.sleep(0.2)
    sender = tstrans.srt.Sender.from_url(caller_url)
    acc.join(timeout=10.0)
    assert not acc.is_alive() and not acc_err, f"[{scenario_id}] accept failed: {acc_err!r}"
    receiver = box[0]
    handle = receiver.cancel_handle()

    outcome: dict[str, Any] = {}

    def recv_worker() -> None:
        try:
            receiver.recv_bytes(max_len=1316)
            outcome["end"] = "ok"
        except SrtError as exc:
            outcome["kind"] = exc.kind.name
            outcome["detail"] = exc.args[0]
        except BaseException as exc:  # noqa: BLE001
            outcome["end"] = repr(exc)

    w = threading.Thread(target=recv_worker, daemon=True, name="parked-recv")
    w.start()
    time.sleep(0.3)
    handle.cancel()
    w.join(timeout=10.0)
    if w.is_alive():
        # Rescue: one frame from the still-connected peer unparks the daemon
        # thread so it does not sit in a native read at interpreter exit.
        sender.send_bytes(b"\x47\x1f\xff\x10" + b"\xff" * 184)
        w.join(timeout=5.0)
        pytest.fail(f"[{scenario_id}] cancel() did not end the parked recv within 10 s")
    sender.close()
    receiver.close()
    assert "kind" in outcome, f"[{scenario_id}] recv ended without SrtError: {outcome}"
    assert outcome["detail"] == "cancelled from another thread", (
        f"[{scenario_id}] detail mismatch: {outcome}"
    )
    return [{"event": "error", "code": outcome["kind"]}]


def _run_binding_contract(
    scenario_id: str,
    input_path: Path,
) -> list[dict[str, Any]]:
    """Run a ``kind=binding_contract`` scenario by dispatching on the id.

    Each contract exercises its nearest honest Python guarantee and emits the
    sentinel/umbrella code its committed golden carries.  See the per-runner
    docstrings for the exact mechanism and what (if anything) is deferred to the
    C adapter (Task 13).
    """
    input_bytes = input_path.read_bytes()

    if scenario_id == "strict-rejection":
        return _run_strict_rejection(scenario_id, input_bytes)
    if scenario_id in ("malformed-psi-strict", "exception-kind-stability"):
        return _run_strict_psi_rejection(scenario_id, input_bytes)
    if scenario_id == "drop-idempotence":
        return _run_drop_idempotence(scenario_id, input_bytes)
    if scenario_id == "forged-handle":
        return _run_forged_handle(scenario_id, input_bytes)
    if scenario_id == "cancelled-recv-kind":
        return _run_cancelled_recv_kind(scenario_id)

    pytest.fail(f"unknown binding_contract scenario id: {scenario_id!r}")


# ── Golden validation ─────────────────────────────────────────────────────────

_KNOWN_EVENT_TAGS = frozenset(
    {"video", "audio", "subtitle", "klv", "unknown", "error"}
)


def _validate_golden_tags(golden: dict[str, Any], scenario_id: str) -> None:
    """Fail loudly if the committed golden contains an unrecognised event tag.

    An older consumer encountering an unknown tag must never silently skip —
    it must fail so the adapter author knows it needs updating.  Mirrors the
    Rust ``#[serde(deny_unknown_fields)]`` / no ``#[serde(other)]`` stance.
    """
    for entry in golden.get("core", []):
        tag = entry.get("event", "<missing>")
        if tag not in _KNOWN_EVENT_TAGS:
            pytest.fail(
                f"[{scenario_id}] committed golden contains unrecognised event "
                f"tag {tag!r}; this Python adapter must be updated to handle it."
            )


# ── Parametrised test ─────────────────────────────────────────────────────────

def _scenario_ids() -> list[str]:
    """Scenario ids for parametrization. NEVER returns an empty list silently:
    on any load failure it returns a single sentinel id so the parametrised
    test runs and fails loudly with the underlying reason (see the test body).

    Deliberately does NOT delegate to conftest's ``_load_manifest``: that helper
    calls ``pytest.skip`` on failure, which would silently empty the
    parametrization rather than emit a loud sentinel."""
    if not _TOML_AVAILABLE:
        return ["__error__:toml-parser-unavailable"]
    manifest_path = scenarios_dir() / "scenarios.toml"
    if not manifest_path.is_file():
        return [f"__error__:manifest-not-found:{manifest_path}"]
    try:
        if sys.version_info >= (3, 11):
            import tomllib as _toml
        else:
            import tomli as _toml  # type: ignore[import-not-found]
        with open(manifest_path, "rb") as fh:
            data = _toml.load(fh)
        ids = [e["id"] for e in data.get("scenario", [])]
    except Exception as exc:  # noqa: BLE001 — surfaced as a loud test failure below
        return [f"__error__:manifest-parse-failed:{exc!r}"]
    return ids if ids else ["__error__:manifest-has-zero-scenarios"]


@pytest.mark.parametrize("scenario_id", _scenario_ids())
def test_scenario_matches_committed_golden(scenario_id: str) -> None:
    """For each scenario in scenarios.toml, run the Python adapter and assert
    the result equals the committed golden.json."""
    if scenario_id.startswith("__error__:"):
        pytest.fail(
            "scenario collection failed: "
            + scenario_id.removeprefix("__error__:")
            + " — the Python cross-binding contract suite collected no real "
            "scenarios. Fix the manifest/TOML parser; do not let this pass."
        )
    manifest_entry, sdir = require_scenario(scenario_id)
    kind = manifest_entry["kind"]
    input_rel = Path(manifest_entry["input"])
    golden_rel = Path(manifest_entry["golden"])

    sd = scenarios_dir()
    input_path = (sd / input_rel).resolve()
    golden_path = (sd / golden_rel).resolve()

    if not input_path.is_file():
        pytest.skip(f"input artifact missing: {input_path}")
    if not golden_path.is_file():
        pytest.skip(f"golden missing: {golden_path}")

    with open(golden_path) as fh:
        committed = json.load(fh)

    _validate_golden_tags(committed, scenario_id)

    if kind == "demux":
        observed_core = _run_demux(scenario_id, input_path)
    elif kind == "roundtrip":
        observed_core = _run_roundtrip(
            scenario_id,
            input_path,
            committed.get("extensions") or {},
        )
    elif kind == "binding_contract":
        observed_core = _run_binding_contract(scenario_id, input_path)
    else:
        pytest.fail(
            f"unknown scenario kind {kind!r} for scenario {scenario_id!r}"
        )

    observed = {
        "schema_version": committed["schema_version"],
        "lossy": committed["lossy"],
        "core": observed_core,
        "extensions": committed.get("extensions"),
    }

    assert observed == committed, (
        f"scenario '{scenario_id}': Python adapter output differs from "
        f"committed golden.\n"
        f"  observed_core: {json.dumps(observed_core, indent=2)}\n"
        f"  committed_core: {json.dumps(committed['core'], indent=2)}"
    )
