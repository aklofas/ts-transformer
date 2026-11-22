"""tstrans.rtp — the RTSP server's live per-mount rules table.

Covers `RtspServer.mount_rules()`, `MountRule` / `MountPolicy` /
`MountCredential`, `RefuseStatus`, `AuthScheme` and
`RtspServerConfig.default_mount_rule`. The publisher is a raw socket sending
one ANNOUNCE, since a rule is decided before any mount is touched.
"""

from __future__ import annotations

import base64
import socket

import pytest
from conftest import rtsp_header

from tstrans.exceptions import RtspError, RtspErrorKind
from tstrans.rtp import (
    AuthScheme,
    MountCredential,
    MountPolicy,
    MountRule,
    MountRuleAuthFailures,
    MountRules,
    RefuseStatus,
    RtspServer,
    RtspServerConfig,
)

SDP_MP2T = (
    "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=publish\r\nc=IN IP4 127.0.0.1\r\n"
    "t=0 0\r\nm=video 0 RTP/AVP 33\r\na=rtpmap:33 MP2T/90000\r\n"
    "a=control:streamid=0\r\n"
)


def _start(**kw: object) -> RtspServer:
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50, **kw)  # type: ignore[arg-type]
    return RtspServer.start(cfg)


def _port(server: RtspServer) -> int:
    addr = server.local_addr()
    assert addr is not None
    return addr[1]


def _announce(port: int, path: str, authorization: str | None = None) -> str:
    """One ANNOUNCE on a fresh connection; returns the response head."""
    with socket.create_connection(("127.0.0.1", port), timeout=5.0) as sock:
        auth = f"Authorization: {authorization}\r\n" if authorization else ""
        sock.sendall(
            (
                f"ANNOUNCE rtsp://127.0.0.1:{port}{path} RTSP/1.0\r\nCSeq: 1\r\n{auth}"
                f"Content-Type: application/sdp\r\nContent-Length: {len(SDP_MP2T)}\r\n\r\n"
                f"{SDP_MP2T}"
            ).encode()
        )
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = sock.recv(1024)
            assert chunk, "server closed before answering"
            buf += chunk
        return buf.decode(errors="replace")


def _describe(port: int, path: str, authorization: str | None = None) -> str:
    """One DESCRIBE on a fresh connection; returns the response head."""
    with socket.create_connection(("127.0.0.1", port), timeout=5.0) as sock:
        auth = f"Authorization: {authorization}\r\n" if authorization else ""
        sock.sendall(
            f"DESCRIBE rtsp://127.0.0.1:{port}{path} RTSP/1.0\r\nCSeq: 1\r\n{auth}\r\n".encode()
        )
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = sock.recv(1024)
            assert chunk, "server closed before answering"
            buf += chunk
        return buf.decode(errors="replace")


def _status(resp: str) -> int:
    return int(resp.split()[1])



def _basic(user: str, pw: str) -> str:
    return "Basic " + base64.b64encode(f"{user}:{pw}".encode()).decode()


def test_require_on_publish_challenges_then_admits_basic():
    with _start() as server:
        server.add_publish_mount("/amber")
        rules = server.mount_rules()
        assert isinstance(rules, MountRules)
        rules.set_challenge(AuthScheme.BASIC, "lab")
        rules.set(
            "/amber",
            MountRule(publish=MountPolicy.require(MountCredential("amber", "pw"))),
        )
        assert len(rules) == 1
        port = _port(server)

        r = _announce(port, "/amber")
        assert _status(r) == 401
        challenge = rtsp_header(r, "WWW-Authenticate")
        assert challenge.startswith("Basic") and 'realm="lab"' in challenge

        assert _status(_announce(port, "/amber", _basic("amber", "wrong"))) == 401
        assert _status(_announce(port, "/amber", _basic("amber", "pw"))) == 200


def test_refused_credentials_are_counted_on_their_rule():
    with _start() as server:
        rules = server.mount_rules()
        rules.set_challenge(AuthScheme.BASIC, "lab")
        rules.set("/amber", MountRule(publish=MountPolicy.require(MountCredential("amber", "pw"))))
        rules.set_default(MountRule(publish=MountPolicy.require(MountCredential("other", "pw"))))
        port = _port(server)
        counts = rules.auth_failures("/amber")
        assert isinstance(counts, MountRuleAuthFailures)
        assert (counts.publish, counts.read) == (0, 0)

        # The bare first challenge is not counted; two wrong passwords are.
        assert _status(_announce(port, "/amber")) == 401
        assert rules.auth_failures("/amber").publish == 0
        assert _status(_announce(port, "/amber", _basic("amber", "wrong"))) == 401
        assert _status(_announce(port, "/amber", _basic("nobody", "pw"))) == 401
        counts = rules.auth_failures("/amber")
        assert (counts.publish, counts.read) == (2, 0)
        assert "publish=2" in repr(counts)

        # A refused read counts in the read direction.
        rules.set("/eyes", MountRule(read=MountPolicy.require(MountCredential("u", "pw"))))
        assert _status(_describe(port, "/eyes")) == 401
        assert _status(_describe(port, "/eyes", _basic("u", "bad"))) == 401
        eyes = rules.auth_failures("/eyes")
        assert (eyes.publish, eyes.read) == (0, 1)
        assert type(eyes.read) is int

        # A path without a rule of its own counts on the default rule.
        assert _status(_announce(port, "/elsewhere", _basic("other", "wrong"))) == 401
        assert rules.auth_failures("/elsewhere") is None
        default = rules.default_auth_failures()
        assert (default.publish, default.read) == (1, 0)

        # `set` on the same path keeps the counts; `remove` drops them.
        rules.set("/amber", MountRule(publish=MountPolicy.require(MountCredential("amber", "new"))))
        assert rules.auth_failures("/amber").publish == 2
        rules.remove("/amber")
        assert rules.auth_failures("/amber") is None
    # The counts answer after the server ended.
    assert rules.default_auth_failures().publish == 1


