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
    "      --login [chatgpt]         OAuth login for ChatGPT Codex subscription.",
    "      --login-chatgpt           Shortcut for: --login chatgpt",
    "                                (Claude uses an Anthropic API key, not subscription login.)",
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
 * Map a user-facing provider name to the forge-agent OAuth flag.
 * ChatGPT Codex is the only subscription provider. Returns null otherwise.
 */
function providerToAgentFlag(name: string): string | null {
  const normalized = name.trim().toLowerCase();
  if (["chatgpt", "codex", "openai", "gpt"].includes(normalized)) return "--login-chatgpt";
  return null;
}

/** True for names that mean "Claude/Anthropic" — recognized only to error clearly. */
function isClaudeProvider(name: string): boolean {
  return ["claude", "anthropic", "claude.ai"].includes(name.trim().toLowerCase());
}

const CLAUDE_LOGIN_UNSUPPORTED =
  "forge: Claude subscription login is not supported. Anthropic restricts subscription " +
  "credentials to its own apps, so Forge uses an Anthropic API key instead — add one in the " +
  "/model menu or in ~/.config/forge/config.toml.";

/**
 * Intercept the login flags at the wrapper level so `forge --login-chatgpt`
 * works, not just `forge-agent --login-chatgpt`. We bypass the Ink TUI entirely
 * and run forge-agent inline with inherited stdio so the OAuth prompts (and the
 * paste-the-code fallback) feel native. ChatGPT Codex is the only subscription
 * provider; Claude authenticates via an Anthropic API key.
 *
 * Accepts:
 *   forge --login            → ChatGPT Codex OAuth (only supported provider)
 *   forge --login chatgpt    → ChatGPT Codex OAuth
 *   forge --login-chatgpt    → shortcut
 */
async function maybeRunLogin(argv: string[]): Promise<void> {
  if (argv.includes("--login-chatgpt")) {
    return runLogin("--login-chatgpt", argv, "--login-chatgpt");
  }

  const idx = argv.indexOf("--login");
  if (idx < 0) return;

  const providerArg = argv[idx + 1];

  if (providerArg && !providerArg.startsWith("-")) {
    if (isClaudeProvider(providerArg)) {
      console.error(CLAUDE_LOGIN_UNSUPPORTED);
      process.exit(2);
    }
    const flag = providerToAgentFlag(providerArg);
    if (!flag) {
      console.error(`forge: unknown login provider '${providerArg}'. Supported: chatgpt.`);
      console.error(`  (Claude uses an Anthropic API key; Gemini, DeepSeek, etc. work via OpenRouter or an API key in the /model menu.)`);
      process.exit(2);
    }
    const rest = [...argv.slice(0, idx), ...argv.slice(idx + 2)];
    if (rest.length > 0) {
      console.error(`forge: --login <provider> cannot be combined with other options. Got: ${rest.join(" ")}`);
      process.exit(2);
    }
    return runLogin(flag, [], "--login");
  }

  // Bare --login — ChatGPT Codex is the only subscription provider.
  const rest = [...argv.slice(0, idx), ...argv.slice(idx + 1)];
  if (rest.length > 0) {
    console.error(`forge: --login cannot be combined with other options. Got: ${rest.join(" ")}`);
    process.exit(2);
  }
  return runLogin("--login-chatgpt", [], "--login");
}

async function runLogin(agentFlag: string, _argv: string[], _userForm: string): Promise<void> {
  const binary = findAgentBinary();
  const proc = Bun.spawn([binary, agentFlag], {
    stdin: "inherit",
    stdout: "inherit",
    stderr: "inherit",
  });
  const exitCode = await proc.exited;
  process.exit(exitCode);
}

await maybeRunLogin(process.argv.slice(2));

const cli = parseArgs(process.argv.slice(2));

// --dangerously-allow-all is exactly what it says: every tool approval gate
// is bypassed for the whole session. The flag name is the warning, but a
// confirmation gate that defaults to NO catches the case where someone (or
// some other tool) launched forge with this flag without the user actually
// reading what it does. Skip when FORGE_SKIP_DANGEROUS_CONFIRM=1 is set,
// for scripted / CI usage where the operator wants to opt out of the prompt.
if (
  cli.agentArgs.includes("--dangerously-allow-all") &&
  !process.env["FORGE_SKIP_DANGEROUS_CONFIRM"]
) {
  const readline = await import("node:readline/promises");
  const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
  console.log("");
  console.log("\x1b[31m\x1b[1mDANGER: --dangerously-allow-all is set.\x1b[0m");
  console.log("");
  console.log("This bypasses EVERY tool approval prompt for this entire session.");
  console.log("Forge can read, write, edit, and execute anything on this machine");
  console.log("(within your user's permissions) without asking you first.");
  console.log("");
  console.log("Only continue if you are in a sandbox, a VM, or a disposable workspace");
  console.log("where you have already accepted that risk.");
  console.log("");
  console.log("  1. No  — exit and run forge without --dangerously-allow-all");
  console.log("  2. Yes — proceed at your own risk");
  console.log("");
  console.log("(To skip this prompt in scripted environments, set");
  console.log(" FORGE_SKIP_DANGEROUS_CONFIRM=1 before launching forge.)");
  console.log("");

  let confirmed = false;
  try {
    // Loop until we get a recognized choice. Default behavior on empty input
    // or anything ambiguous: No (exit).
    while (true) {
      const raw = (await rl.question("Choice [1-2, default: 1]: ")).trim().toLowerCase();
      if (raw === "" || raw === "1" || raw === "no" || raw === "n") {
        confirmed = false;
        break;
      }
      if (raw === "2" || raw === "yes" || raw === "y") {
        confirmed = true;
        break;
      }
      console.log(`  '${raw}' is not a recognized choice. Type 1 (No) or 2 (Yes).`);
    }
  } finally {
    rl.close();
  }
  if (!confirmed) {
    console.log("Cancelled.");
    process.exit(0);
  }
}

// Hide the OS-level terminal cursor while the TUI is rendering. Ink writes
// new content as the agent streams, and the terminal cursor parks itself
// wherever the last write landed — which means it shows up in the middle of
// streaming assistant text, in scrollback, anywhere. The input box has its
// own visible cursor (rendered by PromptInput), so we don't need the OS one
// at all while we own the screen.
//
// Restore on any normal exit so the user's shell behaves correctly afterward.
const CURSOR_HIDE = "\x1b[?25l";
const CURSOR_SHOW = "\x1b[?25h";

process.stdout.write(CURSOR_HIDE);
const restoreCursor = () => {
  try { process.stdout.write(CURSOR_SHOW); } catch { /* ignore */ }
};
process.on("exit", restoreCursor);
process.on("SIGINT", () => { restoreCursor(); process.exit(130); });
process.on("SIGTERM", () => { restoreCursor(); process.exit(143); });

render(<App initialAgentArgs={cli.agentArgs} initialCwd={cli.cwd} />);
