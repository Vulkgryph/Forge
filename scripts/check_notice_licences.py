#!/usr/bin/env python3
"""Every licence in the shipped dependency graph that is not MIT or Apache-2.0
must be named in NOTICE.

Why this exists: NOTICE said its crate list named everything "because their
terms are not those two", and it had missed every ISC crate (including `ring`,
a direct dependency), every BSD-3-Clause crate, and MIT-0. The section was
written once against the graph of the day and nothing re-checked it, while the
graph kept moving — `scraper`'s removal took four MPL-2.0 crates out, and
`russh` brought AWS-LC in.

The rule this enforces is NOTICE's own: a crate whose licence expression offers
plain MIT or Apache-2.0 as a standalone alternative needs no separate mention,
because the blanket statement covers it. Anything else — a conjunction, or a
choice that does not include either — has to be named.

Exit 0 if NOTICE covers the graph; exit 1 listing what is missing.
"""
import json
import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# How each SPDX identifier may be written in NOTICE. Prose is allowed to be
# prose — "Mozilla Public License 2.0" is a better thing to show a compliance
# reviewer than "MPL-2.0" — so each id lists every spelling that counts.
SPELLINGS = {
    "MPL-2.0": ["MPL-2.0", "Mozilla Public License 2.0"],
    "CDLA-Permissive-2.0": ["CDLA-Permissive-2.0"],
    "Unicode-3.0": ["Unicode-3.0"],
    "CC0-1.0": ["CC0-1.0"],
    "ISC": ["ISC"],
    "BSD-3-Clause": ["BSD-3-Clause", "BSD 3-Clause"],
    "BSD-2-Clause": ["BSD-2-Clause", "BSD 2-Clause"],
    "MIT-0": ["MIT-0"],
    "0BSD": ["0BSD"],
    "BSL-1.0": ["BSL-1.0", "Boost Software License"],
    "Zlib": ["Zlib", "zlib"],
    "OFL-1.1": ["OFL-1.1", "Open Font License"],
    "LicenseRef-UFL-1.0": ["UFL-1.0", "Ubuntu Font Licence"],
    "Unlicense": ["Unlicense"],
    "OpenSSL": ["OpenSSL"],
}

# Satisfied by NOTICE's blanket "predominantly MIT and Apache-2.0" sentence.
BLANKET = {"MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception"}


def alternatives(expr):
    """Top-level OR alternatives of an SPDX expression, parens respected."""
    parts, depth, cur = [], 0, ""
    for tok in re.split(r"(\(|\)|\sOR\s|\sAND\s|/)", expr):
        if tok == "(":
            depth += 1
            cur += tok
        elif tok == ")":
            depth -= 1
            cur += tok
        elif depth == 0 and (tok.strip() == "OR" or tok == "/"):
            parts.append(cur.strip())
            cur = ""
        else:
            cur += tok
    parts.append(cur.strip())
    return [p for p in parts if p]


def identifiers(expr):
    return set(re.findall(r"[A-Za-z0-9.\-]+(?:-[A-Za-z0-9.]+)*", expr)) - {
        "OR", "AND", "WITH",
    }


def main():
    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1"],
            cwd=REPO, capture_output=True, text=True, check=True,
        ).stdout
    )
    licences = {
        f"{p['name']} {p['version']}": (p.get("license") or "")
        for p in meta["packages"]
    }

    # The normal graph only: dev- and build-dependencies are not linked into
    # anything distributed, and NOTICE's boundary section says as much.
    tree = subprocess.run(
        ["cargo", "tree", "-e", "normal", "--workspace",
         "--prefix", "none", "--no-dedupe"],
        cwd=REPO, capture_output=True, text=True, check=True,
    ).stdout
    shipped = {
        f"{m.group(1)} {m.group(2)}"
        for m in (re.match(r"([A-Za-z0-9_.\-]+) v([0-9][^ ]*)", ln.strip())
                  for ln in tree.splitlines())
        if m
    }

    # Optional path so this can be run against a different NOTICE — which is
    # how it was checked against the version that was missing the ISC and
    # BSD-3-Clause blocks, rather than by disturbing the real file.
    target = Path(sys.argv[1]) if len(sys.argv) > 1 else REPO / "NOTICE"
    notice = target.read_text(encoding="utf-8")
    missing, unknown = {}, {}

    for crate in sorted(shipped):
        expr = licences.get(crate, "")
        if not expr:
            continue
        # A standalone MIT or Apache-2.0 option means the blanket covers it.
        if any(alt in BLANKET for alt in alternatives(expr)):
            continue
        for ident in sorted(identifiers(expr)):
            if ident in BLANKET:
                continue
            spellings = SPELLINGS.get(ident)
            if spellings is None:
                unknown.setdefault(ident, []).append(crate)
            elif not any(sp in notice for sp in spellings):
                missing.setdefault(ident, []).append(crate)

    if not missing and not unknown:
        print(f"NOTICE covers all {len(shipped)} crates in the shipped graph.")
        return 0

    for ident, crates in sorted(missing.items()):
        print(f"::error::NOTICE does not name {ident}, required by: "
              f"{', '.join(crates)}")
    for ident, crates in sorted(unknown.items()):
        print(f"::error::unrecognised licence {ident} on {', '.join(crates)} — "
              f"add it to SPELLINGS in this script and to NOTICE")
    print("\nA crate entered the graph whose terms are not MIT or Apache-2.0 "
          "and NOTICE does not attribute it.", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
