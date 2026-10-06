#!/usr/bin/env python3
"""Native-dependency advisory check for the six vendored submodules.

`cargo-deny` sees Cargo.lock only; libsrt, librist, mbedTLS, the FreeRTOS
kernel, FreeRTOS-POSIX and lwIP are git submodules pinned by SHA, and the
2026-08 libsrt security release was noticed by hand. This script reads the
pinned SHAs from the tree (`git ls-tree HEAD`, no submodule checkout), resolves
each to its upstream tag, and reports three kinds of finding:

  release     a newer release exists in the pinned line (same major, or same
              major.minor for 0.x) — "deps: libsrt 1.5.8 released (pinned v1.5.7)"
  major-line  a newer major line exists — one finding per line, so closing the
              issue once silences that line — "deps: mbedtls 4.x line available (…)"
  advisory    a GitHub security advisory whose vulnerable range includes the
              pin, or an NVD CVE for the product's CPE that bounds the pin (or,
              when it carries no version bounds, was published after the pin's
              tag) — "deps: GHSA-… affects pinned libsrt v1.5.7 (critical)"

Pre-release tags are ignored. A pin that is not a tag (FreeRTOS-POSIX) gets
the advisory checks only and a commits-ahead note in the summary.

Findings become GitHub issues labelled `dependency-advisory`, deduplicated by
exact title against every existing issue with that label, open OR closed: a
closed issue is the maintainer's recorded decision (e.g. "stay on the 3.6 LTS
line") and is never reopened by this script. Every run writes a Markdown
summary to $GITHUB_STEP_SUMMARY (or stdout).

Lives under scripts/dev/ (not scripts/check/): it needs network + a token and
is a report, not a gate — it exits 0 whenever every query succeeded.

Usage:
  scripts/dev/deps_advisory.py --dry-run     # findings to stdout, no issues
  scripts/dev/deps_advisory.py               # create missing issues
Env: GH_TOKEN (GitHub API + `gh issue`), NVD_API_KEY (optional; without it NVD
     requests are paced to the public 5-per-30-s limit), DEPS_ADVISORY_REPO
     (aklofas/ts-transformer).
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
import urllib.parse
import urllib.request
from dataclasses import dataclass

LABEL = "dependency-advisory"
LABEL_DESCRIPTION = "Opened by deps-advisory.yml: a vendored native dependency fell behind or has an advisory"

# One entry per submodule in .gitmodules. `cpe` is the NVD product prefix
# (vendor:product) where NVD tracks the product; libsrt and librist have none.
DEPS = [
    {"name": "libsrt", "path": "crates/srt-sys/vendor/srt", "host": "github",
     "repo": "Haivision/srt", "url": "https://github.com/Haivision/srt"},
    {"name": "mbedtls", "path": "crates/mbedtls-src/vendor/mbedtls", "host": "github",
     "repo": "Mbed-TLS/mbedtls", "url": "https://github.com/Mbed-TLS/mbedtls",
     "cpe": "cpe:2.3:a:arm:mbed_tls"},
    {"name": "librist", "path": "crates/rist-sys/vendor/librist", "host": "gitlab",
     "repo": "rist/librist", "url": "https://code.videolan.org/rist/librist"},
    {"name": "freertos-kernel", "path": "embedded/vendor/freertos-kernel", "host": "github",
     "repo": "FreeRTOS/FreeRTOS-Kernel", "url": "https://github.com/FreeRTOS/FreeRTOS-Kernel",
     "cpe": "cpe:2.3:a:amazon:freertos"},
    {"name": "freertos-posix", "path": "embedded/vendor/freertos-plus-posix", "host": "github",
     "repo": "FreeRTOS/Lab-Project-FreeRTOS-POSIX",
     "url": "https://github.com/FreeRTOS/Lab-Project-FreeRTOS-POSIX"},
    {"name": "lwip", "path": "embedded/vendor/lwip", "host": "github",
     "repo": "lwip-tcpip/lwip", "url": "https://github.com/lwip-tcpip/lwip",
     "cpe": "cpe:2.3:a:lwip_project:lwip"},
]


@dataclass
class Finding:
    kind: str   # release | major-line | advisory
    title: str  # the dedupe key — deterministic per (dependency, version/id)
    body: str


# --------------------------------------------------------------------------
# Versions
# --------------------------------------------------------------------------

_VERSION_RE = re.compile(r"(?<![0-9])(\d+)[._](\d+)(?:[._](\d+))?(?![0-9])")
_PRERELEASE_RE = re.compile(r"(?i)(?:^|[-_.])(rc|alpha|beta|pre|dev)\d*(?:$|[-_.])")


def parse_version(tag: str) -> tuple[tuple[int, ...], bool] | None:
    """Return ((major, minor[, patch]), is_prerelease) or None for a non-version tag.

    Accepts `v1.5.7`, `mbedtls-3.6.7`, `V11.3.1`, `STABLE-2_2_1_RELEASE`,
    `v0.2.19-rc4`, `v1.0`. Anything without a dotted/underscored numeric core
    is not a version.
    """
    m = _VERSION_RE.search(tag or "")
    if not m:
        return None
    parts = tuple(int(p) for p in m.groups() if p is not None)
    return parts, bool(_PRERELEASE_RE.search(tag))


def line_key(version: tuple[int, ...]) -> tuple[int, ...]:
    """The release line a version belongs to: major, or (0, minor) for 0.x."""
    return (0, version[1]) if version[0] == 0 else (version[0],)


def _pad(v: tuple[int, ...]) -> tuple[int, int, int]:
    return tuple(list(v) + [0] * (3 - len(v)))[:3]


def _releases(tags) -> dict[tuple[int, ...], str]:
    """Non-prerelease version -> one tag spelling (tags may spell a version twice)."""
    out: dict[tuple[int, ...], str] = {}
    for tag in tags:
        parsed = parse_version(tag)
        if parsed is None or parsed[1]:
            continue
        out.setdefault(parsed[0], tag)
    return out


def newest_in_line(tags, pin: tuple[int, ...]):
    """(version, tag) of the newest release in the pin's line if newer than the pin, else None."""
    best = None
    for v, tag in _releases(tags).items():
        if line_key(v) == line_key(pin) and _pad(v) > _pad(pin) and (best is None or _pad(v) > _pad(best[0])):
            best = (v, tag)
    return best


