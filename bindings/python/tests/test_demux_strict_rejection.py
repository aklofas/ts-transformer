"""Audit-2 #8 — DemuxError raised by StrictMode policy must surface as
STRICT_REJECTION, not INTERNAL.

The fixture builds a TS where the PMT declares the KLV PID as
PRIVATE_DATA (stream_type 0x06, async), but the PES payload is a
sync-shaped H.222.0 §2.12.4.2 Metadata_AU_cell-wrapped KLV record.
The demuxer's linkage builder sees KlvShape::SyncAuCell on an async-
declared PID and emits NonConformantIssue::StreamTypeMismatchSyncOnAsyncPid.
With StrictMode.FULL that non-conformance escalates to
DemuxError::StrictRejection.

This mirrors crates/tst-core/tests/mpegts_demux_strict.rs
`strict_full_rejects_stream_type_mismatch`.
"""

import pytest

from tstrans.exceptions import DemuxError, DemuxErrorKind
from tstrans.mpegts import (
    DemuxerConfig,
    Demuxer,
    KlvStreamType,
    Muxer,
    MuxerConfig,
    MuxerProgramConfigBuilder,
    Pts90khz,
    StrictMode,
    VideoCodec,
)

# ── Synthetic AU-cell wrapper ──────────────────────────────────────────────

# Minimal ST 0601 UAS Datalink LS: 16-byte UL + 1-byte zero BER length.
# No value fields — just the UL + length 0x00. The demuxer only needs to
# recognise the AU-cell shape; it does not attempt to decode the inner KLV.
_BARE_KLV_LS: bytes = bytes([
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01,
    0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
    0x00,
])


def _build_au_cell(inner: bytes) -> bytes:
    """Build a 5-byte H.222.0 §2.12.4.2 Metadata_AU_cell header + inner bytes.

    Header layout (5 bytes):
      byte 0: metadata_service_id = 0x00
      byte 1: sequence_number = 0x00
      byte 2: cfi(2) | decoder_config_flag(1) | random_access_indicator(1) |
               reserved(4).
               reserved bits read as all ones (H.222.0 V9 Table 2-156; a
               nibble other than 1111 is not a cell header).
               Complete(0b11) + no decoder_config + random_access + reserved
               1111 → 0b1101_1111 = 0xDF
      byte 3: AU_cell_data_length_high = (len >> 8) & 0xFF
      byte 4: AU_cell_data_length_low  = len & 0xFF
    """
    length = len(inner)
    header = bytes([
        0x00,                       # metadata_service_id
        0x00,                       # sequence_number
        0xDF,                       # cfi=Complete(3<<6), random_access=1, reserved=1111
        (length >> 8) & 0xFF,       # AU_cell_data_length high
        length & 0xFF,              # AU_cell_data_length low
    ])
    return header + inner


def _build_strict_rejection_ts() -> bytes:
    """Build a TS that StrictMode.FULL will reject.

    PMT declares KLV PID as PRIVATE_DATA (async, stream_type 0x06), but
    the payload pushed is a sync-shaped Metadata_AU_cell.  The demuxer
    classifies the payload as KlvShape::SyncAuCell and emits
    NonConformantIssue::StreamTypeMismatchSyncOnAsyncPid, which strict
    mode escalates to DemuxError::StrictRejection.
    """
    prog = (
        MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x1000)
        .add_video(0x100, VideoCodec.H264)
        # PRIVATE_DATA = async (stream_type 0x06); payload passes through unchanged.
        .add_klv(0x101, KlvStreamType.PRIVATE_DATA, carries_pts=True)
        .build()
    )
    cfg = MuxerConfig.builder().add_program(prog).build()
    mux = Muxer(cfg)

    # Push a video frame so the PMT gets written before the KLV PES.
    nal_aud = b"\x00\x00\x00\x01\x09\x10"
    mux.push_video(nal_aud, pts=Pts90khz.from_raw(0), key_frame=True)

    # Push a sync-shaped AU-cell-wrapped KLV payload via the PRIVATE_DATA
    # stream.  PrivateData streams pass the payload through as-is, so the
    # wire form has KlvShape::SyncAuCell while the PMT says async.
    wrapped = _build_au_cell(_BARE_KLV_LS)
    mux.push_klv(wrapped, pts=Pts90khz.from_raw(0))

    # Drain all TS packets.
    buf = bytearray(mux.pending_packets() * 188)
    n = mux.pull(buf)
    return bytes(buf[:n])


# ── Tests ──────────────────────────────────────────────────────────────────


def test_demux_error_kind_has_strict_rejection_variant() -> None:
    """Audit-2 #8 pre-check — STRICT_REJECTION variant must exist in enum."""
    assert DemuxErrorKind.STRICT_REJECTION is not None
    assert DemuxErrorKind.STRICT_REJECTION.value == "strict_rejection"


def test_strict_mode_rejection_is_strict_rejection_not_internal() -> None:
    """Audit-2 #8 — DemuxError raised by StrictMode.FULL policy must carry
    DemuxErrorKind.STRICT_REJECTION, not INTERNAL."""
    bad_ts = _build_strict_rejection_ts()

    dx = Demuxer(DemuxerConfig(strict_mode=StrictMode.FULL))
    with pytest.raises(DemuxError) as ei:
        dx.feed(bad_ts)
        dx.flush()
        list(dx)
    assert ei.value.kind is DemuxErrorKind.STRICT_REJECTION, (
        f"expected STRICT_REJECTION, got {ei.value.kind!r}"
    )


def test_strict_mode_off_does_not_raise() -> None:
    """Regression: StrictMode.OFF must not raise on the same TS."""
    bad_ts = _build_strict_rejection_ts()
    dx = Demuxer(DemuxerConfig(strict_mode=StrictMode.OFF))
    # Should not raise — just surfaces a NonConformant event.
    dx.feed(bad_ts)
    dx.flush()
    list(dx)
