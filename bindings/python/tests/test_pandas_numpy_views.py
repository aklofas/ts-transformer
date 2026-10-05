"""NumPy snapshot-view accessor tests for tstrans.codec types."""

import pytest

pytestmark = pytest.mark.pandas

# The `pandas` marker filters at run-time, but pytest still imports test
# modules at collection. Skip the whole module when the [pandas] extra
# isn't installed — these tests use NumPy which is part of the same extra.
pytest.importorskip("numpy")

import numpy as np  # noqa: E402

from tstrans.codec import (
    AdtsFrame,
    Av1FrameHeaderLight,
    Av1SequenceHeader,
    H264Pps,
    H264SliceHeaderLight,
    H264Sps,
    H265Pps,
    H265SliceHeaderLight,
    H265Sps,
    H265Vps,
    H266Pps,
    H266SliceHeaderLight,
    H266Sps,
    H266Vps,
    Mpeg2AudioFrame,
    NalUnit,
    Obu,
)


def test_nal_unit_payload_np_returns_ndarray():
    nal = NalUnit.h264(nal_type=5, ref_idc=3, payload=b"\x01\x02\x03")
    arr = nal.payload_np
    assert isinstance(arr, np.ndarray)
    assert arr.dtype == np.uint8
    assert arr.shape == (3,)
    assert bytes(arr) == b"\x01\x02\x03"


def test_nal_unit_payload_np_is_read_only():
    nal = NalUnit.h264(nal_type=5, ref_idc=3, payload=b"\x01\x02\x03")
    arr = nal.payload_np
    with pytest.raises(ValueError, match="read-only"):
        arr[0] = 99


def test_nal_unit_payload_np_empty_bytes():
    nal = NalUnit.h264(nal_type=0, ref_idc=0, payload=b"")
    arr = nal.payload_np
    assert arr.shape == (0,)


def test_obu_payload_np_returns_ndarray():
    obu = Obu(obu_type=6, extension=None, payload=b"\xaa\xbb")
    arr = obu.payload_np
    assert arr.dtype == np.uint8
    assert bytes(arr) == b"\xaa\xbb"


def test_h265_nal_unit_payload_np():
    nal = NalUnit.h265(nal_type=19, layer_id=0, temporal_id_plus1=1, payload=b"\xff")
    assert nal.payload_np.dtype == np.uint8


def test_h266_nal_unit_payload_np():
    nal = NalUnit.h266(nal_type=7, layer_id=0, temporal_id_plus1=1, payload=b"\xab")
    assert nal.payload_np.dtype == np.uint8


# Parametrize over every byte-bearing class to enforce coverage
@pytest.mark.parametrize("cls_name,attr", [
    ("NalUnit", "payload_np"),
    ("Obu", "payload_np"),
    ("AdtsFrame", "payload_np"),
    ("Mpeg2AudioFrame", "payload_np"),
    ("H264Sps", "raw_rbsp_np"),
    ("H264Pps", "raw_rbsp_np"),
    ("H264SliceHeaderLight", "raw_rbsp_np"),
    ("H265Sps", "raw_rbsp_np"),
    ("H265Pps", "raw_rbsp_np"),
    ("H265Vps", "raw_rbsp_np"),
    ("H265SliceHeaderLight", "raw_rbsp_np"),
    ("H266Sps", "raw_rbsp_np"),
    ("H266Pps", "raw_rbsp_np"),
    ("H266Vps", "raw_rbsp_np"),
    ("H266SliceHeaderLight", "raw_rbsp_np"),
    ("Av1SequenceHeader", "raw_np"),
    ("Av1FrameHeaderLight", "raw_np"),
])
def test_class_has_numpy_accessor(cls_name, attr):
    import tstrans.codec as c
    cls = getattr(c, cls_name)
    assert hasattr(cls, attr), f"{cls_name} missing {attr}"


def test_payload_np_returns_fresh_array_each_access():
    # Confirm the snapshot semantic: accessing the property twice returns
    # DISTINCT array objects (because each call allocates a fresh bytes
    # snapshot under the hood). Documents that `.payload_np` is NOT a
    # cached view — repeated callers should cache the result manually.
    nal = NalUnit.h264(nal_type=5, ref_idc=3, payload=b"\x01\x02\x03\x04\x05")
    a1 = nal.payload_np
    a2 = nal.payload_np
    assert a1 is not a2
    # Sanity: both snapshots carry identical content.
    assert bytes(a1) == bytes(a2) == b"\x01\x02\x03\x04\x05"


def test_payload_np_mutation_doesnt_leak_to_next_access():
    # Because each access is a fresh snapshot, mutating one ndarray cannot
    # affect a subsequent access. Well, you can't directly mutate the array
    # because it's read-only — but verify both guards work together: the
    # read-only guard prevents the in-place write, AND the fresh-snapshot
    # guarantee means a subsequent access always reads the original Rust
    # storage unmodified.
    nal = NalUnit.h264(nal_type=5, ref_idc=3, payload=b"\x01\x02\x03")
    a1 = nal.payload_np
    with pytest.raises(ValueError, match="read-only"):
        a1[0] = 99
    # Next access still sees the original bytes from Rust-owned storage.
    a2 = nal.payload_np
    assert bytes(a2) == b"\x01\x02\x03"
    assert a1 is not a2
