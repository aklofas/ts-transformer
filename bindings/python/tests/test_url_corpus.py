"""The Python leg of the URL corpus.

Every row of ``crates/tst-integration/tests/fixtures/scenarios/url-corpus/corpus.toml``
goes through the Python entry point the corpus header names for its scheme and
role. Python exposes no parsed URL, so only the outcome is compared: a refused
row must raise the scheme's error class with the expected ``kind.name``; an
accepted row must not raise one of the scheme's URL kinds (a refused connect is
fine). For the wire schemes (rtsp) a valid row is also connected to a
recording peer, and the request heads must show the record's credentials and
keys.
"""

from __future__ import annotations

import base64
import socket
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


# ── The RTSP wire probe ──────────────────────────────────────────────────────

_SDP = (
    b"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=tst\r\nt=0 0\r\n"
    b"a=control:*\r\nm=video 0 RTP/AVP 33\r\na=control:trackID=0\r\n"
)


def _header(head: str, name: str) -> str | None:
    for line in head.split("\r\n")[1:]:
        key, sep, value = line.partition(":")
        if sep and key.strip().lower() == name:
            return value.strip()
    return None


def recording_peer(challenge: bool) -> tuple[int, list[str], threading.Thread]:
    """A loopback RTSP peer on port 0 that records every request head.

    Answers 461 to a UDP SETUP (so the SETUP sequence shows the transport
    choice), or with ``challenge`` answers 401 to a DESCRIBE without
    credentials and stops at the first one with.
    """
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    port = listener.getsockname()[1]
    heads: list[str] = []

    def serve() -> None:
        try:
            sock, _ = listener.accept()
        except OSError:
            return
        finally:
            listener.close()
        with sock:
            sock.settimeout(10)
            buf = b""
            try:
                while True:
                    while b"\r\n\r\n" not in buf:
                        chunk = sock.recv(8192)
                        if not chunk:
                            return
                        buf += chunk
                    raw, buf = buf.split(b"\r\n\r\n", 1)
                    head = raw.decode("latin-1")
                    heads.append(head)
                    request_line = head.split("\r\n", 1)[0]
                    method = request_line.split(" ", 1)[0]
                    version = request_line.rsplit(" ", 1)[-1]
                    cseq = _header(head, "cseq") or "1"
                    transport = _header(head, "transport") or ""
                    authorized = _header(head, "authorization") is not None
                    if challenge and method == "DESCRIBE" and authorized:
                        return
                    status, extra, body = "200 OK", "", b""
                    if method == "DESCRIBE" and challenge:
                        status, extra = "401 Unauthorized", 'WWW-Authenticate: Basic realm="t"\r\n'
                    elif method == "OPTIONS":
                        extra = "Public: OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN\r\n"
                    elif method == "DESCRIBE":
                        body = _SDP
                        extra = f"Content-Type: application/sdp\r\nContent-Length: {len(body)}\r\n"
                    elif method == "SETUP" and "/TCP" not in transport:
                        status = "461 Unsupported Transport"
                    elif method == "SETUP":
                        extra = f"Session: DEADBEEF;timeout=60\r\nTransport: {transport}\r\n"
                    else:
                        extra = "Session: DEADBEEF\r\n"
                    sock.sendall(f"{version} {status}\r\nCSeq: {cseq}\r\n{extra}\r\n".encode() + body)
                    if method == "TEARDOWN":
                        return
            except OSError:
                return

    server = threading.Thread(target=serve, daemon=True)
    server.start()
    return port, heads, server


def with_port(url: str, port: int) -> str:
    """``url`` with its port replaced by ``port`` (inserted when it has none)."""
    scheme, rest = url.split("://", 1)
    end = min((i for i in (rest.find("/"), rest.find("?")) if i >= 0), default=len(rest))
    authority, tail = rest[:end], rest[end:]
    at = authority.rfind("@")
    userinfo, hostport = authority[: at + 1], authority[at + 1 :]
    host = hostport[: hostport.find("]") + 1] if "]" in hostport else hostport.split(":", 1)[0]
    return f"{scheme}://{userinfo}{host}:{port}{tail}"


