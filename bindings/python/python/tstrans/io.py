"""tstrans.io — convenience helpers for reading + writing `.ts` files.

Read-side helpers:

- `parse_file(path, config=None)` → `Iterator[DemuxEvent]`
- `probe(path)` → `ProbeResult` summary
- `extract_klv(path, with_pts=False)` → iterator of KLV payloads
- `iter_uas_datalink(path)` → iterator of `(pts, klv_index, UasDatalinkLs)`

Read+write bridge:

- `transmux(src, dst)` → demux→edit→remux context manager (`Transmuxer`)

The write-side `Muxer.write_file(path)` context manager lives on the
`Muxer` class in `tstrans.mpegts`.
"""

from dataclasses import dataclass
from pathlib import Path
from typing import Iterator, Optional, Sequence, Union

from tstrans.mpegts import (
    AudioCodec,
    DemuxEvent,
    Demuxer,
    DemuxerConfig,
    Muxer,
    MuxerConfig,
    MuxerFileSink,
    Pts90khz,
    ProgramMap,
    StreamKindTag,
    SubtitleCodec,
    VideoCodec,
)


# Probe scans this many bytes from the start of the file. Enough for
# PSI + a handful of samples; tunable per call if a future caller needs
# more.
_PROBE_BYTES: int = 5 * 1024 * 1024

# Chunk size for feeding the demuxer. 64 KB is small enough to keep
# the iterator's memory footprint low and large enough to amortize the
# feed() call overhead.
_FEED_CHUNK: int = 64 * 1024


@dataclass(frozen=True, slots=True)
class ProbeResult:
    """First-N-MB scan summary. Cheaper than a full parse — enough for
    "what's in this file?" introspection."""

    size_bytes: int
    programs: tuple[ProgramMap, ...]
    pids: tuple[int, ...]
    video_codecs: tuple[VideoCodec, ...]
    audio_codecs: tuple[AudioCodec, ...]
    subtitle_codecs: tuple[SubtitleCodec, ...]
    has_klv: bool
    packet_count: int


def parse_file(
    path: Union[str, Path],
    config: Optional[DemuxerConfig] = None,
) -> Iterator[DemuxEvent]:
    """Open `path`, feed it to a `Demuxer` in chunks, and yield each
    `DemuxEvent` as it becomes available. Caller is responsible for
    iterating to completion (or stopping early — chunks are read on
    demand).

    Raises `tstrans.exceptions.DemuxError` when the demuxer rejects a
    non-conformance (strict modes) or when the byte stream is
    unrecoverable. I/O failures during file reading propagate as the
    underlying `OSError` from the `read` call. This matches the
    contract of Rust's `tst_core::io_file::TryDemuxFromFile` (the
    fallible streaming iterator) — both surface read and demux errors
    rather than coercing them to early EOF. The full file-helper error
    policy is documented in the module-level rustdoc of
    `tst_core::io_file`.
    """

    p = Path(path)
    d = Demuxer(config) if config is not None else Demuxer()
    with p.open("rb") as f:
        while True:
            chunk = f.read(_FEED_CHUNK)
            if not chunk:
                break
            d.feed(chunk)
            # Drain available events between feeds. This keeps the
            # demuxer's queue from growing unboundedly while we read.
            while True:
                ev = d.next_event()
                if ev is None:
                    break
                yield ev
    d.flush()
    while True:
        ev = d.next_event()
        if ev is None:
            break
        yield ev


