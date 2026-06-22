// SPDX-License-Identifier: Apache-2.0
import { useState, useEffect, useCallback, useRef } from "react";
import { AgentBridge, findAgentBinary } from "../agent-bridge.js";
import type {
  AgentMessage,
  UserMessage,
  UsageSnapshot,
  AgentDefInfo,
  EndpointInfo,
  EndpointReasoningConfig,
  QuestionItem,
} from "../protocol.js";
import { activeEndpoint, collapseHome, thinkingIntensityDisplay } from "../model-display.js";
import { nextStartupTip } from "../startup-tips.js";

/**
 * Best-effort system clipboard copy. Returns true on success. Used so OAuth
 * URLs (which Ink wraps with hard newlines, breaking copy-paste) can be made
 * available cleanly to the user — they just hit Cmd+V / Ctrl+V in their
 * browser instead of trying to select the wrapped text from the TUI.
 */
async function copyToClipboard(text: string): Promise<boolean> {
  let cmd: string[];
  if (process.platform === "darwin") cmd = ["pbcopy"];
  else if (process.platform === "win32") cmd = ["clip"];
  else if (process.env["WAYLAND_DISPLAY"]) cmd = ["wl-copy"];
  else cmd = ["xclip", "-selection", "clipboard"];

  try {
    const proc = Bun.spawn(cmd, { stdin: "pipe", stdout: "ignore", stderr: "ignore" });
    const writer = proc.stdin;
    if (writer && typeof writer !== "number") {
      writer.write(text);
      writer.end();
    }
    const exitCode = await proc.exited;
    return exitCode === 0;
  } catch {
    return false;
  }
}

export interface ChatEntry {
  id: string;
  kind: "user" | "assistant" | "streaming" | "tool_call" | "tool_result" | "tool_output" | "system" | "error" | "plan_content" | "plan_status" | "subagent_header";
  content: string;
  success?: boolean;
  toolName?: string;
  toolArgs?: string;
  toolId?: string;
  toolKind?: "read" | "write" | "execute";
}

export interface PendingApproval {
  toolName: string;
  toolId: string;
  toolArgs: string;
  kind: "read" | "write" | "execute";
}

export interface PendingPlan {
  path: string;
  content: string;
}

export interface PendingQuestion {
  question: string;
  toolId: string;
  items: QuestionItem[];
}

export interface ActiveSubagent {
  id: string;
  agentType: string;
  prompt: string;
  currentTool: string;
  detail: string;
}

export interface PendingRewind {
  checkpointId: string;
  preview: string;
  summary: string;
}

export interface RewindCheckpointState {
  id: string;
  preview: string;
  message_count: number;
  displayIndex: number;
  keepOnRestore: boolean;
}

export type PermissionMode = "normal" | "auto_accept" | "plan";

export interface AgentState {
  connected: boolean;
  sessionId: string | null;
  projectRoot: string;
  modelName: string;
  modelId: string;
  maxContextTokens: number;
  logPath: string;
  dangerouslyAllowAll: boolean;
  agentDefs: AgentDefInfo[];
  endpoints: EndpointInfo[];
  isThinking: boolean;
  activityLabel: string;
  waitingForInput: boolean;
  inputPromptText: string;
  pendingBgPrompt: { bg_id: string; command: string; prompt: string } | null;
  permissionMode: PermissionMode;
  scrollback: ChatEntry[];
  transient: ChatEntry[];
  usage: UsageSnapshot | null;
  pendingApproval: PendingApproval | null;
  pendingPlan: PendingPlan | null;
  pendingQuestion: PendingQuestion | null;
  pendingRewind: PendingRewind | null;
  planMode: boolean;
  activeSubagents: Map<string, ActiveSubagent>;
  availableTools: import("../protocol.js").ToolInfo[];
  contextStrategy: string;
  chatgptLoggedIn: boolean;
  loginInProgress: boolean;
  streamingId: string | null;
  rewindCheckpoints: RewindCheckpointState[];
}

let entryCounter = 0;
function nextId(): string {
  return `e-${++entryCounter}`;
}

function startupEntries(
  modelName: string,
  modelId: string,
  thinking: string | null,
  dangerouslyAllowAll: boolean,
): ChatEntry[] {
  return [
    {
      id: nextId(),
      kind: "system",
      content: `\u25C6 ${modelName} (${modelId})${thinking ? ` · ${thinking}` : ""}`,
    },
    ...(dangerouslyAllowAll
      ? [{
          id: nextId(),
          // "error" kind to render red \u2014 this is a session-wide danger state,
          // not just an informational note. Message.tsx's error renderer adds
          // the \u2717 prefix and bold first line.
          kind: "error" as const,
          content: "--dangerously-allow-all enabled. All tools run without approval.",
        }]
      : []),
    {
      id: nextId(),
      kind: "system",
      content: nextStartupTip(),
    },
  ];
}

function removeLastMatchingUserEntry(entries: ChatEntry[], content?: string): ChatEntry[] {
  for (let i = entries.length - 1; i >= 0; i--) {
    const entry = entries[i];
    if (entry.kind === "user" && (!content || entry.content === content)) {
      return [...entries.slice(0, i), ...entries.slice(i + 1)];
    }
  }
  return entries;
}

function stripResumeArgs(args: string[]): string[] {
  const stripped: string[] = [];
  for (let i = 0; i < args.length; i++) {
    if (args[i] === "--resume-session") {
      i++;
      continue;
    }
    stripped.push(args[i]!);
  }
  return stripped;
}

interface UseAgentOptions {
  initialAgentArgs?: string[];
  initialCwd?: string;
}

