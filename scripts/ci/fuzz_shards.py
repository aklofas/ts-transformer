#!/usr/bin/env python3
"""The fuzz leg's shards: which declared targets each job runs, and the one result they add up to.

  plan    --shards N --seconds S --max-minutes M [--root DIR]
          the job matrix on stdout (one line of JSON), the layout on stderr
  targets --shards N --shard K [--root DIR]
          shard K's targets, one `crate name` per line
  merge   --shards N --seconds S --results DIR --out FILE [--root DIR]
          read DIR/fuzz-shard-<K>.json (written by scripts/ci/fuzz-shard.sh), write
          FILE as release-qualify's collector reads it, exit 1 on any problem

The targets are the rows of tests/coverage/fuzz-targets.toml, sorted by crate
then name and cut into at most N consecutive runs whose sizes differ by at most
one. Every target runs for the same time, so equal sizes are equal fuzzing
times, and consecutive runs split a crate only at a shard boundary, so a job
builds few fuzz workspaces. Every command deals the same way from the same
file: a target is in exactly one shard, and `merge` holds each result to the
targets its shard was dealt.

Nothing here passes by absence. An empty manifest, a duplicate target, a crate
without a fuzz workspace and a shard estimated over the limit are errors; a
missing or unreadable result, a target that did not run, a crash and a total
short of the manifest are problems, and `merge` exits 1 on each. It still
writes FILE when at least one shard reported, so the record carries the counts
of a failed leg; with no result at all it writes nothing. Stdlib only.
"""
import argparse
import json
import os
import re
import sys

try:
    import tomllib
except ImportError:  # Python < 3.11
    print("fuzz_shards: needs Python >= 3.11 (tomllib)", file=sys.stderr)
    sys.exit(1)

MANIFEST = "tests/coverage/fuzz-targets.toml"
# Allowances for the estimate `plan` prints and holds to its limit, in minutes:
# a job's steps before the first target, and one fuzz-workspace build. The
# slowest measured build is tst-webrtc's, at a little over three minutes.
SETUP_MINUTES = 3
BUILD_MINUTES_PER_CRATE = 4
# The outcomes scripts/ci/fuzz-shard.sh writes for a target that ran: a crash
# is a finding of a target that did run.
RAN = ("ok", "crash")
WORD = re.compile(r"[A-Za-z0-9_-]+")


class ShardError(Exception):
    pass


class OtherShard(Exception):
    """A result file that says it belongs to another shard or another shard count."""


def load_targets(root):
    """The declared targets as (crate, name), sorted."""
    path = os.path.join(root, MANIFEST)
    try:
        with open(path, "rb") as f:
            rows = tomllib.load(f).get("target", [])
    except (OSError, tomllib.TOMLDecodeError) as e:
        raise ShardError("cannot read %s: %s" % (MANIFEST, e))
    if not rows:
        raise ShardError("%s declares no fuzz target" % MANIFEST)
    targets = []
    for r in rows:
        crate, name = r.get("crate"), r.get("name")
        if not isinstance(crate, str) or not isinstance(name, str):
            raise ShardError("%s: a [[target]] row needs a crate and a name, got %r" % (MANIFEST, r))
        # Both become a path, a job label and part of an artefact name.
        if not WORD.fullmatch(crate) or not WORD.fullmatch(name):
            raise ShardError("%s: %r / %r is not a crate and a target name" % (MANIFEST, crate, name))
        targets.append((crate, name))
    targets.sort()
    for a, b in zip(targets, targets[1:]):
        if a == b:
            raise ShardError("%s: %s/%s is declared twice" % (MANIFEST, a[0], a[1]))
    for crate in sorted({c for c, _ in targets}):
        if not os.path.isfile(os.path.join(root, "crates", crate, "fuzz", "Cargo.toml")):
            raise ShardError("%s names crate %s, but crates/%s/fuzz is not a fuzz workspace"
                             % (MANIFEST, crate, crate))
    return targets


