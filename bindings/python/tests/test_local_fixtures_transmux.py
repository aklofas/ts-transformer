"""Transmux fidelity over local capture files.

Scans `tests/local_fixtures/*.ts` — a gitignored, machine-local
directory — and proves that private/application data streams (PMT
"Unknown" stream kinds) survive `tio.transmux` byte-faithfully:
identical PMT identity (raw stream_type + descriptor bytes) and
identical per-PID ordered `(pts, payload-digest)` sample lists.

Skips cleanly when no local capture files are present (the CI case).

Design notes:
- Per-PID collect-then-compare: output interleaving ACROSS PIDs is
  muxer-scheduled, so a globally-ordered event sequence is not stable;
  within a single PID order IS preserved, so ordered per-PID lists are.
- EVENT-level comparison, not TS-byte-level: a source stream without
  PTS gains one on the output (re-muxed data streams always carry
  PTS), so TS bytes legitimately differ. Both sides' demuxers
  substitute pts=0 for PTS-less samples, so `(pts, payload)` tuples
  still match.
- Payloads are compared via sha256 digests so memory stays bounded on
  multi-GB captures; the demuxer is fed in ~1 MB chunks and drained as
  we go (there is a 4 MB sync-buffer ceiling).
- Out-of-scope fixtures (multi-program sources — transmux v1 raises
  ValueError; streams the strict muxer cannot represent — MuxError)
  skip rather than fail, per-file via parametrization so one skip
  never masks another file.
"""
from __future__ import annotations

import hashlib
from pathlib import Path

import pytest

import tstrans.io as tio
from tstrans.exceptions import MuxError
from tstrans.mpegts import DemuxEvent, Demuxer, DemuxerConfig, StreamKindTag

_FIXTURE_DIR = Path(__file__).parent / "local_fixtures"
_CHUNK_SIZE = 1024 * 1024

_FILES = sorted(_FIXTURE_DIR.glob("*.ts")) if _FIXTURE_DIR.is_dir() else []

# Generic parametrize ids (an index) — local capture filenames must never
# appear in test output.
if _FILES:
    _PARAMS = [pytest.param(p, id=f"capture{i}") for i, p in enumerate(_FILES)]
else:
    _PARAMS = [
        pytest.param(
            None,
            id="absent",
            marks=pytest.mark.skip(reason="no local capture files present"),
        )
    ]


def _unknown_inventory(path: Path):
    """Per-PID inventory of Unknown streams in a TS file.

    Returns `(pmt, samples)` where
      pmt:     {pid: (stream_type, ((descriptor_tag, descriptor_bytes), ...))}
      samples: {pid: [(pts_raw, sha256_digest), ...]}  in emission order
    """
    pmt: dict[int, tuple[int, tuple[tuple[int, bytes], ...]]] = {}
    samples: dict[int, list[tuple[int, bytes]]] = {}

    dx = Demuxer(DemuxerConfig())

    def drain() -> None:
        while (ev := dx.next_event()) is not None:
            if isinstance(ev, DemuxEvent.ProgramMap):
                for prog in ev.programs:
                    for s in prog.streams:
                        if s.kind is StreamKindTag.UNKNOWN:
                            # Dict-overwrite on re-emission is safe only
                            # because Transmuxer raises ValueError on any
                            # non-identical second ProgramMap — for a
                            # layout-changing source the comparison below
                            # is never reached (the file skips instead).
                            pmt[s.pid] = (
                                s.stream_type,
                                tuple(
                                    (d.tag, bytes(d.data))
                                    for d in s.raw_descriptors
                                ),
                            )
            elif isinstance(ev, DemuxEvent.UnknownSample):
                samples.setdefault(ev.stream.pid, []).append(
                    (
                        ev.pts.raw,
                        hashlib.sha256(bytes(ev.payload)).digest(),
                    )
                )

    with open(path, "rb") as f:
        while chunk := f.read(_CHUNK_SIZE):
            dx.feed(chunk)
            drain()
    dx.flush()
    drain()
    return pmt, samples


# Multi-GB captures need three demux passes + a GB-scale mux write —
# far beyond the suite's 60 s default timeout. Generous (not disabled)
# so a genuine hang still dies.
@pytest.mark.timeout(3600)
@pytest.mark.parametrize("src", _PARAMS)
def test_unknown_streams_survive_transmux(src: Path, tmp_path: Path) -> None:
    src_pmt, src_samples = _unknown_inventory(src)

    out = tmp_path / "out.ts"
    try:
        with tio.transmux(src, out) as tx:
            for ev in tx:
                tx.write(ev)
    except (ValueError, MuxError) as exc:
        # The exception text names programs/streams/kinds, never file
        # paths — safe to surface, and it distinguishes a genuine
        # out-of-scope fixture from a push-path regression.
        pytest.skip(f"fixture out of transmux v1 scope: {exc}")

    if not out.exists():
        # No ProgramMap in the source → transmux never opened the sink;
        # there can have been no Unknown streams either.
        assert src_pmt == {} and src_samples == {}
        return

    out_pmt, out_samples = _unknown_inventory(out)

    # PMT identity per PID: raw stream_type byte + verbatim descriptor loop.
    assert out_pmt == src_pmt
    # Per-PID ordered samples: count, per-sample pts, payload digest.
    assert out_samples == src_samples