def rtsp_run(url: str, challenge: bool) -> list[str]:
    """Connect ``url`` through Python's RTSP client against a peer; the heads."""
    port, heads, server = recording_peer(challenge)
    try:
        # Keepalive off: the recorded heads are only the connect's.
        session = rtp.RtspClient.connect(rtp.RtspClientConfig(with_port(url, port), keepalive=False))
        session.teardown()
    except exceptions.RtspError:
        pass
    finally:
        # Unblocks a peer still waiting in accept (the client never dialed).
        try:
            socket.create_connection(("127.0.0.1", port), timeout=1).close()
        except OSError:
            pass
        server.join(10)
    return list(heads)


def rtsp_wire_record(plain: list[str], challenged: list[str]) -> dict[str, Any]:
    """Project the request heads of the two runs into a record."""
    rec: dict[str, Any] = {}
    keys: dict[str, str] = {}
    if plain:
        parts = plain[0].split("\r\n", 1)[0].split(" ")
        uri = parts[1] if len(parts) > 1 else ""
        if len(parts) > 2 and parts[2].startswith("RTSP/"):
            keys["rtsp_version"] = parts[2].removeprefix("RTSP/")
        # A request URI carries no userinfo; one that leaked would show here.
        rest = uri.split("://", 1)[-1]
        end = rest.find("/") if "/" in rest else len(rest)
        authority = rest[:end]
        colon = authority.rfind(":")
        host = authority[:colon] if colon >= 0 and not authority.endswith("]") else authority
        rec["host"] = host.strip("[]")
        rec["path"] = rest[end:]
    setups = [
        "/TCP" in (_header(h, "transport") or "") for h in plain if h.startswith("SETUP ")
    ]
    if setups == [True]:
        keys["transport"] = "tcp"
    elif setups == [False]:
        keys["transport"] = "udp"
    elif setups != [False, True]:
        keys["transport"] = f"SETUP sequence {setups}"
    basic = next(
        (a.removeprefix("Basic ") for h in challenged if (a := _header(h, "authorization")) and a.startswith("Basic ")),
        None,
    )
    if basic is not None:
        decoded = base64.b64decode(basic).decode("utf-8", "replace")
        rec["username"] = decoded.partition(":")[0]
        # The client sends credentials only with a password (an empty one
        # counts), so a Basic header at all means one was set.
        rec["password_set"] = True
    rec["keys"] = keys
    return rec


def field(rec: dict[str, Any], name: str) -> Any:
    if name.startswith("keys."):
        return (rec.get("keys") or {}).get(name.removeprefix("keys."))
    return rec.get(name)


def wire_fields(row: dict[str, Any]) -> list[str] | None:
    table = CORPUS["legs"]["python"][FAMILY[row["scheme"]]]
    if row["scheme"] not in table.get("wire_schemes", "").split():
        return None
    return table["wire"].split()


def check_wire(row: dict[str, Any], fields: list[str]) -> None:
    observed = rtsp_wire_record(rtsp_run(row["url"], False), rtsp_run(row["url"], True))
    absent = row.get("wire_absent", "").split()
    mismatches = [
        f"{f} expected {want!r}, observed {got!r}"
        for f in fields
        if (want := None if f in absent else field(row["expect"], f)) != (got := field(observed, f))
    ]
    assert not mismatches, f"{label(row)} on the wire: " + "; ".join(mismatches)


def check_outcome(row: dict[str, Any]) -> bool:
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


def check_row(row: dict[str, Any]) -> bool:
    """Assert the row's outcome through Python matches its record (and, for
    a valid row of a wire scheme, what reached the wire); False when the
    corpus declares no Python entry point for the row's role."""
    fields = wire_fields(row)
    if fields is not None and "error_kind" not in row["expect"]:
        check_wire(row, fields)
        return True
    return check_outcome(row)


@pytest.mark.parametrize("row", CORPUS["url"], ids=[label(r) for r in CORPUS["url"]])
def test_url_corpus_row_matches_its_record(row: dict[str, Any]) -> None:
    if not check_row(row):
        pytest.skip(f"no Python entry point for {FAMILY[row['scheme']]} {row['role']}")