def probe(
    path: Union[str, Path],
    *,
    config: Optional[DemuxerConfig] = None,
) -> ProbeResult:
    """Scan the first `_PROBE_BYTES` of a file and summarize. Returns
    a `ProbeResult` with program list, PID list, codec sets, and KLV
    presence. Does NOT compute duration (would require a full scan
    plus PCR analysis).

    Pass `config` to use a non-default demuxer configuration (e.g.
    `DemuxerConfig(cfi_tolerance=False)` for spec-strict conformance
    testing — the default is tolerance-on, which rescues KLV from
    producer-malformed AU cell CFI bits while still emitting the
    `CfiTolerated` diagnostic). See `tstrans.mpegts.DemuxerConfig`
    for the full knob list.
    """

    p = Path(path)
    size = p.stat().st_size
    d = Demuxer(config) if config is not None else Demuxer()
    read_total = 0
    with p.open("rb") as f:
        while read_total < _PROBE_BYTES:
            chunk = f.read(_FEED_CHUNK)
            if not chunk:
                break
            d.feed(chunk)
            read_total += len(chunk)
    d.flush()

    programs: list[ProgramMap] = []
    pids: set[int] = set()
    video_codecs: set[VideoCodec] = set()
    audio_codecs: set[AudioCodec] = set()
    subtitle_codecs: set[SubtitleCodec] = set()
    has_klv = False

    while True:
        ev = d.next_event()
        if ev is None:
            break
        if isinstance(ev, DemuxEvent.ProgramMap):
            for pm in ev.programs:
                programs.append(pm)
                for s in pm.streams:
                    pids.add(s.pid)
                    if s.kind is StreamKindTag.VIDEO and s.codec is not None:
                        video_codecs.add(s.codec)
                    elif s.kind is StreamKindTag.AUDIO and s.codec is not None:
                        audio_codecs.add(s.codec)
                    elif s.kind is StreamKindTag.SUBTITLE and s.codec is not None:
                        subtitle_codecs.add(s.codec)
                    elif s.kind in (StreamKindTag.KLV_SYNC, StreamKindTag.KLV_ASYNC):
                        has_klv = True

    # Compute packet_count from bytes actually read during the probe scan.
    # The demuxer doesn't expose a "TS packets fed" counter (see Rust
    # `DemuxerStats` — its fields are PSI/discontinuity/nonconformant
    # counts, not per-packet). Bytes-read / 188 is exact for any properly
    # 188-aligned TS file; an off-by-one in the last fragment is
    # acceptable since `read_total` is read in 64 KiB chunks (which are
    # always multiples of 188 ÷ chunk only when the underlying file is
    # 188-aligned, which valid TS files are). For files larger than the
    # 5 MiB probe budget, `packet_count` reflects packets in the scanned
    # prefix, matching the "first-N-MB scan summary" docstring.
    packet_count = read_total // 188

    return ProbeResult(
        size_bytes=size,
        programs=tuple(programs),
        pids=tuple(sorted(pids)),
        video_codecs=tuple(sorted(video_codecs, key=lambda c: c.name)),
        audio_codecs=tuple(sorted(audio_codecs, key=lambda c: c.name)),
        subtitle_codecs=tuple(sorted(subtitle_codecs, key=lambda c: c.name)),
        has_klv=has_klv,
        packet_count=packet_count,
    )


