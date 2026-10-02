#!/usr/bin/env python3
"""Measure what compaction keeps and what it loses.

The existing tests prove compaction *runs*: that it fires at the threshold,
produces a summary, keeps it, and leaves a conversation that fits. None of them
say anything about whether the summary is any good, because they answer a mock
whose summary is canned.

This drives a real `forge-agent --headless` against a real endpoint. It seeds a
conversation with facts chosen to be unambiguously checkable — an exact path, a
byte count, a constraint the user stated once, a thing that was tried and
failed — forces a compaction, then reads the summary back out of the session log
and reports which of them survived.

The point is not a single score. It is which *categories* survive: a summary
that keeps the goal and the file paths but drops "the mmap approach failed, the
header is not page-aligned" will send an agent to repeat work it already knows
does not work, and that is invisible to any test that only checks compaction
happened.

Usage:
    python3 scripts/compaction_retention.py --base-url http://127.0.0.1:8080/v1 \\
                                            --model deepseek-ai/deepseek-moe-16b-chat

    # or name an endpoint from your own config, read for its url and model only
    python3 scripts/compaction_retention.py --endpoint "Qwen3.8-27B (local M5)"

Costs whatever the endpoint costs. Point it at a local model to pay nothing;
point it at the model you actually use to learn something about your sessions.
Nothing is written outside a temporary directory, and the API key is read but
never printed.
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

# Facts planted in the conversation, each with the category it represents and a
# checker that decides whether it survived.
#
# Chosen to be things a coding session actually turns on, and to be checkable
# without judgement: an exact number either appears or it does not. The
# categories are the output that matters — losing every "tried and failed" is a
# different problem from losing every file path, and calls for a different fix.
FACTS = [
    {
        "id": "user-constraint",
        "category": "a rule the user stated once",
        "say": "Rule for this whole task: never add a third-party crate. "
               "We write it ourselves, even when a crate exists.",
        "check": lambda t: ("third-party" in t.lower() or "third party" in t.lower())
                           and ("crate" in t.lower() or "dependenc" in t.lower()),
        "why": "A constraint stated once governs everything after it. If the "
               "summary drops it, the agent breaks it and believes it is helping.",
    },
    {
        "id": "exact-path",
        "category": "an exact file path",
        "say": "The parser lives in src/loader/header.rs and nothing else touches it.",
        "check": lambda t: "src/loader/header.rs" in t,
        "why": "A paraphrased path is a path the agent cannot open.",
    },
    {
        "id": "exact-identifier",
        "category": "an exact identifier",
        "say": "The function to change is parse_segment_table, not parse_segments.",
        "check": lambda t: "parse_segment_table" in t,
        "why": "Two similar names, one correct. A summary that keeps 'the segment "
               "parser' has kept nothing usable.",
    },
    {
        "id": "measurement",
        "category": "a measured number",
        "say": "The reproducible build is exactly 2875990 bytes. That number is the "
               "fixed point we check against.",
        "check": lambda t: "2875990" in t.replace(",", "").replace("_", ""),
        "why": "A number that is the acceptance criterion cannot be approximated. "
               "'About 2.8 MB' fails the check it exists for.",
    },
    {
        "id": "negative-result",
        "category": "something tried that failed, and why",
        "say": "We tried mmap for the header and it failed: the header is not "
               "page-aligned, so the mapping straddles a boundary. Do not try mmap again.",
        "check": lambda t: "mmap" in t.lower(),
        "why": "The most expensive thing to lose. An agent that forgets what "
               "failed repeats it, and the second attempt looks like progress.",
    },
    {
        "id": "decision-and-reason",
        "category": "a decision and its reasoning",
        "say": "Decision: little-endian only, because the bootloader already "
               "guarantees it and supporting both doubles the test matrix.",
        "check": lambda t: "little-endian" in t.lower() or "little endian" in t.lower(),
        "why": "A decision without its reason gets relitigated on the next turn.",
    },
    {
        "id": "open-question",
        "category": "an unresolved question",
        "say": "Still undecided, and I want to decide it later: whether the on-disk "
               "format gets a version field.",
        "check": lambda t: "version" in t.lower() and ("field" in t.lower()
                                                       or "undecided" in t.lower()
                                                       or "on-disk" in t.lower()),
        "why": "An open question dropped from the summary is silently closed, and "
               "the agent picks whichever answer is convenient.",
    },
    {
        "id": "next-action",
        "category": "what to do next",
        "say": "Next step after the parser: wire it into the ELF oracle and compare "
               "byte-for-byte.",
        "check": lambda t: "oracle" in t.lower(),
        "why": "The one field every summary format has. If this is missing, nothing is working.",
    },
]

# Bulk to push the conversation over the window without saying anything
# checkable — the facts have to be lost on their merits, not drowned.
FILLER = (
    "Context on the surrounding code, which is ordinary and not the point: the "
    "loader walks a table of segments, each with an offset and a length, and the "
    "caller maps them in order. There are tests for the happy path already. "
)


def read_endpoint_from_config(name):
    """url, model and key for a named endpoint. The key is returned, never logged."""
    path = Path(os.path.expanduser("~/.config/forge/config.toml"))
    if not path.exists():
        sys.exit(f"no config at {path}")
    text = path.read_text(encoding="utf-8")
    blocks = text.split("[[models.endpoints]]")
    for block in blocks[1:]:
        got = {}
        for key in ("name", "base_url", "model_id", "api_key", "endpoint_type"):
            m = re.search(key + r'\s*=\s*"([^"]*)"', block)
            if m:
                got[key] = m.group(1)
        if got.get("name") == name:
            return got
    names = [re.search(r'name\s*=\s*"([^"]*)"', b) for b in blocks[1:]]
    names = [m.group(1) for m in names if m]
    sys.exit(f"no endpoint called {name!r}. Configured: {names}")


def write_config(home, base_url, model, api_key, window, endpoint_type):
    (home / ".config" / "forge").mkdir(parents=True, exist_ok=True)
    (home / ".config" / "forge" / "config.toml").write_text(
        f"""
