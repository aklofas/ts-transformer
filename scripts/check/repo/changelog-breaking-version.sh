#!/usr/bin/env bash
# changelog-breaking-version: a `(BREAKING)` CHANGELOG section sits under a
# version that can carry it, checked in the change that adds the section.
#
# A break needs a new compatibility line: a new minor before 1.0, a new major
# from 1.0. The number is decided when the first breaking change lands, not at
# the release: directly under the `## [Unreleased]` heading the CHANGELOG may
# carry one line, its own paragraph,
#
#     Next release: X.Y.Z
#
# and this rail holds the sections, that line and the workspace version to
# each other. It reads CHANGELOG.md and the workspace Cargo.toml only (no git
# tags, so it works in a shallow checkout).
#
# The sections it knows:
#   [Unreleased]             the changes since the newest versioned section
#   in preparation           the first versioned section while it still holds
#                            its `<!-- TODO(release)` date marker (the
#                            release-prep change retitles Unreleased to it)
#   previous release         the first versioned section without that marker
#
# Rules:
#   1. An [Unreleased] section with a `### ... (BREAKING)` heading needs the
#      `Next release:` line, and its version must open a new compatibility
#      line against the versioned section below it.
#   2. A `Next release:` line, breaking or not, is a MAJOR.MINOR.PATCH above
#      the versioned section below it, appears once, and is the first line
#      under the [Unreleased] heading. No other section carries one, so the
#      release-prep change removes it and a line left after a release fails.
#   3. A section in preparation is above the previous release, and with a
#      `(BREAKING)` heading it opens a new compatibility line against it. The
#      workspace version equals its version.
#   4. With no section in preparation, the workspace version is the previous
#      release's or the `Next release:` one.
#   5. Only the first versioned section may be in preparation, and every
#      `## [...]` heading is `[Unreleased]` or `[X.Y.Z] — YYYY-MM-DD`.
# No `(BREAKING)` heading and no line passes: a change between a release and
# the next version sweep needs neither.
#
# Run it:   bash scripts/check/repo/changelog-breaking-version.sh
# Overridable (scripts/ratchets/tests/self_test.sh points these at fixtures):
#   CBV_CHANGELOG    the CHANGELOG
#   CBV_CARGO_TOML   the workspace Cargo.toml
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"

python3 - "${CBV_CHANGELOG:-$ROOT/CHANGELOG.md}" "${CBV_CARGO_TOML:-$ROOT/Cargo.toml}" <<'PY'
import re
import sys

changelog, cargo_toml = sys.argv[1], sys.argv[2]
ME = "changelog-breaking-version"
MARKER = "<!-- TODO(release)"
fails = []


def version(text):
    m = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", text)
    return tuple(int(g) for g in m.groups()) if m else None


def show(v):
    return "%d.%d.%d" % v


def line_of(v):
    """What a release keeps when it admits no break: major.minor before 1.0, the major from 1.0."""
    return v[:2] if v[0] == 0 else v[:1]


def line_name(v):
    return "%d.%d" % v[:2] if v[0] == 0 else "%d.x" % v[0]


def first_breaking_number(v):
    """The lowest version above `v` that may carry a break."""
    return show((0, v[1] + 1, 0) if v[0] == 0 else (v[0] + 1, 0, 0))


def done():
    for f in fails:
        print("FAIL: %s: %s" % (ME, f))
    sys.exit(1 if fails else 0)


try:
    with open(changelog, encoding="utf-8") as f:
        lines = f.read().splitlines()
    with open(cargo_toml, encoding="utf-8") as f:
        cargo = f.read().splitlines()
except OSError as e:
    fails.append("cannot read an input: %s" % e)
    done()

table, workspace = None, None
for l in cargo:
    if l.startswith("["):
        table = l.strip()
    m = re.match(r'version\s*=\s*"([^"]*)"', l)
    if table == "[workspace.package]" and m and workspace is None:
        workspace = version(m.group(1))
        if workspace is None:
            fails.append("the workspace version %r in %s is not MAJOR.MINOR.PATCH" % (m.group(1), cargo_toml))
if workspace is None and not fails:
    fails.append("%s has no [workspace.package] version" % cargo_toml)

# The sections: (name, version or None for Unreleased, body lines).
sections = []
for l in lines:
    if l.startswith("## ["):
        m = re.fullmatch(r"## \[(\d+\.\d+\.\d+)\] — \d{4}-\d\d-\d\d\s*", l)
        if l.strip() == "## [Unreleased]":
            sections.append(("Unreleased", None, []))
        elif m:
            sections.append((m.group(1), version(m.group(1)), []))
        else:
            fails.append("section heading %r is neither `## [Unreleased]` nor `## [X.Y.Z] — YYYY-MM-DD`" % l)
            sections.append((l, None, None))
    elif sections and sections[-1][2] is not None:
        sections[-1][2].append(l)
