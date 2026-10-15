"""Minimal BER-short-form TLV builders shared by the ST 0102 / ST 0903 /
universal-set KLV tests.

This is the single-byte-tag + BER-short-form-length TLV shape used by
ST 0102 Security and ST 0903 VMTI local sets and by the generic
universal-set parser tests. It is unrelated to ST 0601's multi-byte
BER-OID tag + BER-long-form-length shape (`test_klv_st0601_packs.py`
keeps its own `_tlv`/`_ber_oid_tag`, a different wire shape
that happens to share a name).
"""

from __future__ import annotations


def ber_short(n: int) -> bytes:
    """BER short-form length (single byte, value < 0x80)."""
    if not 0 <= n < 0x80:
        raise ValueError(f"value {n} out of BER short-form range")
    return bytes([n])


def tlv(tag: int, value: bytes) -> bytes:
    """1-byte tag + BER short-form length + value (ST 0102 / ST 0903 /
    universal-set body TLV shape)."""
    if not 0 <= tag < 0x80:
        raise ValueError(f"tag {tag} out of single-byte BER-OID range")
    return bytes([tag]) + ber_short(len(value)) + value


def ber_long(n: int) -> bytes:
    """BER definite-form length: short form under 0x80, else 0x8X + X
    big-endian length bytes (the minimal X)."""
    if n < 0x80:
        return ber_short(n)
    payload = n.to_bytes((n.bit_length() + 7) // 8, "big")
    return bytes([0x80 | len(payload)]) + payload


def wrap_with_ul(ul: bytes, body: bytes) -> bytes:
    """`ul` + BER length + `body`: one universal-set KLV packet."""
    return ul + ber_long(len(body)) + body
