"""decode_vmti_standalone: a standalone VMTI packet (VMTI UL + BER length +
body) decoded with its checksum verified, and the strict form."""

import pytest

from tstrans.exceptions import KlvError, KlvErrorKind
from tstrans.klv import VmtiLs, decode_vmti_standalone, encode_vmti_standalone_strict_compliance

# The jMISB standalone VMTI test vector: system name "DSTO_ADSS_VMTI",
# version 4, checksum 9F97, no Item 6.
JMISB_VECTOR = bytes.fromhex(
    "060E2B34020B01010E01030306000000"
    "17030E4453544F5F414453535F564D5449"
    "040104"
    "01029F97"
)

_CORE_ID = bytes.fromhex(
    "0170F592F02373364AF8AA9162C00F2EB2DA16B74341000841A0BE365B5AB96A3645"
)


def test_the_jmisb_vector_decodes():
    ls = decode_vmti_standalone(JMISB_VECTOR)
    assert ls.vmti_system_name == "DSTO_ADSS_VMTI"
    assert ls.version_number == 4
    assert ls.checksum == 0x9F97
    # The vector has no Item 6, which the strict form requires.
    with pytest.raises(KlvError) as ei:
        decode_vmti_standalone(JMISB_VECTOR, strict=True)
    assert ei.value.kind is KlvErrorKind.MISSING_REQUIRED_TAG


def test_a_flipped_byte_is_a_checksum_mismatch():
    bad = bytearray(JMISB_VECTOR)
    bad[20] ^= 0x01  # inside the system name
    with pytest.raises(KlvError) as ei:
        decode_vmti_standalone(bytes(bad))
    assert ei.value.kind is KlvErrorKind.CHECKSUM_MISMATCH


def test_another_label_is_refused():
    bad = bytearray(JMISB_VECTOR)
    bad[12] = 0x01
    with pytest.raises(KlvError) as ei:
        decode_vmti_standalone(bytes(bad))
    assert ei.value.kind is KlvErrorKind.BAD_UNIVERSAL_LABEL


def test_our_strict_standalone_output_passes_the_strict_decode():
    rec = VmtiLs(
        version_number=6,
        num_targets_reported=0,
        precision_time_stamp=1_700_000_000_000_000,
        horizontal_fov=45.0,
        vertical_fov=30.0,
        miis_id=_CORE_ID,
    )
    back = decode_vmti_standalone(encode_vmti_standalone_strict_compliance(rec), strict=True)
    assert back.precision_time_stamp == rec.precision_time_stamp
    assert back.miis_id == _CORE_ID