def newer_lines(tags, pin: tuple[int, ...]) -> list[tuple[tuple[int, ...], str]]:
    """[(line, newest version string in that line)] for every line above the pin's."""
    lines: dict[tuple[int, ...], tuple[int, ...]] = {}
    for v in _releases(tags):
        k = line_key(v)
        if k > line_key(pin) and (k not in lines or _pad(v) > _pad(lines[k])):
            lines[k] = v
    return [(k, ".".join(str(x) for x in lines[k])) for k in sorted(lines)]


def resolve_pin_tag(tags: list[tuple[str, str]], sha: str, prefer=()) -> str | None:
    """The tag whose (peeled) commit is `sha`, or None for an untagged pin.

    When several tags share the commit (mbedtls spells every release twice,
    `v3.6.7` and `mbedtls-3.6.7`), a spelling in `prefer` — the release tag
    names — wins, so titles read the way `git submodule status` does.
    """
    matches = [name for name, tag_sha in tags if tag_sha == sha]
    for name in matches:
        if name in prefer:
            return name
    return matches[0] if matches else None


def choose_versions(release_tags: list[str], tags: list[str]) -> list[str]:
    """Upstream's curated releases list when it has one, else the raw tags.

    Tag lists carry non-release junk (`mbedos-16.03-release`, `V202110.00-SMP`)
    that parses as a huge major line; releases do not.
    """
    return list(release_tags) if release_tags else list(tags)


# --------------------------------------------------------------------------
# Advisory matching
# --------------------------------------------------------------------------

_BOUND_RE = re.compile(r"(<=|>=|<|>|=)\s*([^\s,]+)")


def range_affects(rng: str, pin: tuple[int, ...]) -> bool:
    """Evaluate a GHSA `vulnerable_version_range` (`>= V7.4.0 AND <= V11.3.0`, `< 1.5.6`).

    Unparsable or empty ranges count as affecting: the advisory is then
    surfaced for a human to read, which is the safe direction.
    """
    bounds = _BOUND_RE.findall(rng or "")
    if not bounds:
        return True
    p = _pad(pin)
    for op, raw in bounds:
        parsed = parse_version(raw)
        if parsed is None:
            return True
        v = _pad(parsed[0])
        ok = {"<": p < v, "<=": p <= v, ">": p > v, ">=": p >= v, "=": p == v}[op]
        if not ok:
            return False
    return True


def _after(published: str | None, pin_date: str | None) -> bool:
    """True when `published` is after `pin_date` (ISO-8601 strings compare lexically once trimmed)."""
    if not published or not pin_date:
        return True
    return published[:19] > pin_date[:19]


def ghsa_affects(adv: dict, pin: tuple[int, ...], pin_date: str | None = None) -> bool:
    ranges = [v.get("vulnerable_version_range") for v in adv.get("vulnerabilities") or []]
    ranges = [r for r in ranges if r]
    if not ranges:
        return _after(adv.get("published_at"), pin_date)
    return any(range_affects(r, pin) for r in ranges)