def extract_klv(
    path: Union[str, Path],
    *,
    with_pts: bool = False,
    parsed: bool = False,
    skip_unknown: bool = True,
    skip_malformed: bool = False,
    config: Optional[DemuxerConfig] = None,
) -> Iterator:
    """Iterate over KLV payloads in a file. Yields one of:

    - `bytes` (default — `with_pts=False, parsed=False`)
    - `(Pts90khz, bytes)` (when `with_pts=True, parsed=False`)
    - typed `UasDatalinkLs | SecurityLs | PrecisionTimeStampPack | VmtiLs`
      (when `parsed=True, with_pts=False`)
    - `(Pts90khz, typed)` (when `parsed=True, with_pts=True`)

    With `parsed=True`, each payload is run through
    `tstrans.klv.parse_klv_universal`. Two independent error knobs
    control how the iterator reacts:

    - `skip_unknown` (default `True`) — controls payloads whose
      universal label is not recognized by any of the four supported
      sets (ST 0601 / ST 0102 / ST 0605 / ST 0903). When True, such
      payloads are silently dropped. When False, `None` (or
      `(pts, None)`) is yielded so the caller can count or log them.
    - `skip_malformed` (default `False`) — controls
      `tstrans.exceptions.KlvError` raised by the decoder on a
      recognized UL (truncated set, bad checksum, etc.). When False
      (the default), the exception propagates so data corruption is
      not silently lost. When True, the offending row is skipped.

    These two knobs are independent on purpose: a single overload would
    swallow both unknown ULs and `KlvError` from known ULs, hiding
    decoder bugs and upstream corruption. Catching `KlvError`
    specifically (rather than
    bare `Exception`) also lets binding-shape regressions (TypeError,
    AttributeError) surface naturally instead of being suppressed.

    Pass `config` to use a non-default demuxer configuration. CFI
    tolerance is **on by default** — the demuxer rescues complete KLV
    records from sync-metadata streams whose encoders set the AU cell
    `cell_fragment_indication` bits to `0b00` (Middle) or `0b01`
    (Last) for what are actually single complete records (a
    corpus-dominant real-world malformation pattern), while still
    emitting the `CfiTolerated` diagnostic so the malformation
    remains visible. Set `DemuxerConfig(cfi_tolerance=False)` for
    spec-strict conformance testing. See
    `tstrans.mpegts.DemuxerConfig` for the full knob list.
    """

    # Local imports dodge import-cycle with tstrans.klv at module load.
    from tstrans.exceptions import KlvError
    from tstrans.klv import parse_klv_universal

    for ev in parse_file(path, config=config):
        if not isinstance(ev, DemuxEvent.Metadata):
            continue
        if parsed:
            try:
                typed = parse_klv_universal(ev.payload)
            except KlvError:
                if skip_malformed:
                    continue
                raise
            if typed is None and skip_unknown:
                continue
            yield (ev.pts, typed) if with_pts else typed
        else:
            yield (ev.pts, ev.payload) if with_pts else ev.payload


def iter_uas_datalink(
    path: Union[str, Path],
    *,
    strict: bool = False,
    config: Optional[DemuxerConfig] = None,
) -> "Iterator[tuple[Pts90khz, int, UasDatalinkLs]]":
    """Iterate typed MISB ST 0601 records in a file. Yields
    `(pts, klv_index, record)` tuples where `record` is a decoded
    `tstrans.klv.UasDatalinkLs`.

    `klv_index` is the 0-based ordinal of the KLV event within the
    file's full KLV event sequence — it counts EVERY `DemuxEvent.Metadata`
    (including the non-ST-0601 records this iterator skips), so
    indices line up with `extract_klv(path)` output and with the KLV
    events seen during a re-mux pass over the same file.

    Records whose universal label is outside the ST 0601 family are
    skipped (use `extract_klv(parsed=True)` for the multi-set
    variant). A payload too short to carry a 16-byte universal label
    cannot be identified as ANY set and raises
    `tstrans.exceptions.KlvError` — corruption is not silently
    dropped. `strict` is forwarded to
    `tstrans.klv.decode_uas_datalink`; today its only extra check
    (the ST 0601 family UL pattern — bytes 13/14 are tolerated for
    legacy interop) is subsumed by this iterator's own family filter,
    so it is forward-compat surface for stricter core decode modes. A
    structurally malformed ST 0601 record likewise raises `KlvError`
    in BOTH modes; per-field decode issues land on
    `record.field_errors` (lenient) instead of raising.

    Pass `config` to use a non-default demuxer configuration (e.g.
    `DemuxerConfig(cfi_tolerance=False)` for spec-strict conformance
    testing).
    """

    # Local imports dodge import-cycle with tstrans.klv at module load.
    from tstrans.exceptions import KlvError, KlvErrorKind
    from tstrans.klv import decode_uas_datalink, is_st0601_family

    klv_index = -1
    for ev in parse_file(path, config=config):
        if not isinstance(ev, DemuxEvent.Metadata):
            continue
        klv_index += 1
        if len(ev.payload) < 16:
            # Too short to carry a UL: not identifiable as any set, so
            # the "skip non-0601" rule cannot apply — this is corruption
            # and must not vanish (mirrors parse_klv_universal).
            raise KlvError(
                kind=KlvErrorKind.BAD_UNIVERSAL_LABEL,
                message=(
                    f"KLV event {klv_index}: payload too short for a "
                    f"16-byte UL: have {len(ev.payload)} bytes"
                ),
            )
        if not is_st0601_family(ev.payload):
            continue
        yield (ev.pts, klv_index, decode_uas_datalink(ev.payload, strict=strict))


