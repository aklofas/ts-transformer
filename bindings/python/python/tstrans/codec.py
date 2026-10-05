"""tstrans.codec — codec frame parsers.

Wraps `tst_core::codec::*`. Decode-only; there are no encoders. NumPy
snapshot views (`.payload_np`, `.raw_rbsp_np`, `.raw_np`) are available
when NumPy is installed (the `[pandas]` extra provides it).

Exposes shared types (ChromaFormat, Rational, ColorInfo, colour
primaries, transfer characteristics, matrix coefficients), typed
NAL / OBU wrappers, and per-codec classes / parsers for
H.264 / H.265 / H.266 / AV1 / AAC / MPEG-2 audio.
"""

from tstrans import _native as _native_mod

ChromaFormat = _native_mod.ChromaFormat
ColorInfo = _native_mod.ColorInfo
ColourPrimaries = _native_mod.ColourPrimaries
MatrixCoefficients = _native_mod.MatrixCoefficients
NalUnit = _native_mod.NalUnit
Obu = _native_mod.Obu
ObuExtension = _native_mod.ObuExtension
Rational = _native_mod.Rational
TransferCharacteristics = _native_mod.TransferCharacteristics

# H.264
EntropyCodingMode = _native_mod.EntropyCodingMode
H264ParameterSets = _native_mod.H264ParameterSets
H264Pps = _native_mod.H264Pps
H264SliceHeaderLight = _native_mod.H264SliceHeaderLight
H264SliceType = _native_mod.H264SliceType
H264Sps = _native_mod.H264Sps
parse_h264_parameter_sets = _native_mod.parse_h264_parameter_sets
parse_h264_pps = _native_mod.parse_h264_pps
parse_h264_slice_header_light = _native_mod.parse_h264_slice_header_light
parse_h264_sps = _native_mod.parse_h264_sps

# H.265
H265ParameterSets = _native_mod.H265ParameterSets
H265Pps = _native_mod.H265Pps
H265ProfileTierLevel = _native_mod.H265ProfileTierLevel
H265SliceHeaderLight = _native_mod.H265SliceHeaderLight
H265SliceType = _native_mod.H265SliceType
H265Sps = _native_mod.H265Sps
H265Vps = _native_mod.H265Vps
parse_h265_parameter_sets = _native_mod.parse_h265_parameter_sets
parse_h265_pps = _native_mod.parse_h265_pps
parse_h265_slice_header_light = _native_mod.parse_h265_slice_header_light
parse_h265_sps = _native_mod.parse_h265_sps
parse_h265_vps = _native_mod.parse_h265_vps

# H.266
H266ParameterSets = _native_mod.H266ParameterSets
H266Pps = _native_mod.H266Pps
H266ProfileTierLevel = _native_mod.H266ProfileTierLevel
H266SliceHeaderLight = _native_mod.H266SliceHeaderLight
H266SliceType = _native_mod.H266SliceType
H266Sps = _native_mod.H266Sps
H266Vps = _native_mod.H266Vps
parse_h266_parameter_sets = _native_mod.parse_h266_parameter_sets
parse_h266_pps = _native_mod.parse_h266_pps
parse_h266_slice_header_light = _native_mod.parse_h266_slice_header_light
parse_h266_sps = _native_mod.parse_h266_sps
parse_h266_vps = _native_mod.parse_h266_vps

# AV1
Av1FrameHeaderLight = _native_mod.Av1FrameHeaderLight
Av1ObuStream = _native_mod.Av1ObuStream
Av1SequenceHeader = _native_mod.Av1SequenceHeader
parse_av1_frame_header_light = _native_mod.parse_av1_frame_header_light
parse_av1_obu_stream = _native_mod.parse_av1_obu_stream
parse_av1_sequence_header = _native_mod.parse_av1_sequence_header

# AAC
AacChannelLayout = _native_mod.AacChannelLayout
AacProfile = _native_mod.AacProfile
AdtsFrame = _native_mod.AdtsFrame
AdtsFrameIter = _native_mod.AdtsFrameIter
MpegVersion = _native_mod.MpegVersion
iter_aac_frames = _native_mod.iter_aac_frames
iter_aac_frames_with_resync = _native_mod.iter_aac_frames_with_resync
parse_aac_frames = _native_mod.parse_aac_frames
parse_aac_frames_with_resync = _native_mod.parse_aac_frames_with_resync

# MPEG-2 audio
ChannelMode = _native_mod.ChannelMode
Layer = _native_mod.Layer
Mpeg2AudioFrame = _native_mod.Mpeg2AudioFrame
Mpeg2AudioFrameIter = _native_mod.Mpeg2AudioFrameIter
Version = _native_mod.Version
iter_mpeg2_audio_frames = _native_mod.iter_mpeg2_audio_frames
iter_mpeg2_audio_frames_with_resync = _native_mod.iter_mpeg2_audio_frames_with_resync
parse_mpeg2_audio_frames = _native_mod.parse_mpeg2_audio_frames
parse_mpeg2_audio_frames_with_resync = _native_mod.parse_mpeg2_audio_frames_with_resync

# Opt-in ES parse functions
split_units = _native_mod.split_units
parse_audio = _native_mod.parse_audio