def nvd_affects(cve: dict, cpe: str, pin: tuple[int, ...], pin_date: str | None) -> bool:
    """True when a vulnerable cpeMatch for `cpe` bounds the pin, or is unbounded and newer than the pin."""
    p = _pad(pin)
    unbounded = False
    for conf in cve.get("cve", {}).get("configurations") or []:
        for node in conf.get("nodes") or []:
            for m in node.get("cpeMatch") or []:
                if not m.get("vulnerable") or not m.get("criteria", "").startswith(cpe + ":"):
                    continue
                exact = m["criteria"].split(":")[5]
                if exact not in ("*", "-"):
                    ev = parse_version(exact)
                    if ev is not None and _pad(ev[0]) == p:
                        return True
                    continue
                bounded = False
                inside = True
                for key, op in (("versionStartIncluding", ">="), ("versionStartExcluding", ">"),
                                ("versionEndIncluding", "<="), ("versionEndExcluding", "<")):
                    if key in m:
                        bounded = True
                        bv = parse_version(m[key])
                        if bv is None:
                            continue
                        inside &= range_affects(f"{op} {m[key]}", pin)
                if bounded and inside:
                    return True
                if not bounded:
                    unbounded = True
    return unbounded and _after(cve.get("cve", {}).get("published"), pin_date)


# --------------------------------------------------------------------------
# Findings
# --------------------------------------------------------------------------

def _fmt(v: tuple[int, ...]) -> str:
    return ".".join(str(x) for x in v)


def _nvd_severity(cve: dict) -> str:
    metrics = cve.get("cve", {}).get("metrics") or {}
    for key in ("cvssMetricV40", "cvssMetricV31", "cvssMetricV30", "cvssMetricV2"):
        for m in metrics.get(key) or []:
            sev = m.get("cvssData", {}).get("baseSeverity") or m.get("baseSeverity")
            if sev:
                return sev
    return "unrated"


def _nvd_description(cve: dict) -> str:
    for d in cve.get("cve", {}).get("descriptions") or []:
        if d.get("lang") == "en":
            return d.get("value", "")
    return ""


def build_findings(dep: dict, pin_tag, pin_version, tags, advisories, cves, pin_date=None) -> list[Finding]:
    out: list[Finding] = []
    if pin_version is None:
        return out
    name, path, url = dep["name"], dep["path"], dep["url"]
    pinned = f"(pinned {pin_tag})"
    newest = newest_in_line(tags, pin_version)
    if newest:
        out.append(Finding("release", f"deps: {name} {_fmt(newest[0])} released {pinned}",
                           f"Submodule `{path}` is pinned at `{pin_tag}`; upstream tag `{newest[1]}` "
                           f"is the newest release in the same line.\n\nReleases: {url}/releases\n\n"
                           f"After bumping the pin: full CI plus a `sanitizers.yml` dispatch (the native "
                           f"sanitizer legs build the vendored tree)."))
    for line, newest_v in newer_lines(tags, pin_version):
        label = f"{line[0]}.x" if len(line) == 1 else f"0.{line[1]}.x"
        out.append(Finding("major-line", f"deps: {name} {label} line available {pinned}",
                           f"Submodule `{path}` is pinned at `{pin_tag}`; upstream has a newer major line "
                           f"({label}, newest {newest_v}). A major-line migration is a decision, not a bump — "
                           f"close this issue to record \"stay on the pinned line\"; it is never reopened.\n\n"
                           f"Releases: {url}/releases"))
    for adv in advisories:
        if not ghsa_affects(adv, pin_version, pin_date):
            continue
        gid = adv.get("ghsa_id", "GHSA-?")
        sev = adv.get("severity") or "unrated"
        patched = ", ".join(v.get("patched_versions") or "?" for v in adv.get("vulnerabilities") or []) or "?"
        out.append(Finding("advisory", f"deps: {gid} affects pinned {name} {pin_tag} ({sev})",
                           f"**{adv.get('summary', '')}**\n\n{gid}" + (f" / {adv['cve_id']}" if adv.get("cve_id") else "")
                           + f"\nSeverity: {sev}\nPublished: {adv.get('published_at')}\nPatched: {patched}\n"
                           f"Advisory: {adv.get('html_url', url + '/security/advisories')}\n\n"
                           f"Submodule `{path}` is pinned at `{pin_tag}`, inside the advisory's vulnerable range."))
    cpe = dep.get("cpe")
    for cve in cves:
        if not cpe or not nvd_affects(cve, cpe, pin_version, pin_date):
            continue
        cid = cve.get("cve", {}).get("id", "CVE-?")
        sev = _nvd_severity(cve)
        out.append(Finding("advisory", f"deps: {cid} affects pinned {name} {pin_tag} ({sev})",
                           f"{_nvd_description(cve)}\n\n{cid}\nSeverity: {sev}\n"
                           f"Published: {cve.get('cve', {}).get('published')}\n"
                           f"NVD: https://nvd.nist.gov/vuln/detail/{cid}\n\n"
                           f"Submodule `{path}` is pinned at `{pin_tag}` (`{cpe}`); the CVE's version bounds "
                           f"include it, or it carries no bounds and post-dates the pin's tag."))
    return out


