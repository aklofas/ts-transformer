"""Every Python *ErrorKind is a per-domain subset of
`tst_pipeline::binding::BindingErrorKind`, spelled as
`BindingErrorKind::name()`. Pins the
canonical member sets, the deprecated aliases and the two removals; the
Rust↔Python consistency itself is checked at `import tstrans`
and by the kind-equivalence rail."""

from __future__ import annotations

import pytest

from tstrans import exceptions as exc

from test_error_kind_values import EXPECTED as VALUES

# One source for the values: the literal table in test_error_kind_values.py.
EXPECTED = {
    getattr(exc, name): dict(members) for name, members in VALUES.items()
}

# alias name → the member it aliased; kept for 0.7.x, removed in 0.8.0
REMOVED_ALIASES = {
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
        "BAD_PES": "MALFORMED_PES",
        "SYNC_LOSS": "SYNC_BUF_EXHAUSTED",
    },
    exc.KlvEncodeErrorKind: {"VTARGET_PACK_EMPTY": "V_TARGET_PACK_EMPTY"},
}

REMOVED = {
    exc.DemuxErrorKind: ["UNEXPECTED_EOF", "MALFORMED_PSI"],
    exc.KlvErrorKind: ["UNKNOWN_SET"],
    exc.CodecErrorKind: ["DANGLING_SPS_REFERENCE", "DANGLING_VPS_REFERENCE"],
}


@pytest.mark.parametrize("enum_cls", list(EXPECTED), ids=lambda e: e.__name__)
def test_canonical_member_set_is_exactly_the_arc2_table(enum_cls) -> None:
    canonical = {m.name: m.value for m in enum_cls}  # iteration excludes aliases
    assert canonical == EXPECTED[enum_cls], f"{enum_cls.__name__} drifted from the canonical kind table"


@pytest.mark.parametrize("enum_cls", list(REMOVED_ALIASES), ids=lambda e: e.__name__)
def test_the_0_7_x_aliases_are_gone(enum_cls) -> None:
    for alias, target in REMOVED_ALIASES[enum_cls].items():
        assert alias not in enum_cls.__members__, f"{enum_cls.__name__}.{alias} was removed in 0.8.0"
        assert target in enum_cls.__members__, f"{enum_cls.__name__}.{target} must remain"
    assert "deprecated alias" not in (enum_cls.__doc__ or "").lower(), (
        f"{enum_cls.__name__} docstring still advertises removed aliases"
    )


@pytest.mark.parametrize("enum_cls", list(REMOVED), ids=lambda e: e.__name__)
def test_removed_members_are_gone(enum_cls) -> None:
    for name in REMOVED[enum_cls]:
        assert name not in enum_cls.__members__, (
            f"{enum_cls.__name__}.{name} was never produced and is removed"
        )
