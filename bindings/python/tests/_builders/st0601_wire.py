"""Hand-built ST 0601 wire helpers shared by the ST 0601 KLV tests."""

from __future__ import annotations

from tstrans.klv import ST_0601_UL

from _builders.klv_tlv import ber_long


def st0601_checksum(buf: bytes) -> int:
    """ST 0601 section 6.3 16-bit running sum: even index -> high byte, odd -> low.
    Mirrors `tst_core::klv::checksum::checksum_running_sum_16`; `buf` is
    `[UL .. start of Tag 1 value]`."""
    bcc = 0
    for i, b in enumerate(buf):
        shift = 8 * ((i + 1) % 2)
        bcc = (bcc + (b << shift)) & 0xFFFF
    return bcc


def wrap_st0601_with_checksum(body_without_checksum: bytes) -> bytes:
    """Wrap an ST 0601 LS body with UL + outer BER length + Tag 1 TLV,
    computing a valid running-sum checksum so lenient decode accepts it."""
    body_with_checksum_tlv = body_without_checksum + b"\x01\x02"
    outer_len = len(body_with_checksum_tlv) + 2  # +2 for the checksum value
    prefix = ST_0601_UL + ber_long(outer_len) + body_with_checksum_tlv
    return prefix + st0601_checksum(prefix).to_bytes(2, "big")


def local_set_ber_oid_tag(buf: bytes, i: int) -> tuple[int, int]:
    """Read a BER-OID (base-128, high-bit-continuation) local-set tag at
    index `i`. Returns `(tag, bytes_consumed)`."""
    tag = 0
    j = i
    while True:
        b = buf[j]
        tag = (tag << 7) | (b & 0x7F)
        j += 1
        if not (b & 0x80):
            break
    return tag, j - i
