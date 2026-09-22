"""Exception hierarchy raised by tstrans.

Every error raised by any tstrans surface is a subclass of `TstError`.
Domain errors (`MuxError`, `DemuxError`, `KlvError`, `KlvEncodeError`,
`CodecError`) carry a typed `.kind` attribute matching the corresponding
Rust *ErrorKind enum. These enums mirror Rust enums marked
`#[non_exhaustive]`, so new variants may appear in minor releases;
matchers should include a default arm.

Field-level KLV parse warnings are NOT raised — they live on the
parsed object as `field_errors`, matching Rust's "best-effort parse"
semantics for ST 0601 in the field.

Since 0.7.0 every `*ErrorKind` member below is a
`tst_pipeline::binding::BindingErrorKind::name()` string — the one
Rust-side kind table shared by the C, Python and JVM bindings, with the
domain prefix stripped (`BindingErrorKind::UdpIo` → `UdpErrorKind.IO`).
The native extension resolves every member it can raise at
`import tstrans`, so a table/enum mismatch is an `ImportError`, never a
surprise in an `except` clause. Members retired by that alignment stay as
value-compatible deprecated aliases for 0.7.x and are removed in 0.8.0;
each enum's docstring lists its own.

`KlvEncodeErrorKind` accompanies the `klv.encode_*` Python wrappers.
"""

import enum
from typing import Any, Optional


class TstError(Exception):
    """Base class for every error raised by tstrans. Catch this to
    handle anything from this package."""


class _KindMessageError(TstError):
    """Private base for the nine transport/domain error classes that share
    an identical ``(*, kind, message: str)`` constructor.  Rust raises
    these via ``cls.call((), Some(&kwargs))`` with ``{kind=...,
    message=...}``, so the constructor must accept exactly those two
    keyword-only arguments.

    Not exported (absent from ``__all__``); do not instantiate directly.
    Subclasses redeclare ``kind`` with the concrete ``*Kind`` type so
    static type checkers see the narrower annotation.
    """

    # `Any` (not `object`): subclasses redeclare `kind` with their concrete
    # `*Kind` enum, and narrowing a mutable `object` attribute would be an
    # invalid override to type checkers; `Any` permits the narrowing.
    kind: Any
    message: str

    def __init__(self, *, kind: Any, message: str) -> None:
        super().__init__(message)
        self.kind = kind
        self.message = message


class MuxErrorKind(enum.IntEnum):
    """`MuxError.kind` — the muxer subset of the Rust
    `tst_pipeline::binding::BindingErrorKind` table; members are
    `BindingErrorKind::name()`. The five coarse kinds of
    `tst_core::error::MuxErrorKind` plus, since 0.7.0, the four precise
    variants the C ABI always numbered separately: `INVALID_NAL`
    (`MuxError::InvalidNal` — was `INPUT_MALFORMED`), `KLV_TOO_LARGE`,
    `INVALID_AV1_OBU`, `MISP_TIME`. Matchers should include a default arm.
    """

    # Caller pushed input bytes that don't conform — non-Annex-B NAL,
    # KLV / audio / subtitle PES payloads over the 16-bit PES length cap.
    INPUT_MALFORMED = 0

    # `MuxerConfig::validate()` rejected the construction-time config —
    # duplicate PIDs, too many streams, malformed descriptor TLV,
    # PCR-PID conflicts, ISO 639 / DVB teletext field violations, PMT
    # over-budget, etc. The muxer was not constructed.
    CONFIG_INVALID = 1

    # API misuse on a successfully-built muxer — wrong-muxer stream
    # handle, ambiguous-target shorthand on a multi-stream muxer,
    # unknown program reference, out-of-range descriptor / abs index.
    INVALID_USAGE = 2

    # Muxer output buffer is full; drain via pull and retry.
    BACKPRESSURE = 3

    # Bug-path invariant tripped inside the muxer — should not occur in
    # well-formed use; file an issue with reproduction bytes.
    INTERNAL = 4

    # `MuxError::InvalidNal` — a pushed video AU carried no Annex-B start
    # code (was reported as INPUT_MALFORMED before 0.7.0).
    INVALID_NAL = 5

    # `MuxError::KlvTooLarge` — a KLV record exceeded the PES length cap.
    KLV_TOO_LARGE = 6

    # `MuxError::InvalidAv1Obu` — the AV1 OBU stream could not be framed.
    INVALID_AV1_OBU = 7

    # `MuxError::MispTime` — the MISP ST 0604 timestamp SEI could not be
    # built or inserted (`push_video_misp_to` family).
    MISP_TIME = 8


