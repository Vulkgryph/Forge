#!/usr/bin/env bun
// SPDX-License-Identifier: Apache-2.0
import React from "react";
import { render } from "ink";
import { App } from "./components/App.js";
import { findAgentBinary } from "./agent-bridge.js";

const VERSION = "0.1.0";

const VALID_OPTIONS = [
  "--help",
  "-h",
  "--version",
  "-V",
  "--cwd",
  "--dangerously-allow-all",
  "--resume-session",
  "--login",
  "--login-chatgpt",
] as const;

function usage(): string {
  return [
    "Forge",
    "",
    "Usage:",
    "  forge [options]",
    "",
    "Options:",
    "  -h, --help                    Show this help and exit",
    "  -V, --version                 Show version and exit",
    "      --cwd <path>              Start Forge in a specific project directory",
    "      --dangerously-allow-all   Bypass all tool approval prompts",
    "      --resume-session <id>     Resume a session by ID",
    "      --login                   Log in to Claude via OAuth (claude.ai / Pro / Max)",
    "      --login-chatgpt           Log in to ChatGPT via OAuth (Codex subscription)",
  ].join("\n");
}

function editDistance(a: string, b: string): number {
  const dp = Array.from({ length: a.length + 1 }, () => new Array<number>(b.length + 1).fill(0));
  for (let i = 0; i <= a.length; i++) dp[i]![0] = i;
  for (let j = 0; j <= b.length; j++) dp[0]![j] = j;
  for (let i = 1; i <= a.length; i++) {
    for (let j = 1; j <= b.length; j++) {
      const cost = a[i - 1] === b[j - 1] ? 0 : 1;
      dp[i]![j] = Math.min(
        dp[i - 1]![j]! + 1,
        dp[i]![j - 1]! + 1,
        dp[i - 1]![j - 1]! + cost
      );
    }
  }
  return dp[a.length]![b.length]!;
}

function closestOption(input: string): string | null {
  let best: { option: string; distance: number } | null = null;
  for (const option of VALID_OPTIONS) {
    const distance = editDistance(input, option);
    if (!best || distance < best.distance) best = { option, distance };
  }
  return best && best.distance <= Math.max(2, Math.floor(input.length / 3)) ? best.option : null;
}

function fail(message: string, option?: string): never {
  console.error(`forge: ${message}`);
  if (option) console.error(`Did you mean ${option}?`);
  console.error("");
  console.error(usage());
  process.exit(2);
}

function parseArgs(argv: string[]): { agentArgs: string[]; cwd?: string } {
  const agentArgs: string[] = [];
  let cwd: string | undefined;

  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]!;
    if (arg === "--help" || arg === "-h") {
      console.log(usage());
      process.exit(0);
    }
    if (arg === "--version" || arg === "-V") {
      console.log(`forge ${VERSION}`);
      process.exit(0);
    }
    if (arg === "--cwd") {
      const value = argv[++i];
      if (!value || value.startsWith("-")) fail("--cwd requires a path");
      cwd = value;
      continue;
    }
    if (arg === "--resume-session") {
      const value = argv[++i];
      if (!value || value.startsWith("-")) fail("--resume-session requires a session ID");
      agentArgs.push(arg, value);
      continue;
    }
    if (arg === "--dangerously-allow-all") {
      agentArgs.push(arg);
      continue;
    }
    if (arg.startsWith("-")) {
      fail(`invalid option: ${arg}`, closestOption(arg) ?? undefined);
    }
    fail(`unexpected argument: ${arg}`);
  }

  return { agentArgs, cwd };
}

/**
 * Intercept --login / --login-chatgpt at the wrapper level so users coming
 * from Claude Code or ChatGPT Codex see the convention they expect — i.e.
 * `forge --login` works, not just `forge-agent --login`. We bypass the Ink
 * TUI entirely and run forge-agent inline with inherited stdio so the OAuth
 * prompts (and the paste-the-code fallback) feel native.
 */
async function maybeRunLogin(argv: string[]): Promise<void> {
  const loginArg = argv.find((a) => a === "--login" || a === "--login-chatgpt");
  if (!loginArg) return;

  const otherArgs = argv.filter((a) => a !== loginArg);
  if (otherArgs.length > 0) {
    console.error(`forge: ${loginArg} cannot be combined with other options. Got: ${otherArgs.join(" ")}`);
    process.exit(2);
  }

  const binary = findAgentBinary();
  const proc = Bun.spawn([binary, loginArg], {
    stdin: "inherit",
    stdout: "inherit",
    stderr: "inherit",
  });
  const exitCode = await proc.exited;
  process.exit(exitCode);
}

await maybeRunLogin(process.argv.slice(2));

const cli = parseArgs(process.argv.slice(2));

render(<App initialAgentArgs={cli.agentArgs} initialCwd={cli.cwd} />);