def transmux(
    src: Union[str, Path],
    dst: Union[str, Path],
    *,
    drop: Sequence[StreamKindTag] = (),
    atomic: bool = False,
    config: Optional[DemuxerConfig] = None,
) -> "Transmuxer":
    """Open a demux→remux bridge from `src` to `dst`: iterate demux
    events and write back the ones to keep — byte-faithful for
    everything you don't edit.

        with tio.transmux("in.ts", "out.ts") as tx:
            for ev in tx:
                if isinstance(ev, DemuxEvent.Metadata):
                    tx.write_klv(ev, klv.patch_uas_datalink(ev.payload, {...}))
                else:
                    tx.write(ev)

    The output muxer is constructed lazily from the FIRST `ProgramMap`
    event via `MuxerConfig.from_program_map(pm, drop=drop)`, so `dst`
    reproduces the source program topology (program number, PMT PID,
    stream PIDs, codecs). `dst` is created at that moment — a source
    with no PSI yields no events and produces NO output file.

    Strict by default: streams the muxer cannot represent (DVB
    subtitling/teletext) fail the conversion with `MuxError` naming the
    offenders. Private/application data streams (unknown stream types)
    pass through byte-faithfully: `from_program_map` reproduces their
    PMT entry (raw stream_type byte + descriptor loop verbatim) and
    each `UnknownSample` payload is re-emitted as-is via
    `push_data_to`. One nuance: re-muxed data streams always carry
    PTS, and the demuxer substitutes 0 for a PTS-less source PES — so
    a source sample with no PTS re-emerges with a literal PTS of 0.
    Pass `drop=[StreamKindTag.UNKNOWN]` to exclude data streams
    instead; events for dropped streams are skipped by `write`. v1
    supports single-program sources with a stable program map — a
    second program (or a mid-stream layout change) raises
    `ValueError`.

    `atomic=True` routes the output through the `MuxerFileSink`
    temp-file machinery: `dst` appears only on clean exit (`os.replace`
    from a same-directory `*.partial`); on an exception nothing is left
    at the destination.

    `config` — optional `DemuxerConfig` forwarded to the source demuxer.
    Use `DemuxerConfig(av1_carriage=Av1CarriageMode.INTEROP_RAW_OBU)` when
    the source was produced by ffmpeg / libaom / mediamtx interop tools
    so the demuxer uses the right AV1 framing expectation and the
    destination muxer is configured to match.
    """

    return Transmuxer(src, dst, drop=drop, atomic=atomic, config=config)