[models]
default = "retention"
[[models.endpoints]]
name = "retention"
base_url = "{base_url}"
model_id = "{model}"
max_context_tokens = {window}
max_output_tokens = 4096
request_timeout_secs = 180
endpoint_type = "{endpoint_type}"
api_key = "{api_key}"
[agent]
thinking_mode = false
auto_approve_reads = true
auto_approve_writes = false
max_history_messages = 500
context_strategy = "compaction"
compact_at_percent = 80
""",
        encoding="utf-8",
    )


class Agent:
    """The real binary, over its JSON-newline protocol."""

    def __init__(self, home, binary):
        workspace = home / "workspace"
        workspace.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "init", "--quiet"], cwd=workspace, check=True)
        for key, value in (("user.email", "retention@forge.invalid"),
                           ("user.name", "Retention fixture")):
            subprocess.run(["git", "config", key, value], cwd=workspace, check=True)

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
            [str(binary), "--headless"],
            cwd=workspace, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr,
            text=True, bufsize=1,
        )
        self.frames = []
        self.lock = threading.Lock()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            try:
                frame = json.loads(line)
            except json.JSONDecodeError:
                frame = {"type": "unparseable", "line": line.rstrip()}
            with self.lock:
                self.frames.append(frame)

    def wait_for(self, kinds, timeout, after=0):
        """First frame of one of `kinds` at index >= `after`.

        `after` matters and its absence was a real defect in this harness: the
        scan used to start at zero every call, so once any `done` had arrived
        it returned that same frame instantly for every later turn. Twenty
        messages went out without waiting for any of them, and the context
        figure this script reported was turn one's, read twenty times — which
        looked exactly like a conversation that would not accumulate.
        """
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self.lock:
                for frame in self.frames[after:]:
                    if frame.get("type") in kinds:
                        return frame
            time.sleep(0.05)
        return None

    def mark(self):
        """The current end of the frame list, to wait from."""
        with self.lock:
            return len(self.frames)

    def say(self, text, timeout):
        after = self.mark()
        self.proc.stdin.write(json.dumps({"type": "send_message", "content": text}) + "\n")
        self.proc.stdin.flush()
        return self.wait_for(["done", "error", "cancelled"], timeout, after=after)

    def usage(self):
        """The latest usage snapshot the agent reported, if any."""
        with self.lock:
            for frame in reversed(self.frames):
                if frame.get("type") == "usage_update":
                    return frame.get("snapshot", {})
        return {}

    def transcript(self):
        with self.lock:
            return "\n".join(json.dumps(f) for f in self.frames)

    def stop(self):
        try:
            self.proc.terminate()
            self.proc.wait(timeout=10)
        except Exception:
            self.proc.kill()
        self.stderr.close()


def summary_from_log(home):
    """The summary compaction wrote, read back out of the session log."""
    sessions = home / "workspace" / ".forge" / "sessions"
    for log in sorted(sessions.glob("*/conversation.jsonl")):
        for line in log.read_text(encoding="utf-8").splitlines():
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            if record.get("type") == "compaction_summary":
                return record.get("summary", {})
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--endpoint", help="name of an endpoint in your config")
    ap.add_argument("--base-url")
    ap.add_argument("--model")
    ap.add_argument("--api-key", default="not-a-real-key")
    ap.add_argument("--window", type=int, default=6000,
                    help="max_context_tokens for the run; small so compaction fires soon")
    ap.add_argument("--turn-timeout", type=int, default=240)
    ap.add_argument("--keep", action="store_true", help="leave the fixture directory")
    args = ap.parse_args()

    if args.endpoint:
        found = read_endpoint_from_config(args.endpoint)
        base_url = args.base_url or found.get("base_url")
        model = args.model or found.get("model_id")
        api_key = found.get("api_key", args.api_key)
        endpoint_type = found.get("endpoint_type", "open_ai")
    else:
        if not (args.base_url and args.model):
            sys.exit("give --endpoint, or both --base-url and --model")
        base_url, model, api_key = args.base_url, args.model, args.api_key
        endpoint_type = "open_ai"

    binary = REPO / "target" / "debug" / "forge-agent"
    if not binary.exists():
        binary = REPO / "target" / "release" / "forge-agent"
    if not binary.exists():
        sys.exit("no forge-agent binary built; run `cargo build -p forge-agent`")

    home = Path(tempfile.mkdtemp(prefix="forge-retention-"))
    # ChatGPT Codex authenticates from a token file beside the config, not
    # from an api_key — so an isolated HOME has no credential and the run
    # would fail on auth rather than on anything being measured. Copied in,
    # and removed with the fixture unless --keep is given.
    if endpoint_type == "chatgpt_codex":
        token = Path(os.path.expanduser("~/.config/forge/chatgpt_auth.json"))
        if not token.exists():
            sys.exit("that endpoint is chatgpt_codex and ~/.config/forge/chatgpt_auth.json "
                     "does not exist — run `forge --login-chatgpt` first")
        (home / ".config" / "forge").mkdir(parents=True, exist_ok=True)
        shutil.copy2(token, home / ".config" / "forge" / "chatgpt_auth.json")
        os.chmod(home / ".config" / "forge" / "chatgpt_auth.json", 0o600)
        print("  auth     : copied chatgpt_auth.json into the fixture (0600, removed with it)")
    write_config(home, base_url, model, api_key, args.window, endpoint_type)
    print(f"  endpoint : {base_url}  [{endpoint_type}]")
    print(f"  model    : {model}")
    print(f"  window   : {args.window} tokens, compacting at 80%")
    print(f"  fixture  : {home}\n")

    agent = Agent(home, binary)
    if not agent.wait_for(["init"], 60):
        agent.stop()
        sys.exit(f"the agent never started. stderr:\n{(home / 'stderr.log').read_text()}")

    # Plant the facts, one per turn, each padded so the window fills.
    for i, fact in enumerate(FACTS, 1):
        print(f"  [{i}/{len(FACTS)}] planting {fact['id']}…", end="", flush=True)
        end = agent.say(f"{fact['say']}\n\n{FILLER * 6}\nJust acknowledge, briefly.",
                        args.turn_timeout)
        u = agent.usage()
        pct = (100 * u.get("last_prompt_tokens", 0) / max(u.get("max_context_tokens", 1), 1))
        print(f"  ctx {u.get('last_prompt_tokens', 0)}/{u.get('max_context_tokens', 0)}"
              f" ({pct:.0f}%), {u.get('history_messages', 0)} msgs", flush=True)
        if end is None or end.get("type") != "done":
            agent.stop()
            sys.exit(f"turn {i} did not complete: {end}\n"
                     f"stderr:\n{(home / 'stderr.log').read_text()[-2000:]}")

    # Then push until it compacts.
    #
    # Matching the notice, not the word. `"compact" in transcript` is true from
    # the first frame, because the init frame carries
    # `"context_strategy":"compaction"` — which skipped this loop entirely the
    # first time and reported that compaction had never fired.
    def has_compacted():
        text = agent.transcript()
        return "Compacting context" in text or "Context compacted" in text

    compacted = has_compacted()
    for i in range(12):
        if compacted:
            break
        print(f"  pushing for compaction ({i + 1})…", end="", flush=True)
        end = agent.say(f"Keep going.\n\n{FILLER * 14}", args.turn_timeout)
        u = agent.usage()
        pct = (100 * u.get("last_prompt_tokens", 0) / max(u.get("max_context_tokens", 1), 1))
        print(f"  ctx {u.get('last_prompt_tokens', 0)}/{u.get('max_context_tokens', 0)}"
              f" ({pct:.0f}%), {u.get('history_messages', 0)} msgs", flush=True)
        if end is None or end.get("type") != "done":
            print(f"  a push turn ended as {end.get('type') if end else 'timeout'}; stopping")
            break
        compacted = has_compacted()

    transcript = agent.transcript()
    agent_usage_final = agent.usage()
    agent.stop()

    summary = summary_from_log(home)
    if summary is None:
        print("\n  No compaction summary was written.")
        if not compacted:
            u = agent_usage_final
            print(f"  Compaction never fired. Last reported context: "
                  f"{u.get('last_prompt_tokens', 0)}/{u.get('max_context_tokens', 0)} tokens "
                  f"across {u.get('history_messages', 0)} messages, and the threshold is "
                  f"{args.window * 80 // 100}.")
            print("  If the message count is not climbing, the history is not growing — "
                  "check stderr below rather than lowering the window.")
        print(f"\n  stderr tail:\n{(home / 'stderr.log').read_text()[-1500:]}")
        if not args.keep:
            shutil.rmtree(home, ignore_errors=True)
        return 2

    blob = json.dumps(summary).lower()
    kept, lost = [], []
    for fact in FACTS:
        (kept if fact["check"](json.dumps(summary)) else lost).append(fact)

    print(f"\n  {'=' * 72}")
    print(f"  Retention: {len(kept)}/{len(FACTS)} facts survived compaction")
    print(f"  {'=' * 72}\n")
    for fact in FACTS:
        mark = "kept" if fact in kept else "LOST"
        print(f"  [{mark}] {fact['category']}")
        if fact in lost:
            print(f"         {fact['why']}")
    print()

    degraded = str(summary.get("current_state", "")).startswith("(unstructured summary")
    if degraded:
        print("  The summary could not be parsed as structured JSON — the model did "
              "not return the requested shape. What is reported above is matched "
              "against its raw prose.\n")

    print("  The summary, as the agent will read it:")
    for field in ("goal", "current_state"):
        value = summary.get(field, "")
        if value:
            print(f"    {field}: {str(value)[:300]}")
    for field in ("repo_map", "work_completed", "commands_run", "decisions",
                  "next_actions", "pitfalls"):
        items = summary.get(field) or []
        if items:
            print(f"    {field}:")
            for item in items[:6]:
                print(f"      - {str(item)[:160]}")
    print()

    if not args.keep:
        shutil.rmtree(home, ignore_errors=True)
    else:
        print(f"  fixture kept at {home}")

    return 0 if len(lost) == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