class DemuxErrorKind(enum.Enum):
    """`DemuxError.kind` — mirrors `tst_core::mpegts::DemuxError`'s
    variant names since 0.7.0 (`UNRECOVERABLE`, `STRICT_REJECTION`,
    `MALFORMED_PSI`, `MALFORMED_PES`, `SYNC_BUF_EXHAUSTED`).

    ``STRICT_REJECTION`` is the Python-side representation of
    ``DemuxError::StrictRejection`` — raised when ``StrictMode`` is
    non-Off and the demuxer encounters a non-conformance that the
    configured policy escalates to a fatal error.  The underlying
    ``DemuxError`` message carries the specific ``NonConformantIssue``
    name as a diagnostic string.

    Deprecated aliases (0.7.x only, removed in 0.8.0): `INTERNAL` →
    `UNRECOVERABLE`, `BAD_PMT` → `MALFORMED_PSI`, `BAD_PES` → `MALFORMED_PES`,
    `SYNC_LOSS` → `SYNC_BUF_EXHAUSTED`. `UNEXPECTED_EOF` is removed — it was
    never produced (truncation is a clean EOF, read failures are `OSError`).
    Matchers should include a default arm — the Rust ``DemuxError`` enum is
    ``#[non_exhaustive]`` and new variants may appear in minor releases.
    """

    # `DemuxError::Unrecoverable` — the demuxer cannot continue.
    UNRECOVERABLE = "unrecoverable"
    INTERNAL = "unrecoverable"  # deprecated alias (0.7.x): use UNRECOVERABLE
    # Strict-mode policy rejection — StrictMode converted a non-conformance
    # into a fatal error.
    STRICT_REJECTION = "strict_rejection"
    # `DemuxError::MalformedPsi` — PAT/PMT section structurally invalid.
    MALFORMED_PSI = "malformed_psi"
    BAD_PMT = "malformed_psi"  # deprecated alias (0.7.x): use MALFORMED_PSI
    # `DemuxError::MalformedPes` — PES header/payload structurally invalid.
    MALFORMED_PES = "malformed_pes"
    BAD_PES = "malformed_pes"  # deprecated alias (0.7.x): use MALFORMED_PES
    # `DemuxError::SyncBufExhausted` — the resync buffer filled without
    # finding the four confirming sync bytes.
    SYNC_BUF_EXHAUSTED = "sync_buf_exhausted"
    SYNC_LOSS = "sync_buf_exhausted"  # deprecated alias (0.7.x): use SYNC_BUF_EXHAUSTED


class KlvErrorKind(enum.Enum):
    """Mirrors Rust's `tst_core::error::KlvDecodeError` variants
    collapsed to user-facing buckets. Only set-level (structural)
    errors raise as `KlvError` — per-field validation failures land
    on the decoded typed-set object as `.field_errors: list[KlvFieldError]`
    instead. See `docs/specs/2026-05-22-tst-py-design.md` "Error
    mapping" for the full mapping table.

    The Rust `KlvDecodeError` enum is `#[non_exhaustive]` — Python
    matchers should include a default arm. `UNKNOWN_SET` was removed in
    0.7.0 — no decoder produced it."""

    BAD_UNIVERSAL_LABEL = "bad_universal_label"
    TRUNCATED_SET = "truncated_set"
    CHECKSUM_MISMATCH = "checksum_mismatch"
    DUPLICATE_TAG = "duplicate_tag"
    MISSING_REQUIRED_TAG = "missing_required_tag"
    MALFORMED_BYTES = "malformed_bytes"
    INTERNAL = "internal"


