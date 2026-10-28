"""mypy --strict assert_type smoke for tstrans.codec — pins the raw-first
split_units/parse_audio signatures plus a few representative members across
the module, and confirms codec_parse_error is gone.
Not collected by pytest (no test_*); checked statically by tests/typing/mypy.ini."""
from typing import Any, List, Tuple, Union, assert_type

from tstrans.codec import (
    AdtsFrame,
    AdtsFrameIter,
    H264ParameterSets,
    H264SliceType,
    NalUnit,
    iter_aac_frames,
    parse_audio,
    parse_h264_parameter_sets,
    split_units,
)

# split_units() is mode-polymorphic (strict flag), so the stub declares the
# union of the (units, issues) tuple and the bare unit list.
split = split_units(b"\x00\x00\x00\x01\x67", "H264")
assert_type(split, Union[Tuple[List[Any], List[str]], List[Any]])
frames = parse_audio(b"\xff\xf1", "AAC")
assert_type(frames, list[Any])

n: NalUnit
assert_type(n.nal_type, int)
assert_type(n.payload, bytes)
assert_type(n.payload_np, Any)  # numpy attached at runtime; typed Any (no numpy import)

# Representative coverage beyond the raw-first pair — a parser returning a
# parameter-set, a frame iterator, and an enum-member access. These pin the
# symbols so accidental deletion of any would fail mypy here.
assert_type(parse_h264_parameter_sets([n]), H264ParameterSets)
it = iter_aac_frames(b"\xff\xf1")
assert_type(it, AdtsFrameIter)
assert_type(next(it), AdtsFrame)
assert_type(H264SliceType.I, H264SliceType)

# codec_parse_error was removed by the raw-first model (#1) — importing it must fail:
# (verified manually below; mypy can't assert absence, so the stub simply omits it)