export function useAgent(options: UseAgentOptions = {}) {
  const bridgeRef = useRef<AgentBridge | null>(null);
  const bridgeGenerationRef = useRef(0);
  const bridgeReadyRef = useRef(false);
  const baseAgentArgsRef = useRef(stripResumeArgs(options.initialAgentArgs ?? []));
  const approvedToolsRef = useRef<Set<string>>(new Set());
  const projectRootRef = useRef<string>("");
  const turnStartRef = useRef<number>(0);
  const toolCountRef = useRef<number>(0);
  const pendingUserTurnsRef = useRef<Array<{ content: string; displayIndex: number }>>([]);
  const pendingOutboundRef = useRef<UserMessage[]>([]);

  const [state, setState] = useState<AgentState>({
    connected: false,
    sessionId: null,
    projectRoot: "",
    modelName: "",
    modelId: "",
    maxContextTokens: 0,
    logPath: "",
    dangerouslyAllowAll: false,
    agentDefs: [],
    endpoints: [],
    isThinking: false,
    activityLabel: "Idle",
    waitingForInput: false,
    inputPromptText: "",
    pendingBgPrompt: null,
    permissionMode: "normal",
    scrollback: [],
    transient: [],
    usage: null,
    pendingApproval: null,
    pendingPlan: null,
    pendingQuestion: null,
    pendingRewind: null,
    planMode: false,
    activeSubagents: new Map(),
    availableTools: [],
    contextStrategy: "compaction",
    chatgptLoggedIn: false,
    loginInProgress: false,
    streamingId: null,
    rewindCheckpoints: [],
  });
  const stateRef = useRef(state);

  useEffect(() => {
    stateRef.current = state;
  }, [state]);

  const send = useCallback((msg: UserMessage) => {
    const bridge = bridgeRef.current;
    if (!bridge || !bridgeReadyRef.current) {
      pendingOutboundRef.current.push(msg);
      return;
    }
    bridge.send(msg);
  }, []);

  const sendUserMessage = useCallback((content: string) => {
    turnStartRef.current = Date.now();
    toolCountRef.current = 0;
    pendingUserTurnsRef.current.push({
      content,
      displayIndex: stateRef.current.scrollback.length + stateRef.current.transient.length,
    });
    setState((prev) => ({
      ...prev,
      isThinking: true,
      activityLabel: "Sending message",
      scrollback: [
        ...prev.scrollback,
        { id: nextId(), kind: "user", content },
      ],
    }));
    send({ type: "send_message", content });
  }, [send]);

  const queueUserMessage = useCallback((content: string) => {
    pendingUserTurnsRef.current.push({
      content,
      displayIndex: stateRef.current.scrollback.length + stateRef.current.transient.length,
    });
    setState((prev) => ({
      ...prev,
      isThinking: true,
      activityLabel: "Queued message",
      scrollback: [
        ...prev.scrollback,
        { id: nextId(), kind: "user", content },
      ],
    }));
    send({ type: "send_message", content });
  }, [send]);

  // Promote a transient entry to scrollback
  const promoteEntry = useCallback((entry: ChatEntry) => {
    setState((prev) => ({
      ...prev,
      scrollback: [...prev.scrollback, entry],
    }));
  }, []);

  // Add an entry to transient
  const addTransient = useCallback((entry: ChatEntry) => {
    setState((prev) => ({
      ...prev,
      transient: [...prev.transient, entry],
    }));
  }, []);

  // Move all transient to scrollback
  const flushTransient = useCallback(() => {
    setState((prev) => ({
      ...prev,
      scrollback: [...prev.scrollback, ...prev.transient],
      transient: [],
    }));
  }, []);

  const setupBridge = useCallback((args: string[] = [], cwd?: string) => {
    const generation = ++bridgeGenerationRef.current;

    // Kill existing bridge if any
    if (bridgeRef.current) {
      bridgeRef.current.kill();
      bridgeRef.current = null;
    }
    bridgeReadyRef.current = false;

    const agentPath = findAgentBinary();
    const bridge = new AgentBridge(agentPath, args, cwd);
    bridgeRef.current = bridge;
    setState((prev) => ({ ...prev, connected: false }));

    bridge.on("message", handleMessage);

    // Surface agent stderr (e.g. OAuth URL prints, model-detect warnings)
    // as system entries in scrollback. The bridge pipes stderr instead of
    // inheriting it so the raw text doesn't paint over Ink's rendering;
    // this hook decides what to do with each line. Cap line length to
    // prevent a single very long line from blowing up the layout.
    //
    // Special case: OAuth URLs. Ink's text wrapping inserts actual newline
    // characters into the rendered output, so a wrapped URL becomes broken
    // on copy-paste. We detect a bare URL line and copy it to the system
    // clipboard automatically, then emit a confirmation so the user knows
    // they can just paste straight into the browser without selecting.
    bridge.on("stderr", (line: string) => {
      if (generation !== bridgeGenerationRef.current) return;
      const trimmed = line.length > 2000 ? line.slice(0, 2000) + "..." : line;

      const urlMatch = trimmed.match(/^\s*(https?:\/\/\S+)\s*$/);
      if (urlMatch) {
        const url = urlMatch[1]!;
        // Fire-and-forget; the OS-level copy is best-effort. Appends a tip
        // entry after the URL itself when the copy succeeds.
        copyToClipboard(url).then((ok) => {
          if (!ok || generation !== bridgeGenerationRef.current) return;
          setState((prev) => ({
            ...prev,
            scrollback: [
              ...prev.scrollback,
              {
                id: nextId(),
                kind: "system",
                content: "Tip: URL copied to clipboard — paste it into your browser.",
              },
            ],
          }));
        }).catch(() => { /* no-op */ });
      }

      setState((prev) => ({
        ...prev,
        scrollback: [
          ...prev.scrollback,
          { id: nextId(), kind: "system", content: trimmed },
        ],
      }));
    });

    bridge.on("exit", () => {
      if (generation !== bridgeGenerationRef.current) return;
      bridgeReadyRef.current = false;
      setState((prev) => ({
        ...prev,
        connected: false,
        isThinking: false,
        activityLabel: "Idle",
        waitingForInput: false,
        pendingApproval: null,
        pendingPlan: null,
        pendingQuestion: null,
        scrollback: [
          ...prev.scrollback,
          ...prev.transient,
          { id: nextId(), kind: "system", content: "Agent process exited. Restart the model server, then run /restart to resume this session." },
        ],
        transient: [],
      }));
    });

    return bridge;
  }, []);

  const restartAgent = useCallback(() => {
    const sessionId = stateRef.current.sessionId;
    const cwd = projectRootRef.current || undefined;

    setState((prev) => ({
      ...prev,
      connected: false,
      isThinking: false,
      activityLabel: "Restarting",
      waitingForInput: false,
      pendingApproval: null,
      pendingPlan: null,
      pendingQuestion: null,
      scrollback: [
        ...prev.scrollback,
        ...prev.transient,
        {
          id: nextId(),
          kind: "system",
          content: sessionId ? `Restarting agent for session ${sessionId}...` : "Restarting agent...",
        },
      ],
      transient: [],
      activeSubagents: new Map(),
    }));

    approvedToolsRef.current.clear();
    if (sessionId) {
      setupBridge([...baseAgentArgsRef.current, "--resume-session", sessionId], cwd);
    } else {
      setupBridge([...baseAgentArgsRef.current], cwd);
    }
  }, [setupBridge]);

  const resumeSession = useCallback((sessionId: string) => {
    // Clear state and restart with the chosen session
    setState((prev) => ({
      ...prev,
      connected: false,
      sessionId,
      isThinking: false,
      activityLabel: "Resuming",
      scrollback: [
          ...prev.scrollback,
          { id: nextId(), kind: "system", content: `Resuming session ${sessionId}...` },
        ],
      transient: [],
      pendingApproval: null,
      pendingPlan: null,
      pendingQuestion: null,
      planMode: false,
      activeSubagents: new Map(),
      usage: null,
    }));
    approvedToolsRef.current.clear();
    // Pass project root as cwd so the agent can find the session
    const cwd = projectRootRef.current || undefined;
    setupBridge([...baseAgentArgsRef.current, "--resume-session", sessionId], cwd);
  }, [setupBridge]);

  // Message handler extracted so it can be reused by setupBridge
  const handleMessage = useCallback((msg: AgentMessage) => {
      switch (msg.type) {
        case "init":
          projectRootRef.current = msg.project_root;
          bridgeReadyRef.current = true;
          const initThinking = thinkingIntensityDisplay(activeEndpoint(
            msg.endpoints,
            msg.model_name,
            msg.model_id,
            msg.max_context_tokens
          ));
          setState((prev) => ({
            ...prev,
            connected: true,
            sessionId: msg.session_id ?? prev.sessionId,
            projectRoot: msg.project_root,
            modelName: msg.model_name,
            modelId: msg.model_id,
            maxContextTokens: msg.max_context_tokens,
            logPath: msg.log_path,
            dangerouslyAllowAll: msg.dangerously_allow_all,
            agentDefs: msg.agent_definitions,
            endpoints: msg.endpoints,
            availableTools: msg.available_tools ?? [],
            contextStrategy: msg.context_strategy ?? "compaction",
            chatgptLoggedIn: msg.chatgpt_logged_in ?? false,
            scrollback: [...prev.scrollback, ...startupEntries(
              msg.model_name,
              msg.model_id,
              initThinking,
              msg.dangerously_allow_all,
            )],
          }));
          if (bridgeRef.current) {
            const queued = pendingOutboundRef.current.splice(0);
            for (const pending of queued) {
              bridgeRef.current.send(pending);
            }
          }
          break;

        case "thinking":
          if (!stateRef.current.isThinking) {
            turnStartRef.current = Date.now();
            toolCountRef.current = 0;
          }
          setState((prev) => ({ ...prev, isThinking: true, activityLabel: "Waiting for model" }));
          break;

        case "reasoning":
          if (!stateRef.current.isThinking) {
            turnStartRef.current = Date.now();
            toolCountRef.current = 0;
          }
          setState((prev) => ({ ...prev, isThinking: true, activityLabel: "Thinking" }));
          break;

        case "rewind_checkpoint":
          setState((prev) => ({
            ...prev,
            rewindCheckpoints: [
              ...prev.rewindCheckpoints,
              {
                id: msg.id,
                preview: msg.preview,
                message_count: msg.message_count,
                keepOnRestore: msg.keep_on_restore,
                displayIndex: pendingUserTurnsRef.current.shift()?.displayIndex ?? (prev.scrollback.length + prev.transient.length),
              },
            ],
          }));
          break;

        case "rewind_preview":
          setState((prev) => ({
            ...prev,
            pendingRewind: {
              checkpointId: msg.checkpoint_id,
              preview: msg.preview,
              summary: msg.summary,
            },
          }));
          break;

        case "assistant_message": {
          const entry = {
            id: nextId(),
            kind: "assistant" as const,
            content: msg.content,
          };

          setState((prev) => ({
            ...prev,
            isThinking: false,
            activityLabel: "Idle",
            scrollback: [...prev.scrollback, ...prev.transient, entry],
            transient: [],
          }));
          break;
        }

        case "assistant_token": {
          // Streaming token — append to the live transient entry (or create it).
          // Because this is in `transient` (not `<Static>`), Ink re-renders it
          // in-place on every token, with no stdout buffer overflow risk.
          setState((prev) => {
            if (prev.streamingId) {
              // Append to the existing streaming entry
              return {
                ...prev,
                isThinking: true,
                activityLabel: "Writing response",
                transient: prev.transient.map((e) =>
                  e.id === prev.streamingId
                    ? { ...e, content: e.content + msg.content }
                    : e
                ),
              };
            } else {
              // First token — create the live streaming entry
              const id = nextId();
              return {
                ...prev,
                isThinking: true,
                activityLabel: "Writing response",
                streamingId: id,
                transient: [
                  ...prev.transient,
                  { id, kind: "streaming" as const, content: msg.content },
                ],
              };
            }
          });
          break;
        }

        case "assistant_done": {
          setState((prev) => {
            const authoritative = msg.content;
            if (!prev.streamingId) {
              if (!authoritative) return prev;
              return {
                ...prev,
                streamingId: null,
                activityLabel: "Waiting for model",
                scrollback: [
                  ...prev.scrollback,
                  { id: nextId(), kind: "assistant" as const, content: authoritative },
                ],
              };
            }
            const entry = prev.transient.find((e) => e.id === prev.streamingId);
            if (!entry) return { ...prev, streamingId: null };
            const committed: ChatEntry = {
              id: nextId(),
              kind: "assistant" as const,
              content: authoritative || entry.content,
            };
            return {
              ...prev,
              streamingId: null,
              activityLabel: "Waiting for model",
              scrollback: [...prev.scrollback, committed],
              transient: prev.transient.filter((e) => e.id !== prev.streamingId),
            };
          });
          break;
        }

        case "session_cleared": {
          pendingUserTurnsRef.current = [];
          toolCountRef.current = 0;
          approvedToolsRef.current.clear();
          const clearThinking = thinkingIntensityDisplay(activeEndpoint(
            stateRef.current.endpoints,
            stateRef.current.modelName,
            stateRef.current.modelId,
            stateRef.current.maxContextTokens,
          ));
          setState((prev) => ({
            ...prev,
            sessionId: msg.session_id,
            logPath: msg.log_path,
            isThinking: false,
            activityLabel: "Idle",
            waitingForInput: false,
            inputPromptText: "",
            pendingBgPrompt: null,
            pendingApproval: null,
            pendingPlan: null,
            pendingQuestion: null,
            pendingRewind: null,
            streamingId: null,
            transient: [],
            scrollback: startupEntries(
              prev.modelName,
              prev.modelId,
              clearThinking,
              prev.dangerouslyAllowAll,
            ),
            activeSubagents: new Map(),
            rewindCheckpoints: [],
            planMode: false,
            usage: null,
            loginInProgress: false,
          }));
          break;
        }

        case "tool_request": {
          if (msg.tool_name === "ask_question") {
            break;
          }

          const entry: ChatEntry = {
            id: nextId(),
            kind: "tool_call",
            content: formatToolLabel(msg.tool_name, msg.tool_args),
            toolName: msg.tool_name,
            toolArgs: msg.tool_args,
            toolId: msg.tool_id,
            toolKind: msg.kind,
          };

          // Determine auto-approval synchronously so we can send immediately
          const mode = stateRef.current.permissionMode;
          const shouldDeny = mode === "plan" && msg.kind !== "read";
          const shouldAutoApprove =
            msg.kind === "read" ||
            stateRef.current.dangerouslyAllowAll ||
            (mode === "auto_accept" && msg.kind === "write") ||
            (approvedToolsRef.current.has(msg.tool_name) && !isDangerousCommand(msg.tool_name, msg.tool_args)) ||
            (msg.tool_name === "shell_exec" && isSafeCommand(msg.tool_args));

          setState((prev) => {
            const initialActivity = shouldAutoApprove
              ? `Running ${msg.tool_name}`
              : `Planning ${msg.tool_name}`;
            const newState = {
              ...prev,
              isThinking: true,
              activityLabel: initialActivity,
              transient: [...prev.transient, entry],
            };

            // Plan mode: only reads are allowed, deny everything else
            if (shouldDeny) {
              return newState;
            }

            if (shouldAutoApprove) {
              return newState;
            }

            // Needs manual approval
            if (msg.kind === "write" || msg.kind === "execute") {
              return {
                ...newState,
                isThinking: false,
                activityLabel: "Waiting for approval",
                pendingApproval: {
                  toolName: msg.tool_name,
                  toolId: msg.tool_id,
                  toolArgs: msg.tool_args,
                  kind: msg.kind,
                },
              };
            }

            return newState;
          });

          if (shouldDeny) {
            bridgeRef.current?.send({ type: "deny_action", reason: "Blocked by plan mode" });
          } else if (shouldAutoApprove) {
            bridgeRef.current?.send({ type: "approve_action", tool_id: "" });
          }
          break;
        }

        case "tool_result":
          if (msg.tool_name === "ask_question") {
            break;
          }

          toolCountRef.current++;
          setState((prev) => {
            const entry: ChatEntry = {
              id: nextId(),
              kind: "tool_result",
              content: msg.result.length > 500 ? msg.result.slice(0, 500) + "..." : msg.result,
              success: msg.success,
              toolName: msg.tool_name,
            };
            // Flush tool_call entries to scrollback with this result.
            // Keep streaming entries in transient so assistant_done can commit them properly.
            const toFlush = prev.transient.filter((e) => e.kind === "tool_call");
            const remaining = prev.transient.filter((e) => e.kind !== "tool_call" && e.kind !== "tool_output");
            return {
              ...prev,
              activityLabel: "Waiting for model",
              waitingForInput: false,
              scrollback: [...prev.scrollback, ...toFlush, entry],
              transient: remaining,
            };
          });
          break;

        case "tool_output":
          // Streaming output line from a running command
          setState((prev) => {
            // Find existing streaming output entry or create one
            const existingIdx = prev.transient.findIndex(
              (e) => e.kind === "tool_output" && e.toolName === msg.tool_name
            );
            if (existingIdx >= 0) {
              // Append to existing entry (keep last ~20 lines)
              const existing = prev.transient[existingIdx]!;
              const lines = (existing.content + msg.content).split("\n");
              const trimmed = lines.length > 20
                ? lines.slice(-20).join("\n")
                : lines.join("\n");
              const updated = [...prev.transient];
              updated[existingIdx] = { ...existing, content: trimmed };
              return { ...prev, isThinking: true, activityLabel: `Running ${msg.tool_name}`, transient: updated };
            } else {
              // Create new streaming output entry
              return {
                ...prev,
                isThinking: true,
                activityLabel: `Running ${msg.tool_name}`,
                transient: [
                  ...prev.transient,
                  {
                    id: nextId(),
                    kind: "tool_output" as ChatEntry["kind"],
                    content: msg.content,
                    toolName: msg.tool_name,
                  },
                ],
              };
            }
          });
          break;

        case "process_input_needed":
          setState((prev) => ({
            ...prev,
            waitingForInput: true,
            isThinking: false,
            activityLabel: "Waiting for input",
            inputPromptText: msg.prompt || "Input needed",
            scrollback: [...prev.scrollback, ...prev.transient],
            transient: [],
          }));
          break;

        case "background_prompt_needed":
          setState((prev) => ({
            ...prev,
            isThinking: false,
            activityLabel: "Waiting for input",
            pendingBgPrompt: { bg_id: msg.bg_id, command: msg.command, prompt: msg.prompt },
            scrollback: [
              ...prev.scrollback,
              ...prev.transient,
              { id: nextId(), kind: "system" as const, content: `Background command '${msg.command}' needs input` },
            ],
            transient: [],
          }));
          break;

        case "error":
          setState((prev) => ({
            ...prev,
            isThinking: false,
            activityLabel: "Idle",
            scrollback: [
              ...prev.scrollback,
              ...prev.transient,
              { id: nextId(), kind: "error", content: msg.message },
            ],
            transient: [],
          }));
          break;

        case "api_retry":
          setState((prev) => ({
            ...prev,
            isThinking: true,
            activityLabel: `Network retry ${msg.attempt}/${msg.max_attempts} in ${msg.delay_secs}s`,
          }));
          break;

        case "turn_discarded": {
          const discarded = pendingUserTurnsRef.current.pop();
          setState((prev) => ({
            ...prev,
            scrollback: removeLastMatchingUserEntry(prev.scrollback, discarded?.content),
            transient: [],
          }));
          break;
        }

        case "done": {
          const summary = formatTurnSummary(turnStartRef.current, toolCountRef.current);
          setState((prev) => {
            // Drop orphaned tool_calls (no matching result arrived) — they have
            // nothing useful to show and shouldn't appear after the response.
            const streaming = prev.streamingId
              ? prev.transient.find((e) => e.id === prev.streamingId)
              : undefined;
            const committedStreaming = streaming
              ? [{ id: nextId(), kind: "assistant" as const, content: streaming.content }]
              : [];
            const toCommit = prev.transient.filter(
              (e) => e.kind !== "tool_call" && e.id !== prev.streamingId
            );
            return {
              ...prev,
              isThinking: false,
              activityLabel: "Idle",
              waitingForInput: false,
              pendingApproval: null,
              pendingRewind: null,
              streamingId: null,
              scrollback: [
                ...prev.scrollback,
                ...committedStreaming,
                ...toCommit,
                ...(summary ? [{ id: nextId(), kind: "system" as const, content: summary }] : []),
              ],
              transient: [],
            };
          });
          break;
        }

        case "cancelled": {
          const summary = formatTurnSummary(turnStartRef.current, toolCountRef.current, true);
          setState((prev) => ({
            ...prev,
            isThinking: false,
            activityLabel: "Idle",
            waitingForInput: false,
            pendingApproval: null,
            scrollback: [
              ...prev.scrollback,
              ...prev.transient,
              { id: nextId(), kind: "system" as const, content: summary || "Cancelled." },
            ],
            transient: [],
          }));
          break;
        }

        case "usage":
          setState((prev) => {
            const u = msg.snapshot;
            const saturation = u.max_context_tokens > 0
              ? ((u.last_prompt_tokens / u.max_context_tokens) * 100).toFixed(1)
              : "0.0";
            const totalTokens = u.total_prompt_tokens + u.total_completion_tokens;
            return {
              ...prev,
              usage: u,
              scrollback: [
                ...prev.scrollback,
                {
                  id: nextId(),
                  kind: "system",
                  content: `Context: ${saturation}% (${u.last_prompt_tokens}/${u.max_context_tokens}) | Session: ${totalTokens} tokens (${u.total_requests} requests) | ${u.history_messages} messages`,
                },
              ],
            };
          });
          break;

        case "usage_update":
          setState((prev) => ({ ...prev, usage: msg.snapshot }));
          break;

        case "model_switched":
          setState((prev) => ({
            ...prev,
            modelName: msg.name,
            modelId: msg.model_id,
            maxContextTokens: msg.max_context_tokens,
            scrollback: [
              ...prev.scrollback,
              {
                id: nextId(),
                kind: "system",
                content: `Switched to ${msg.name} (${msg.model_id}), context: ${Math.floor(msg.max_context_tokens / 1000)}k tokens`,
              },
            ],
          }));
          break;

        case "subagent_started":
          setState((prev) => {
            const next = new Map(prev.activeSubagents);
            next.set(msg.id, {
              id: msg.id,
              agentType: msg.agent_type,
              prompt: msg.prompt,
              currentTool: "",
              detail: "starting...",
            });
            return {
              ...prev,
              activeSubagents: next,
              scrollback: [
                ...prev.scrollback,
                {
                  id: nextId(),
                  kind: "subagent_header",
                  content: `[${msg.agent_type}] ${msg.prompt.slice(0, 80)}${msg.prompt.length > 80 ? "..." : ""}`,
                },
              ],
            };
          });
          break;

        case "subagent_status":
          setState((prev) => {
            const next = new Map(prev.activeSubagents);
            const existing = next.get(msg.id);
            if (existing) {
              next.set(msg.id, {
                ...existing,
                currentTool: msg.tool_name,
                detail: msg.detail,
              });
            }
            return { ...prev, activeSubagents: next };
          });
          break;

        case "subagent_finished":
          setState((prev) => {
            const next = new Map(prev.activeSubagents);
            next.delete(msg.id);
            return { ...prev, activeSubagents: next };
          });
          break;

        case "question_request":
          setState((prev) => ({
            ...prev,
            isThinking: false,
            activityLabel: "Waiting for answer",
            pendingQuestion: {
              question: msg.question,
              toolId: msg.tool_id,
              items: msg.items,
            },
          }));
          break;

        case "endpoints_updated":
          setState((prev) => {
            const merged = new Map(prev.endpoints.map((e) => [`${e.endpoint_type}:${e.name}:${e.model_id}`, e]));
            for (const endpoint of msg.endpoints) {
              merged.set(`${endpoint.endpoint_type}:${endpoint.name}:${endpoint.model_id}`, endpoint);
            }
            return { ...prev, endpoints: Array.from(merged.values()) };
          });
          break;

        case "login_status":
          setState((prev) => ({
            ...prev,
            loginInProgress: true,
            scrollback: [...prev.scrollback, { id: nextId(), kind: "system", content: msg.message }],
          }));
          break;

        case "login_complete":
          setState((prev) => ({
            ...prev,
            loginInProgress: false,
            chatgptLoggedIn: msg.success && msg.message.includes("ChatGPT") ? true : prev.chatgptLoggedIn,
            // Failure is an error, not a neutral system note — render with the
            // error styling so the user actually notices it.
            scrollback: [
              ...prev.scrollback,
              {
                id: nextId(),
                kind: msg.success ? "system" : "error",
                content: msg.message,
              },
            ],
          }));
          break;

        case "plan_mode_entered":
          setState((prev) => ({
            ...prev,
            planMode: true,
            scrollback: [
              ...prev.scrollback,
              { id: nextId(), kind: "plan_status", content: `Plan file: ${collapseHome(msg.plan_path)}` },
            ],
          }));
          break;

        case "plan_ready":
          setState((prev) => ({
            ...prev,
            pendingPlan: { path: msg.plan_path, content: msg.content },
            // Plan content is shown in the PlanApproval dialog — don't duplicate in scrollback.
          }));
          break;

        case "plan_mode_exited":
          setState((prev) => ({
            ...prev,
            planMode: false,
            pendingPlan: null,
            scrollback: msg.reason === "discuss"
              ? prev.scrollback  // agent will ask the user — no status banner needed
              : [
                  ...prev.scrollback,
                  { id: nextId(), kind: "plan_status" as const, content: "Plan approved. Implementing..." },
                ],
          }));
          break;

        case "session_loaded": {
          // Replay conversation history from a resumed session
          const replayEntries: ChatEntry[] = msg.entries.map((e) => ({
            id: nextId(),
            kind: e.kind as ChatEntry["kind"],
            content: e.content,
            toolName: e.tool_name,
            success: e.success,
          }));
          const separator: ChatEntry = {
            id: nextId(),
            kind: "system",
            content: `── Resumed: "${msg.title}" (${msg.message_count} msgs, ${msg.compaction_count} compactions) ──`,
          };
          setState((prev) => ({
            ...prev,
            sessionId: msg.session_id,
            rewindCheckpoints: msg.rewind_checkpoints.map((checkpoint) => ({
              id: checkpoint.id,
              preview: checkpoint.preview,
              message_count: checkpoint.message_count,
              displayIndex: checkpoint.display_index + prev.scrollback.length,
              keepOnRestore: checkpoint.keep_on_restore,
            })),
            scrollback: [...prev.scrollback, ...replayEntries, separator],
          }));
          break;
        }
      }
  }, []);

  useEffect(() => {
    const bridge = setupBridge(options.initialAgentArgs ?? [], options.initialCwd);
    return () => {
      bridge.kill();
    };
  }, []);

  const approveAction = useCallback((toolId: string) => {
    send({ type: "approve_action", tool_id: toolId });
    setState((prev) => ({ ...prev, pendingApproval: null }));
  }, [send]);

  const approveAlways = useCallback((toolName: string, toolId: string) => {
    approvedToolsRef.current.add(toolName);
    send({ type: "approve_action", tool_id: toolId });
    setState((prev) => ({
      ...prev,
      pendingApproval: null,
      scrollback: [
        ...prev.scrollback,
        { id: nextId(), kind: "system", content: `Auto-approving ${toolName} for this session` },
      ],
    }));
  }, [send]);

  const denyAction = useCallback((toolId: string) => {
    send({ type: "deny_action", reason: "User denied" });
    setState((prev) => ({ ...prev, pendingApproval: null }));
  }, [send]);

  const sendMessage = sendUserMessage;

  const answerQuestion = useCallback((answer: string) => {
    send({ type: "answer_question", answer });
    setState((prev) => ({
      ...prev,
      pendingQuestion: null,
      activityLabel: "Sending answer",
      scrollback: [
        ...prev.scrollback,
        { id: nextId(), kind: "user", content: answer },
      ],
    }));
  }, [send]);

  const switchModel = useCallback((ep: EndpointInfo) => {
    send({
      type: "switch_model",
      name: ep.name,
      base_url: ep.base_url,
      model_id: ep.model_id,
      max_context_tokens: ep.max_context_tokens,
      max_output_tokens: ep.max_output_tokens,
      endpoint_type: ep.endpoint_type,
      reasoning: ep.reasoning,
    });
    setState((prev) => ({
      ...prev,
      scrollback: [
        ...prev.scrollback,
        { id: nextId(), kind: "system", content: `Switching to ${ep.name} (${ep.model_id})` },
      ],
    }));
  }, [send]);

  const updateEndpointReasoning = useCallback((endpointName: string, reasoning: EndpointReasoningConfig) => {
    send({
      type: "update_endpoint_reasoning",
      endpoint_name: endpointName,
      reasoning,
    });
    setState((prev) => ({
      ...prev,
      endpoints: prev.endpoints.map((ep) =>
        ep.name === endpointName ? { ...ep, reasoning } : ep
      ),
    }));
  }, [send]);

  const compact = useCallback(() => {
    send({ type: "compact" });
    setState((prev) => ({
      ...prev,
      scrollback: [
        ...prev.scrollback,
        { id: nextId(), kind: "system", content: "Requesting compaction..." },
      ],
    }));
  }, [send]);

  const clearSession = useCallback(() => {
    send({ type: "clear_session" });
    setState((prev) => ({
      ...prev,
      isThinking: true,
      activityLabel: "Clearing session",
      pendingApproval: null,
      pendingPlan: null,
      pendingQuestion: null,
    }));
  }, [send]);

  const rewind = useCallback((checkpointId?: string) => {
    send(checkpointId ? { type: "revert", checkpoint_id: checkpointId } : { type: "revert" });
    pendingUserTurnsRef.current = [];
    setState((prev) => {
      const checkpointIndex = checkpointId
        ? prev.rewindCheckpoints.findIndex((cp) => cp.id === checkpointId)
        : prev.rewindCheckpoints.length - 1;
      const checkpoint = checkpointIndex >= 0 ? prev.rewindCheckpoints[checkpointIndex] : undefined;
      const baseScrollback = checkpoint
        ? prev.scrollback.slice(0, checkpoint.displayIndex)
        : prev.scrollback;
      return {
        ...prev,
        pendingRewind: null,
        transient: [],
        rewindCheckpoints: checkpointIndex >= 0
          ? prev.rewindCheckpoints.slice(0, checkpoint?.keepOnRestore ? checkpointIndex + 1 : checkpointIndex)
          : prev.rewindCheckpoints,
        scrollback: [
          ...baseScrollback,
          { id: nextId(), kind: "system", content: "Reverting to the selected user turn..." },
        ],
      };
    });
  }, [send]);

  const previewRewind = useCallback((checkpointId: string) => {
    send({ type: "revert_preview", checkpoint_id: checkpointId });
    setState((prev) => ({
      ...prev,
      pendingRewind: {
        checkpointId,
        preview: "",
        summary: "Loading revert preview...",
      },
    }));
  }, [send]);

  const clearRewindPreview = useCallback(() => {
    setState((prev) => ({ ...prev, pendingRewind: null }));
  }, []);

  const requestUsage = useCallback(() => {
    send({ type: "request_usage" });
  }, [send]);

  const enterPlanMode = useCallback(() => {
    send({ type: "enter_plan_mode" });
  }, [send]);

  const approvePlan = useCallback(() => {
    approvedToolsRef.current.add("apply_patch");
    approvedToolsRef.current.add("write_file");
    approvedToolsRef.current.add("edit_file");
    send({ type: "approve_plan" });
    setState((prev) => ({
      ...prev,
      pendingPlan: null,
      activityLabel: "Implementing plan",
      scrollback: [
        ...prev.scrollback,
        { id: nextId(), kind: "plan_status", content: "Plan approved (auto-approve edits)" },
      ],
    }));
  }, [send]);

  const clearAndApprovePlan = useCallback(() => {
    approvedToolsRef.current.add("apply_patch");
    approvedToolsRef.current.add("write_file");
    approvedToolsRef.current.add("edit_file");
    send({ type: "clear_and_approve_plan" });
    setState((prev) => ({
      ...prev,
      pendingPlan: null,
      activityLabel: "Implementing plan",
      scrollback: [
        ...prev.scrollback,
        { id: nextId(), kind: "plan_status", content: "Plan approved (context cleared, auto-approve edits)" },
      ],
    }));
  }, [send]);

  const rejectPlan = useCallback((feedback: string) => {
    send({ type: "reject_plan", feedback });
    setState((prev) => ({
      ...prev,
      pendingPlan: null,
      activityLabel: "Revising plan",
      scrollback: [
        ...prev.scrollback,
        { id: nextId(), kind: "plan_status", content: feedback ? "Plan sent back for revision" : "Plan revision requested" },
      ],
    }));
  }, [send]);

  const addSystemEntry = useCallback((content: string) => {
    setState((prev) => ({
      ...prev,
      scrollback: [
        ...prev.scrollback,
        { id: nextId(), kind: "system", content },
      ],
    }));
  }, []);

  const cyclePermissionMode = useCallback(() => {
    setState((prev) => {
      const next: PermissionMode =
        prev.permissionMode === "normal" ? "auto_accept"
        : prev.permissionMode === "auto_accept" ? "plan"
        : "normal";
      const labels: Record<PermissionMode, string> = {
        normal: "Normal mode",
        auto_accept: "Auto-accept edits",
        plan: "Plan mode (read-only)",
      };
      return {
        ...prev,
        permissionMode: next,
        scrollback: [
          ...prev.scrollback,
          { id: nextId(), kind: "system" as const, content: labels[next] },
        ],
      };
    });
  }, []);

  const sendProcessInput = useCallback((content: string) => {
    send({ type: "process_input", content });
    setState((prev) => ({
      ...prev,
      waitingForInput: false,
      inputPromptText: "",
    }));
  }, [send]);

  const sendBgProcessInput = useCallback((bg_id: string, content: string) => {
    send({ type: "bg_process_input", bg_id, content });
    setState((prev) => ({ ...prev, pendingBgPrompt: null }));
  }, [send]);

  const cancelRun = useCallback(() => {
    send({ type: "cancel_run" });
  }, [send]);

  const quit = useCallback(() => {
    const bridge = bridgeRef.current;
    send({ type: "quit" });
    setTimeout(() => {
      if (bridgeRef.current === bridge) {
        bridgeRef.current?.kill();
        bridgeRef.current = null;
      }
    }, 100);
  }, [send]);

  return {
    state,
    send,
    sendMessage,
    queueUserMessage,
    switchModel,
    approveAction,
    approveAlways,
    denyAction,
    answerQuestion,
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
    cyclePermissionMode,
    setContextStrategy: (strategy: string) => setState((prev) => ({ ...prev, contextStrategy: strategy })),
    setToolEnabled: (tool: string, enabled: boolean) => setState((prev) => ({
      ...prev,
      availableTools: prev.availableTools.map((t) => t.name === tool ? { ...t, enabled } : t),
    })),
    updateEndpointReasoning,
    setPermissionMode: (mode: PermissionMode) => setState((prev) => ({ ...prev, permissionMode: mode })),
    sendProcessInput,
    sendBgProcessInput,
    cancelRun,
    quit,
    restartAgent,
    resumeSession,
  };
}