class KlvEncodeErrorKind(enum.IntEnum):
    """Mirrors Rust `tst_core::error::KlvEncodeError` variant tags.
    Raised by KLV `encode_*` functions when a typed record cannot be
    serialized to wire bytes — output buffer too small, value outside
    the spec-declared range, IMAPB params violating ST 1201.5 §6
    preconditions, or mandatory ST 0601 items missing under
    `encode_strict_compliance`.

    `RESERVED_TAG_IN_UNKNOWN` is a real Rust `KlvEncodeError` variant,
    but this binding's encode path (`py_to_unknown`) silently filters
    any typed/reserved tag out of `unknown` before the Rust encoder
    runs (the typed field wins) — so that cause is never raised from
    Python.

    The Rust enum is `#[non_exhaustive]`; new variants land as the
    encoder catches more failure modes. Python matchers should include
    a default arm.

    Deprecated alias (0.7.x only, removed in 0.8.0):
    `VTARGET_PACK_EMPTY` → `V_TARGET_PACK_EMPTY` (the Rust variant is
    `VTargetPackEmpty`, whose SCREAMING_SNAKE is `V_TARGET_PACK_EMPTY`).
    """

    BUFFER_TOO_SMALL = 0
    RECORD_TOO_LARGE = 1
    OUT_OF_RANGE = 2
    STRING_TOO_LONG = 3
    UNSUPPORTED_IMAPB_LENGTH = 4
    INVALID_IMAPB_PARAMS = 5
    MISSING_MANDATORY_ITEM = 6
    RESERVED_TAG_IN_UNKNOWN = 7
    V_TARGET_PACK_EMPTY = 8
    VTARGET_PACK_EMPTY = 8  # deprecated alias (0.7.x): use V_TARGET_PACK_EMPTY
    DUPLICATE_TARGET_ID = 9
    FORBIDDEN_STANDALONE_OFFSET = 10


class CodecErrorKind(enum.IntEnum):
    """Mirrors `tst_core::codec::CodecParseError` variants.

    Unknown Rust variants (e.g. from a newer library version) are mapped
    to ``ENGINE_ERROR`` by the native extension; Python pattern-matchers
    must include a default fallback to remain robust.

    ``BUFFER_TOO_SMALL`` (0.7.0) mirrors
    ``CodecParseError::BufferTooSmall { needed, have }`` — raised only by
    the write-into-buffer entry points, which have no Python caller today;
    it was folded into ``ENGINE_ERROR`` before.
    """

    TRUNCATED_RBSP = 1
    INVALID_GOLOMB = 2
    RESERVED_VALUE = 3
    UNSUPPORTED_PROFILE = 4
    DANGLING_SPS_REFERENCE = 5
    DANGLING_VPS_REFERENCE = 6
    ENGINE_ERROR = 7
    INVALID_LEB128 = 8
    BAD_SYNC_WORD = 9
    TRUNCATED = 10
    FORBIDDEN = 11
    UNSUPPORTED_FREE_FORMAT = 12
    INVALID_LENGTH_SIZE = 13
    NAL_LENGTH_OVERFLOW = 14
    BUFFER_TOO_SMALL = 15


class MuxError(TstError):
    """Raised by `tstrans.mpegts.Muxer` construction, `push_*`, and
    builder `.build()` calls. Carries `MuxErrorKind` on `.kind` and a
    free-text message on `.message` / `.args[0]`. Optional `.pid` is
    populated for stream-not-found and PID-conflict diagnostics.

    Both signatures supported:
      `MuxError("bad config", kind=MuxErrorKind.CONFIG_INVALID)`
      `MuxError(kind=MuxErrorKind.CONFIG_INVALID, message="bad config")`
    """

    kind: MuxErrorKind
    message: str
    pid: Optional[int]

    def __init__(
        self,
        message: Optional[str] = None,
        *,
        kind: MuxErrorKind,
        pid: Optional[int] = None,
    ) -> None:
        if message is None:
            raise TypeError("MuxError requires a message (positional or via message=)")
        super().__init__(message)
        self.kind = kind
        self.message = message
        self.pid = pid


