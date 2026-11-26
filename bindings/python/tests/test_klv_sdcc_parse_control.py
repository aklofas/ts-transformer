"""ST 1010.3 SDCC-FLP through ``decode_sdcc_flp`` and ``encode_sdcc_flp_mode2``:
a Parse Control bit Table 2 reserves in Mode 2, a binary16 value (ST 0107.5
allows binary16, binary32 and binary64) and the largest matrix the decoder
reads (256)."""

from __future__ import annotations

import pytest

from tstrans.exceptions import KlvEncodeError, KlvEncodeErrorKind, KlvError, KlvErrorKind
from tstrans.klv import decode_sdcc_flp, encode_sdcc_flp_mode2


def test_a_reserved_parse_control_bit_is_malformed() -> None:
    # N=1, Mode 2 Parse Control C0 04: Byte 1 bit 6 is reserved.
    with pytest.raises(KlvError) as excinfo:
        decode_sdcc_flp(bytes.fromhex("01C0043F800000"))
    assert excinfo.value.kind == KlvErrorKind.MALFORMED_BYTES


def test_the_same_pack_without_the_reserved_bit_decodes() -> None:
    assert decode_sdcc_flp(bytes.fromhex("0180043F800000")).std_devs == (1.0,)


def test_a_binary16_standard_deviation_decodes() -> None:
    # Mode 2 Parse Control 80 02: Slen 2, the value 3C 00 = 1.0.
    assert decode_sdcc_flp(bytes.fromhex("0180023C00")).std_devs == (1.0,)


def test_more_than_256_standard_deviations_are_out_of_range() -> None:
    n = 257
    with pytest.raises(KlvEncodeError) as excinfo:
        encode_sdcc_flp_mode2([1.0] * n, [0.0] * (n * (n - 1) // 2), 2)
    assert excinfo.value.kind is KlvEncodeErrorKind.OUT_OF_RANGE