// ── Helpers ───────────────────────────────────────────────────────────

function formatTurnSummary(startMs: number, toolCount: number, cancelled = false): string | null {
  if (!startMs) return cancelled ? "Cancelled." : null;
  const elapsed = Date.now() - startMs;
  const secs = Math.floor(elapsed / 1000);
  const mins = Math.floor(secs / 60);
  const remSecs = secs % 60;
  const timeStr = mins > 0 ? `${mins}m ${remSecs}s` : `${secs}s`;
  const toolStr = toolCount > 0 ? `, ${toolCount} tool use${toolCount !== 1 ? "s" : ""}` : "";
  const prefix = cancelled ? "Cancelled" : "Worked";
  return `✦ ${prefix} for ${timeStr}${toolStr}`;
}

function formatToolLabel(toolName: string, argsJson: string): string {
  let obj: Record<string, unknown> = {};
  try {
    obj = JSON.parse(argsJson);
  } catch {
    return `${toolName}(...)`;
  }

  const getStr = (key: string): string | undefined => {
    const v = obj[key];
    return typeof v === "string" ? v : undefined;
  };

  const truncate = (s: string, max: number): string =>
    s.length > max ? s.slice(0, max) + "..." : s;

  switch (toolName) {
    case "apply_patch":
    case "edit_file": {
      const path = getStr("path") ?? "file";
      return `Update(${path})`;
    }
    case "write_file":
      return `Write(${getStr("path") ?? "file"})`;
    case "read_file":
      return `Read(${getStr("path") ?? "file"})`;
    case "search_code":
      return `Search(${truncate(getStr("query") ?? "...", 60)})`;
    case "shell_exec": {
      const cmd = getStr("command") ?? "...";
      const firstLine = cmd.split("\n")[0];
      return `Bash(${truncate(firstLine, 80)})`;
    }
    case "list_directory":
      return `List(${getStr("path") ?? "."})`;
    case "delegate_task": {
      const agentType = getStr("agent_type") ?? "agent";
      const prompt = getStr("prompt") ?? "...";
      return `Task(${agentType}: ${truncate(prompt, 60)})`;
    }
    case "glob_files":
      return `Glob(${getStr("pattern") ?? "*"})`;
    default: {
      const firstVal = Object.values(obj).find((v) => typeof v === "string") as string | undefined;
      return `${toolName}(${truncate(firstVal ?? "...", 80)})`;
    }
  }
}

