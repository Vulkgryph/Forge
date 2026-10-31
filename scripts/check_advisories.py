#!/usr/bin/env python3
"""Check every crate in `Cargo.lock` against the GitHub Advisory Database.

This replaces Dependabot's pull requests, which this project does not take. The
alerts GitHub raises on its own are still useful — they are how the eight
advisories fixed in 0.7.0 were found — but they arrive on GitHub's schedule and
are read from a web page. This asks the same database directly, on demand,
against the lockfile as it stands right now, and it answers for the whole graph
rather than only for direct dependencies.

Deliberately no `cargo audit`: it is another tool to install and keep current,
and the data it reads is this same database. The only requirement here is `gh`,
which a release already needs.

Usage:
    python3 scripts/check_advisories.py            # fail if anything matches
    python3 scripts/check_advisories.py --offline  # skip, and say so
    python3 scripts/check_advisories.py --self-test
    python3 scripts/check_advisories.py --json     # machine-readable findings

Exit status is 1 when a crate in the lockfile falls inside an advisory's
vulnerable range, and 1 as well when the database cannot be reached — a check
that passes because it never ran is worse than no check, since it reads as a
clean bill of health. `--offline` is the way to say that out loud.
"""
from __future__ import annotations

import json
import re
import subprocess
import sys

LOCKFILE = "Cargo.lock"

# GraphQL field aliases have to be valid names, and crate names have hyphens,
# so each package is asked for under a positional alias. 80 at a time keeps
# each query well inside the API's complexity budget.
BATCH = 80


# ---------------------------------------------------------------- versions

def parse_version(text: str) -> tuple:
    """A semver version as a sortable key.

    Returns `(release, is_final, prerelease)`. `is_final` carries the rule that
    a prerelease sorts *below* the release it leads to, so 1.0.0-rc1 < 1.0.0.
    Build metadata is dropped: semver says it takes no part in precedence.
    """
    text = text.strip().lstrip("v")
    text = text.split("+", 1)[0]
    if "-" in text:
        core, pre = text.split("-", 1)
    else:
        core, pre = text, ""

    nums = []
    for part in core.split("."):
        nums.append(int(part) if part.isdigit() else 0)
    while len(nums) < 3:
        nums.append(0)

    # Numeric prerelease identifiers compare numerically, alphanumeric ones
    # compare as text, and numeric sorts below alphanumeric.
    ident: list = []
    if pre:
        for part in pre.split("."):
            if part.isdigit():
                ident.append((0, int(part), ""))
            else:
                ident.append((1, 0, part))

    return (tuple(nums), 1 if not pre else 0, tuple(ident))


def satisfies(version: str, constraint: str) -> bool:
    """Does `version` satisfy one comparator, e.g. `>= 1.2.3` or `< 2.0`?"""
    constraint = constraint.strip()
    match = re.match(r"^(>=|<=|==|=|>|<)\s*(.+)$", constraint)
    if not match:
        # A bare version in a range means exactly that version.
        return parse_version(version) == parse_version(constraint)
    op, rhs = match.group(1), match.group(2)
    a, b = parse_version(version), parse_version(rhs)
    if op == ">=":
        return a >= b
    if op == "<=":
        return a <= b
    if op in ("==", "="):
        return a == b
    if op == ">":
        return a > b
    return a < b


def in_range(version: str, vulnerable_range: str) -> bool:
    """Is `version` inside a GHSA `vulnerableVersionRange`?

    The ranges look like `>= 0.23.44, < 0.23.46` or `< 1.3.0` or `= 2.0.1`:
    comma-separated comparators, all of which must hold.
    """
    return all(
        satisfies(version, part)
        for part in vulnerable_range.split(",")
        if part.strip()
    )


# ---------------------------------------------------------------- lockfile

def lockfile_packages(path: str = LOCKFILE) -> dict[str, set[str]]:
    """Every package in the lockfile, as name -> set of versions.

    A set, because a lockfile legitimately carries two majors of the same
    crate at once, and only one of them may be affected.
    """
    try:
        text = open(path, encoding="utf-8").read()
    except OSError as exc:
        print(f"::error::cannot read {path}: {exc}")
        sys.exit(1)

    found: dict[str, set[str]] = {}
    for block in text.split("[[package]]"):
        name = re.search(r'^name = "([^"]+)"', block, re.M)
        version = re.search(r'^version = "([^"]+)"', block, re.M)
        if name and version:
            found.setdefault(name.group(1), set()).add(version.group(1))
    return found


# ---------------------------------------------------------------- database

