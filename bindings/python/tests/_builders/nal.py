"""Minimal H.264 Annex-B byte fixtures shared by mux/demux tests."""

from __future__ import annotations


def nal_aud() -> bytes:
    """AUD (NAL type 9): 4-byte start code + header + one trailing byte -
    the smallest legal H.264 NAL the muxer accepts."""
    return b"\x00\x00\x00\x01\x09\xF0"


def minimal_h264_au() -> bytes:
    """AUD (type 9) + IDR (type 5), Annex-B. The raw-first demuxer does not
    parse the AU, so any bytes round-trip; mirrors the Rust fixture in
    `pairing_demuxer_round_trip.rs`."""
    return bytes(
        [0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC]
    )