class DemuxError(_KindMessageError):
    """Raised by `tstrans.mpegts.Demuxer` operations."""

    kind: DemuxErrorKind


class KlvError(_KindMessageError):
    """Raised by `tstrans.klv` set-level decoders. Field-level warnings
    appear as `field_errors` on the returned typed-set object, not
    raised."""

    kind: KlvErrorKind


class KlvEncodeError(TstError):
    """Raised by `tstrans.klv.encode_*` functions when the typed record
    cannot be serialized. Carries `.kind` (`KlvEncodeErrorKind`) and an
    optional `.tag`. For most tag-bearing variants `.tag` is the offending
    ST item (KLV tag) code — `OUT_OF_RANGE`, `STRING_TOO_LONG`,
    `MISSING_MANDATORY_ITEM`, `RESERVED_TAG_IN_UNKNOWN`, and
    `FORBIDDEN_STANDALONE_OFFSET`. For `V_TARGET_PACK_EMPTY` and
    `DUPLICATE_TARGET_ID`, `.tag` instead carries the VTarget Pack
    `target_id` (a target identifier, not a KLV tag). `BUFFER_TOO_SMALL`,
    `RECORD_TOO_LARGE`, `UNSUPPORTED_IMAPB_LENGTH`, and
    `INVALID_IMAPB_PARAMS` have no associated value — `.tag` is `None`.
    """

    kind: KlvEncodeErrorKind
    message: str
    tag: Optional[int]

    def __init__(
        self,
        message: Optional[str] = None,
        *,
        kind: KlvEncodeErrorKind,
        tag: Optional[int] = None,
    ) -> None:
        if message is None:
            raise TypeError("KlvEncodeError requires a message (positional or via message=)")
        super().__init__(message)
        self.kind = kind
        self.message = message
        self.tag = tag


class RtspErrorKind(enum.IntEnum):
    """`RtspError.kind` — the RTSP subset of the Rust
    `BindingErrorKind` table; members are `BindingErrorKind::name()`.
    Raised by `tstrans.rtp.RtspClient` / `tstrans.rtp.RtspServer`
    operations (connect, play, pause, teardown, start, stop, add_mount).

    `CLOSED` (0.7.0) is the shared closed-handle kind: a call on a client,
    session or mount whose handle was already closed, or one cancelled from
    another thread.

    Available only when tstrans was built with the `rtp` cargo
    feature (default-on in published wheels).
    """

    PROTOCOL = 1
    AUTH_FAILED = 2
    AUTH_REQUIRED = 3
    NOT_FOUND = 4
    UNSUPPORTED_TRANSPORT = 5
    TLS = 6
    IO = 7
    TIMEOUT = 8
    SERVER = 9
    MOUNT = 10
    CLOSED = 11


class RtspError(_KindMessageError):
    """Raised by `tstrans.rtp.RtspClient` / `RtspServer` / `MountHandle`
    operations. Carries a typed `.kind` (`RtspErrorKind`) plus a
    free-text message on `.message` / `.args[0]`."""

    kind: RtspErrorKind