class Transmuxer:
    """Iterable + writable transmux bridge — construct via `transmux()`.

    Iteration yields every `DemuxEvent` from the source, single-pass
    (file-object semantics: `iter()` always returns the same generator).
    `write(ev)` copies an event to the output; `write_klv(ev, bytes)`
    substitutes a KLV payload. Both push through the sink's drain proxy,
    so TS packets stream to disk per write (the `write_file` drain
    contract — pushes routed anywhere else would overflow the muxer's
    packet buffer).

    Per-event dispatch — `handle` is the muxer stream handle resolved
    internally from the event's source PID (see `transmux` for the
    construction story):

    - Video → `push_video_wire_to(handle, raw, pts, dts, key_frame)`;
      emits the demuxed bytes verbatim (no re-wrapping). For AV1
      binding-mode this avoids the double-wrap that would otherwise
      produce an empty AU. `dts=None` is preserved as a
      PTS-only PES, not coerced to dts=pts.
    - Audio → `push_audio_to(handle, raw, pts)` (the mux push API
      carries no audio dts; dts≠pts audio does not occur for
      MP2/AAC/AC-3).
    - Klv → `push_klv_to(handle, payload, pts)`. Payloads are raw KLV
      LS bytes in BOTH directions: the demuxer strips the sync-metadata
      AU-cell header and the muxer re-wraps it — never pre-wrap. The AU
      cell's `metadata_service_id` is not recoverable from the demux
      event, so the muxer default applies on the way out.
    - Subtitle → `push_subtitle_to(handle, payload, pts)` (kept
      CEA-708/WebVTT streams; DVB offenders must be dropped or fail
      config).
    - UnknownSample → `push_data_to(handle, payload, pts)`. The payload
      is the raw PES payload, passed through verbatim (no framing, no
      AU-cell wrap). Re-muxed data streams always carry PTS and the
      demuxer substitutes 0 for a PTS-less source PES, so a source
      sample with no PTS re-emerges with a literal PTS of 0.
    - `ProgramMap` / `Discontinuity` / `NonConformant` /
      `ReconnectDiscontinuity` (no mux representation) are accepted
      and skipped, as are sample events for dropped streams.

    Writing a sample event before the first `ProgramMap` has been
    iterated, or after the `with` block exits, raises `RuntimeError`.
    """

    __slots__ = (
        "_src",
        "_dst",
        "_drop",
        "_atomic",
        "_config",
        "_events",
        "_pm",
        "_sink",
        "_proxy",
        "_handles",
        "_dropped_pids",
        "_closed",
    )

    # Events with no mux representation: accepted + skipped by write().
    _SKIP_EVENTS = (
        DemuxEvent.ProgramMap,
        DemuxEvent.Discontinuity,
        DemuxEvent.NonConformant,
        DemuxEvent.ReconnectDiscontinuity,
    )

    def __init__(
        self,
        src: Union[str, Path],
        dst: Union[str, Path],
        *,
        drop: Sequence[StreamKindTag] = (),
        atomic: bool = False,
        config: Optional[DemuxerConfig] = None,
    ) -> None:
        self._src = Path(src)
        self._dst = Path(dst)
        self._drop = tuple(drop)
        self._atomic = atomic
        self._config = config
        self._events = None  # shared single-pass generator
        self._pm = None  # first program's ProgramMap (identity anchor)
        self._sink = None  # MuxerFileSink, entered on first ProgramMap
        self._proxy = None  # MuxerDrainProxy — ALL pushes go through it
        self._handles = {}  # source pid -> muxer stream handle
        self._dropped_pids: frozenset = frozenset()  # intentionally-dropped PIDs
        self._closed = False

    def __enter__(self) -> "Transmuxer":
        if self._closed:
            # Re-entering would bypass the already-run teardown: the
            # next ProgramMap would open a fresh sink whose __exit__
            # never runs (stray *.partial / clobbered dst).
            raise RuntimeError(
                "transmux: closed — create a new transmuxer for another pass"
            )
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self._closed = True
        try:
            if self._events is not None:
                # Close the parse_file generator so the source file
                # handle is released deterministically, not at GC.
                self._events.close()
        finally:
            if self._sink is not None:
                # Final drain + close + atomic promote/unlink. The sink
                # never suppresses the in-flight exception.
                self._sink.__exit__(exc_type, exc, tb)

    def __iter__(self) -> Iterator[DemuxEvent]:
        if self._events is None:
            if self._closed:
                # A never-iterated-but-closed transmuxer must not start
                # demuxing now: the first ProgramMap would open a sink
                # that no __exit__ will ever clean up. (An exhausted
                # generator from inside the block is fine to hand back.)
                raise RuntimeError(
                    "transmux: closed — iterate inside the `with` block"
                )
            self._events = self._event_gen()
        return self._events

    def _event_gen(self) -> Iterator[DemuxEvent]:
        for ev in parse_file(self._src, config=self._config):
            if isinstance(ev, DemuxEvent.ProgramMap):
                self._on_program_map(ev)
            yield ev

    def _on_program_map(self, ev) -> None:
        if len(ev.programs) != 1:
            raise ValueError(
                f"transmux v1 supports single-program sources only; got a "
                f"ProgramMap event carrying {len(ev.programs)} programs"
            )
        pm = ev.programs[0]
        if self._pm is not None:
            # The demuxer emits ProgramMap once per fresh PMT version. A
            # content-identical re-emission (version bump, no layout
            # change) is harmless; anything else is a second program or
            # a mid-stream layout change — both out of v1 scope.
            if pm != self._pm:
                raise ValueError(
                    "transmux v1 supports single-program sources with a "
                    "stable program map; saw a second, different "
                    "ProgramMap (new program or mid-stream layout change)"
                )
            return
        # Build the output side. Order matters: nothing is recorded
        # until every fallible step succeeded, so a from_program_map
        # strictness error (or an unopenable dst) leaves no half-state.
        #
        # Thread the source AV1 carriage from the demuxer config so the
        # destination muxer writes the same framing (binding-mode or
        # interop-raw-OBU). For binding-mode sources (default) this is a
        # no-op (from_program_map already defaults to MPEG2_TS_BINDING).
        src_carriage = self._config.av1_carriage if self._config is not None else None
        config = MuxerConfig.from_program_map(
            pm, drop=list(self._drop), av1_carriage=src_carriage
        )
        muxer = Muxer(config)
        sink = MuxerFileSink(muxer, self._dst, atomic=self._atomic)
        proxy = sink.__enter__()
        # Record the sink FIRST: from here on, __exit__ owns its
        # cleanup even if a later step (or future insertion) raises.
        self._sink = sink
        self._pm = pm
        self._proxy = proxy
        self._dropped_pids = frozenset(
            s.pid for s in pm.streams if s.kind in set(self._drop)
        )
        self._handles = self._build_handles(pm, muxer)

    def _build_handles(self, pm, muxer) -> dict:
        """Map source PIDs to muxer stream handles, positionally per
        kind: `from_program_map` adds streams in PMT order within each
        kind (skipping dropped kinds), and the muxer's per-kind handle
        lists are in add order — so zip() pairs them faithfully.

        Guards:
        - Per-kind: asserts len(pids) == len(handles) so zip() never
          silently truncates a retained stream (internal invariant: both
          sides derive from the same ProgramMap).
        - Partition total: asserts every kept stream was routed to a
          category (guards a future unrouted StreamKindTag value).
        """

        dropped = set(self._drop)
        kept = [s for s in pm.streams if s.kind not in dropped]
        video = [s.pid for s in kept if s.kind is StreamKindTag.VIDEO]
        audio = [s.pid for s in kept if s.kind is StreamKindTag.AUDIO]
        klv = [
            s.pid
            for s in kept
            if s.kind in (StreamKindTag.KLV_SYNC, StreamKindTag.KLV_ASYNC)
        ]
        subtitle = [s.pid for s in kept if s.kind is StreamKindTag.SUBTITLE]
        data = [s.pid for s in kept if s.kind is StreamKindTag.UNKNOWN]

        # Partition-totality guard: every kept stream must appear in exactly
        # one category.  If a future StreamKindTag value is added without
        # updating the per-kind lists above, this assertion fires at setup
        # rather than silently dropping streams.
        routed = len(video) + len(audio) + len(klv) + len(subtitle) + len(data)
        if routed != len(kept):
            raise RuntimeError(
                f"transmux internal error: partition incomplete — "
                f"{len(kept)} kept streams but only {routed} were routed "
                f"to a kind category (unhandled StreamKindTag?)"
            )

        # Materialize handle lists once so we can length-check before zipping.
        vh = list(muxer.video_handles())
        ah = list(muxer.audio_handles())
        kh = list(muxer.klv_handles())
        sh = list(muxer.subtitle_handles())
        dh = list(muxer.data_handles())

        def _check(kind_name: str, pids: list, kind_handles: list) -> None:
            if len(pids) != len(kind_handles):
                raise RuntimeError(
                    f"transmux internal error: {kind_name} handle count mismatch — "
                    f"{len(pids)} PID(s) from ProgramMap but "
                    f"{len(kind_handles)} handle(s) from muxer "
                    f"(from_program_map per-kind order drifted from muxer handle lists)"
                )

        _check("video", video, vh)
        _check("audio", audio, ah)
        _check("klv", klv, kh)
        _check("subtitle", subtitle, sh)
        _check("data", data, dh)

        handles: dict = {}
        handles.update(zip(video, vh))
        handles.update(zip(audio, ah))
        handles.update(zip(klv, kh))
        handles.update(zip(subtitle, sh))
        handles.update(zip(data, dh))
        return handles

    def _handle_for(self, ev):
        """Resolve a sample event's muxer handle.

        Returns:
          - The handle if the PID is mapped (retained stream).
          - None if the PID is in _dropped_pids (intentional drop —
            caller skips).

        Raises:
          - RuntimeError on lifecycle misuse (closed / no ProgramMap yet).
          - RuntimeError if the PID is retained but has no mapping
            (internal invariant violation; should be caught by
            _build_handles, but guarded here as a second line of defence).
        """

        if self._closed:
            raise RuntimeError(
                "transmux: closed — write only inside the `with` block"
            )
        if self._proxy is None:
            raise RuntimeError(
                "transmux: no ProgramMap seen yet — iterate the "
                "transmuxer at least to the first ProgramMap event "
                "before writing sample events"
            )
        pid = ev.stream.pid
        if pid in self._handles:
            return self._handles[pid]
        if pid in self._dropped_pids:
            return None
        raise RuntimeError(
            f"transmux internal error: retained PID {pid} has no "
            f"destination handle (handle mapping is incomplete)"
        )

    def write(self, ev) -> None:
        """Copy one demux event to the output (see class docstring for
        the per-type dispatch). Accepts every `DemuxEvent` subclass;
        skips the ones with no mux representation and any event from a
        dropped stream."""

        if isinstance(ev, Transmuxer._SKIP_EVENTS):
            return
        if isinstance(ev, DemuxEvent.Metadata):
            self._push_klv(ev, ev.payload)
        elif isinstance(ev, DemuxEvent.Video):
            handle = self._handle_for(ev)
            if handle is None:
                return
            # Wire-push: emit `ev.raw` verbatim without re-wrapping. For AV1
            # binding-mode, `ev.raw` is already `ts_open_bitstream_unit()`-
            # framed; re-wrapping via `push_video_to_with_dts` would produce
            # an empty AU. For H.26x the wire bytes are identical
            # to what `push_video_to_with_dts` would write, so this is safe.
            self._proxy.push_video_wire_to(
                handle,
                ev.raw,
                pts=ev.pts,
                dts=ev.dts,
                key_frame=ev.random_access_indicator,
            )
        elif isinstance(ev, DemuxEvent.Audio):
            handle = self._handle_for(ev)
            if handle is None:
                return
            self._proxy.push_audio_to(handle, ev.raw, pts=ev.pts)
        elif isinstance(ev, DemuxEvent.Subtitle):
            handle = self._handle_for(ev)
            if handle is None:
                return
            self._proxy.push_subtitle_to(handle, ev.payload, pts=ev.pts)
        elif isinstance(ev, DemuxEvent.UnknownSample):
            handle = self._handle_for(ev)
            if handle is None:
                return
            # Raw PES payload pass-through. `ev.pts` is always set: the
            # demuxer substitutes 0 when the source PES carried no PTS.
            self._proxy.push_data_to(handle, ev.payload, pts=ev.pts)
        else:
            raise TypeError(
                f"transmux.write: unsupported event type "
                f"{type(ev).__name__!r} (expected a DemuxEvent)"
            )

    def write_klv(self, ev, new_bytes) -> None:
        """Write a KLV event with `new_bytes` substituted for its
        payload — the metadata-editing half of the transmux workflow
        (pair with `tstrans.klv.patch_uas_datalink` for byte-faithful
        tag edits). `new_bytes` is raw KLV LS bytes; sync-metadata
        AU-cell wrapping is the muxer's job — never pre-wrap."""

        if not isinstance(ev, DemuxEvent.Metadata):
            raise TypeError(
                f"transmux.write_klv: expected a DemuxEvent.Metadata, got "
                f"{type(ev).__name__!r}"
            )
        self._push_klv(ev, new_bytes)

    def _push_klv(self, ev, payload) -> None:
        handle = self._handle_for(ev)
        if handle is None:
            return
        self._proxy.push_klv_to(handle, payload, pts=ev.pts)


__all__: list[str] = [
    "parse_file",
    "probe",
    "extract_klv",
    "iter_uas_datalink",
    "transmux",
    "ProbeResult",
    "Transmuxer",
]
