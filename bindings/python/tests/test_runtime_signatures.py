"""Runtime `inspect.signature` introspection of native constructors (v0.2.0 #8).

PyO3 auto-generates class-level `__text_signature__` from `#[new]`
(+ `#[pyo3(signature)]`), so `inspect.signature(cls)` matches the .pyi
stubs with NO explicit text_signature attributes in src/*.rs. These
tests lock that contract: they fail if a future `#[new]` rewrite or a
PyO3 upgrade silently drops constructor introspection. (`cls.__init__` /
`cls.__new__` still report `(*args, **kwargs)` — a CPython slot-wrapper
artifact; the class-level signature is what `inspect` and IDEs use.)"""

import inspect

from tstrans.mpegts import (
    Demuxer,
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
)


def _params(cls):
    return list(inspect.signature(cls).parameters.values())


def test_muxer_program_config_builder_signature():
    p = _params(MuxerProgramConfigBuilder)
    assert [x.name for x in p] == ["program_number", "pmt_pid"]
    assert all(x.default is inspect.Parameter.empty for x in p)


def test_muxer_config_builder_signature():
    assert _params(MuxerConfigBuilder) == []


def test_muxer_signature():
    assert [x.name for x in _params(Muxer)] == ["config"]


def test_demuxer_signature():
    p = _params(Demuxer)
    assert [x.name for x in p] == ["config"]
    assert p[0].default is None
