#!/usr/bin/env python3
"""Compaction retention, measured over real work instead of planted facts.

`compaction_retention.py` plants facts in user messages and checks which
survive. That misses the half that matters most: in real work the information
is in *tool results* — file contents, compiler errors, test output — and those
are rendered into the summariser's transcript truncated to 300 characters each.
A user message is 500. Nothing in the synthetic test exercises either.

So this gives the agent an actual task, lets it work until its context fills,
and then asks what its own summary knows about the work it did. The task is
chosen to generate exactly the kind of material that is expensive to lose:
files created, functions named, compiler errors hit and fixed, and design
decisions taken under a stated constraint.

What is measured, automatically and without judgement:

  * the files it actually created, against the files its summary names
  * the identifiers it defined, against the identifiers its summary names
  * whether the build state it reports matches the build state on disk

A summary that says "implemented the opcode dispatch" while the file it was
written in goes unnamed is a summary that cannot be resumed from.

Costs real tokens. Runs entirely inside a temporary directory with its own
HOME; the agent cannot see this repository.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

TASK = """\
Build a tiny stack-based virtual machine in Rust, from scratch, in this \
directory. Hard rule for the whole task: no third-party crates. Standard \
library only, and if you need something a crate would give you, write it.

Work in `src/`, one concern per file. I want at least:

- a `Value` type the stack holds,
- an `Op` enum for the instruction set, with at least Push, Pop, Add, Sub, \
  Mul, Dup, Swap, Jump, JumpIfZero, Print and Halt,
- a `Vm` struct with a `run` method that executes a program and returns a \
  result rather than panicking on bad input,
- real error handling: a stack underflow or a jump out of bounds is an error \
  value, never a panic,
- tests covering arithmetic, a loop built from Jump/JumpIfZero, and both \
  error cases.

Start by writing the files, then get `cargo test` passing. Tell me the exact \
error text if the compiler rejects something — I want to see what it said.
"""

NUDGES = [
    "Keep going. Run the tests and fix whatever fails.",
    "Continue. Add the loop test if it is not there yet.",
    "Carry on — make sure both error cases are covered and the tests pass.",
    "Keep working through it.",
    "Continue with whatever is left.",
]


def write_config(home, base_url, model, api_key, window, endpoint_type):
    (home / ".config" / "forge").mkdir(parents=True, exist_ok=True)
    (home / ".config" / "forge" / "config.toml").write_text(f"""
