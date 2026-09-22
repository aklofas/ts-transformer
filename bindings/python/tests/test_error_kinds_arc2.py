"""Arc 2 WP-B2: every Python *ErrorKind is a per-domain subset of
`tst_pipeline::binding::BindingErrorKind`, spelled as
`BindingErrorKind::name()` (spec §3.3 + A2's K1/K3/K4 rulings). Pins the
canonical member sets, the deprecated aliases and the two removals; the
Rust↔Python consistency itself is checked at `import tstrans` (Task B2.2)
and by the kind-equivalence rail (WP-A2)."""

from __future__ import annotations

import pytest

from tstrans import exceptions as exc

EXPECTED = {
    exc.SrtErrorKind: {
        "CONNECT_FAILED": 0,
        "ACCEPT_FAILED": 1,
        "BACKPRESSURE": 2,
        "TIMEOUT": 3,
        "CLOSED": 4,
        "BROKEN": 5,
        "CONFIG_INVALID": 6,
        "IO": 7,
        "TOO_LARGE": 8,
        "INPUT_MALFORMED": 9,
        "END_OF_STREAM": 10,
    },
    exc.RtpErrorKind: {
        "BROKEN": 1,
        "TOO_LARGE": 2,
        "CLOSED": 3,
        "BACKPRESSURE": 4,
        "PAYLOAD_TYPE_PARAM": 5,
        "MISSING_PAYLOAD_TYPE_PARAM": 6,
        "URL": 7,
        "HOST_NOT_LITERAL": 8,
        "IO": 9,
        "IFACE_UNSUPPORTED": 10,
        "END_OF_STREAM": 11,
    },
    exc.UdpErrorKind: {
        "URL": 0,
        "IO": 2,
        "TOO_LARGE": 4,
        "CLOSED": 5,
        "INVALID_CONFIG": 6,
        "BACKPRESSURE": 7,
        "BROKEN": 8,
    },
    exc.TcpErrorKind: {
        "URL": 0,
        "IO": 1,
        "TOO_LARGE": 2,
        "CLOSED": 3,
        "CONNECT_TIMEOUT": 4,
        "INVALID_CONFIG": 5,
        "TLS": 6,
        "TLS_DISABLED": 7,
        "BACKPRESSURE": 8,
        "BROKEN": 9,
    },
    exc.RistErrorKind: {
        "URL": 0,
        "FFI": 1,
        "TOO_LARGE": 2,
        "CLOSED": 3,
        "INVALID_CONFIG": 4,
        "ENCRYPTION_DISABLED": 5,
        "CONTEXT_CREATE_FAILED": 6,
        "PEER_CREATE_FAILED": 7,
        "BACKPRESSURE": 8,
        "BROKEN": 9,
    },
    exc.HlsErrorKind: {
        "URL": 0,
        "IO": 1,
        "BIND_FAILED": 2,
        "INVALID_CONFIG": 3,
        "UNALIGNED_PUSH_TS": 4,
        "FINISHED": 5,
        "TLS_DISABLED": 6,
        "TLS": 7,
        "INTERNAL": 8,
        "CLOSED": 9,
    },
    exc.RtspErrorKind: {
        "PROTOCOL": 1,
        "AUTH_FAILED": 2,
        "AUTH_REQUIRED": 3,
        "NOT_FOUND": 4,
        "UNSUPPORTED_TRANSPORT": 5,
        "TLS": 6,
        "IO": 7,
        "TIMEOUT": 8,
        "SERVER": 9,
        "MOUNT": 10,
    },
    exc.MuxErrorKind: {
        "INPUT_MALFORMED": 0,
        "CONFIG_INVALID": 1,
        "INVALID_USAGE": 2,
        "BACKPRESSURE": 3,
        "INTERNAL": 4,
        "INVALID_NAL": 5,
        "KLV_TOO_LARGE": 6,
        "INVALID_AV1_OBU": 7,
        "MISP_TIME": 8,
    },
    exc.DemuxErrorKind: {
        "UNRECOVERABLE": "unrecoverable",
        "STRICT_REJECTION": "strict_rejection",
        "MALFORMED_PSI": "malformed_psi",
        "MALFORMED_PES": "malformed_pes",
        "SYNC_BUF_EXHAUSTED": "sync_buf_exhausted",
    },
    exc.KlvErrorKind: {
        "BAD_UNIVERSAL_LABEL": "bad_universal_label",
        "TRUNCATED_SET": "truncated_set",
        "CHECKSUM_MISMATCH": "checksum_mismatch",
        "DUPLICATE_TAG": "duplicate_tag",
        "MISSING_REQUIRED_TAG": "missing_required_tag",
        "MALFORMED_BYTES": "malformed_bytes",
        "INTERNAL": "internal",
    },
    exc.KlvEncodeErrorKind: {
        "BUFFER_TOO_SMALL": 0,
        "RECORD_TOO_LARGE": 1,
        "OUT_OF_RANGE": 2,
        "STRING_TOO_LONG": 3,
        "UNSUPPORTED_IMAPB_LENGTH": 4,
        "INVALID_IMAPB_PARAMS": 5,
        "MISSING_MANDATORY_ITEM": 6,
        "RESERVED_TAG_IN_UNKNOWN": 7,
        "V_TARGET_PACK_EMPTY": 8,
        "DUPLICATE_TARGET_ID": 9,
        "FORBIDDEN_STANDALONE_OFFSET": 10,
    },
    exc.CodecErrorKind: {
        "TRUNCATED_RBSP": 1,
        "INVALID_GOLOMB": 2,
        "RESERVED_VALUE": 3,
        "UNSUPPORTED_PROFILE": 4,
        "DANGLING_SPS_REFERENCE": 5,
        "DANGLING_VPS_REFERENCE": 6,
        "ENGINE_ERROR": 7,
        "INVALID_LEB128": 8,
        "BAD_SYNC_WORD": 9,
        "TRUNCATED": 10,
        "FORBIDDEN": 11,
        "UNSUPPORTED_FREE_FORMAT": 12,
        "INVALID_LENGTH_SIZE": 13,
        "NAL_LENGTH_OVERFLOW": 14,
        "BUFFER_TOO_SMALL": 15,
    },
}

