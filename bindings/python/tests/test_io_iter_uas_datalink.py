"""tio.iter_uas_datalink — typed ST 0601 iterator with file-order KLV indices (v0.2.0 #5)."""

from pathlib import Path

import pytest

from tstrans.exceptions import KlvError
from tstrans.io import iter_uas_datalink
from tstrans.klv import UasDatalinkLs, encode_uas_datalink
from tstrans.mpegts import (
    DemuxerConfig,
    KlvStreamType,
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)

# Minimal H.264 access-unit delimiter NAL — enough for the muxer to
# accept a video push so the program has a PCR-bearing stream.
_NAL_AUD = b"\x00\x00\x00\x01\x09\xF0"


def _build_ts(klv_payloads, tmp_path: Path) -> Path:
    """Mux a tiny .ts with one video AUD plus the given raw KLV LS
    payloads on a SYNCHRONOUS_METADATA stream (push_klv passes them
    through verbatim under the H.222.0 AU cell auto-wrap contract)."""
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )
    cfg = MuxerConfigBuilder().add_program(prog).build()
    path = tmp_path / "klv.ts"
    m = Muxer(cfg)
    with m.write_file(path) as proxy:
        proxy.push_video(_NAL_AUD, pts=Pts90khz.from_raw(900_000))
        for i, payload in enumerate(klv_payloads):
            proxy.push_klv(payload, pts=Pts90khz.from_raw(900_000 + i * 9_000))
    return path


def test_yields_typed_records_with_pts_and_index(tmp_path):
    a = encode_uas_datalink(UasDatalinkLs(mission_id="A"))
    b = encode_uas_datalink(UasDatalinkLs(mission_id="B"))
    rows = list(iter_uas_datalink(_build_ts([a, b], tmp_path)))
    assert [(r[1], r[2].mission_id) for r in rows] == [(0, "A"), (1, "B")]
    assert all(isinstance(r[2], UasDatalinkLs) for r in rows)
    assert [r[0].raw for r in rows] == [900_000, 909_000]


def test_skips_non_st0601_but_counts_index(tmp_path):
    from _builders.synthetic_klv_universal import synthetic_security_ls

    a = encode_uas_datalink(UasDatalinkLs(mission_id="A"))
    c = encode_uas_datalink(UasDatalinkLs(mission_id="C"))
    rows = list(
        iter_uas_datalink(_build_ts([a, synthetic_security_ls(), c], tmp_path))
    )
    # The ST 0102 record occupies index 1 — skipped but counted, so
    # indices line up with extract_klv output / a re-mux pass.
    assert [(r[1], r[2].mission_id) for r in rows] == [(0, "A"), (2, "C")]


def test_no_klv_yields_nothing():
    fixture = (
        Path(__file__).parent.parent.parent.parent
        / "crates" / "tst-core" / "tests" / "fixtures" / "audio" / "mp2.ts"
    )
    assert fixture.is_file(), f"checked-in fixture missing: {fixture}"
    assert list(iter_uas_datalink(fixture)) == []


def test_accepts_config_kwarg(tmp_path):
    a = encode_uas_datalink(UasDatalinkLs(mission_id="A"))
    rows = list(iter_uas_datalink(_build_ts([a], tmp_path), config=DemuxerConfig()))
    assert len(rows) == 1


def test_strict_kwarg_forwarded_to_decoder(tmp_path, monkeypatch):
    # strict's only core-side effect today (the ST 0601 family UL
    # pattern) is subsumed by this iterator's own family filter, so
    # prove the thread-through with a spy rather than a behavioral
    # fixture.
    import tstrans.klv as klv_mod

    a = encode_uas_datalink(UasDatalinkLs(mission_id="A"))
    path = _build_ts([a], tmp_path)
    seen = []
    real = klv_mod.decode_uas_datalink

    def spy(buf, **kw):
        seen.append(kw)
        return real(buf, **kw)

    monkeypatch.setattr(klv_mod, "decode_uas_datalink", spy)
    rows = list(iter_uas_datalink(path, strict=True))
    assert len(rows) == 1
    assert seen and all(kw.get("strict") is True for kw in seen)


def test_malformed_record_raises(tmp_path):
    bad = encode_uas_datalink(UasDatalinkLs(mission_id="A"))[:-4]
    with pytest.raises(KlvError):
        list(iter_uas_datalink(_build_ts([bad], tmp_path)))


def test_short_payload_raises_instead_of_silent_skip(tmp_path):
    # A payload too short to carry a 16-byte UL is corruption, not an
    # identifiable "different set" — it must raise, not vanish
    # (the family filter alone would silently skip it).
    path = _build_ts([b"\x06\x0e\x2b\x34"], tmp_path)
    with pytest.raises(KlvError):
        list(iter_uas_datalink(path))
