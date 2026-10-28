"""The Python leg of the URL corpus.

Every row of ``crates/tst-integration/tests/fixtures/scenarios/url-corpus/corpus.toml``
goes through the Python entry point the corpus header names for its scheme and
role. Python exposes no parsed URL, so only the outcome is compared: a refused
row must raise the scheme's error class with the expected ``kind.name``; an
accepted row must not raise one of the scheme's URL kinds (a refused connect is
fine).
"""

from __future__ import annotations

import threading
import urllib.parse
from typing import Any, Callable

import pytest

import tomllib

from conftest import scenarios_dir
from tstrans import exceptions, hls, rtp, srt, webrtc
from tstrans.shells import Receiver, Sender


def load_corpus() -> dict[str, Any]:
    with open(scenarios_dir() / "url-corpus" / "corpus.toml", "rb") as fh:
        return tomllib.load(fh)


CORPUS = load_corpus()

# The header keys its tables on the scheme family (`tcps` is `tcp`).
FAMILY = {
    "srt": "srt",
    "rtp": "rtp",
    "rtsp": "rtsp",
    "rtsps": "rtsp",
    "udp": "udp",
    "tcp": "tcp",
    "tcps": "tcp",
    "hls": "hls",
    "hlss": "hls",
    "rist": "rist",
    "whep": "whep",
    "wheps": "whep",
}


def label(row: dict[str, Any]) -> str:
    return f"[{row['scheme']} {row['role']} {row['case']}] {row['url']}"


# The error class each family's entry points raise.
ERROR_CLASS: dict[str, type[Exception]] = {
    "srt": exceptions.SrtError,
    "rtp": exceptions.RtpError,
    "rtsp": exceptions.RtspError,
    "udp": exceptions.UdpError,
    "tcp": exceptions.TcpError,
    "hls": exceptions.HlsError,
    "rist": exceptions.RistError,
    "whep": exceptions.WebrtcError,
}


def srt_peer_url(url: str) -> str | None:
    """The loopback address of an srt URL, as a bare ``srt://`` URL.

    The peer goes up whatever the record says: should Python accept a URL the
    record calls invalid, the open still returns (and the mismatch is
    reported) instead of waiting in accept forever.
    """
    try:
        parts = urllib.parse.urlsplit(url)
        port = parts.port
    except ValueError:
        return None
    if port is None:
        return None
    host = parts.hostname or "127.0.0.1"
    return f"srt://[{host}]:{port}" if ":" in host else f"srt://{host}:{port}"


def open_close(open_: Callable[[], Any]) -> None:
    handle = open_()
    handle.close()


def open_srt_caller(url: str) -> None:
    # An SRT caller would wait out its connect timeout against a closed port,
    # so the URL's address gets a loopback listener. The peer sets no options:
    # a handshake it rejects (a passphrase it lacks) still returns at once, as
    # a connect failure.
    peer_url = srt_peer_url(url)
    peer = srt.Builder(f"{peer_url}?mode=listener").listen() if peer_url else None
    try:
        open_close(lambda: Sender.open(url))
    finally:
        if peer is not None:
            peer.close()


def open_srt_listener(url: str) -> None:
    # The listener's open blocks in accept, so the URL's address gets a
    # loopback caller; its handshake retries until the bind lands.
    peer_url = srt_peer_url(url)
    dialed: list[Any] = []

    def dial() -> None:
        try:
            dialed.append(srt.Builder(peer_url, connect_timeout_ms=5000).connect())
        except exceptions.SrtError:
            pass

    caller = threading.Thread(target=dial, daemon=True) if peer_url else None
    if caller is not None:
        caller.start()
    try:
        open_close(lambda: Receiver.open(url))
    finally:
        if caller is not None:
            caller.join(timeout=10)
        for sock in dialed:
            sock.close()


ENTRIES: dict[str, Callable[[str], None]] = {
    "shells.Sender.open": lambda url: open_close(lambda: Sender.open(url)),
    "shells.Receiver.open": lambda url: open_close(lambda: Receiver.open(url)),
    "rtp.H264Receiver.listen": lambda url: open_close(lambda: rtp.H264Receiver.listen(url)),
    "rtp.RtspClient.connect": lambda url: open_close(
        lambda: rtp.RtspClient.connect(rtp.RtspClientConfig(url))
    ),
    "hls.HlsPublisherBuilder.from_url": lambda url: hls.HlsPublisherBuilder().from_url(url),
    "webrtc.WhepServer.bind": lambda url: open_close(lambda: webrtc.WhepServer.bind(url)),
}


def probe(entry: str, row: dict[str, Any]) -> str | None:
    """Open the row's URL through ``entry``; the refused kind's name, or None."""
    family = FAMILY[row["scheme"]]
    if family == "srt":
        run = {"shells.Sender.open": open_srt_caller, "shells.Receiver.open": open_srt_listener}[entry]
    else:
        run = ENTRIES[entry]
    try:
        run(row["url"])
    except ERROR_CLASS[family] as exc:
        return exc.kind.name
    return None


def check_row(row: dict[str, Any]) -> bool:
    """Assert the row's outcome through Python matches its record; False when
    the corpus declares no Python entry point for the row's role."""
    family = FAMILY[row["scheme"]]
    entry = CORPUS["legs"]["python"][family][row["role"]]
    if entry == "absent":
        return False
    observed = probe(entry, row)
    expected = row["expect"].get("error_kind")
    if expected is not None:
        assert observed == expected, f"{label(row)} via {entry}: expected {expected}, observed {observed}"
    else:
        url_kinds = {
            r["expect"]["error_kind"]
            for r in CORPUS["url"]
            if FAMILY[r["scheme"]] == family and "error_kind" in r["expect"]
        }
        assert observed not in url_kinds, (
            f"{label(row)} via {entry}: expected the URL to be accepted, observed {observed}"
        )
    return True


@pytest.mark.parametrize("row", CORPUS["url"], ids=[label(r) for r in CORPUS["url"]])
def test_url_corpus_row_matches_its_record(row: dict[str, Any]) -> None:
    if not check_row(row):
        pytest.skip(f"no Python entry point for {FAMILY[row['scheme']]} {row['role']}")