/** Detect read-only / harmless bash commands that can auto-approve. */
function isSafeCommand(toolArgs: string): boolean {
  let cmd: string;
  try {
    cmd = (JSON.parse(toolArgs).command ?? "").trim();
  } catch {
    return false;
  }

  // Reject anything with pipes, redirections, subshells, or chaining
  if (/[|><`]|\$\(|&&|\|\||;/.test(cmd)) return false;

  // Parse out leading env-var assignments (FOO=bar cmd ...)
  const tokens = cmd.split(/\s+/);
  let i = 0;
  while (i < tokens.length && /^[A-Za-z_]\w*=/.test(tokens[i]!)) i++;
  const base = tokens[i] ?? "";
  const arg1 = tokens[i + 1] ?? "";

  const safeCommands = new Set([
    // filesystem reads
    "ls", "cat", "head", "tail", "wc", "sort", "uniq", "diff", "file", "stat",
    "tree", "du", "df", "readlink", "realpath", "basename", "dirname",
    // search
    "grep", "egrep", "fgrep", "rg", "find", "fd", "ag", "ack",
    // info
    "pwd", "echo", "printf", "which", "type", "whoami", "hostname",
    "id", "date", "uname", "env", "printenv", "locale",
    // no-ops
    "true", "false", "test", "[",
  ]);

  if (safeCommands.has(base)) return true;

  // git read-only subcommands
  if (base === "git") {
    const safeGit = new Set([
      "status", "log", "diff", "show", "branch", "tag", "remote",
      "rev-parse", "ls-files", "ls-tree", "describe", "shortlog",
      "blame", "reflog", "config", "stash",
    ]);
    // "git stash list" is safe; "git stash pop" is not
    if (arg1 === "stash") {
      const arg2 = tokens[i + 2] ?? "list";
      return arg2 === "list" || arg2 === "show";
    }
    return safeGit.has(arg1);
  }

  // cargo read-only
  if (base === "cargo") {
    const safeCargo = new Set(["check", "clippy", "test", "bench", "doc", "metadata", "tree", "version"]);
    return safeCargo.has(arg1);
  }

  // npm/bun/yarn read-only
  if (base === "npm" || base === "bun" || base === "yarn" || base === "pnpm") {
    const safePkg = new Set(["test", "run", "list", "ls", "info", "view", "outdated", "audit", "why"]);
    return safePkg.has(arg1);
  }

  // python/node version checks
  if (base === "python" || base === "python3" || base === "node" || base === "rustc" || base === "go") {
    if (arg1 === "--version" || arg1 === "-V" || arg1 === "version") return true;
  }

  // multipass read-only
  if (base === "multipass") {
    const safeMultipass = new Set(["list", "info", "version", "get", "find"]);
    return safeMultipass.has(arg1);
  }

  return false;
}

function isDangerousCommand(toolName: string, toolArgs: string): boolean {
  if (toolName !== "shell_exec") return false;
  try {
    const obj = JSON.parse(toolArgs);
    const cmd = (obj.command ?? "").trim();
    const dangerousPrefixes = [
      "sudo ", "sudo\t", "rm -rf /", "rm -rf ~",
      "chmod 777", "mkfs", "dd if=", "> /dev/",
    ];
    const dangerousPatterns = ["| sudo ", "&& sudo ", "; sudo "];
    for (const p of dangerousPrefixes) {
      if (cmd.startsWith(p)) return true;
    }
    for (const p of dangerousPatterns) {
      if (cmd.includes(p)) return true;
    }
  } catch {
    // ignore
  }
  return false;
}
