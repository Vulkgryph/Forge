#!/usr/bin/env python3
"""Tests for check_private.py, written after it failed CI three times running.

Every one of those failures was a false positive on a clean tree, which is the
way a check gets switched off rather than fixed. Two of them were only reachable
on a GitHub runner, where the account is `runner` and $HOME is /Users/runner —
both placeholders, so every machine-derived literal is correctly discarded and
the literal list is empty *by design*. That configuration cannot be produced by
setting an environment variable, because the account name comes from the
password database, so it is injected here instead.

What each case protects:

  the runner is clean            an empty literal list is not an error
  the runner still catches       the generic rule works without any identity
  a placeholder home is ignored  /Users/runner in a binary names nobody
  a real home is caught          and that did not cost us the actual check

Run: python3 scripts/test_check_private.py
"""

import importlib.util
import io
import os
import sys
import tempfile
from contextlib import redirect_stdout
from pathlib import Path

HERE = Path(__file__).resolve().parent

# A home directory belonging to nobody, assembled at runtime rather than written
# out. These cases need a name the guard *must* flag, so spelling it literally
# would make this file fail the very check it tests — it did, and the pre-push
# hook caught it before it reached anyone. Do not join these back together.
FAKE_USER = "a" + "person"
FAKE_HOME = "/Users/" + FAKE_USER
FAKE_HOME_WIN = "C:\\Users\\" + FAKE_USER.capitalize()


def load():
    """A fresh module each time, since Scanner reads the environment at init."""
    spec = importlib.util.spec_from_file_location("cp", HERE / "check_private.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def as_runner(mod):
    """Make `mod` see a GitHub runner: placeholder account, placeholder home."""
    mod.machine_names = lambda: ["/Users/runner", "runner"]
    mod.names_from_git_dir = lambda repo: []
    return mod


def as_developer(mod, home=None):
    home = home or FAKE_HOME
    mod.machine_names = lambda: [home, os.path.basename(home)]
    mod.names_from_git_dir = lambda repo: []
    return mod


def run(mod, argv):
    """`main()` with argv, returning (exit code, output)."""
    sys.argv = ["check_private.py"] + argv
    buf = io.StringIO()
    with redirect_stdout(buf):
        code = mod.main()
    return code, buf.getvalue()


def scan_text(mod, text):
    s = mod.Scanner(Path("."))
    s.inspect("probe", text)
    return s.findings


FAILURES = []


def check(name, condition, detail=""):
    if condition:
        print(f"  ok    {name}")
    else:
        print(f"  FAIL  {name}  {detail}")
        FAILURES.append(name)


def main() -> int:
    print("check_private.py")

    # --- a runner has no identity of its own, and that is not a failure -----
    # This exited 1 on a clean tree and failed the packaging job.
    mod = as_runner(load())
    check("a runner's empty literal list is not an error", mod.Scanner(Path(".")).literals == [])
    code, out = run(as_runner(load()), ["--tracked"])
    check("--tracked passes on a runner", code == 0, f"exit {code}: {out.strip()[:120]}")
    check("and says the identity half is inactive", "placeholder" in out.lower(), out.strip()[:120])

    # --- but the generic rule still has to work there ------------------------
    # Otherwise the fix above bought a pass that proves nothing.
    found = scan_text(as_runner(load()), f'let p = "{FAKE_HOME}/notes.md";')
    check("a real home path is still caught on a runner", len(found) == 1, str(found))
    # `Someone` would be the wrong name to test with — it is in PLACEHOLDERS,
    # so a passing assertion here would prove the opposite of what it looks like.
    found = scan_text(as_runner(load()), f"powershell -File {FAKE_HOME_WIN}\\x.ps1")
    check("and a Windows home path too", len(found) == 1, str(found))
    found = scan_text(as_runner(load()), "powershell -File C:\\Users\\Someone\\x.ps1")
    check("but not a placeholder Windows name", not found, str(found))

    # --- a placeholder home inside a binary names nobody --------------------
    # This failed packaging over /Users/runner/work/Forge/Forge/forge-tui-rs.
    with tempfile.TemporaryDirectory() as d:
        runner_bin = Path(d) / "runner-built"
        runner_bin.write_bytes(
            b"junk\x00../release/Users/runner/work/Forge/Forge/forge-tui-rs\x00junk"
        )
        real_bin = Path(d) / "developer-built"
        real_bin.write_bytes(
            f"junk\x00{FAKE_HOME}/.cargo/registry/src/x/lib.rs\x00junk".encode()
        )

        code, out = run(as_runner(load()), ["--artifacts", str(runner_bin)])
        check("a runner-built binary is clean", code == 0, f"exit {code}: {out.strip()[:160]}")

        code, _ = run(as_runner(load()), ["--artifacts", str(real_bin)])
        check("a binary naming a real home still fails on a runner", code == 1, f"exit {code}")

        code, _ = run(as_developer(load()), ["--artifacts", str(real_bin)])
        check("and fails on a developer machine", code == 1, f"exit {code}")

        code, _ = run(as_developer(load()), ["--artifacts", str(runner_bin)])
        check("a runner path is not a leak anywhere", code == 0, f"exit {code}")

    # --- placeholders the check previously tripped over ---------------------
    # It flagged `/Users/<name>` in its own documentation.
    for placeholder in ["/Users/someone", "/Users/<name>", "/home/sysadmin",
                        "C:\\Users\\<username>", "/Users/.../elided"]:
        found = scan_text(as_developer(load()), f"see {placeholder}/thing for details")
        check(f"{placeholder} is a placeholder", not found, str(found))

    # And the remapped forms a release build produces, which must not read as
    # leaks or the fix for the binaries would trip the check on the binaries.
    for remapped in ["crates/anyhow-1.0.104/src/error.rs", "home/.cargo/registry/x.rs"]:
        found = scan_text(as_developer(load()), f"panicked at {remapped}:2:1")
        check(f"{remapped} is the remapped form", not found, str(found))

    print()
    if FAILURES:
        print(f"{len(FAILURES)} failure(s): {', '.join(FAILURES)}")
        return 1
    print("all good")
    return 0


if __name__ == "__main__":
    sys.exit(main())
