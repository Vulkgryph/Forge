// SPDX-License-Identifier: Apache-2.0
import { spawn, type Subprocess } from "bun";
import { EventEmitter } from "events";
import { AgentMessageSchema, type AgentMessage, type UserMessage } from "./protocol.js";

export class AgentBridge extends EventEmitter {
  private proc: Subprocess;
  private buffer: string = "";
  private eventQueue: AgentMessage[] = [];
  private drainScheduled = false;
  private pendingAssistantToken = "";
  private tokenFlushTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(agentPath: string, args: string[] = [], cwd?: string) {
    super();

    this.proc = spawn([agentPath, "--headless", ...args], {
      stdin: "pipe",
      stdout: "pipe",
      stderr: "inherit",
      ...(cwd ? { cwd } : {}),
    });

    this.readLoop();
  }

  private async readLoop() {
    const stdout = this.proc.stdout;
    if (!stdout || typeof stdout === "number") return;

    const reader = (stdout as ReadableStream<Uint8Array>).getReader();
    const decoder = new TextDecoder();

    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;

        this.buffer += decoder.decode(value, { stream: true });
        this.processBuffer();
      }
    } catch (_err) {
      // Process ended
    }

    if (this.tokenFlushTimer) {
      clearTimeout(this.tokenFlushTimer);
      this.tokenFlushTimer = null;
    }
    this.flushAssistantToken();
    this.emit("exit", this.proc.exitCode);
  }

  private processBuffer() {
    let newlineIdx: number;
    while ((newlineIdx = this.buffer.indexOf("\n")) !== -1) {
      const line = this.buffer.slice(0, newlineIdx).trim();
      this.buffer = this.buffer.slice(newlineIdx + 1);
      if (line) this.enqueueLine(line);
    }
  }

  private enqueueLine(line: string) {
    try {
      const raw = JSON.parse(line);
      const parsed = AgentMessageSchema.parse(raw);
      this.enqueueMessage(parsed);
    } catch (err) {
      console.error("Failed to parse agent message:", err);
      console.error("Line was:", line.slice(0, 200));
    }
  }

  private enqueueMessage(msg: AgentMessage) {
    if (msg.type === "assistant_token") {
      this.pendingAssistantToken += msg.content;
      this.scheduleTokenFlush();
      return;
    }

    this.flushAssistantToken();
    this.eventQueue.push(msg);
    this.scheduleDrain();
  }

  private scheduleTokenFlush() {
    if (this.tokenFlushTimer) return;
    this.tokenFlushTimer = setTimeout(() => {
      this.tokenFlushTimer = null;
      this.flushAssistantToken();
    }, 75);
  }

  private flushAssistantToken() {
    if (!this.pendingAssistantToken) return;
    const content = this.pendingAssistantToken;
    this.pendingAssistantToken = "";
    this.eventQueue.push({ type: "assistant_token", content });
    this.scheduleDrain();
  }

  private scheduleDrain() {
    if (this.drainScheduled) return;
    this.drainScheduled = true;
    setTimeout(() => this.drainQueue(), 0);
  }

  private drainQueue() {
    this.drainScheduled = false;

    const maxPerTick = 20;
    for (let i = 0; i < maxPerTick && this.eventQueue.length > 0; i++) {
      const msg = this.eventQueue.shift()!;
      this.emit("message", msg);
    }

    if (this.eventQueue.length > 0) {
      this.scheduleDrain();
    }
  }

  send(msg: UserMessage) {
    const writer = this.proc.stdin;
    if (!writer || typeof writer === "number") return;

    const json = JSON.stringify(msg) + "\n";
    (writer as import("bun").FileSink).write(json);
    (writer as import("bun").FileSink).flush();
  }

  kill() {
    if (this.tokenFlushTimer) {
      clearTimeout(this.tokenFlushTimer);
      this.tokenFlushTimer = null;
    }
    this.proc.kill();
  }
}

/**
 * Find the forge-agent binary.
 * Search order: FORGE_AGENT_PATH env → same directory as this script → PATH
 */
export function findAgentBinary(): string {
  const envPath = process.env.FORGE_AGENT_PATH;
  if (envPath) return envPath;

  // Same directory as the running script
  const scriptDir = import.meta.dir;
  // Release before debug — debug binary can be stale if only release was rebuilt
  const candidates = [
    `${scriptDir}/forge-agent`,
    `${scriptDir}/../target/release/forge-agent`,
    `${scriptDir}/../../target/release/forge-agent`,
    `${scriptDir}/../target/debug/forge-agent`,
    `${scriptDir}/../../target/debug/forge-agent`,
  ];

  for (const candidate of candidates) {
    try {
      const stat = Bun.file(candidate);
      // If we can access it, use the resolved path
      if (stat.size > 0) return candidate;
    } catch {
      continue;
    }
  }

  // Fall back to PATH
  return "forge-agent";
}