class RtpErrorKind(enum.IntEnum):
    """`RtpError.kind` — the `tstrans.rtp` subset of the Rust
    `BindingErrorKind` table; members are `BindingErrorKind::name()`.
    Runtime: `CLOSED` (cancel / close from another thread — detail
    "cancelled from another thread" — or a call on a closed handle),
    `BROKEN` (wire failure), `BACKPRESSURE` (recv deadline expired:
    `?recv_timeout=` or `timeout_ms=`; retryable, the receiver stays open),
    `TOO_LARGE` (payload over the datagram cap), `END_OF_STREAM` (peer
    ended the stream on a non-iterator `recv()`). Construction
    (`tst_rtp::ConnectError`, one member per variant): `PAYLOAD_TYPE_PARAM`,
    `MISSING_PAYLOAD_TYPE_PARAM`, `URL`, `HOST_NOT_LITERAL`, `IO`,
    `IFACE_UNSUPPORTED`.

    Deprecated aliases (0.7.x only, removed in 0.8.0): `TRANSPORT` →
    `BROKEN`, `MALFORMED_PACKET` → `TOO_LARGE`, `CANCELLED` → `CLOSED`,
    `TIMEOUT` → `BACKPRESSURE` — each `is` its successor, so existing
    `e.kind == RtpErrorKind.CANCELLED` comparisons keep working; compare
    against the successor. Before 0.7.0 `TRANSPORT` also covered the
    construction-time errors, which now carry their own member.

    Available only when tstrans was built with the `rtp` cargo
    feature (default-on in published wheels).
    """

    BROKEN = 1
    TRANSPORT = 1  # deprecated alias (0.7.x): use BROKEN (or the construction members)
    TOO_LARGE = 2
    MALFORMED_PACKET = 2  # deprecated alias (0.7.x): use TOO_LARGE
    CLOSED = 3
    CANCELLED = 3  # deprecated alias (0.7.x): use CLOSED
    # Recv deadline expired — retryable; the transport/session is still
    # alive. Raised from two triggers: a persistent deadline configured
    # via the `?recv_timeout=<ms>` URL query key on a Receiver /
    # DemuxReceiver URL, or a per-call `timeout_ms=` argument to
    # `recv()` / `recv_au()`. A receiver with neither configured blocks
    # indefinitely instead of raising this.
    BACKPRESSURE = 4
    TIMEOUT = 4  # deprecated alias (0.7.x): use BACKPRESSURE
    # `tst_rtp::ConnectError` — one member per variant since 0.7.0
    # (all of these were TRANSPORT before).
    PAYLOAD_TYPE_PARAM = 5
    MISSING_PAYLOAD_TYPE_PARAM = 6
    URL = 7
    HOST_NOT_LITERAL = 8
    IO = 9
    IFACE_UNSUPPORTED = 10
    # Peer ended the stream cleanly on a non-iterator `recv()` (iterators
    # raise `StopIteration` instead).
    END_OF_STREAM = 11


class RtpError(_KindMessageError):
    """Raised by `tstrans.rtp` transport operations."""

    kind: RtpErrorKind


class SrtErrorKind(enum.IntEnum):
    """`SrtError.kind` — the `tstrans.srt` subset of the Rust
    `BindingErrorKind` table; members are `BindingErrorKind::name()`.
    Construction: `CONNECT_FAILED` / `ACCEPT_FAILED` / `TIMEOUT` /
    `CONFIG_INVALID` (URL, mode, option and address errors). Runtime:
    `CLOSED` (own close; a cancel/close from another thread — detail
    "cancelled from another thread"; a call on a closed handle), `BROKEN`
    (wire failure — and, until the SRT transport-level cancel change lands
    later in 0.7.0, what a cancelled parked call on a plain shell may still
    raise), `BACKPRESSURE` (libsrt would-block / send queue full; retry),
    `TOO_LARGE` (payload over the `payloadsize` cap; was `CONFIG_INVALID`),
    `INPUT_MALFORMED` (TS framing lost sync in `send_bytes`; was
    `CONFIG_INVALID`), `END_OF_STREAM` (the peer closed cleanly on a
    non-iterator `recv_bytes`; was `CLOSED`), `IO` (libsrt system errors).

    Deprecated alias (0.7.x only, removed in 0.8.0): `WOULD_BLOCK` →
    `BACKPRESSURE`.

    Available only when tstrans was built with the `srt` cargo
    feature (default-on in published wheels).
    """

    CONNECT_FAILED = 0
    ACCEPT_FAILED = 1
    BACKPRESSURE = 2
    WOULD_BLOCK = 2  # deprecated alias (0.7.x): use BACKPRESSURE
    TIMEOUT = 3
    CLOSED = 4
    BROKEN = 5
    CONFIG_INVALID = 6
    IO = 7
    TOO_LARGE = 8
    INPUT_MALFORMED = 9
    END_OF_STREAM = 10


class SrtError(_KindMessageError):
    """Raised by `tstrans.srt` operations. Discriminate via `.kind`."""

    kind: SrtErrorKind


