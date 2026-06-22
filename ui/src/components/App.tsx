// SPDX-License-Identifier: Apache-2.0
import React, { useState, useMemo } from "react";
import { Box, Static, Text, useApp, useInput, useStdout } from "ink";
import { useAgent } from "../hooks/useAgent.js";
import type { ChatEntry, PermissionMode } from "../hooks/useAgent.js";
import { Message } from "./Message.js";
import { Spinner } from "./Spinner.js";
import { PromptInput, type SlashCommand } from "./PromptInput.js";
import { ContextBar } from "./ContextBar.js";
import { ApprovalDialog } from "./ApprovalDialog.js";
import { PlanApproval } from "./PlanApproval.js";
import { Menu, type MenuOption } from "./Menu.js";
import { SubagentStatus } from "./SubagentStatus.js";
import { QuestionDialog } from "./QuestionDialog.js";
import { BgPromptDialog } from "./BgPromptDialog.js";
import type { EndpointInfo } from "../protocol.js";
import { activeEndpoint, collapseHome, thinkingIntensityDisplay } from "../model-display.js";

interface SessionMeta {
  id: string;
  title: string;
  updated_at: string;
  message_count: number;
  compaction_count: number;
  model: string;
}

const SLASH_COMMANDS: SlashCommand[] = [
  { command: "/quit", description: "Exit Forge" },
  { command: "/restart", description: "Restart the agent process" },
  { command: "/clear", description: "Start a fresh session" },
  { command: "/compact", description: "Compact conversation context" },
  { command: "/revert", description: "Restore a previous user turn and code snapshot" },
  { command: "/usage", description: "Show token and context usage" },
  { command: "/model", description: "Open model selector" },
  { command: "/thinking", description: "Open reasoning and thinking controls" },
  { command: "/context", description: "Open context strategy settings" },
  { command: "/settings", description: "Open settings" },
  { command: "/copy", description: "Freeze live updates so text can be selected" },
  { command: "/subagent", description: "Configure subagents" },
  { command: "/plan", description: "Enter plan mode" },
  { command: "/resume", description: "Resume a saved session" },
  { command: "/sessions", description: "Open saved sessions" },
  { command: "/log", description: "Show current session log path" },
  { command: "/login", description: "Log in to Claude or ChatGPT" },
  { command: "/help", description: "Show command help" },
  { command: "/exit", description: "Exit Forge" },
  { command: "/tokens", description: "Alias for /usage" },
  { command: "/ctx", description: "Alias for /usage" },
  { command: "/agents", description: "Alias for /subagent" },
  { command: "/think", description: "Alias for /thinking" },
  { command: "/reasoning", description: "Alias for /thinking" },
  { command: "/reconnect", description: "Alias for /restart" },
];

const MAX_LIVE_SCROLLBACK_ENTRIES = 24;
const BOTTOM_OVERWRITE_GUARD_LINES = 4;

function isContextArchiveBoundary(entry: ChatEntry): boolean {
  if (entry.kind !== "assistant" && entry.kind !== "system") return false;
  return (
    entry.content.startsWith("[Rolling window:") ||
    entry.content.startsWith("[Context overflow") ||
    entry.content.startsWith("[Context compacted")
  );
}

function lastContextArchiveBoundaryIndex(entries: ChatEntry[]): number {
  for (let i = entries.length - 1; i >= 0; i--) {
    if (isContextArchiveBoundary(entries[i]!)) return i;
  }
  return -1;
}

function estimateEntryLines(entry: ChatEntry, columns: number): number {
  const width = Math.max(20, columns - 4);
  const rawLines = entry.content.split("\n");
  let lines = 0;
  for (const line of rawLines) {
    lines += Math.max(1, Math.ceil(line.length / width));
  }

  switch (entry.kind) {
    case "assistant":
    case "streaming":
    case "user":
    case "error":
    case "plan_status":
      return lines + 1;
    default:
      return lines;
  }
}

function isTurnSummary(entry: ChatEntry | undefined): boolean {
  return entry?.kind === "system" && entry.content.startsWith("✦ Worked for");
}

function archiveSplitForLiveBudget(entries: ChatEntry[], columns: number, maxLines: number): number {
  let lines = 0;
  let count = 0;
  let split = entries.length;
  const latestAssistantIndex = (() => {
    for (let i = entries.length - 1; i >= 0; i--) {
      if (entries[i]?.kind === "assistant") return i;
    }
    return -1;
  })();
  const keepLatestAssistantLive =
    latestAssistantIndex === entries.length - 1 ||
    (latestAssistantIndex === entries.length - 2 && isTurnSummary(entries[entries.length - 1]));

  for (let i = entries.length - 1; i >= 0; i--) {
    // Completed assistant messages can be arbitrarily tall after Markdown
    // rendering. Keep older ones in <Static> so the bottom prompt/status chrome
    // never redraws over their tail. The latest completed assistant response
    // stays live through the turn summary; moving it to <Static> immediately can
    // drop the visible tail in some terminals even though the transcript is
    // complete on disk.
    if (entries[i]?.kind === "assistant") {
      if (keepLatestAssistantLive && i === latestAssistantIndex) {
        split = i;
        count++;
        continue;
      }
      break;
    }

    const nextLines = estimateEntryLines(entries[i]!, columns);
    if (count >= MAX_LIVE_SCROLLBACK_ENTRIES || lines + nextLines > maxLines) {
      break;
    }
    lines += nextLines;
    count++;
    split = i;
  }

  return split;
}

const slashHelpText = () => {
  const primary = SLASH_COMMANDS.filter((cmd) => !cmd.description.startsWith("Alias for"));
  const aliases = SLASH_COMMANDS.filter((cmd) => cmd.description.startsWith("Alias for"));
  return [
    "Commands:",
    ...primary.map((cmd) => `  ${cmd.command.padEnd(12)} ${cmd.description}`),
    "Aliases:",
    ...aliases.map((cmd) => `  ${cmd.command.padEnd(12)} ${cmd.description.replace("Alias for ", "")}`),
    "Keys: Enter=send, Ctrl+C=quit, Esc=cancel",
  ].join("\n");
};

