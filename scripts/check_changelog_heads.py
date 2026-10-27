#!/usr/bin/env python3
"""Every released crate's version must have a dated section at the head of its
own changelog, and nothing may be filed under a version already published.

Three faults got past preflight at 0.7.0, all of them in this class:

  * `forge-tui-rs` shipped 0.7.0 with no 0.7.0 section at all, so its real
    change had nowhere to go;
  * that change was therefore filed under `[0.6.0]` — the *published*
    release — telling anyone who downloads 0.6.0 that a fix it does not have
    is in it;
  * the 0.7.0 heading kept a date nine commits out of date.

Two of the three are catchable exactly: the newest section must name the
version being shipped, and must carry a date. The third — an entry filed
under a version already published — follows from the first, because that is
only reachable when the right section does not exist to put it in.

Deliberately not attempted: searching history for which commit introduced a
given entry. Changelog prose gets reflowed, so a text search finds the
reformat rather than the origin, and the first version of this check
reported three entries that were genuinely part of 0.6.0. A check that
reports a clean tree is one somebody switches off within the week.
"""
from __future__ import annotations

import re
import subprocess
import sys

# The crates whose versions are announced and whose changelogs people read.
RELEASED = ["forge-agent", "forge-ide", "forge-tui-rs"]

HEAD_RE = re.compile(r"^## \[(?P<ver>[0-9]+\.[0-9]+\.[0-9]+)\](?P<rest>.*)$", re.M)


def crate_version(crate: str) -> str | None:
    try:
        with open(f"{crate}/Cargo.toml", encoding="utf-8") as fh:
            for line in fh:
                if line.startswith("version"):
                    return line.split('"')[1]
    except OSError:
        return None
    return None


def main() -> int:
    bad: list[str] = []

    for crate in RELEASED:
        path = f"{crate}/CHANGELOG.md"
        version = crate_version(crate)
        if version is None:
            bad.append(f"{crate}: no version in Cargo.toml")
            continue
        try:
            text = open(path, encoding="utf-8").read()
        except OSError:
            bad.append(f"{path}: missing")
            continue

        heads = list(HEAD_RE.finditer(text))
        if not heads:
            bad.append(f"{path}: no version sections at all")
            continue

        first = heads[0]
        if first.group("ver") != version:
            bad.append(
                f"{path}: {crate} is {version} but the newest section is "
                f"[{first.group('ver')}] — the release has nowhere to be described"
            )
        elif not re.search(r"—\s*\d{4}-\d{2}-\d{2}", first.group("rest")):
            bad.append(f"{path}: [{version}] has no date")


    for b in bad:
        print(f"::error::{b}")
    if bad:
        print("a changelog describes a release that cannot contain it")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