if fails:
    done()

unreleased = [s for s in sections if s[0] == "Unreleased"]
versioned = [s for s in sections if s[1] is not None]
if len(unreleased) > 1 or (unreleased and sections[0][0] != "Unreleased"):
    fails.append("[Unreleased] must be the first section, once")
preparing = [s for s in versioned if any(MARKER in l for l in s[2])]
if any(s is not versioned[0] for s in preparing):
    fails.append("only the first versioned section may hold the `%s` date marker; found it in %s"
                 % (MARKER, ", ".join("[%s]" % s[0] for s in preparing)))
released = [s for s in versioned if s not in preparing]
if not released:
    fails.append("%s has no released section (`## [X.Y.Z] — YYYY-MM-DD` without the date marker) to compare with"
                 % changelog)
if fails:
    done()
previous = released[0]
prep = preparing[0] if preparing else None


def breaking(section):
    return [l for l in section[2] if re.match(r"#{3,} .*\(BREAKING\b", l)]


def next_lines(section):
    return [l for l in section[2] if l.startswith("Next release:")]


for s in versioned:
    if next_lines(s):
        fails.append("section [%s] carries a `Next release:` line; it belongs under [Unreleased] only, and the "
                     "release-prep change removes it" % s[0])

# [Unreleased] is judged against the versioned section below it.
nxt = None
if unreleased:
    un, base = unreleased[0], versioned[0]
    found = next_lines(un)
    if len(found) > 1:
        fails.append("[Unreleased] carries %d `Next release:` lines; it may carry one" % len(found))
    elif found:
        body = [l for l in un[2] if l.strip()]
        value = found[0][len("Next release:"):].strip()
        nxt = version(value)
        if body[0] != found[0]:
            fails.append("`%s` must be the first line under the [Unreleased] heading" % found[0])
        if nxt is None:
            fails.append("`%s` does not name a MAJOR.MINOR.PATCH version" % found[0])
        elif nxt <= base[1]:
            fails.append("`Next release: %s` is not above the previous release %s; write the number of the "
                         "release these changes go into, or remove the line" % (value, base[0]))
    broke = breaking(un)
    if broke and not found:
        fails.append("[Unreleased] has the breaking section `%s` but no `Next release:` line. A break needs a "
                     "new %s after %s: add the paragraph `Next release: %s` directly under the `## [Unreleased]` "
                     "heading" % (broke[0], "minor" if base[1][0] == 0 else "major", base[0],
                                  first_breaking_number(base[1])))
    elif broke and nxt and nxt > base[1] and line_of(nxt) == line_of(base[1]):
        fails.append("[Unreleased] has the breaking section `%s`, but %s stays in the %s line of %s; a break "
                     "needs a new %s: write `Next release: %s`"
                     % (broke[0], show(nxt), line_name(base[1]), base[0],
                        "minor" if base[1][0] == 0 else "major", first_breaking_number(base[1])))

if prep is not None:
    if prep[1] <= previous[1]:
        fails.append("the section in preparation [%s] is not above the previous release %s" % (prep[0], previous[0]))
    elif breaking(prep) and line_of(prep[1]) == line_of(previous[1]):
        fails.append("the section in preparation [%s] has the breaking section `%s`, but %s stays in the %s line "
                     "of %s; a break needs a new %s: this release is %s or above"
                     % (prep[0], breaking(prep)[0], prep[0], line_name(previous[1]), previous[0],
                        "minor" if previous[1][0] == 0 else "major", first_breaking_number(previous[1])))
    if workspace != prep[1]:
        fails.append("the workspace version is %s but the section in preparation is [%s]; the release-prep "
                     "change carries the version sweep" % (show(workspace), prep[0]))
else:
    allowed = [previous[1]] + ([nxt] if nxt else [])
    if workspace not in allowed:
        fails.append("the workspace version is %s; it must be the previous release's (%s)%s"
                     % (show(workspace), previous[0],
                        " or the `Next release:` one (%s)" % show(nxt) if nxt else
                        ", or [Unreleased] must name it in a `Next release:` line"))
if fails:
    done()
print("%s: ok (previous release %s, %snext release %s, workspace %s)" % (
    ME, previous[0], "in preparation %s, " % prep[0] if prep else "",
    show(nxt) if nxt else "not named", show(workspace)))
PY