def query_advisories(names: list[str]) -> list[dict] | None:
    """Advisory entries for these crate names, or None if unreachable."""
    fields = []
    for i, name in enumerate(names):
        escaped = name.replace('"', '\\"')
        fields.append(
            f'p{i}: securityVulnerabilities(ecosystem: RUST, package: "{escaped}", first: 20) '
            "{ nodes { package { name } vulnerableVersionRange "
            "firstPatchedVersion { identifier } "
            "advisory { ghsaId severity summary permalink withdrawnAt } } }"
        )
    query = "{ " + " ".join(fields) + " }"

    try:
        done = subprocess.run(
            ["gh", "api", "graphql", "-f", f"query={query}"],
            capture_output=True,
            text=True,
            timeout=120,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        print(f"::error::could not reach the advisory database: {exc}")
        return None

    if done.returncode != 0:
        print(f"::error::advisory query failed: {done.stderr.strip()[:300]}")
        return None

    try:
        payload = json.loads(done.stdout)
    except json.JSONDecodeError as exc:
        print(f"::error::advisory query returned unreadable JSON: {exc}")
        return None

    out = []
    for value in (payload.get("data") or {}).values():
        for node in (value or {}).get("nodes", []):
            out.append(node)
    return out


def findings(packages: dict[str, set[str]]) -> list[dict] | None:
    names = sorted(packages)
    hits: list[dict] = []
    for start in range(0, len(names), BATCH):
        batch = names[start : start + BATCH]
        nodes = query_advisories(batch)
        if nodes is None:
            return None
        for node in nodes:
            advisory = node.get("advisory") or {}
            # A withdrawn advisory is one the database itself has retracted.
            if advisory.get("withdrawnAt"):
                continue
            name = (node.get("package") or {}).get("name")
            vulnerable = node.get("vulnerableVersionRange") or ""
            for version in packages.get(name, ()):
                if in_range(version, vulnerable):
                    patched = (node.get("firstPatchedVersion") or {}).get("identifier")
                    hits.append(
                        {
                            "package": name,
                            "version": version,
                            "range": vulnerable,
                            "ghsa": advisory.get("ghsaId"),
                            "severity": (advisory.get("severity") or "").upper(),
                            "summary": advisory.get("summary"),
                            "patched": patched,
                            "url": advisory.get("permalink"),
                        }
                    )
    return hits


# ---------------------------------------------------------------- self-test

def self_test(quiet: bool = False) -> int:
    """Fixed cases for the range matching, including one known to have bitten.

    The comparison is the only part of this that can be wrong quietly: a range
    parsed too narrowly reports a clean tree. The rustls case is the real one
    from 0.7.0 — 0.23.45 was affected and 0.23.46 was the fix.
    """
    cases = [
        # (version, range, expected)
        ("0.23.45", ">= 0.23.44, < 0.23.46", True),
        ("0.23.46", ">= 0.23.44, < 0.23.46", False),
        ("0.23.43", ">= 0.23.44, < 0.23.46", False),
        ("1.2.9", "< 1.3.0", True),
        ("1.3.0", "< 1.3.0", False),
        ("2.0.1", "= 2.0.1", True),
        ("2.0.2", "= 2.0.1", False),
        ("0.63.3", ">= 0.63.0, <= 0.63.3", True),
        ("0.64.1", ">= 0.63.0, <= 0.63.3", False),
        # A prerelease sorts below the release it leads to.
        ("1.0.0-rc1", ">= 1.0.0", False),
        ("1.0.0", ">= 1.0.0", True),
        ("0.24.0-dev.1", ">= 0.24.0-dev.1, < 0.24.0-dev.2", True),
        ("0.24.0-dev.2", ">= 0.24.0-dev.1, < 0.24.0-dev.2", False),
        # Two-component versions appear in real ranges.
        ("1.5.0", ">= 1.0, < 2.0", True),
        ("2.0.0", ">= 1.0, < 2.0", False),
        # Build metadata takes no part in precedence.
        ("0.38.0+1.3.281", ">= 0.38.0, < 0.39.0", True),
    ]
    bad = []
    for version, rng, expected in cases:
        got = in_range(version, rng)
        if got != expected:
            bad.append(f"  {version} in '{rng}': expected {expected}, got {got}")
    if bad:
        print("range matching is wrong:")
        print("\n".join(bad))
        return 1
    if not quiet:
        print(f"range matching: {len(cases)} cases ok")
    return 0


# ---------------------------------------------------------------- entry

def main(argv: list[str]) -> int:
    if "--self-test" in argv:
        return self_test()

    if "--offline" in argv:
        print("advisories: SKIPPED — asked to run offline, so nothing was checked")
        return 0

    as_json = "--json" in argv

    # The matching is cheap, and a wrong comparison reports a clean tree, so
    # it is checked every run rather than only when someone remembers to.
    # Quiet under --json: that output is piped, and a stray human-readable
    # line ahead of it makes the whole document unparseable.
    if self_test(quiet=as_json) != 0:
        return 1

    packages = lockfile_packages()
    hits = findings(packages)
    if hits is None:
        print(
            "::error::the advisory database could not be reached, so nothing was "
            "checked. Re-run with network access, or --offline to say so out loud."
        )
        return 1

    if as_json:
        print(json.dumps(hits, indent=2, sort_keys=True))
    else:
        order = {"CRITICAL": 0, "HIGH": 1, "MODERATE": 2, "LOW": 3}
        for h in sorted(hits, key=lambda x: (order.get(x["severity"], 9), x["package"])):
            print(f"::error::{h['package']} {h['version']} — {h['severity']} {h['ghsa']}")
            print(f"    {h['summary']}")
            print(f"    affected: {h['range']}   fixed in: {h['patched'] or 'nothing yet'}")
            print(f"    {h['url']}")

    count = len(packages)
    if as_json:
        # Nothing but the document on stdout, so it can be piped.
        return 1 if hits else 0
    if hits:
        print(f"\n{len(hits)} advisory match(es) across {count} packages in {LOCKFILE}")
        return 1
    print(f"advisories: none affecting the {count} packages in {LOCKFILE}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