def new_findings(findings: list[Finding], existing_titles) -> list[Finding]:
    return [f for f in findings if f.title not in existing_titles]


# --------------------------------------------------------------------------
# Network (GitHub, GitLab, NVD) — nothing below is unit-tested; `--dry-run`
# exercises it against the live APIs.
# --------------------------------------------------------------------------

def _get(url: str, headers: dict | None = None) -> tuple[object, dict]:
    req = urllib.request.Request(url, headers={"User-Agent": "ts-transformer-deps-advisory", **(headers or {})})
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.loads(r.read().decode()), dict(r.headers)


def _gh_headers() -> dict:
    h = {"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28"}
    tok = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if tok:
        h["Authorization"] = f"Bearer {tok}"
    return h


def _gh_paginate(path: str) -> list:
    url = f"https://api.github.com/{path}{'&' if '?' in path else '?'}per_page=100"
    out: list = []
    while url:
        data, headers = _get(url, _gh_headers())
        out.extend(data)
        link = headers.get("Link", "")
        nxt = re.search(r'<([^>]+)>;\s*rel="next"', link)
        url = nxt.group(1) if nxt else None
    return out


def fetch_tags(dep: dict) -> list[tuple[str, str, str | None]]:
    """[(tag, peeled commit sha, commit date or None)] newest-agnostic (ordering is not relied on)."""
    if dep["host"] == "gitlab":
        proj = urllib.parse.quote(dep["repo"], safe="")
        tags, page = [], 1
        while True:
            data, _ = _get(f"https://code.videolan.org/api/v4/projects/{proj}/repository/tags?per_page=100&page={page}")
            tags += [(t["name"], t["commit"]["id"], t["commit"].get("committed_date")) for t in data]
            if len(data) < 100:
                return tags
            page += 1
    return [(t["name"], t["commit"]["sha"], None) for t in _gh_paginate(f"repos/{dep['repo']}/tags")]


def fetch_release_tags(dep: dict) -> list[str]:
    """Tag names of upstream's non-draft, non-prerelease GitHub releases (GitLab: none)."""
    if dep["host"] != "github":
        return []
    return [r["tag_name"] for r in _gh_paginate(f"repos/{dep['repo']}/releases")
            if not r.get("draft") and not r.get("prerelease")]


def fetch_commit_date(dep: dict, sha: str) -> str | None:
    if dep["host"] == "gitlab":
        proj = urllib.parse.quote(dep["repo"], safe="")
        data, _ = _get(f"https://code.videolan.org/api/v4/projects/{proj}/repository/commits/{sha}")
        return data.get("committed_date")
    data, _ = _get(f"https://api.github.com/repos/{dep['repo']}/commits/{sha}", _gh_headers())
    return data.get("commit", {}).get("committer", {}).get("date")


def fetch_advisories(dep: dict) -> list[dict]:
    if dep["host"] != "github":
        return []
    return _gh_paginate(f"repos/{dep['repo']}/security-advisories?state=published")


def fetch_commits_ahead(dep: dict, sha: str) -> int | None:
    if dep["host"] != "github":
        return None
    data, _ = _get(f"https://api.github.com/repos/{dep['repo']}/compare/{sha}...HEAD", _gh_headers())
    return data.get("ahead_by")


_nvd_last = [0.0]


def fetch_cves(cpe: str) -> list[dict]:
    """Every CVE NVD lists for the product (small sets: tens to ~a hundred); filtering is local."""
    key = os.environ.get("NVD_API_KEY")
    headers = {"apiKey": key} if key else {}
    out, start = [], 0
    while True:
        wait = (0.7 if key else 6.5) - (time.monotonic() - _nvd_last[0])
        if wait > 0:
            time.sleep(wait)
        _nvd_last[0] = time.monotonic()
        data, _ = _get("https://services.nvd.nist.gov/rest/json/cves/2.0?"
                       + urllib.parse.urlencode({"virtualMatchString": cpe, "resultsPerPage": 2000, "startIndex": start}),
                       headers)
        out += data.get("vulnerabilities") or []
        start += data.get("resultsPerPage") or 2000
        if start >= (data.get("totalResults") or 0):
            return out


