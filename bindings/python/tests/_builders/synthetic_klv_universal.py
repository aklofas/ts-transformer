"""Synthetic builders for ST 0102 (Security) and ST 0903 (VMTI) universal
LS records.

These builders call the public tstrans.klv encoder functions on minimal
Python dataclasses and capture the resulting bytes. This gives self-checking
builders: if the encoder regresses, the test regresses.

Used by tests that previously skipped on a missing tests/fixtures/local/
fixture. The returned bytes are valid SMPTE-UL-prefixed records suitable for
passing directly to parse_klv_universal().

Lets the ST 0102 and ST 0903 pandas tests run without a capture fixture.
"""

from __future__ import annotations

from tstrans.klv import (
    SECURITY_LS_UL,
    VMTI_LS_UL,
    ClassifyingCountryCodingMethod,
    ObjectCountryCodingMethod,
    SecurityClassification,
    SecurityLs,
    VTargetPack,
    VmtiLs,
    encode_security,
    encode_vmti,
)


def _ber_encode_length(n: int) -> bytes:
    """BER short-form or long-form (1-byte) length encoding."""
    if n < 0x80:
        return bytes([n])
    if n <= 0xFF:
        return bytes([0x81, n])
    # Two-byte long form (supports up to 65535)
    return bytes([0x82, (n >> 8) & 0xFF, n & 0xFF])


def synthetic_security_ls() -> bytes:
    """Return a spec-minimal ST 0102 LS as a full universal-LS record.

    Encodes a SecurityLs with Security Classification = UNCLASSIFIED,
    Classifying Country Coding Method = ISO-3166 two-letter, Classifying
    Country = '//US', Object Country Coding Method = ISO-3166 two-letter,
    Object Country Codes = 'US', Version = 12.

    The record is prefixed with the 16-byte SECURITY_LS_UL and a BER
    outer length, making it suitable for parse_klv_universal().

    Round-trips: parse_klv_universal(synthetic_security_ls()) returns a
    SecurityLs with security_classification == SecurityClassification.UNCLASSIFIED.
    """
    rec = SecurityLs(
        security_classification=SecurityClassification.UNCLASSIFIED,
        classifying_country_coding_method=ClassifyingCountryCodingMethod.ISO_3166_TWO_LETTER,
        classifying_country="//US",
        object_country_coding_method=ObjectCountryCodingMethod.ISO_3166_TWO_LETTER,
        object_country_codes="US",
        version=12,
    )
    body = encode_security(rec)
    return SECURITY_LS_UL + _ber_encode_length(len(body)) + body


def synthetic_vmti_ls(*, n_targets: int = 2) -> bytes:
    """Return a spec-minimal ST 0903 LS as a full universal-LS record.

    Encodes a VmtiLs with a precision_time_stamp (required for DatetimeIndex),
    version_number = 6, and n_targets VTargetPacks (target_id 1..n_targets).

    The record is prefixed with the 16-byte VMTI_LS_UL and a BER outer length,
    making it suitable for parse_klv_universal().

    Round-trips: parse_klv_universal(synthetic_vmti_ls()) returns a VmtiLs
    whose len(targets) == n_targets.
    """
    targets = tuple(
        VTargetPack(target_id=i + 1, confidence_level=80 + i)
        for i in range(n_targets)
    )
    vmti = VmtiLs(
        precision_time_stamp=1_700_000_000_000_000,
        version_number=6,
        targets=targets,
    )
    body = encode_vmti(vmti)
    return VMTI_LS_UL + _ber_encode_length(len(body)) + body
