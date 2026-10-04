#!/usr/bin/env python3
"""Refuses to let the build machine, or the people behind it, into anything published.

Four surfaces can carry an identity out of here, and until this existed only the
first was checked:

    tracked files      prose and source in the repository
    commit messages    never scanned; a username leaked twice this way, the
                       second time in the very commit that removed the first
    pushed range       the last moment anything is still private
    build artifacts    what a person actually downloads. A release binary bakes
                       every panic site's source path into .rodata as a string
                       literal, so a build from a home directory ships the
                       absolute path of every dependency that can panic —
                       hundreds of them, and `strip` does not remove one.

Names worth protecting cannot be written down here: a list of them in a public
repository is the leak it is meant to prevent. So the patterns come from two
places that are not published — the machine itself (its home directory and
account), and an untracked file inside .git. See `names_from_git_dir`.

Usage:
    check_private.py --tracked
    check_private.py --messages <range>      # e.g. origin/main..HEAD
    check_private.py --artifacts <path>...
    check_private.py --all <range>           # everything, for a pre-push hook

Exits non-zero when something would carry an identity out, and prints what and
where.
"""

import os
import pwd
import re
import subprocess
import sys
from pathlib import Path

# Names that mean "a person's name goes here". A path under one of these is a
# documented placeholder, not someone's machine, and a check that shouted about
# them would be switched off inside a week.
PLACEHOLDERS = {
    "someone", "me", "you", "user", "username", "<username>", "youruser",
    "your-user", "yourname", "sysadmin", "runner", "ci", "root", "test",
    "tester", "example", "home", "forge", "dev", "build", "builder", "admin",
}

# A home directory on any of the three platforms, with whoever it belongs to
# captured. Deliberately not anchored to the current machine: a path out of
# someone else's checkout is just as much a leak, and a contributor's would be
# worse.
HOME_PATHS = [
    re.compile(r"/Users/([A-Za-z0-9._-]+)"),
    re.compile(r"/home/([A-Za-z0-9._-]+)"),
    re.compile(r"C:\\\\?Users\\\\?([A-Za-z0-9._<>-]+)", re.IGNORECASE),
]

# Paths that are allowed to name a home directory because they are what the
# release build *replaces* real ones with — see package_macos.sh. Matching the
# remapped form must not be an error, or the fix would trip the check.
REMAPPED = re.compile(r"^(crates|rust|home)/")

SKIP_DIRS = {"vendor", "target", ".git", "node_modules"}
# Lockfiles name every dependency and nobody's machine; they are large and pure
# noise here.
SKIP_FILES = {"Cargo.lock", "package-lock.json"}


def names_from_git_dir(repo: Path) -> list[str]:
    """Extra literals to refuse, read from `.git/private-names`.

    Inside .git deliberately: that directory is never part of a commit, never
    pushed, and never in an archive, so the file cannot itself become the leak.
    One name per line; `#` comments and blank lines ignored. This is where
    family names, a legal name, a street, or an old handle go.
    """
    out = subprocess.run(
        ["git", "rev-parse", "--git-common-dir"],
        cwd=repo, capture_output=True, text=True,
    )
    if out.returncode != 0:
        return []
    git_dir = Path(out.stdout.strip())
    if not git_dir.is_absolute():
        git_dir = repo / git_dir
    path = git_dir / "private-names"
    if not path.is_file():
        return []
    names = []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            names.append(line)
    return names


def machine_names() -> list[str]:
    """Who and what this machine is, as strings that must not be published."""
    names = []
    home = os.environ.get("HOME", "").rstrip("/")
    if home:
        names.append(home)
        names.append(os.path.basename(home))
    try:
        names.append(pwd.getpwuid(os.getuid()).pw_name)
    except KeyError:
        pass
    # A short name matches far too much to carry information — `ci`, `dev`. The
    # full home path above still covers those cases.
    return [n for n in dict.fromkeys(names) if len(n) > 3]


class Scanner:
    def __init__(self, repo: Path):
        self.literals = [
            n for n in machine_names() + names_from_git_dir(repo)
            if n.lower() not in PLACEHOLDERS
        ]
        self.findings: list[tuple[str, str]] = []

    def inspect(self, where: str, text: str) -> None:
        """Record anything in `text` that would name a person or a machine."""
        for literal in self.literals:
            idx = text.lower().find(literal.lower())
            if idx != -1:
                self.findings.append((where, self._excerpt(text, idx, len(literal))))
        for pattern in HOME_PATHS:
            for m in pattern.finditer(text):
                who = m.group(1).strip("<>").lower()
                if who in PLACEHOLDERS:
                    continue
                # `/Users/.../Forge-IDE` — an elision in prose, where the name
                # has already been taken out. Nothing but punctuation is not a
                # person.
                if not who.strip("._-"):
                    continue
                if REMAPPED.match(m.group(0).lstrip("/")):
                    continue
                self.findings.append((where, self._excerpt(text, m.start(), len(m.group(0)))))

    @staticmethod
    def _excerpt(text: str, at: int, length: int) -> str:
        start, end = max(0, at - 30), min(len(text), at + length + 30)
        return ("…" if start else "") + text[start:end].replace("\n", "⏎") + ("…" if end < len(text) else "")