# MISP timestamp (ST 0604)
MispTimeKind = _native_mod.MispTimeKind
MispTimestamp = _native_mod.MispTimestamp
extract_misp_timestamp = _native_mod.extract_misp_timestamp

__all__ = [
    "ChromaFormat",
    "ColorInfo",
    "ColourPrimaries",
    "MatrixCoefficients",
    "NalUnit",
    "Obu",
    "ObuExtension",
    "Rational",
    "TransferCharacteristics",
    # H.264
    "EntropyCodingMode",
    "H264ParameterSets",
    "H264Pps",
    "H264SliceHeaderLight",
    "H264SliceType",
    "H264Sps",
    "parse_h264_parameter_sets",
    "parse_h264_pps",
    "parse_h264_slice_header_light",
    "parse_h264_sps",
    # H.265
    "H265ParameterSets",
    "H265Pps",
    "H265ProfileTierLevel",
    "H265SliceHeaderLight",
    "H265SliceType",
    "H265Sps",
    "H265Vps",
    "parse_h265_parameter_sets",
    "parse_h265_pps",
    "parse_h265_slice_header_light",
    "parse_h265_sps",
    "parse_h265_vps",
    # H.266
    "H266ParameterSets",
    "H266Pps",
    "H266ProfileTierLevel",
    "H266SliceHeaderLight",
    "H266SliceType",
    "H266Sps",
    "H266Vps",
    "parse_h266_parameter_sets",
    "parse_h266_pps",
    "parse_h266_slice_header_light",
    "parse_h266_sps",
    "parse_h266_vps",
    # AV1
    "Av1FrameHeaderLight",
    "Av1ObuStream",
    "Av1SequenceHeader",
    "parse_av1_frame_header_light",
    "parse_av1_obu_stream",
    "parse_av1_sequence_header",
    # AAC
    "AacChannelLayout",
    "AacProfile",
    "AdtsFrame",
    "AdtsFrameIter",
    "MpegVersion",
    "iter_aac_frames",
    "iter_aac_frames_with_resync",
    "parse_aac_frames",
    "parse_aac_frames_with_resync",
    # MPEG-2 audio
    "ChannelMode",
    "Layer",
    "Mpeg2AudioFrame",
    "Mpeg2AudioFrameIter",
    "Version",
    "iter_mpeg2_audio_frames",
    "iter_mpeg2_audio_frames_with_resync",
    "parse_mpeg2_audio_frames",
    "parse_mpeg2_audio_frames_with_resync",
    # Opt-in ES parse functions
    "split_units",
    "parse_audio",
    # MISP timestamp (ST 0604)
    "MispTimeKind",
    "MispTimestamp",
    "extract_misp_timestamp",
]


def _make_np_property(attr_name: str):
    """Build a property that returns np.frombuffer(getattr(self, attr_name), uint8).

    Returns a NumPy ndarray snapshot view. Each property access materializes
    a fresh Python `bytes` from Rust-owned storage (one copy of payload_length
    bytes), then NumPy views that bytes object with zero further copies.

    **The copy is NOT cached on the wrapper instance.** Every read of
    `obj.payload_np` (or `.raw_rbsp_np` / `.raw_np`) re-runs the
    Rust→Python `PyBytes` allocation. In tight pandas / notebook loops
    that touch the same field repeatedly (e.g. `df["nal"].apply(lambda
    n: n.payload_np.mean())` followed by `.apply(lambda n:
    n.payload_np.std())`), this doubles the allocation cost.

    Recommendation: cache the snapshot manually when you'll touch it more
    than once::

        # In a pandas/numpy hot loop:
        payload = obj.payload_np   # one Rust→Python copy
        mean = payload.mean()
        std = payload.std()
        # ... no further copies, all NumPy ops on the cached ndarray.

    PyO3 abi3 classes cannot store side attributes cleanly across
    versions, so transparent per-instance caching at the Rust layer is
    not provided — the cost is deliberately surfaced to the caller.

    Lazy-imports numpy on first call; raises ImportError with install hint
    if [pandas] extra not installed.
    """
    def getter(self):
        try:
            import numpy as np
        except ImportError as e:
            raise ImportError(
                "tstrans NumPy views require: pip install 'tstrans[pandas]'"
            ) from e
        return np.frombuffer(getattr(self, attr_name), dtype=np.uint8)
    return property(getter)


# Attach .payload_np to byte-payload classes (NAL/OBU/audio frames)
for _cls in (NalUnit, Obu, AdtsFrame, Mpeg2AudioFrame):
    _cls.payload_np = _make_np_property("payload")

# Attach .raw_rbsp_np to parameter-set / slice-header classes
for _cls in (
    H264Sps, H264Pps, H264SliceHeaderLight,
    H265Sps, H265Pps, H265Vps, H265SliceHeaderLight,
    H266Sps, H266Pps, H266Vps, H266SliceHeaderLight,
):
    _cls.raw_rbsp_np = _make_np_property("raw_rbsp")

# Attach .raw_np to AV1 types (field is `raw`, not `raw_rbsp`)
for _cls in (Av1SequenceHeader, Av1FrameHeaderLight):
    _cls.raw_np = _make_np_property("raw")

del _cls  # don't pollute module namespace