def pins_from_tree() -> dict[str, str]:
    """Submodule path -> pinned SHA, from HEAD's tree (no submodule checkout needed)."""
    out = subprocess.run(["git", "ls-tree", "HEAD"] + [d["path"] for d in DEPS],
                         check=True, capture_output=True, text=True).stdout
    pins = {}
    for line in out.splitlines():
        meta, path = line.split("\t", 1)
        pins[path] = meta.split()[2]
    return pins


# --------------------------------------------------------------------------
# Issues
# --------------------------------------------------------------------------

def gh(*args: str) -> str:
    return subprocess.run(["gh", *args], check=True, capture_output=True, text=True).stdout


def existing_issue_titles(repo: str) -> set[str]:
    data = json.loads(gh("issue", "list", "--repo", repo, "--label", LABEL, "--state", "all",
                         "--limit", "1000", "--json", "title"))
    return {i["title"] for i in data}


def ensure_label(repo: str) -> None:
    try:
        gh("api", f"repos/{repo}/labels/{LABEL}")
    except subprocess.CalledProcessError:
        gh("api", "-X", "POST", f"repos/{repo}/labels", "-f", f"name={LABEL}", "-f", "color=d93f0b",
           "-f", f"description={LABEL_DESCRIPTION}")


def create_issue(repo: str, f: Finding) -> str:
    return gh("issue", "create", "--repo", repo, "--title", f.title, "--label", LABEL,
              "--body", f.body + "\n\n_Opened by `deps-advisory.yml`; closing records the decision._").strip()


# --------------------------------------------------------------------------
# Main
# --------------------------------------------------------------------------

def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--dry-run", action="store_true", help="print findings; create no issues")
    args = ap.parse_args(argv)
    repo = os.environ.get("DEPS_ADVISORY_REPO", "aklofas/ts-transformer")

    pins = pins_from_tree()
    rows, findings = [], []
    for dep in DEPS:
        sha = pins[dep["path"]]
        tags = fetch_tags(dep)
        release_tags = fetch_release_tags(dep)
        versions = choose_versions(release_tags, [t for t, _, _ in tags])
        pin_tag = resolve_pin_tag([(t, s) for t, s, _ in tags], sha, prefer=set(release_tags))
        parsed = parse_version(pin_tag) if pin_tag else None
        pin_version = parsed[0] if parsed else None
        pin_date = next((d for t, _, d in tags if t == pin_tag and d), None) or fetch_commit_date(dep, sha)
        advisories = fetch_advisories(dep)
        cves = fetch_cves(dep["cpe"]) if dep.get("cpe") else []
        found = build_findings(dep, pin_tag, pin_version, versions, advisories, cves, pin_date)
        findings += found
        ahead = fetch_commits_ahead(dep, sha) if pin_version is None else None
        newest = newest_in_line(versions, pin_version) if pin_version else None
        rows.append((dep["name"], pin_tag or f"{sha[:8]} (untagged)", pin_date or "?",
                     newest[1] if newest else ("current" if pin_version else f"{ahead} commit(s) ahead upstream"),
                     len(advisories), len(cves), len(found)))

    existing = set() if args.dry_run else existing_issue_titles(repo)
    fresh = new_findings(findings, existing)
    created = []
    if not args.dry_run and fresh:
        ensure_label(repo)
        created = [create_issue(repo, f) for f in fresh]

    lines = ["## Native dependency advisory check", "",
             "| dependency | pinned | tag date | newest in line | GHSA | NVD CVEs | findings |",
             "|---|---|---|---|---|---|---|"]
    lines += ["| " + " | ".join(str(c) for c in r) + " |" for r in rows]
    lines += ["", f"**{len(findings)} finding(s)**, {len(fresh)} new"
              + (" (dry run: issues not consulted or created)" if args.dry_run else
                 f", {len(findings) - len(fresh)} already tracked by an open or closed `{LABEL}` issue"), ""]
    for f in findings:
        mark = "NEW" if f in fresh else "tracked"
        lines.append(f"- [{f.kind}] {f.title} — {mark}")
    lines += [f"  - created {u}" for u in created]
    text = "\n".join(lines) + "\n"
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as fh:
            fh.write(text)
    sys.stdout.write(text)
    if args.dry_run:
        for f in findings:
            sys.stdout.write(f"\n--- {f.title}\n{f.body}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