# ── Plan A5b — udp / tcp / hls / rist transport error classes ────────────
# Kind enums mirror the Rust `*ErrorKind` variant sets (SCREAMING_SNAKE).
# The Rust side raises these via `errors::make_<proto>_error(py, "VARIANT",
# message)` (import-based, mirroring make_rtsp_error — NOT create_exception!).


class UdpErrorKind(enum.IntEnum):
    """`UdpError.kind` — the `tstrans.udp` subset of the Rust
    `BindingErrorKind` table. `URL` / `IO` / `INVALID_CONFIG` come from
    `tst_udp::UdpErrorKind` at build time; `CLOSED` / `BROKEN` /
    `BACKPRESSURE` (recv deadline expired — retryable; raised `IO` before
    0.7.0) / `TOO_LARGE` from the transport. Raised by `tstrans.udp`
    operations (built with the `udp` cargo feature, default-on).

    Deprecated alias (0.7.x only, removed in 0.8.0): `PAYLOAD_TOO_LARGE` →
    `TOO_LARGE`."""

    URL = 0
    IO = 2
    TOO_LARGE = 4
    PAYLOAD_TOO_LARGE = 4  # deprecated alias (0.7.x): use TOO_LARGE
    CLOSED = 5
    INVALID_CONFIG = 6
    BACKPRESSURE = 7
    BROKEN = 8


class UdpError(_KindMessageError):
    """Raised by `tstrans.udp` operations. Discriminate via `.kind`."""

    kind: UdpErrorKind


class TcpErrorKind(enum.IntEnum):
    """`TcpError.kind` — the `tstrans.tcp` subset of the Rust
    `BindingErrorKind` table. `URL` / `IO` / `CLOSED` / `CONNECT_TIMEOUT` /
    `INVALID_CONFIG` / `TLS` / `TLS_DISABLED` come from
    `tst_tcp::TcpErrorKind`; `BROKEN` (a wire failure — raised `IO` before
    0.7.0) / `BACKPRESSURE` / `TOO_LARGE` from the transport; `CLOSED` also
    from a cancel/close from another thread. Raised by `tstrans.tcp`
    operations (built with the `tcp` cargo feature, default-on).

    Deprecated alias (0.7.x only, removed in 0.8.0): `PAYLOAD_TOO_LARGE` →
    `TOO_LARGE`."""

    URL = 0
    IO = 1
    TOO_LARGE = 2
    PAYLOAD_TOO_LARGE = 2  # deprecated alias (0.7.x): use TOO_LARGE
    CLOSED = 3
    CONNECT_TIMEOUT = 4
    INVALID_CONFIG = 5
    TLS = 6
    TLS_DISABLED = 7
    BACKPRESSURE = 8
    BROKEN = 9


class TcpError(_KindMessageError):
    """Raised by `tstrans.tcp` operations. Discriminate via `.kind`."""

    kind: TcpErrorKind


class HlsErrorKind(enum.IntEnum):
    """`HlsError.kind` — the `tstrans.hls` subset of the Rust
    `BindingErrorKind` table; the `tst_hls::HlsErrorKind` variants plus
    `CLOSED` (0.7.0: `MuxPublisherError::Closed` — a call after the
    publisher was taken; folded into `FINISHED` before). Raised by
    `tstrans.hls` operations (built with the `hls` cargo feature,
    default-on)."""

    URL = 0
    IO = 1
    BIND_FAILED = 2
    INVALID_CONFIG = 3
    UNALIGNED_PUSH_TS = 4
    FINISHED = 5
    TLS_DISABLED = 6
    TLS = 7
    INTERNAL = 8
    CLOSED = 9


class HlsError(_KindMessageError):
    """Raised by `tstrans.hls` operations. Discriminate via `.kind`."""

    kind: HlsErrorKind


