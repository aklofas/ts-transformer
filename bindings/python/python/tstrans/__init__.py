"""tstrans — Python bindings for the ts-transformer MPEG-TS + KLV + codec parsers.

This package re-exports types from the compiled `tstrans._native` extension
module, organized into topic-focused submodules:

- `tstrans.mpegts` — MPEG-TS packet / PES / PSI / mux / demux
- `tstrans.klv` — KLV typed sets (ST 0601, ST 0102, ST 0903, ...)
- `tstrans.codec` — H.264 / H.265 / H.266 / AV1 / AAC frame parsers
- `tstrans.io` — convenience helpers for reading and writing .ts files
- `tstrans.exceptions` — exception hierarchy raised by the above
- `tstrans.pipeline` — KLV ↔ video PTS pairing (byte-feeding `Pairer`)

See the per-submodule docstrings for usage.
"""

from tstrans import _native, codec, exceptions, io, klv, mpegts
from tstrans.exceptions import TstError

# `rtp` import is conditional: the `tstrans.rtp` submodule raises a
# friendly ImportError when the `rtp` cargo feature was off at build
# time (default-on, so published wheels always include it). We import
# it unconditionally here to surface the failure at package-import
# time rather than first-use time.
try:
    from tstrans import rtp  # noqa: F401
    _RTP_AVAILABLE = True
except ImportError:
    _RTP_AVAILABLE = False

# `srt` import is conditional on the `srt` cargo feature (default-on,
# so published wheels always include it). Same shape as `_RTP_AVAILABLE`
# above — surface the failure at package-import time rather than
# first-use time.
try:
    from tstrans import srt  # noqa: F401
    _SRT_AVAILABLE = True
except ImportError:
    _SRT_AVAILABLE = False

# udp / tcp / hls / rist submodules (each default-on; same
# conditional-import shape as rtp/srt to surface a feature-off build at
# package-import time).
try:
    from tstrans import udp  # noqa: F401
    _UDP_AVAILABLE = True
except ImportError:
    _UDP_AVAILABLE = False
try:
    from tstrans import tcp  # noqa: F401
    _TCP_AVAILABLE = True
except ImportError:
    _TCP_AVAILABLE = False
try:
    from tstrans import hls  # noqa: F401
    _HLS_AVAILABLE = True
except ImportError:
    _HLS_AVAILABLE = False
try:
    from tstrans import rist  # noqa: F401
    _RIST_AVAILABLE = True
except ImportError:
    _RIST_AVAILABLE = False

# `pipeline` import is conditional on the `pipeline` cargo feature
# (default-on; published wheels always include it). Same shape as rtp/srt.
try:
    from tstrans import pipeline  # noqa: F401
    _PIPELINE_AVAILABLE = True
except ImportError:
    _PIPELINE_AVAILABLE = False

__version__: str = _native.__version__

__all__: list[str] = [
    "__version__",
    "TstError",
    "codec",
    "exceptions",
    "io",
    "klv",
    "mpegts",
]
if _RTP_AVAILABLE:
    __all__.append("rtp")
if _SRT_AVAILABLE:
    __all__.append("srt")
if _UDP_AVAILABLE:
    __all__.append("udp")
if _TCP_AVAILABLE:
    __all__.append("tcp")
if _HLS_AVAILABLE:
    __all__.append("hls")
if _RIST_AVAILABLE:
    __all__.append("rist")
if _PIPELINE_AVAILABLE:
    __all__.append("pipeline")