def deal(targets, shards):
    """`targets` cut into min(shards, len(targets)) consecutive, non-empty runs whose
    sizes differ by at most one, the longer ones first."""
    if shards < 1:
        raise ShardError("the shard count must be at least 1, not %d" % shards)
    if not targets:
        raise ShardError("there is no fuzz target to deal")
    count = min(shards, len(targets))
    size, extra = divmod(len(targets), count)
    out, at = [], 0
    for i in range(count):
        n = size + (1 if i < extra else 0)
        out.append(targets[at:at + n])
        at += n
    return out


def crates_of(shard):
    return sorted({c for c, _ in shard})


def estimate_minutes(shard, seconds):
    return len(shard) * seconds / 60 + SETUP_MINUTES + BUILD_MINUTES_PER_CRATE * len(crates_of(shard))


def _dealt(root, shards, seconds=None):
    if seconds is not None and seconds < 1:
        raise ShardError("the seconds per target must be at least 1, not %d" % seconds)
    return deal(load_targets(root), shards)


def plan(root, shards, seconds, max_minutes):
    """The job matrix: one entry per shard. A shard estimated to outlast
    `max_minutes` is refused here, before any job starts."""
    dealt = _dealt(root, shards, seconds)
    include = []
    for i, shard in enumerate(dealt, 1):
        minutes = estimate_minutes(shard, seconds)
        if minutes > max_minutes:
            raise ShardError(
                "shard %d of %d would take about %d min at %d s per target, over the limit of %d min: "
                "deal more shards (FUZZ_SHARDS in .github/workflows/fuzz.yml)"
                % (i, len(dealt), round(minutes), seconds, max_minutes))
        include.append({"shard": i, "of": len(dealt), "targets": len(shard),
                        "crates": ", ".join(crates_of(shard))})
    return {"include": include}


def layout(root, shards, seconds):
    """The plan as text: one line per shard, then the total."""
    dealt = _dealt(root, shards, seconds)
    lines = ["shard %d of %d: %d targets, about %d min (%s)"
             % (i, len(dealt), len(s), round(estimate_minutes(s, seconds)), " ".join("%s/%s" % t for t in s))
             for i, s in enumerate(dealt, 1)]
    total = sum(len(s) for s in dealt)
    lines.append("%d targets in %d shards at %d s each: %d min of fuzzing in all"
                 % (total, len(dealt), seconds, round(total * seconds / 60)))
    return "\n".join(lines) + "\n"


def _read_result(path, shard, shards):
    """One shard's result as {(crate, name): (outcome, artefacts)}, its seconds and the
    targets it lists twice; raises OtherShard when it is the result of another shard
    or shard count, another error on anything else than what fuzz-shard.sh writes."""
    with open(path) as f:
        doc = json.load(f)
    if not isinstance(doc, dict) or not isinstance(doc.get("targets"), list):
        raise ValueError("no target list")
    for key in ("shard", "shards", "seconds_per_target"):
        if isinstance(doc.get(key), bool) or not isinstance(doc.get(key), int):
            raise ValueError("%s is not an integer" % key)
    if (doc["shard"], doc["shards"]) != (shard, shards):
        raise OtherShard("says it is shard %d of %d" % (doc["shard"], doc["shards"]))
    rows, twice = {}, []
    for t in doc["targets"]:
        key = (t["crate"], t["name"])
        if key in rows:
            twice.append("%s/%s" % key)
        rows[key] = (str(t["outcome"]), int(t["artefacts"]))
    return rows, doc["seconds_per_target"], twice


