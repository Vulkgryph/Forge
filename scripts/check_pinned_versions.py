#!/usr/bin/env python3
"""Fail if a version literal would reach somebody — printed in documentation,
or sent on the wire as a user agent.

Two examples were announcing 0.5.0 at 0.5.2, which is a lie told to the sites
least able to check it. Build it from CARGO_PKG_VERSION, or write <version>.

What this must NOT flag, because the first version of this check did, and that
is how a check gets switched off rather than fixed:

  * dated CHANGELOG entries, which are history and are supposed to name the
    version they shipped in
  * comments and doc comments, which illustrate the format
  * test fixtures, which need a concrete string to parse

Lives here rather than inline in CI so that `scripts/preflight.sh` runs the
same rules rather than a simplified rewrite of them — a second copy of a rule
is a second thing to get wrong, and the copy is always the one that is wrong.
"""
import re
import subprocess
import sys

PIN = re.compile(r"forge-(search|agent|ide|tui)/[0-9]+\.[0-9]+\.[0-9]+")


def offenders() -> list[str]:
    out = []
    files = subprocess.run(
        ["git", "ls-files", "*.md", "*.rs"], capture_output=True, text=True
    ).stdout.split()
    for path in files:
        if path.endswith("CHANGELOG.md") or "/vendor/" in path:
            continue
        in_tests = False
        with open(path, encoding="utf-8") as fh:
            for n, line in enumerate(fh, 1):
                if path.endswith(".rs"):
                    if "#[cfg(test)]" in line:
                        in_tests = True
                    if in_tests:
                        continue
                    if line.lstrip().startswith("//"):
                        continue
                if PIN.search(line):
                    out.append(f"{path}:{n}: {line.strip()}")
    return out


def main() -> int:
    bad = offenders()
    for b in bad:
        print(f"::error::pinned version: {b}")
    if bad:
        print("build it from CARGO_PKG_VERSION, or write <version>")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