function normalizeSubmittedPrompt(text: string): string {
  const lines = text.split("\n");
  const guttered = lines.filter((line) => /^\s*[▎│┃]\s?/.test(line)).length;
  if (guttered < 3) return text;

  return lines
    .map((line) => line.replace(/^\s*[▎│┃]\s?/, ""))
    .join("\n")
    .trim();
}

// Menu stack — each entry is a menu screen
type MenuScreen =
  | { kind: "model_hub" }
  | { kind: "main_model" }
  | { kind: "subagent_model" }
  | { kind: "web_model" }
  | { kind: "reasoning_openai"; ep: EndpointInfo }
  | { kind: "reasoning_anthropic"; ep: EndpointInfo }
  | { kind: "reasoning_chatgpt"; ep: EndpointInfo }
  | { kind: "settings" }
  | { kind: "settings_permission" }
  | { kind: "settings_tools_basic" }
  | { kind: "settings_tools_advanced" }
  | { kind: "settings_context" }
  | { kind: "subagent" }
  | { kind: "revert" }
  | { kind: "revert_confirm" }
  | { kind: "sessions" }
  | { kind: "session_delete_confirm"; meta: SessionMeta };

interface AppProps {
  initialAgentArgs?: string[];
  initialCwd?: string;
}

export function App({ initialAgentArgs, initialCwd }: AppProps) {
  const app = useApp();
  const { stdout } = useStdout();
  const {
    state,
    send,
    sendMessage,
    queueUserMessage,
    switchModel,
    approveAction,
    approveAlways,
    denyAction,
    compact,
    clearSession,
    rewind,
    previewRewind,
    clearRewindPreview,
    requestUsage,
    enterPlanMode,
    approvePlan,
    clearAndApprovePlan,
    rejectPlan,
    addSystemEntry,
    answerQuestion,
    cyclePermissionMode,
    setContextStrategy,
    setPermissionMode,
    setToolEnabled,
    updateEndpointReasoning,
    sendProcessInput,
    sendBgProcessInput,
    cancelRun,
    quit,
    restartAgent,
    resumeSession,
  } = useAgent({ initialAgentArgs, initialCwd });

  const [menuStack, setMenuStack] = useState<MenuScreen[]>([]);
  const [copySnapshot, setCopySnapshot] = useState<ChatEntry[] | null>(null);
  const activeMenu = menuStack.length > 0 ? menuStack[menuStack.length - 1] : null;
  const copyMode = copySnapshot !== null;

  const pushMenu = (screen: MenuScreen) => setMenuStack((s) => [...s, screen]);
  const popMenu = () => setMenuStack((s) => s.slice(0, -1));
  const closeMenus = () => setMenuStack([]);
  const exitForge = () => {
    quit();
    setTimeout(() => {
      app.exit();
      process.exit(0);
    }, 150);
  };
  const toggleCopyMode = () => {
    setCopySnapshot((current) => current ? null : [...state.scrollback, ...state.transient]);
  };

  // Global keybindings
  useInput((input, key) => {
    if (input === "c" && key.ctrl) {
      exitForge();
      return;
    }
    if (input === "f" && key.ctrl && !state.pendingApproval && !state.pendingPlan && !activeMenu) {
      toggleCopyMode();
      return;
    }
    if (key.escape && state.isThinking && !state.pendingApproval && !state.pendingPlan && !activeMenu) {
      cancelRun();
      return;
    }
    // Shift+Tab cycles permission mode
    if (key.tab && key.shift && !state.pendingApproval && !state.pendingPlan && !activeMenu) {
      cyclePermissionMode();
    }
  });

  const getSessions = (): SessionMeta[] => {
    const fs = require("fs");
    const path = require("path");
    const sessionsDir = path.join(state.projectRoot, ".forge", "sessions");
    if (!fs.existsSync(sessionsDir)) return [];
    const dirs = fs.readdirSync(sessionsDir).filter((d: string) => {
      return fs.existsSync(path.join(sessionsDir, d, "meta.json"));
    });
    return dirs.map((d: string) => {
      const raw = fs.readFileSync(path.join(sessionsDir, d, "meta.json"), "utf-8");
      return JSON.parse(raw) as SessionMeta;
    }).sort((a: SessionMeta, b: SessionMeta) =>
      b.updated_at.localeCompare(a.updated_at)
    );
  };

  // Stash session metas so the select handler can look up by index
  const sessionMetasRef = React.useRef<SessionMeta[]>([]);

  const buildSessionsMenu = (): { items: MenuOption[]; footer: string[] } => {
    const metas = getSessions();
    sessionMetasRef.current = metas;
    if (metas.length === 0) {
      return {
        items: [{ label: "No previous sessions", description: "" }],
        footer: ["Start a conversation to create a session."],
      };
    }
    const items: MenuOption[] = metas.map((m) => {
      const date = new Date(m.updated_at).toLocaleString();
      const title = m.title.length > 50 ? m.title.slice(0, 47) + "..." : m.title;
      return {
        label: title,
        description: `${date} · ${m.message_count} msgs`,
      };
    });
    return {
      items,
      footer: ["Select a session to resume. Esc to cancel."],
    };
  };

  const handleSessionSelect = (index: number) => {
    const metas = sessionMetasRef.current;
    if (index >= 0 && index < metas.length) {
      closeMenus();
      resumeSession(metas[index]!.id);
    } else {
      closeMenus();
    }
  };

  const handleSessionDeleteKey = (key: string, selectedIndex: number) => {
    if (key !== "d") return;
    const metas = sessionMetasRef.current;
    const meta = metas[selectedIndex];
    if (meta) {
      pushMenu({ kind: "session_delete_confirm", meta });
    }
  };

  const handleSessionDeleteConfirm = (index: number, meta: SessionMeta) => {
    if (index === 0) {
      const fs = require("fs");
      const path = require("path");
      const sessionDir = path.join(state.projectRoot, ".forge", "sessions", meta.id);
      try {
        fs.rmSync(sessionDir, { recursive: true, force: true });
        addSystemEntry(`Deleted session: ${meta.title}`);
      } catch (e: unknown) {
        const msg = e instanceof Error ? e.message : String(e);
        addSystemEntry(`Failed to delete session: ${msg}`);
      }
    }
    popMenu();
  };

  const showSlashHelp = () => {
    addSystemEntry(slashHelpText());
  };

  const openRevertMenu = () => {
    if (state.rewindCheckpoints.length === 0) {
      addSystemEntry("No revert targets are available in this UI session yet.");
      return;
    }
    pushMenu({ kind: "revert" });
  };

  const activeMainEndpoint = activeEndpoint(
    state.endpoints,
    state.modelName,
    state.modelId,
    state.maxContextTokens
  );
  const terminalColumns = stdout.columns || process.stdout.columns || 80;
  const terminalRows = stdout.rows || process.stdout.rows || 24;
  const bottomChromeReserve = 6 + BOTTOM_OVERWRITE_GUARD_LINES;
  const usableRows = Math.max(8, terminalRows - bottomChromeReserve);
  const liveLineBudget = Math.max(3, Math.min(8, Math.floor(usableRows * 0.22)));
  const streamingMaxLines = Math.max(3, Math.min(10, Math.floor(terminalRows * 0.32)));
  const lastContextBoundary = lastContextArchiveBoundaryIndex(state.scrollback);
  const heightBudgetSplit = archiveSplitForLiveBudget(
    state.scrollback,
    terminalColumns,
    liveLineBudget
  );
  const archiveSplit = Math.max(
    lastContextBoundary >= 0 ? lastContextBoundary + 1 : 0,
    heightBudgetSplit
  );
  const archivedScrollback = state.scrollback.slice(0, archiveSplit);
  const liveScrollback = state.scrollback.slice(archiveSplit);

  const openCurrentReasoningMenu = () => {
    const ep = activeMainEndpoint;
    if (!ep) {
      addSystemEntry("No active endpoint found for reasoning settings.");
      return;
    }
    switch (ep.endpoint_type) {
      case "anthropic":
        pushMenu({ kind: "reasoning_anthropic", ep });
        break;
      case "chatgpt_codex":
        pushMenu({ kind: "reasoning_chatgpt", ep });
        break;
      default:
        pushMenu({ kind: "reasoning_openai", ep });
        break;
    }
  };

  const handleLoginCommand = (args: string) => {
    const flag = args.trim().toLowerCase();
    if (!flag || flag === "--help") {
      addSystemEntry("Usage: /login --chatgpt\n(Claude uses an Anthropic API key — set one via /model — not subscription login.)");
      return;
    }
    if (flag === "--anthropic" || flag === "--claude") {
      addSystemEntry(
        "Claude subscription login is not supported. Anthropic restricts subscription credentials\n" +
        "to its own apps, so Forge uses an Anthropic API key instead — add one via /model or in\n" +
        "~/.config/forge/config.toml."
      );
    } else if (flag === "--chatgpt" || flag === "--codex") {
      if (state.loginInProgress) {
        addSystemEntry("Login already in progress.");
        return;
      }
      addSystemEntry(
        "Tip: if the browser opens but login never completes (callback port may be busy — e.g. VS Code, codex CLI),\n" +
        "quit forge (/quit) and run  forge --login-chatgpt  from your shell. That path supports manual code-paste\n" +
        "even when the localhost callback can't bind."
      );
      send({ type: "login_chatgpt" });
    } else {
      addSystemEntry(`Unknown provider: ${flag}\nUsage: /login --help`);
    }
  };

  const handleSubmit = (text: string) => {
    const trimmed = normalizeSubmittedPrompt(text.trim());
    if (!trimmed) return;

    // If the agent is working, queue the message for the next agent boundary.
    if (state.isThinking && !trimmed.startsWith("/")) {
      queueUserMessage(trimmed);
      return;
    }

    switch (trimmed) {
      case "/quit":
      case "/exit":
        exitForge();
        break;
      case "/restart":
      case "/reconnect":
        restartAgent();
        break;
      case "/clear":
        clearSession();
        break;
      case "/compact":
        compact();
        break;
      case "/revert":
        openRevertMenu();
        break;
      case "/usage":
      case "/tokens":
      case "/ctx":
        requestUsage();
        break;
      case "/model":
        pushMenu({ kind: "model_hub" });
        break;
      case "/thinking":
      case "/think":
      case "/reasoning":
        openCurrentReasoningMenu();
        break;
      case "/context":
        pushMenu({ kind: "settings_context" });
        break;
      case "/settings":
        pushMenu({ kind: "settings" });
        break;
      case "/copy":
        toggleCopyMode();
        break;
      case "/subagent":
      case "/agents":
        pushMenu({ kind: "subagent" });
        break;
      case "/plan":
        enterPlanMode();
        break;
      case "/log":
        addSystemEntry(`Log: ${collapseHome(state.logPath)}`);
        break;
      case "/sessions":
      case "/resume":
        pushMenu({ kind: "sessions" });
        break;
      case "/help":
        showSlashHelp();
        break;
      default:
        if (trimmed.startsWith("/login")) {
          handleLoginCommand(trimmed.slice("/login".length));
          break;
        }
        if (trimmed.startsWith("/")) {
          const cmd = trimmed.split(/\s/)[0];
          addSystemEntry(`Unknown command: ${cmd}`);
          showSlashHelp();
        } else {
          sendMessage(trimmed);
        }
        break;
    }
  };

  // ── Menu builders ──────────────────────────────────────────────────

  const mainModelName = useMemo(() => {
    const ep = state.endpoints.find((e) => e.model_id === state.modelId);
    return ep?.name ?? state.modelId;
  }, [state.endpoints, state.modelId]);

  const buildModelHubMenu = (): { items: MenuOption[]; footer: string[] } => {
    return {
      items: [
        { label: "Main model", description: `Primary agent model \u00B7 ${mainModelName}` },
        { label: "Subagent model", description: "Default for subagents" },
        { label: "Web tool model", description: "web_fetch summarization" },
        { label: "Reasoning", description: `Current model \u00B7 ${mainModelName}` },
      ],
      footer: ["Configure which models are used for different tasks."],
    };
  };

  const toggleLabel = (value: "provider_default" | "on" | "off"): string => ({
    provider_default: "Provider default",
    on: "On",
    off: "Off",
  })[value];

  const effortLabel = (value: EndpointInfo["reasoning"]["chatgpt_codex"]["effort"]): string => ({
    provider_default: "Provider default",
    none: "None",
    minimal: "Minimal",
    low: "Low",
    medium: "Medium",
    high: "High",
    xhigh: "XHigh",
  })[value];

  const nextToggle = (value: "provider_default" | "on" | "off"): "provider_default" | "on" | "off" => (
    value === "provider_default" ? "on" : value === "on" ? "off" : "provider_default"
  );

  const anthropicBudgetLevels = (ep: EndpointInfo): Array<{ label: string; tokens: number }> => {
    const maxBudget = Math.max(1024, ep.max_output_tokens - 1);
    return [
      { label: "Low", tokens: Math.min(1024, maxBudget) },
      { label: "Medium", tokens: Math.min(4096, maxBudget) },
      { label: "High", tokens: Math.min(8192, maxBudget) },
      { label: "XHigh", tokens: Math.min(32768, maxBudget) },
    ].filter((level, index, levels) =>
      index === 0 || level.tokens !== levels[index - 1]?.tokens
    );
  };

  const anthropicBudgetLabel = (ep: EndpointInfo): string => {
    const current = ep.reasoning.anthropic.budget_tokens;
    const levels = anthropicBudgetLevels(ep);
    const exact = levels.find((level) => level.tokens === current);
    if (exact) return `${exact.label} (${exact.tokens})`;
    const closest = levels.reduce((best, level) =>
      Math.abs(level.tokens - current) < Math.abs(best.tokens - current) ? level : best
    , levels[0] ?? { label: "Low", tokens: 1024 });
    return `${closest.label} (${current})`;
  };

  /**
   * Build a grouped endpoint list with provider section headers.
   * endpointMap[i] is the EndpointInfo for item i, null for headers,
   * or a login action item.
   */
  type EndpointMenuEntry = EndpointInfo | null | "login_chatgpt";
  const isCurrentEndpoint = (ep: EndpointInfo) =>
    ep.model_id === state.modelId && ep.max_context_tokens === state.maxContextTokens;

  const buildGroupedEndpoints = (): { items: MenuOption[]; endpointMap: EndpointMenuEntry[] } => {
    const items: MenuOption[] = [];
    const endpointMap: EndpointMenuEntry[] = [];

    const localEps = state.endpoints.filter((e) => e.endpoint_type !== "anthropic" && e.endpoint_type !== "chatgpt_codex");
    const anthropicEps = state.endpoints.filter((e) => e.endpoint_type === "anthropic");
    const chatgptEps = state.endpoints.filter((e) => e.endpoint_type === "chatgpt_codex");

    if (localEps.length > 0) {
      items.push({ label: "Local", description: "", header: true });
      endpointMap.push(null);
      for (const ep of localEps) {
        items.push({
          label: ep.name,
          description: `${ep.model_id} · ${Math.floor(ep.max_context_tokens / 1000)}k context`,
          marker: isCurrentEndpoint(ep) ? "✔" : undefined,
        });
        endpointMap.push(ep);
      }
    }

    // Always show Anthropic section
    items.push({ label: "Anthropic", description: "", header: true });
    endpointMap.push(null);
    for (const ep of anthropicEps) {
      items.push({
        label: ep.name,
        description: `${ep.model_id} · ${Math.floor(ep.max_context_tokens / 1000)}k context`,
        marker: isCurrentEndpoint(ep) ? "✔" : undefined,
      });
      endpointMap.push(ep);
    }

    items.push({ label: "ChatGPT Codex", description: "", header: true });
    endpointMap.push(null);
    for (const ep of chatgptEps) {
      items.push({
        label: ep.name,
        description: `${ep.model_id} · ${Math.floor(ep.max_context_tokens / 1000)}k context`,
        marker: isCurrentEndpoint(ep) ? "✔" : undefined,
      });
      endpointMap.push(ep);
    }
    if (!state.chatgptLoggedIn) {
      items.push({
        label: state.loginInProgress ? "Logging in…" : "Login to ChatGPT Codex →",
        description: "Authorize with your ChatGPT subscription",
      });
      endpointMap.push("login_chatgpt");
    }

    return { items, endpointMap };
  };

  const buildMainModelMenu = (): { items: MenuOption[]; endpointMap: EndpointMenuEntry[]; initial: number } => {
    const { items, endpointMap } = buildGroupedEndpoints();
    const initial = Math.max(0, endpointMap.findIndex((ep) => ep && ep !== "login_chatgpt" && isCurrentEndpoint(ep as EndpointInfo)));
    return { items, endpointMap, initial };
  };

  const buildEndpointListMenu = (): { items: MenuOption[]; endpointMap: EndpointMenuEntry[] } => {
    const { items: epItems, endpointMap: epMap } = buildGroupedEndpoints();
    const items: MenuOption[] = [{ label: "Inherit", description: "Use the parent's model" }, ...epItems];
    const endpointMap: EndpointMenuEntry[] = [null, ...epMap];
    return { items, endpointMap };
  };


  const BASIC_TOOLS = ["web_search", "web_fetch", "shell_exec", "delegate_task"];

  const toolLabel = (name: string): string => ({
    read_file: "Read files",
    list_directory: "List directory",
    search_code: "Search code",
    apply_patch: "Apply patch",
    write_file: "Write files",
    edit_file: "Edit files",
    glob_files: "Glob files",
    todo_write: "Todo write",
    web_search: "Web search",
    web_fetch: "Web fetch",
    shell_exec: "Shell commands",
    delegate_task: "Subagents",
  } as Record<string, string>)[name] ?? name;

  const buildSettingsMenu = (): { items: MenuOption[]; footer: string[] } => {
    const modeLabel = { normal: "Normal", auto_accept: "Auto-accept", plan: "Plan mode" }[state.permissionMode];
    const basicDisabled = state.availableTools.filter((t) => BASIC_TOOLS.includes(t.name) && !t.enabled).length;
    const advDisabled = state.availableTools.filter((t) => !t.enabled).length;
    return {
      items: [
        { label: "Permission mode", description: modeLabel },
        { label: "Basic tools", description: basicDisabled > 0 ? `${basicDisabled} disabled` : "All enabled" },
        { label: "Advanced tools", description: advDisabled > 0 ? `${advDisabled} disabled` : "All enabled" },
        { label: "Context strategy", description: state.contextStrategy === "rolling_window" ? "Rolling window" : "Compaction" },
      ],
      footer: ["Configure Forge behaviour and which tools the agent can use."],
    };
  };

  const handleSettingsSelect = (idx: number) => {
    switch (idx) {
      case 0: pushMenu({ kind: "settings_permission" }); break;
      case 1: pushMenu({ kind: "settings_tools_basic" }); break;
      case 2: pushMenu({ kind: "settings_tools_advanced" }); break;
      case 3: pushMenu({ kind: "settings_context" }); break;
    }
  };

  const buildPermissionMenu = (): { items: MenuOption[]; footer: string[] } => {
    const mode = state.permissionMode;
    return {
      items: [
        { label: "Normal mode", description: "Ask before writing files or running commands", marker: mode === "normal" ? "\u2714" : undefined },
        { label: "Auto-accept", description: "Auto-approve all actions — faster, less safe", marker: mode === "auto_accept" ? "\u2714" : undefined },
        { label: "Plan mode", description: "Read-only — agent drafts a plan before acting", marker: mode === "plan" ? "\u2714" : undefined },
      ],
      footer: ["Also togglable with Shift+Tab."],
    };
  };

  const handlePermissionSelect = (idx: number) => {
    const modes: PermissionMode[] = ["normal", "auto_accept", "plan"];
    const labels = ["Normal mode", "Auto-accept", "Plan mode"];
    const next = modes[idx];
    if (next && next !== state.permissionMode) {
      setPermissionMode(next);
      addSystemEntry(labels[idx]);
    }
    popMenu();
  };

  const buildContextStrategyMenu = (): { items: MenuOption[]; footer: string[] } => {
    const current = state.contextStrategy;
    return {
      items: [
        {
          label: "Compaction",
          description: "Summarize old messages with the LLM before dropping them",
          marker: current === "compaction" ? "\u2714" : undefined,
        },
        {
          label: "Rolling window",
          description: "Drop oldest messages directly \u2014 no API call, no summary",
          marker: current === "rolling_window" ? "\u2714" : undefined,
        },
      ],
      footer: ["Compaction preserves context at the cost of an extra LLM call. Rolling window is instant and free."],
    };
  };

  const handleContextStrategySelect = (idx: number) => {
    const strategies = ["compaction", "rolling_window"];
    const labels = ["Compaction", "Rolling window"];
    const next = strategies[idx];
    if (next && next !== state.contextStrategy) {
      send({ type: "update_context_strategy", strategy: next });
      setContextStrategy(next);
      addSystemEntry(`Context strategy: ${labels[idx]}`);
    }
    popMenu();
  };

  const buildToolsMenu = (filter: "basic" | "advanced"): { items: MenuOption[]; footer: string[] } => {
    const tools = filter === "basic"
      ? state.availableTools.filter((t) => BASIC_TOOLS.includes(t.name))
      : state.availableTools;
    return {
      items: tools.map((t) => ({
        label: toolLabel(t.name),
        description: t.name,
        marker: t.enabled ? "\u2714" : "\u2715",
      })),
      footer: ["Select a tool to toggle it on or off."],
    };
  };

  const handleToolToggle = (idx: number, filter: "basic" | "advanced") => {
    const tools = filter === "basic"
      ? state.availableTools.filter((t) => BASIC_TOOLS.includes(t.name))
      : state.availableTools;
    const tool = tools[idx];
    if (tool) {
      const next = !tool.enabled;
      send({ type: "update_tool_config", tool: tool.name, enabled: next });
      setToolEnabled(tool.name, next);
      addSystemEntry(`${toolLabel(tool.name)}: ${next ? "enabled" : "disabled"}`);
    }
  };

  const buildSubagentMenu = (): { items: MenuOption[]; footer: string[] } => {
    const items: MenuOption[] = [
      { label: "Toggle subagents", description: "Enable/disable subagent delegation" },
      { label: "Set default model", description: "Choose which model subagents use" },
      { label: "Max concurrent: 1", description: "Sequential \u2014 one subagent at a time" },
      { label: "Max concurrent: 2", description: "Up to 2 parallel subagents" },
      { label: "Max concurrent: 4", description: "Up to 4 parallel subagents" },
      { label: "Max depth: 1", description: "No nesting" },
      { label: "Max depth: 2", description: "One level of nesting" },
      { label: "Max depth: 3", description: "Two levels of nesting" },
      { label: "Max depth: 4", description: "Three levels of nesting" },
    ];
    // Append agent definitions
    for (const def of state.agentDefs) {
      items.push({
        label: def.name,
        description: `${def.description} \u00B7 ${def.model} \u00B7 ${def.source}`,
      });
    }
    return {
      items,
      footer: [
        "Each subagent has its own context window, system prompt, and tool allowlist.",
        "Place .md files in .agent/agents/ or ~/.config/forge/agents/ to add custom agents.",
      ],
    };
  };

  const buildRevertMenu = (): { items: MenuOption[]; footer: string[] } => {
    const checkpoints = [...state.rewindCheckpoints].reverse();
    return {
      items: checkpoints.map((checkpoint, idx) => ({
        label: checkpoint.preview || "(empty message)",
        description: idx === 0
          ? "Previous user turn"
          : `${idx + 1} user turns back`,
      })),
      footer: ["Select the user message to revert to. Forge will restore files and trim conversation state."],
    };
  };

  const handleRevertSelect = (idx: number) => {
    const checkpoints = [...state.rewindCheckpoints].reverse();
    const checkpoint = checkpoints[idx];
    if (!checkpoint) return;
    previewRewind(checkpoint.id);
    pushMenu({ kind: "revert_confirm" });
  };

  const buildRevertConfirmMenu = (): { items: MenuOption[]; footer: string[] } => {
    const pending = state.pendingRewind;
    const ready = Boolean(pending && pending.summary !== "Loading revert preview...");
    return {
      items: [
        { label: ready ? "Revert" : "Loading", description: ready ? "Restore files and conversation to this message" : "Waiting for file and line counts" },
        { label: "Cancel", description: "Keep the current files and conversation" },
      ],
      footer: pending
        ? pending.summary.split("\n")
        : ["Loading revert preview..."],
    };
  };

  const handleRevertConfirmSelect = (idx: number) => {
    const pending = state.pendingRewind;
    const ready = pending && pending.summary !== "Loading revert preview...";
    if (idx === 0 && !ready) return;
    if (idx === 0 && pending) {
      rewind(pending.checkpointId);
      clearRewindPreview();
      closeMenus();
      return;
    }
    clearRewindPreview();
    popMenu();
  };

  const cancelRevertConfirm = () => {
    clearRewindPreview();
    popMenu();
  };



  // ── Menu selection handlers ────────────────────────────────────────

  const handleModelHubSelect = (idx: number) => {
    switch (idx) {
      case 0: pushMenu({ kind: "main_model" }); break;
      case 1: pushMenu({ kind: "subagent_model" }); break;
      case 2: pushMenu({ kind: "web_model" }); break;
      case 3: openCurrentReasoningMenu(); break;
    }
  };

  const handleMainModelSelect = (idx: number) => {
    const { endpointMap } = buildMainModelMenu();
    const entry = endpointMap[idx];
    if (entry === "login_chatgpt") {
      send({ type: "login_chatgpt" });
      closeMenus();
    } else if (entry) {
      switchModel(entry);
      closeMenus();
    } else {
      closeMenus();
    }
  };

  const resolveEndpoint = (entry: EndpointMenuEntry): EndpointInfo | null =>
    entry && entry !== "login_chatgpt" ? entry as EndpointInfo : null;

  const handleSubagentModelSelect = (idx: number) => {
    const { endpointMap } = buildEndpointListMenu();
    const ep = resolveEndpoint(endpointMap[idx]);
    if (idx === 0 || !ep) {
      send({ type: "update_subagent_config", clear_default_model: true });
      addSystemEntry("Subagent default model: inherit");
    } else {
      send({ type: "update_subagent_config", default_model: ep.name });
      addSystemEntry(`Subagent default model: ${ep.name} (${ep.model_id})`);
    }
    closeMenus();
  };

  const handleWebModelSelect = (idx: number) => {
    const { endpointMap } = buildEndpointListMenu();
    const ep = resolveEndpoint(endpointMap[idx]);
    if (idx === 0 || !ep) {
      send({ type: "update_web_model", model: "" });
      addSystemEntry("Web tool model: inherit");
    } else {
      send({ type: "update_web_model", model: ep.name });
      addSystemEntry(`Web tool model: ${ep.name} (${ep.model_id})`);
    }
    closeMenus();
  };

  const handleOpenAiReasoningSelect = (idx: number, ep: EndpointInfo) => {
    const reasoning = structuredClone(ep.reasoning);
    if (idx === 0) {
      reasoning.open_ai_compatible.thinking = nextToggle(reasoning.open_ai_compatible.thinking);
      addSystemEntry(`Reasoning for ${ep.name}: thinking ${toggleLabel(reasoning.open_ai_compatible.thinking)}`);
    } else if (idx === 1) {
      reasoning.open_ai_compatible.preserve_thinking = nextToggle(
        reasoning.open_ai_compatible.preserve_thinking
      );
      addSystemEntry(`Reasoning for ${ep.name}: preserve thinking ${toggleLabel(reasoning.open_ai_compatible.preserve_thinking)}`);
    }
    updateEndpointReasoning(ep.name, reasoning);
  };

  const handleAnthropicReasoningSelect = (idx: number, ep: EndpointInfo) => {
    const reasoning = structuredClone(ep.reasoning);
    if (idx === 0) {
      reasoning.anthropic.thinking = nextToggle(reasoning.anthropic.thinking);
      addSystemEntry(`Reasoning for ${ep.name}: thinking ${toggleLabel(reasoning.anthropic.thinking)}`);
    } else if (idx === 1) {
      const budgetLevels = anthropicBudgetLevels(ep);
      const currentIndex = budgetLevels.findIndex(
        (level) => level.tokens === reasoning.anthropic.budget_tokens
      );
      const next = budgetLevels[
        currentIndex >= 0 ? (currentIndex + 1) % budgetLevels.length : 0
      ] ?? budgetLevels[0] ?? { label: "Low", tokens: 1024 };
      reasoning.anthropic.budget_tokens = next.tokens;
      addSystemEntry(`Reasoning for ${ep.name}: thinking budget ${next.label} (${next.tokens})`);
    }
    updateEndpointReasoning(ep.name, reasoning);
  };

  const handleChatGptReasoningSelect = (idx: number, ep: EndpointInfo) => {
    if (idx !== 0) return;
    const reasoning = structuredClone(ep.reasoning);
    const efforts: EndpointInfo["reasoning"]["chatgpt_codex"]["effort"][] = [
      "provider_default",
      "none",
      "minimal",
      "low",
      "medium",
      "high",
      "xhigh",
    ];
    const currentIndex = efforts.indexOf(reasoning.chatgpt_codex.effort);
    reasoning.chatgpt_codex.effort = efforts[
      currentIndex >= 0 ? (currentIndex + 1) % efforts.length : 0
    ]!;
    addSystemEntry(`Reasoning for ${ep.name}: effort ${effortLabel(reasoning.chatgpt_codex.effort)}`);
    updateEndpointReasoning(ep.name, reasoning);
  };

  const handleSubagentSelect = (idx: number) => {
    switch (idx) {
      case 0: // Toggle
        send({ type: "update_subagent_config", enabled: true }); // TODO: toggle needs current state
        addSystemEntry("Subagent config updated");
        closeMenus();
        break;
      case 1: // Set model
        pushMenu({ kind: "subagent_model" });
        break;
      case 2: send({ type: "update_subagent_config", max_concurrent: 1 }); addSystemEntry("Max concurrent subagents: 1"); closeMenus(); break;
      case 3: send({ type: "update_subagent_config", max_concurrent: 2 }); addSystemEntry("Max concurrent subagents: 2"); closeMenus(); break;
      case 4: send({ type: "update_subagent_config", max_concurrent: 4 }); addSystemEntry("Max concurrent subagents: 4"); closeMenus(); break;
      case 5: send({ type: "update_subagent_config", max_depth: 1 }); addSystemEntry("Subagent max depth: 1"); closeMenus(); break;
      case 6: send({ type: "update_subagent_config", max_depth: 2 }); addSystemEntry("Subagent max depth: 2"); closeMenus(); break;
      case 7: send({ type: "update_subagent_config", max_depth: 3 }); addSystemEntry("Subagent max depth: 3"); closeMenus(); break;
      case 8: send({ type: "update_subagent_config", max_depth: 4 }); addSystemEntry("Subagent max depth: 4"); closeMenus(); break;
      default: {
        // Agent definition info
        const defIdx = idx - 9;
        const def = state.agentDefs[defIdx];
        if (def) {
          const turns = def.max_turns !== null ? `${def.max_turns}` : "unlimited";
          addSystemEntry(
            `Agent: ${def.name}\n  Description: ${def.description}\n  Source: ${def.source}\n  Model: ${def.model}\n  Max turns: ${turns}\n  Tools: [${def.tools.join(", ")}]`
          );
        }
        closeMenus();
        break;
      }
    }
  };

  // ── Render ─────────────────────────────────────────────────────────

  const isInputDisabled =
    state.pendingApproval !== null ||
    state.pendingPlan !== null ||
    state.pendingQuestion !== null ||
    activeMenu !== null;

  const renderMenu = () => {
    if (!activeMenu) return null;

    switch (activeMenu.kind) {
      case "model_hub": {
        const { items, footer } = buildModelHubMenu();
        return <Menu title="Model configuration" items={items} onSelect={handleModelHubSelect} onCancel={popMenu} footer={footer} />;
      }
      case "main_model": {
        const { items, initial } = buildMainModelMenu();
        return <Menu title="Main model" items={items} initialSelected={initial} onSelect={handleMainModelSelect} onCancel={popMenu} footer={["Switch the primary agent model. Changes take effect immediately."]} nested />;
      }
      case "subagent_model": {
        const { items } = buildEndpointListMenu();
        return <Menu title="Subagent default model" items={items} onSelect={handleSubagentModelSelect} onCancel={popMenu} footer={["Choose which model subagents use by default."]} nested />;
      }
      case "web_model": {
        const { items } = buildEndpointListMenu();
        return <Menu title="Web tool model" items={items} onSelect={handleWebModelSelect} onCancel={popMenu} footer={["Choose which model summarizes web_fetch content."]} nested />;
      }
      case "reasoning_openai": {
        const liveEp = state.endpoints.find((candidate) => candidate.name === activeMenu.ep.name) ?? activeMenu.ep;
        const items: MenuOption[] = [
          {
            label: "Thinking",
            description: toggleLabel(liveEp.reasoning.open_ai_compatible.thinking),
          },
          {
            label: "Preserve thinking",
            description: toggleLabel(liveEp.reasoning.open_ai_compatible.preserve_thinking),
          },
        ];
        return <Menu
          title={`${liveEp.name} reasoning`}
          items={items}
          onSelect={(idx) => handleOpenAiReasoningSelect(idx, liveEp)}
          onCancel={popMenu}
          footer={["OpenAI-compatible local endpoints use chat_template_kwargs. Select an item to cycle its value."]}
          nested
        />;
      }
      case "reasoning_anthropic": {
        const liveEp = state.endpoints.find((candidate) => candidate.name === activeMenu.ep.name) ?? activeMenu.ep;
        const items: MenuOption[] = [
          {
            label: "Thinking",
            description: toggleLabel(liveEp.reasoning.anthropic.thinking),
          },
          {
            label: "Thinking budget",
            description: anthropicBudgetLabel(liveEp),
          },
        ];
        return <Menu
          title={`${liveEp.name} reasoning`}
          items={items}
          onSelect={(idx) => handleAnthropicReasoningSelect(idx, liveEp)}
          onCancel={popMenu}
          footer={["Select an item to cycle its value. Budget maps to Low, Medium, High, and XHigh token amounts."]}
          nested
        />;
      }
      case "reasoning_chatgpt": {
        const liveEp = state.endpoints.find((candidate) => candidate.name === activeMenu.ep.name) ?? activeMenu.ep;
        const items: MenuOption[] = [
          {
            label: "Reasoning effort",
            description: effortLabel(liveEp.reasoning.chatgpt_codex.effort),
          },
        ];
        return <Menu
          title={`${liveEp.name} reasoning`}
          items={items}
          onSelect={(idx) => handleChatGptReasoningSelect(idx, liveEp)}
          onCancel={popMenu}
          footer={["ChatGPT/Codex uses the Responses API reasoning.effort field. Select to cycle values."]}
          nested
        />;
      }
      case "settings": {
        const { items, footer } = buildSettingsMenu();
        return <Menu title="Settings" items={items} onSelect={handleSettingsSelect} onCancel={popMenu} footer={footer} />;
      }
      case "settings_permission": {
        const { items, footer } = buildPermissionMenu();
        return <Menu title="Permission mode" items={items} onSelect={handlePermissionSelect} onCancel={popMenu} footer={footer} nested />;
      }
      case "settings_tools_basic": {
        const { items, footer } = buildToolsMenu("basic");
        return <Menu title="Basic tools" items={items} onSelect={(idx) => handleToolToggle(idx, "basic")} onCancel={popMenu} footer={footer} nested />;
      }
      case "settings_tools_advanced": {
        const { items, footer } = buildToolsMenu("advanced");
        return <Menu title="Advanced tools" items={items} onSelect={(idx) => handleToolToggle(idx, "advanced")} onCancel={popMenu} footer={footer} nested />;
      }
      case "settings_context": {
        const menu = buildContextStrategyMenu();
        return <Menu title="Context Strategy" items={menu.items} footer={menu.footer} onSelect={handleContextStrategySelect} onCancel={popMenu} nested />;
      }
      case "subagent": {
        const { items, footer } = buildSubagentMenu();
        return <Menu title="Agents" items={items} onSelect={handleSubagentSelect} onCancel={popMenu} footer={footer} />;
      }
      case "revert": {
        const { items, footer } = buildRevertMenu();
        return <Menu title="Revert" items={items} onSelect={handleRevertSelect} onCancel={popMenu} footer={footer} />;
      }
      case "revert_confirm": {
        const { items, footer } = buildRevertConfirmMenu();
        return <Menu title="Confirm revert" items={items} onSelect={handleRevertConfirmSelect} onCancel={cancelRevertConfirm} footer={footer} nested />;
      }
      case "sessions": {
        const { items, footer } = buildSessionsMenu();
        const hasItems = sessionMetasRef.current.length > 0;
        return (
          <Menu
            title="Resume session"
            items={items}
            onSelect={handleSessionSelect}
            onCancel={popMenu}
            onAction={hasItems ? handleSessionDeleteKey : undefined}
            footer={footer}
            actionHint={hasItems ? "D delete" : undefined}
          />
        );
      }
      case "session_delete_confirm": {
        const { meta } = activeMenu;
        const title = meta.title.length > 50 ? meta.title.slice(0, 47) + "..." : meta.title;
        return (
          <Menu
            title="Delete session?"
            items={[
              { label: "Delete", description: `Permanently delete "${title}"` },
              { label: "Cancel", description: "Keep the session" },
            ]}
            onSelect={(idx) => handleSessionDeleteConfirm(idx, meta)}
            onCancel={popMenu}
            footer={["This cannot be undone."]}
            nested
          />
        );
      }
    }
  };

  if (copyMode) {
    return (
      <Box flexDirection="column">
      {(copySnapshot ?? []).map((entry) => (
          <Message key={entry.id} entry={entry} columns={terminalColumns} streamingMaxLines={streamingMaxLines} />
        ))}
        <Box marginTop={1}>
          <Text color="yellow">Copy mode</Text>
          <Text dimColor>{" · live updates paused in the TUI, agent still running · Ctrl+F to resume"}</Text>
        </Box>
      </Box>
    );
  }

  return (
    <Box flexDirection="column">
      {/* Archived scrollback is printed once and never participates in live redraws. */}
      <Static items={archivedScrollback}>
        {(entry) => <Message key={entry.id} entry={entry} columns={terminalColumns} streamingMaxLines={streamingMaxLines} />}
      </Static>

      {/* Recent scrollback remains live so menus, revert, and active UI stay coherent. */}
      {liveScrollback.map((entry) => (
        <Message key={entry.id} entry={entry} columns={terminalColumns} streamingMaxLines={streamingMaxLines} />
      ))}

      {/* Transient */}
      {state.transient.map((entry) => (
        <Message key={entry.id} entry={entry} columns={terminalColumns} streamingMaxLines={streamingMaxLines} />
      ))}

      {/* Thinking spinner */}
      {state.isThinking && !state.waitingForInput && !state.pendingApproval && !state.pendingPlan && (
        <Spinner label={state.activityLabel} />
      )}

      {/* Active subagents */}
      <SubagentStatus subagents={state.activeSubagents} />

      {/* Approval dialog */}
      {state.pendingApproval && (
        <ApprovalDialog
          approval={state.pendingApproval}
          onApprove={approveAction}
          onApproveAlways={approveAlways}
          onDeny={denyAction}
        />
      )}

      {/* Plan approval */}
      {state.pendingPlan && (
        <PlanApproval
          content={state.pendingPlan.content}
          onClearAndApprove={clearAndApprovePlan}
          onApprove={approvePlan}
          onReject={rejectPlan}
        />
      )}

      {/* Question dialog */}
      {state.pendingQuestion && (
        <QuestionDialog
          question={state.pendingQuestion}
          onAnswer={answerQuestion}
          onCancel={cancelRun}
        />
      )}

      {/* Background process prompt */}
      {state.pendingBgPrompt && (
        <BgPromptDialog
          command={state.pendingBgPrompt.command}
          prompt={state.pendingBgPrompt.prompt}
          onSubmit={(text) => sendBgProcessInput(state.pendingBgPrompt!.bg_id, text)}
        />
      )}

      {/* Menus */}
      {renderMenu()}

      {/* Input — always rendered to preserve typed text across re-renders */}
      <PromptInput
        onSubmit={state.waitingForInput ? sendProcessInput : handleSubmit}
        disabled={isInputDisabled}
        placeholder={state.waitingForInput ? (state.inputPromptText || "Input needed") : undefined}
        allowEmpty={state.waitingForInput}
        slashCommands={state.waitingForInput ? [] : SLASH_COMMANDS}
        hidden={!!(state.pendingApproval || state.pendingPlan || state.pendingQuestion || state.pendingBgPrompt || activeMenu)}
      />

      {/* Status bar */}
      <ContextBar
        modelName={state.modelName}
        reasoningLabel={thinkingIntensityDisplay(activeMainEndpoint)}
        usage={state.usage}
        planMode={state.planMode}
        isThinking={state.isThinking}
        permissionMode={state.permissionMode}
      />
    </Box>
  );
}