def merge(root, shards, seconds, results):
    """(the merged result or None, the problems). The merged result counts a target only
    in the shard it was dealt to, and only when that shard reports it ran."""
    dealt = _dealt(root, shards, seconds)
    of = len(dealt)
    declared = sum(len(s) for s in dealt)
    problems, ran, crashes, secs, reported = [], [], 0, [], 0
    for i, shard in enumerate(dealt, 1):
        name = "fuzz-shard-%d.json" % i
        where = "shard %d of %d" % (i, of)
        path = os.path.join(results, name)
        if not os.path.isfile(path):
            problems.append("%s left no result (%s)" % (where, name))
            continue
        try:
            rows, shard_seconds, twice = _read_result(path, i, shards)
        except OtherShard as e:
            problems.append("%s: %s %s" % (where, name, e))
            continue
        except (OSError, ValueError, KeyError, TypeError) as e:
            problems.append("%s is unreadable: %s" % (name, e))
            continue
        reported += 1
        secs.append(shard_seconds)
        if shard_seconds != seconds:
            problems.append("%s ran %d s per target, not the %d asked for" % (where, shard_seconds, seconds))
        if twice:
            problems.append("%s lists %s twice" % (where, ", ".join(twice)))
        missing = ["%s/%s" % t for t in shard if t not in rows]
        unplanned = ["%s/%s" % t for t in sorted(rows) if t not in shard]
        if missing or unplanned:
            problems.append("%s did not report its own targets: %s" % (where, "; ".join(
                ["missing " + ", ".join(missing)] * bool(missing)
                + ["unplanned " + ", ".join(unplanned)] * bool(unplanned))))
        before = len(ran)
        for t in shard:
            if t not in rows:
                continue
            outcome, artefacts = rows[t]
            if outcome in RAN:
                ran.append(t)
            if outcome == "crash":
                crashes += artefacts
                problems.append("%s/%s (%s): crash, %d artefact file(s)" % (t[0], t[1], where, artefacts))
            elif outcome != "ok":
                problems.append("%s/%s (%s): %s" % (t[0], t[1], where, outcome))
        if len(ran) == before:
            problems.append("%s ran none of its %d targets" % (where, len(shard)))
    planned = {"fuzz-shard-%d.json" % i for i in range(1, of + 1)}
    for extra in sorted(n for n in (os.listdir(results) if os.path.isdir(results) else [])
                        if re.fullmatch(r"fuzz-shard-\d+\.json", n) and n not in planned):
        problems.append("%s is not one of the %d planned shards" % (extra, of))
    if not reported:
        return None, problems
    if len(ran) != declared:
        problems.append("ran %d of %d declared targets" % (len(ran), declared))
    return {"targets_run": len(ran), "seconds_per_target": min(secs), "crashes": crashes,
            "shards": of, "crates": crates_of(ran)}, problems


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("plan", "targets", "merge"):
        p = sub.add_parser(name)
        p.add_argument("--root", default=".")
        p.add_argument("--shards", type=int, required=True)
        if name == "targets":
            p.add_argument("--shard", type=int, required=True)
        else:
            p.add_argument("--seconds", type=int, required=True)
        if name == "plan":
            p.add_argument("--max-minutes", type=int, required=True)
        if name == "merge":
            p.add_argument("--results", required=True)
            p.add_argument("--out", required=True)
    args = ap.parse_args(argv)
    try:
        if args.cmd == "plan":
            matrix = plan(args.root, args.shards, args.seconds, args.max_minutes)
            sys.stderr.write(layout(args.root, args.shards, args.seconds))
            print(json.dumps(matrix, separators=(",", ":")))
            return 0
        if args.cmd == "targets":
            dealt = _dealt(args.root, args.shards)
            if not 1 <= args.shard <= len(dealt):
                raise ShardError("there is no shard %d: %d were dealt" % (args.shard, len(dealt)))
            for t in dealt[args.shard - 1]:
                print("%s %s" % t)
            return 0
        merged, problems = merge(args.root, args.shards, args.seconds, args.results)
    except ShardError as e:
        print("fuzz_shards: " + str(e), file=sys.stderr)
        return 1
    if merged is not None:
        with open(args.out, "w") as f:
            json.dump(merged, f, indent=2)
            f.write("\n")
        declared = len(load_targets(args.root))
        print("fuzz: %d of %d declared targets ran for %d s each with %d crash(es)"
              % (merged["targets_run"], declared, merged["seconds_per_target"], merged["crashes"]))
    for p in problems:
        print("fuzz_shards: " + p, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
