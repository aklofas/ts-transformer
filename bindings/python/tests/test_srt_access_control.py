"""Tests for `tstrans.srt.AccessControl`, the `#!::k=v` stream-ID form."""

from __future__ import annotations

import pytest

import tstrans.srt
from tstrans.srt import AccessControl


def test_exported() -> None:
    assert "AccessControl" in tstrans.srt.__all__


def test_libsrt_doc_example_request() -> None:
    ac = AccessControl.parse("#!::u=admin,r=bluesbrothers1_hi")
    assert ac.user == "admin"
    assert ac.resource == "bluesbrothers1_hi"
    assert ac.host is None
    assert ac.session is None
    assert ac.kind == "stream"
    assert ac.mode == "request"


def test_libsrt_doc_example_file_publish() -> None:
    ac = AccessControl.parse("#!::u=johnny,t=file,m=publish,r=results.csv")
    assert ac.kind == "file"
    assert ac.mode == "publish"
    assert ac.resource == "results.csv"


def test_custom_keys_and_raw_kind_mode() -> None:
    ac = AccessControl.parse("#!::user_tier=gold,t=telemetry,m=bidirectional,h=cam.example")
    assert ac.get("user_tier") == "gold"
    assert ac.get("missing") is None
    assert ac.kind == "telemetry"
    assert ac.mode == "bidirectional"
    assert ac.host == "cam.example"


def test_not_access_control() -> None:
    with pytest.raises(ValueError, match="not an access-control"):
        AccessControl.parse("publish:amber")


def test_nested_form_unsupported() -> None:
    with pytest.raises(ValueError, match="not supported"):
        AccessControl.parse("#!{u=admin}")


def test_duplicate_key() -> None:
    with pytest.raises(ValueError, match="duplicate key"):
        AccessControl.parse("#!::u=a,u=b")


def test_items_keep_input_order() -> None:
    ac = AccessControl.parse("#!::r=x,u=y,m=publish")
    assert ac.items() == [("r", "x"), ("u", "y"), ("m", "publish")]
    assert "AccessControl" in repr(ac)
