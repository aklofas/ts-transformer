"""Secrets in URLs and stream IDs never reach `repr()` or error text."""

from __future__ import annotations

import pytest

import tstrans.rist as rist
import tstrans.rtp as rtp
import tstrans.srt as srt
import tstrans.tcp as tcp
import tstrans.udp as udp
from tstrans.shells import Sender

SECRET = "hunter2"


def _clean(text: str, host_port: str) -> None:
    assert SECRET not in text, text
    assert host_port in text, text
    assert "<redacted>" in text, text


def test_rist_transport_builder_repr():
    url = f"rist://user:{SECRET}@10.0.0.5:5000?secret={SECRET}"
    _clean(repr(rist.Transport.builder().url(url)), "10.0.0.5:5000")


def test_rist_recv_transport_builder_repr():
    url = f"rist://@0.0.0.0:5000?secret={SECRET}"
    _clean(repr(rist.RecvTransport.builder().bind_url(url)), "0.0.0.0:5000")


def test_srt_builder_repr():
    url = f"srt://10.0.0.5:9000?passphrase={SECRET}"
    _clean(repr(srt.Builder(url)), "10.0.0.5:9000")


@pytest.mark.parametrize("key", ["streamid", "srt_streamid"])
def test_srt_builder_repr_hides_stream_id(key):
    url = f"srt://10.0.0.5:9000?{key}={SECRET}"
    _clean(repr(srt.Builder(url)), "10.0.0.5:9000")


def test_rtsp_client_config_repr_password_with_question_mark():
    url = f"rtsp://cam:{SECRET}?x@10.0.0.5:554/s"
    _clean(repr(rtp.RtspClientConfig(url)), "10.0.0.5:554")


def test_rtsp_client_config_repr():
    url = f"rtsp://cam:{SECRET}@10.0.0.5:554/s"
    _clean(repr(rtp.RtspClientConfig(url)), "10.0.0.5:554")


def test_tcp_transport_builder_repr():
    url = f"tcp://u:{SECRET}@10.0.0.5:7000"
    _clean(repr(tcp.Transport.builder().url(url)), "10.0.0.5:7000")


def test_udp_transport_builder_repr():
    url = f"udp://u:{SECRET}@10.0.0.5:7000"
    _clean(repr(udp.Transport.builder().url(url)), "10.0.0.5:7000")


def test_udp_recv_transport_builder_repr():
    url = f"udp://u:{SECRET}@0.0.0.0:7000"
    _clean(repr(udp.RecvTransport.builder().bind_url(url)), "0.0.0.0:7000")


def test_unsupported_scheme_error_text():
    url = f"nope://u:{SECRET}@10.0.0.5:1?token={SECRET}"
    with pytest.raises(ValueError) as exc:
        Sender.open(url)
    _clean(str(exc.value), "10.0.0.5:1")


def test_access_control_repr_hides_values():
    ac = srt.AccessControl.parse(f"#!::u=admin,r={SECRET}")
    text = repr(ac)
    assert SECRET not in text and "admin" not in text, text
    assert '"u"' in text and '"r"' in text, text
    assert text.count("<redacted>") == 2, text