def test_refuse_answers_before_any_mount_exists():
    with _start(accept_unregistered_publishers=True) as server:
        rules = server.mount_rules()
        rules.set("/taken", MountRule(publish=MountPolicy.refuse(RefuseStatus.NOT_ENOUGH_BANDWIDTH)))
        r = _announce(_port(server), "/taken")
        assert _status(r) == 453
        assert server.next_publisher(0.2) is None


def test_default_mount_rule_in_the_config_guards_the_first_announce():
    rule = MountRule(publish=MountPolicy.refuse(RefuseStatus.NOT_FOUND))
    with _start(accept_unregistered_publishers=True, default_mount_rule=rule) as server:
        assert _status(_announce(_port(server), "/first")) == 404
        assert server.next_publisher(0.2) is None


def test_invalid_path_is_mount_kind_and_empty_require_is_value_error():
    with _start() as server:
        rules = server.mount_rules()
        with pytest.raises(RtspError) as ei:
            rules.set("bad path", MountRule())
        assert ei.value.kind is RtspErrorKind.MOUNT
        assert len(rules) == 0
        with pytest.raises(ValueError):
            MountPolicy.require()
        # An empty username reaches the table and is refused there.
        with pytest.raises(RtspError) as ei:
            rules.set("/amber", MountRule(publish=MountPolicy.require(MountCredential("", "pw"))))
        assert ei.value.kind is RtspErrorKind.MOUNT
        twice = MountPolicy.require(MountCredential("amber", "a"), MountCredential("amber", "b"))
        with pytest.raises(RtspError) as ei:
            rules.set("/amber", MountRule(read=twice))
        assert ei.value.kind is RtspErrorKind.MOUNT
        with pytest.raises(ValueError):
            MountPolicy.refuse(200)  # type: ignore[arg-type]


def test_edits_after_stop_raise_closed_kind_but_len_answers():
    server = _start()
    rules = server.mount_rules()
    rules.set("/a", MountRule(publish=MountPolicy.open()))
    assert not rules.is_closed()
    server.stop()
    assert rules.is_closed()
    assert len(rules) == 1
    edits = [
        lambda: rules.set("/b", MountRule()),
        lambda: rules.remove("/a"),
        lambda: rules.set_default(MountRule()),
        lambda: rules.clear(),
        lambda: rules.set_challenge(AuthScheme.DIGEST_SHA256, "x"),
    ]
    for edit in edits:
        with pytest.raises(RtspError) as ei:
            edit()
        assert ei.value.kind is RtspErrorKind.CLOSED
    assert len(rules) == 1


def test_remove_and_clear():
    with _start() as server:
        rules = server.mount_rules()
        rules.set("/a", MountRule())
        rules.set("/b", MountRule())
        assert rules.remove("/a") is True
        assert rules.remove("/a") is False
        rules.clear()
        assert len(rules) == 0
        rules.set_default(MountRule(publish=MountPolicy.open()))


def test_repr_redacts_passwords():
    cred = MountCredential("amber", "hunter2-secret")
    pol = MountPolicy.require(cred)
    rule = MountRule(publish=pol)
    for obj in (cred, pol, rule):
        text = repr(obj)
        assert "hunter2-secret" not in text
    assert "amber" in repr(cred)
    assert "redacted" in repr(cred).lower()
    assert not hasattr(cred, "password")


def test_enum_values_are_pinned():
    assert [int(s) for s in RefuseStatus] == [403, 404, 453, 503]
    assert RefuseStatus.NOT_ENOUGH_BANDWIDTH == 453
    assert [int(s) for s in AuthScheme] == [0, 1, 2]


def test_config_rejects_a_non_mount_rule_default():
    with pytest.raises(TypeError, match="default_mount_rule"):
        RtspServerConfig(default_mount_rule="x")  # type: ignore[arg-type]
    assert RtspServerConfig().default_mount_rule is None


def test_rule_arguments_are_type_checked():
    with pytest.raises(TypeError):
        MountRule(publish="open")  # type: ignore[arg-type]
    with pytest.raises(TypeError):
        MountPolicy.require("amber")  # type: ignore[arg-type]
