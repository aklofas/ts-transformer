"""The test-only names a `tstrans._native` build carries.

Everything built only with the `py-test-hooks` cargo feature has a name
ending in one of `SUFFIXES`, and `EXPECTED` lists every such name a build
with the hooks and the default transports carries. Standard library only, so
the wheel jobs run it against a wheel:

    python hook_names.py absent           # the installed tstrans has none
    python hook_names.py present          # it has exactly EXPECTED
    python hook_names.py absent-in F...   # no EXPECTED name in the extension
                                          # binary of wheel or library F
                                          # (a wheel this host cannot import)
    python hook_names.py present-in F...  # every EXPECTED name in it
"""

from __future__ import annotations

import importlib
import sys
import types
import zipfile

SUFFIXES = ("_for_test", "_probe", "ForTest")

EXPECTED = (
    "_native._addr_tuple_for_test",
    "_native._raise_hls_error_for_test",
    "_native._raise_mux_error_for_test",
    "_native._raise_rist_error_for_test",
    "_native._raise_srt_error_for_test",
    "_native._raise_tcp_error_for_test",
    "_native._raise_udp_error_for_test",
    "_native._raise_webrtc_error_for_test",
    "_native.rtp._push_close_rounds_for_test",
    "_native.rtp._push_conversion_rounds_for_test",
    "_native.rtp._push_hold_as_reader_for_test",
    "_native.rtp._push_parking_waiters_for_test",
    "_native.rtp._push_poison_for_test",
    "_native.rtp._push_reader_holds_for_test",
    "_native.rtp._push_release_for_test",
    "_native.rtp._rtsp_session_poison_for_test",
    "_native.shells._open_cancel_with_slow_open_for_test",
    "_native.shells._slow_cancel_handle_for_test",
    "_native.shells._slot_held_for_test",
    "_native.shells._slow_drop_for_test",
    "_native.webrtc._loopback_viewer_probe",
)


def hook_names() -> list[str]:
    """Every attribute of `_native`, its submodules and its classes (and
    theirs) whose name ends in one of `SUFFIXES`, as dotted paths."""
    found: list[str] = []
    seen: set[int] = set()

    def walk(obj: object, path: str) -> None:
        if id(obj) in seen:
            return
        seen.add(id(obj))
        for name in dir(obj):
            if name.endswith(SUFFIXES):
                found.append(f"{path}.{name}")
            try:
                value = getattr(obj, name)
            except Exception:
                continue
            if isinstance(value, types.ModuleType) or (
                isinstance(value, type)
                and getattr(value, "__module__", "").startswith("tstrans")
            ):
                walk(value, f"{path}.{name}")

    walk(importlib.import_module("tstrans._native"), "_native")
    return sorted(found)


def names_in_binary(path: str) -> list[str]:
    """The `EXPECTED` names whose NUL-terminated form (as PyO3 stores a
    function's name) is in the extension binary `path`, or in the
    `tstrans/_native` binary inside the wheel `path`."""
    if path.endswith(".whl"):
        with zipfile.ZipFile(path) as whl:
            members = [n for n in whl.namelist() if n.startswith("tstrans/_native.")]
            if len(members) != 1:
                raise SystemExit(f"{path}: expected one tstrans/_native binary, got {members}")
            data = whl.read(members[0])
    else:
        with open(path, "rb") as f:
            data = f.read()
    return [n for n in EXPECTED if n.rsplit(".", 1)[1].encode() + b"\0" in data]


def main(argv: list[str]) -> int:
    mode = argv[1] if len(argv) > 1 else ""
    if mode in ("absent-in", "present-in") and len(argv) > 2:
        bad = 0
        for path in argv[2:]:
            found = names_in_binary(path)
            if mode == "absent-in" and found:
                bad = 1
                print(f"test-only names in {path}:", *found, sep="\n  ")
            elif mode == "present-in" and len(found) != len(EXPECTED):
                bad = 1
                missing = sorted(set(EXPECTED) - set(found))
                print(f"missing from {path}:", *missing, sep="\n  ")
            else:
                print(f"{mode} {path}: {len(found)} of {len(EXPECTED)} test-only names")
        return bad
    if mode not in ("absent", "present") or len(argv) != 2:
        print(
            "usage: hook_names.py absent | present | absent-in FILE... | present-in FILE...",
            file=sys.stderr,
        )
        return 2
    found = hook_names()
    if mode == "absent":
        if found:
            print("test-only names in tstrans._native:", *found, sep="\n  ")
            return 1
        print("no test-only names in tstrans._native")
        return 0
    missing = sorted(set(EXPECTED) - set(found))
    unlisted = sorted(set(found) - set(EXPECTED))
    for label, names in (("missing", missing), ("not in EXPECTED", unlisted)):
        if names:
            print(f"{label}:", *names, sep="\n  ")
    if missing or unlisted:
        return 1
    print(f"all {len(EXPECTED)} test-only names present in tstrans._native")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