# alias name → canonical member name (deprecated for 0.7.x, removed in 0.8.0)
ALIASES = {
    exc.SrtErrorKind: {"WOULD_BLOCK": "BACKPRESSURE"},
    exc.RtpErrorKind: {
        "TRANSPORT": "BROKEN",
        "MALFORMED_PACKET": "TOO_LARGE",
        "CANCELLED": "CLOSED",
        "TIMEOUT": "BACKPRESSURE",
    },
    exc.UdpErrorKind: {"PAYLOAD_TOO_LARGE": "TOO_LARGE"},
    exc.TcpErrorKind: {"PAYLOAD_TOO_LARGE": "TOO_LARGE"},
    exc.RistErrorKind: {
        "PAYLOAD_TOO_LARGE": "TOO_LARGE",
        "RECV_TIMEOUT": "BACKPRESSURE",
        "IO": "BROKEN",
    },
    exc.DemuxErrorKind: {
        "INTERNAL": "UNRECOVERABLE",
        "BAD_PMT": "MALFORMED_PSI",
        "BAD_PES": "MALFORMED_PES",
        "SYNC_LOSS": "SYNC_BUF_EXHAUSTED",
    },
    exc.KlvEncodeErrorKind: {"VTARGET_PACK_EMPTY": "V_TARGET_PACK_EMPTY"},
}

REMOVED = {exc.DemuxErrorKind: ["UNEXPECTED_EOF"], exc.KlvErrorKind: ["UNKNOWN_SET"]}


@pytest.mark.parametrize("enum_cls", list(EXPECTED), ids=lambda e: e.__name__)
def test_canonical_member_set_is_exactly_the_arc2_table(enum_cls) -> None:
    canonical = {m.name: m.value for m in enum_cls}  # iteration excludes aliases
    assert canonical == EXPECTED[enum_cls], f"{enum_cls.__name__} drifted from the Arc 2 kind table"


@pytest.mark.parametrize("enum_cls", list(ALIASES), ids=lambda e: e.__name__)
def test_deprecated_aliases_resolve_to_their_successor(enum_cls) -> None:
    for alias, target in ALIASES[enum_cls].items():
        assert enum_cls.__members__[alias] is enum_cls[target], (
            f"{enum_cls.__name__}.{alias} must alias {target}"
        )
        assert alias not in {m.name for m in enum_cls}, f"{alias} must be an alias, not a member"
    assert "deprecated" in (enum_cls.__doc__ or "").lower(), (
        f"{enum_cls.__name__} docstring must name the deprecated aliases"
    )


@pytest.mark.parametrize("enum_cls", list(REMOVED), ids=lambda e: e.__name__)
def test_removed_members_are_gone(enum_cls) -> None:
    for name in REMOVED[enum_cls]:
        assert name not in enum_cls.__members__, (
            f"{enum_cls.__name__}.{name} was never produced and is removed in 0.7.0"
        )