[models]
default = "work"
[[models.endpoints]]
name = "work"
base_url = "{base_url}"
model_id = "{model}"
max_context_tokens = {window}
max_output_tokens = 8192
request_timeout_secs = 600
endpoint_type = "{endpoint_type}"
api_key = "{api_key}"
[agent]
thinking_mode = false
auto_approve_reads = true
auto_approve_writes = true
max_history_messages = 500
context_strategy = "compaction"
compact_at_percent = 80
""", encoding="utf-8")


def read_endpoint(name):
    path = Path(os.path.expanduser("~/.config/forge/config.toml"))
    text = path.read_text(encoding="utf-8")
    for block in text.split("[[models.endpoints]]")[1:]:
        got = {}
        for key in ("name", "base_url", "model_id", "api_key", "endpoint_type"):
            m = re.search(key + r'\s*=\s*"([^"]*)"', block)
            if m:
                got[key] = m.group(1)
        if got.get("name") == name:
            return got
    sys.exit(f"no endpoint called {name!r}")


class Agent:
    def __init__(self, home, binary):
        workspace = home / "workspace"
        workspace.mkdir(parents=True, exist_ok=True)
        # A real cargo project, so `cargo test` is a thing it can actually run.
        (workspace / "src").mkdir(exist_ok=True)
        (workspace / "Cargo.toml").write_text(
            '[package]\nname = "tinyvm"\nversion = "0.1.0"\nedition = "2021"\n\n'
            "[dependencies]\n", encoding="utf-8")
        (workspace / "src" / "lib.rs").write_text("", encoding="utf-8")
        subprocess.run(["git", "init", "--quiet"], cwd=workspace, check=True)
        for k, v in (("user.email", "work@forge.invalid"), ("user.name", "Work fixture")):
            subprocess.run(["git", "config", k, v], cwd=workspace, check=True)

        env = dict(os.environ)
        env.update({
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_DATA_HOME": str(home / ".local" / "share"),
            "FORGE_CONFIG_FILE": str(home / ".config" / "forge" / "config.toml"),
            "FORGE_NO_AUTO_VERSION_CHECK": "1",
        })
        self.stderr = open(home / "stderr.log", "w", encoding="utf-8")
        self.proc = subprocess.Popen(
            [str(binary), "--headless"], cwd=workspace, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr,
            text=True, bufsize=1)
        self.frames = []
        self.lock = threading.Lock()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            try:
                frame = json.loads(line)
            except json.JSONDecodeError:
                continue
            with self.lock:
                self.frames.append(frame)

    def mark(self):
        with self.lock:
            return len(self.frames)

    def wait_for(self, kinds, timeout, after=0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self.lock:
                for f in self.frames[after:]:
                    if f.get("type") in kinds:
                        return f
            time.sleep(0.1)
        return None

    def say(self, text, timeout):
        after = self.mark()
        self.proc.stdin.write(json.dumps({"type": "send_message", "content": text}) + "\n")
        self.proc.stdin.flush()
        return self.wait_for(["done", "error", "cancelled"], timeout, after=after)

    def usage(self):
        with self.lock:
            for f in reversed(self.frames):
                if f.get("type") == "usage_update":
                    return f.get("snapshot", {})
        return {}

    def tool_calls(self):
        with self.lock:
            return [f for f in self.frames if f.get("type") in ("tool_request", "tool_result")]

    def transcript(self):
        with self.lock:
            return "\n".join(json.dumps(f) for f in self.frames)

    def stop(self):
        try:
            self.proc.terminate(); self.proc.wait(timeout=10)
        except Exception:
            self.proc.kill()
        self.stderr.close()


def summary_from_log(home):
    sessions = home / "workspace" / ".forge" / "sessions"
    for log in sorted(sessions.glob("*/conversation.jsonl")):
        for line in log.read_text(encoding="utf-8").splitlines():
            try:
                r = json.loads(line)
            except json.JSONDecodeError:
                continue
            if r.get("type") == "compaction_summary":
                return r.get("summary", {})
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--endpoint", required=True)
    ap.add_argument("--window", type=int, default=16000)
    ap.add_argument("--turn-timeout", type=int, default=900)
    ap.add_argument("--max-turns", type=int, default=14)
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    found = read_endpoint(args.endpoint)
    endpoint_type = found.get("endpoint_type", "open_ai")
    home = Path(tempfile.mkdtemp(prefix="forge-realwork-"))
    if endpoint_type == "chatgpt_codex":
        token = Path(os.path.expanduser("~/.config/forge/chatgpt_auth.json"))
        if not token.exists():
            sys.exit("chatgpt_codex endpoint but no chatgpt_auth.json")
        (home / ".config" / "forge").mkdir(parents=True, exist_ok=True)
        shutil.copy2(token, home / ".config" / "forge" / "chatgpt_auth.json")
        os.chmod(home / ".config" / "forge" / "chatgpt_auth.json", 0o600)
    write_config(home, found["base_url"], found["model_id"],
                 found.get("api_key", "x"), args.window, endpoint_type)

    binary = REPO / "target" / "debug" / "forge-agent"
    if not binary.exists():
        binary = REPO / "target" / "release" / "forge-agent"

    print(f"  endpoint : {found['base_url']}  [{endpoint_type}]")
    print(f"  model    : {found['model_id']}")
    print(f"  window   : {args.window}, compacting at 80%")
    print(f"  workspace: {home / 'workspace'}\n")

    agent = Agent(home, binary)
    if not agent.wait_for(["init"], 90):
        agent.stop()
        sys.exit("the agent never started:\n" + (home / "stderr.log").read_text()[-2000:])

    def compacted():
        t = agent.transcript()
        return "Context compacted" in t or "Compacting context" in t

    messages = [TASK] + [NUDGES[i % len(NUDGES)] for i in range(args.max_turns - 1)]
    for i, message in enumerate(messages, 1):
        if compacted():
            print(f"\n  compaction fired after {i - 1} turns\n")
            break
        print(f"  [{i}] working…", end="", flush=True)
        end = agent.say(message, args.turn_timeout)
        u = agent.usage()
        pct = 100 * u.get("last_prompt_tokens", 0) / max(u.get("max_context_tokens", 1), 1)
        kind = end.get("type") if end else "timeout"
        print(f"  ctx {u.get('last_prompt_tokens',0)}/{u.get('max_context_tokens',0)}"
              f" ({pct:.0f}%), {u.get('history_messages',0)} msgs, {kind}", flush=True)
        if kind not in ("done",):
            break

    workspace = home / "workspace"
    # What it actually built, before anything is torn down.
    sources = sorted(p for p in (workspace / "src").rglob("*.rs"))
    created = [str(p.relative_to(workspace)) for p in sources]
    identifiers = set()
    for p in sources:
        text = p.read_text(encoding="utf-8", errors="ignore")
        identifiers |= set(re.findall(r'\b(?:fn|struct|enum)\s+([A-Za-z_][A-Za-z0-9_]*)', text))

    build = subprocess.run(["cargo", "test", "--quiet"], cwd=workspace,
                           capture_output=True, text=True, timeout=600)
    builds = build.returncode == 0

    summary = summary_from_log(home)
    transcript = agent.transcript()
    agent.stop()

    print(f"\n  {'=' * 72}")
    print("  What it built")
    print(f"  {'=' * 72}")
    print(f"    files   : {created or 'none'}")
    print(f"    defined : {sorted(identifiers)[:18] or 'none'}")
    print(f"    cargo test passes: {builds}")
    if not builds:
        tail = (build.stderr or build.stdout).strip().splitlines()[-4:]
        for line in tail:
            print(f"      {line[:140]}")

    if summary is None:
        print("\n  No compaction happened — nothing to measure. Try a smaller --window.")
        if not args.keep:
            shutil.rmtree(home, ignore_errors=True)
        return 2

    blob = json.dumps(summary)
    print(f"\n  {'=' * 72}")
    print("  What its summary knows about it")
    print(f"  {'=' * 72}")
    named_files = [f for f in created if Path(f).name in blob or f in blob]
    named_ids = sorted(i for i in identifiers if re.search(rf'\b{re.escape(i)}\b', blob))
    print(f"    files named in the summary      : {len(named_files)}/{len(created)}  {named_files}")
    print(f"    identifiers named in the summary: {len(named_ids)}/{len(identifiers)}  {named_ids[:12]}")
    print(f"    mentions the no-crates rule     : "
          f"{'crate' in blob.lower() or 'third-party' in blob.lower() or 'third party' in blob.lower()}")
    print(f"    mentions a compiler error       : "
          f"{'error[' in blob or 'error:' in blob.lower() or 'compil' in blob.lower()}")
    print(f"    marked degraded                 : "
          f"{str(summary.get('current_state','')).startswith('(unstructured')}")
    print(f"    written as the agent's own handoff: {'Handoff, written by me' in blob}")

    print(f"\n  The summary:")
    for field in ("goal", "current_state"):
        if summary.get(field):
            print(f"    {field}: {str(summary[field])[:400]}")
    for field in ("repo_map", "work_completed", "decisions", "next_actions", "pitfalls"):
        items = summary.get(field) or []
        if items:
            print(f"    {field}:")
            for item in items[:6]:
                print(f"      - {str(item)[:150]}")

    if args.keep:
        print(f"\n  kept: {home}")
    else:
        shutil.rmtree(home, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