class RistErrorKind(enum.IntEnum):
    """`RistError.kind` — the `tstrans.rist` subset of the Rust
    `BindingErrorKind` table. `URL` / `FFI` / `INVALID_CONFIG` /
    `ENCRYPTION_DISABLED` / `CONTEXT_CREATE_FAILED` / `PEER_CREATE_FAILED`
    come from `tst_rist::RistErrorKind`; `CLOSED` / `BROKEN` /
    `BACKPRESSURE` (recv deadline expired — retryable) / `TOO_LARGE` from
    the transport. Raised by `tstrans.rist` operations (built with the
    `rist` cargo feature, default-on).

    Deprecated aliases (0.7.x only, removed in 0.8.0): `PAYLOAD_TOO_LARGE`
    → `TOO_LARGE`, `RECV_TIMEOUT` → `BACKPRESSURE`, `IO` → `BROKEN`
    (librist has no generic I/O kind of its own)."""

    URL = 0
    FFI = 1
    TOO_LARGE = 2
    PAYLOAD_TOO_LARGE = 2  # deprecated alias (0.7.x): use TOO_LARGE
    CLOSED = 3
    INVALID_CONFIG = 4
    ENCRYPTION_DISABLED = 5
    CONTEXT_CREATE_FAILED = 6
    PEER_CREATE_FAILED = 7
    BACKPRESSURE = 8
    RECV_TIMEOUT = 8  # deprecated alias (0.7.x): use BACKPRESSURE
    BROKEN = 9
    IO = 9  # deprecated alias (0.7.x): use BROKEN


class RistError(_KindMessageError):
    """Raised by `tstrans.rist` operations. Discriminate via `.kind`."""

    kind: RistErrorKind


class CodecError(TstError):
    """Codec parser failure. See `CodecErrorKind` for the variant set.

    Variant-specific optional attributes:
    - `offset_bits` / `needed_bits` on TRUNCATED_RBSP / INVALID_GOLOMB
    - `field` / `value` on RESERVED_VALUE (Forbidden has only `field`)
    - `layer` on UNSUPPORTED_FREE_FORMAT
    - `profile_idc` on UNSUPPORTED_PROFILE
    - `sps_id` on DANGLING_SPS_REFERENCE
    - `vps_id` on DANGLING_VPS_REFERENCE
    - `offset_bytes` on INVALID_LEB128 / TRUNCATED
    - `expected` / `found` on BAD_SYNC_WORD
    - `needed` / `had` on TRUNCATED
    - `got` on INVALID_LENGTH_SIZE
    - `nal_len` / `length_size` on NAL_LENGTH_OVERFLOW
    """

    def __init__(
        self,
        kind: CodecErrorKind,
        codec: str,
        message: str,
        *,
        offset_bits: Optional[int] = None,
        needed_bits: Optional[int] = None,
        field: Optional[str] = None,
        value: Optional[int] = None,
        profile_idc: Optional[int] = None,
        sps_id: Optional[int] = None,
        vps_id: Optional[int] = None,
        offset_bytes: Optional[int] = None,
        expected: Optional[int] = None,
        found: Optional[int] = None,
        needed: Optional[int] = None,
        had: Optional[int] = None,
        layer: Optional[int] = None,
        got: Optional[int] = None,
        nal_len: Optional[int] = None,
        length_size: Optional[int] = None,
    ) -> None:
        super().__init__(f"{codec}: {message}")
        self.kind = kind
        self.codec = codec
        self.message = message
        self.offset_bits = offset_bits
        self.needed_bits = needed_bits
        self.field = field
        self.value = value
        self.profile_idc = profile_idc
        self.sps_id = sps_id
        self.vps_id = vps_id
        self.offset_bytes = offset_bytes
        self.expected = expected
        self.found = found
        self.needed = needed
        self.had = had
        self.layer = layer
        self.got = got
        self.nal_len = nal_len
        self.length_size = length_size


__all__ = [
    "TstError",
    "MuxError",
    "MuxErrorKind",
    "DemuxError",
    "DemuxErrorKind",
    "KlvError",
    "KlvErrorKind",
    "KlvEncodeError",
    "KlvEncodeErrorKind",
    "CodecError",
    "CodecErrorKind",
    "RtspError",
    "RtspErrorKind",
    "RtpError",
    "RtpErrorKind",
    "SrtError",
    "SrtErrorKind",
    "UdpErrorKind",
    "UdpError",
    "TcpErrorKind",
    "TcpError",
    "HlsErrorKind",
    "HlsError",
    "RistErrorKind",
    "RistError",
]