def tracked_files(repo: Path) -> list[str]:
    out = subprocess.run(
        ["git", "ls-files", "-z"], cwd=repo, capture_output=True, text=True, check=True
    )
    keep = []
    for name in out.stdout.split("\0"):
        if not name:
            continue
        parts = Path(name).parts
        if SKIP_DIRS.intersection(parts) or Path(name).name in SKIP_FILES:
            continue
        keep.append(name)
    return keep


def scan_tracked(scanner: Scanner, repo: Path) -> None:
    for name in tracked_files(repo):
        path = repo / name
        try:
            data = path.read_bytes()
        except OSError:
            continue
        if b"\0" in data[:8192]:  # a binary blob in the tree; scanned as an artifact
            scan_artifact(scanner, path)
            continue
        text = data.decode("utf-8", errors="replace")
        for n, line in enumerate(text.splitlines(), 1):
            scanner.inspect(f"{name}:{n}", line)


def scan_messages(scanner: Scanner, repo: Path, rev_range: str) -> int:
    out = subprocess.run(
        ["git", "log", "--format=%H%x00%B%x00", rev_range],
        cwd=repo, capture_output=True, text=True,
    )
    if out.returncode != 0:
        print(f"  (range {rev_range} is not resolvable here — skipping messages)")
        return 0
    chunks = out.stdout.split("\0")
    count = 0
    for i in range(0, len(chunks) - 1, 2):
        sha, body = chunks[i].strip(), chunks[i + 1]
        if not sha:
            continue
        count += 1
        scanner.inspect(f"commit {sha[:9]} (message)", body)
    return count


def printable_runs(data: bytes, least: int = 6):
    """The strings a `strings` run would show — how a downloader reads a binary."""
    run = bytearray()
    for byte in data:
        if 32 <= byte < 127:
            run.append(byte)
            continue
        if len(run) >= least:
            yield run.decode("ascii")
        run.clear()
    if len(run) >= least:
        yield run.decode("ascii")


def scan_artifact(scanner: Scanner, path: Path) -> None:
    try:
        data = path.read_bytes()
    except OSError as exc:
        print(f"  (cannot read {path}: {exc})")
        return
    for run in printable_runs(data):
        scanner.inspect(str(path), run)


def main() -> int:
    argv = sys.argv[1:]
    if not argv:
        print(__doc__)
        return 2
    repo = Path(
        subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
    )
    scanner = Scanner(repo)
    if not scanner.literals:
        # Nothing to match on means a pass that proves nothing. Say so rather
        # than printing a reassuring tick.
        print("!!! no identity to check against (no HOME, no .git/private-names)")
        return 1

    mode = argv[0]
    print(f"==> checking against {len(scanner.literals)} identity string(s) "
          f"plus any unknown home path")
    if mode in ("--tracked", "--all"):
        files = tracked_files(repo)
        print(f"    {len(files)} tracked file(s)")
        scan_tracked(scanner, repo)
    if mode in ("--messages", "--all"):
        rev_range = argv[1] if len(argv) > 1 else "origin/main..HEAD"
        n = scan_messages(scanner, repo, rev_range)
        print(f"    {n} commit message(s) in {rev_range}")
    if mode == "--artifacts":
        for name in argv[1:]:
            path = Path(name)
            if path.is_file():
                scan_artifact(scanner, path)
        print(f"    {len(argv[1:])} artifact(s)")

    if not scanner.findings:
        print("    clean")
        return 0

    print(f"\n!!! {len(scanner.findings)} place(s) would carry an identity out:\n")
    for where, excerpt in scanner.findings[:40]:
        print(f"  {where}")
        print(f"      {excerpt}")
    if len(scanner.findings) > 40:
        print(f"  … and {len(scanner.findings) - 40} more")
    print("\n    A placeholder (/Users/someone) is the fix for prose and source.")
    print("    For a binary it is --remap-path-prefix, not `strip` — panic")
    print("    locations are string literals that stripping leaves in place.")
    print("    Add names to .git/private-names (untracked) to check for more.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
