// SPDX-License-Identifier: Apache-2.0
import React from "react";
import { Text, Box } from "ink";
import type { ChatEntry } from "../hooks/useAgent.js";
import { MarkdownRenderer } from "./MarkdownRenderer.js";

interface Props {
  entry: ChatEntry;
  columns?: number;
  streamingMaxLines?: number;
  /** When true, committed "thought" entries render their full reasoning text. */
  reasoningExpanded?: boolean;
}

function formatDuration(ms?: number): string {
  if (!ms || ms < 0) return "";
  const secs = Math.round(ms / 1000);
  if (secs < 60) return `${secs}s`;
  const mins = Math.floor(secs / 60);
  return `${mins}m ${secs % 60}s`;
}

function visualLineCount(line: string, columns: number): number {
  const width = Math.max(20, columns - 4);
  return Math.max(1, Math.ceil(line.length / width));
}

function streamingTail(content: string, columns: number, maxVisualLines: number): { text: string; hiddenLines: number } {
  const lines = content.split("\n");
  const selected: string[] = [];
  let used = 0;
  let hiddenLines = 0;

  for (let i = lines.length - 1; i >= 0; i--) {
    const line = lines[i] ?? "";
    const lineHeight = visualLineCount(line, columns);
    if (selected.length > 0 && used + lineHeight > maxVisualLines) {
      hiddenLines = i + 1;
      break;
    }
    selected.unshift(line);
    used += lineHeight;
  }

  return {
    text: selected.join("\n"),
    hiddenLines,
  };
}

interface StructuredLogLine {
  level: string;
  target: string;
  body: string;
  pc?: string;
}

function parseStructuredLogLine(line: string): StructuredLogLine | null {
  const match = line.match(/^\[[^\]]+\s+([A-Z]+)\]\s+([A-Za-z0-9_]+(?:::[A-Za-z0-9_]+)*):\s+(.+?)(?:\s+pc=(0x[0-9a-fA-F]+))?$/);
  if (!match) return null;
  return {
    level: match[1]!,
    target: match[2]!,
    body: match[3]!,
    pc: match[4],
  };
}

function compactToolOutput(content: string): string[] {
  const lines = content.split("\n");
  const output: string[] = [];
  let group: StructuredLogLine[] = [];
  let groupKey: string | null = null;

  const flushGroup = () => {
    if (group.length === 0) return;
    if (group.length < 3) {
      output.push(...group.map((line) => {
        const pc = line.pc ? ` pc=${line.pc}` : "";
        return `[${line.level}] ${line.target}: ${line.body}${pc}`;
      }));
    } else {
      const first = group[0]!;
      const pcs = group.map((line) => line.pc).filter(Boolean) as string[];
      const uniquePcs = Array.from(new Set(pcs));
      const sample = uniquePcs.slice(0, 8).join(", ");
      const more = uniquePcs.length > 8 ? ` (+${uniquePcs.length - 8} more)` : "";
      const pcSummary = sample ? ` pc=${sample}${more}` : "";
      output.push(`[${first.level}] ${first.target}: ${first.body} x${group.length}${pcSummary}`);
    }
    group = [];
    groupKey = null;
  };

  for (const line of lines) {
    const parsed = parseStructuredLogLine(line);
    if (!parsed) {
      flushGroup();
      output.push(line);
      continue;
    }

    const key = `${parsed.level}\0${parsed.target}\0${parsed.body}`;
    if (groupKey !== null && key !== groupKey) {
      flushGroup();
    }
    groupKey = key;
    group.push(parsed);
  }

  flushGroup();
  return output;
}

export const Message = React.memo(function Message({ entry, columns = 80, streamingMaxLines = 8, reasoningExpanded = false }: Props) {
  switch (entry.kind) {
    case "user":
      return (
        <Box marginTop={1}>
          <Text bold color="blue">
            {"❯ "}
          </Text>
          <Text>{entry.content}</Text>
        </Box>
      );

    case "assistant":
      return (
        <Box flexDirection="column" marginTop={1}>
          <MarkdownRenderer content={entry.content} />
        </Box>
      );

    case "reasoning": {
      // Live chain-of-thought. Compact status line by default (elapsed + an
      // approximate token count, which both advance as tokens stream), with the
      // full text revealed only when expanded — so it stays separate from and
      // far quieter than the assistant's answer.
      const secs = entry.startedAt ? Math.max(0, Math.round((Date.now() - entry.startedAt) / 1000)) : 0;
      const approxTokens = Math.max(1, Math.round(entry.content.length / 4));
      const stat = `${secs}s · ~${approxTokens} tokens`;
      if (!reasoningExpanded) {
        return (
          <Box marginTop={1}>
            <Text color="magenta">✻ </Text>
            <Text dimColor color="magenta">Thinking… ({stat})  </Text>
            <Text dimColor>(ctrl+t to expand)</Text>
          </Box>
        );
      }
      const tail = streamingTail(entry.content, columns, streamingMaxLines);
      return (
        <Box flexDirection="column" marginTop={1}>
          <Box>
            <Text color="magenta">✻ </Text>
            <Text dimColor color="magenta">Thinking… ({stat})  </Text>
            <Text dimColor>(ctrl+t to collapse)</Text>
          </Box>
          {tail.hiddenLines > 0 && <Text dimColor>  ↑ {tail.hiddenLines} lines above</Text>}
          <Text dimColor color="gray">{tail.text}</Text>
        </Box>
      );
    }

    case "thought": {
      // Committed reasoning: a collapsed one-liner by default, expandable with
      // the keyboard shortcut to reveal the full chain-of-thought.
      const dur = formatDuration(entry.durationMs);
      const header = dur ? `✻ Thought for ${dur}` : "✻ Thought";
      if (!reasoningExpanded) {
        return (
          <Box marginTop={1}>
            <Text dimColor color="magenta">{header}  </Text>
            <Text dimColor>(ctrl+t to expand)</Text>
          </Box>
        );
      }
      return (
        <Box flexDirection="column" marginTop={1}>
          <Box>
            <Text dimColor color="magenta">{header}  </Text>
            <Text dimColor>(ctrl+t to collapse)</Text>
          </Box>
          <Text dimColor color="gray">{entry.content}</Text>
        </Box>
      );
    }

    case "streaming": {
      // Cap by estimated rendered lines, not raw newlines. Long wrapped
      // paragraphs can otherwise grow beyond the viewport and make Ink redraws
      // visibly jitter while tokens stream.
      //
      // The Spinner / "Writing response…" line shown below the scrollback
      // already indicates the streaming state, so we don't render a separate
      // cursor-style ▋ here — it ended up floating on its own row under the
      // text, which read as visual noise rather than a writing indicator.
      const tail = streamingTail(entry.content, columns, streamingMaxLines);
      return (
        <Box flexDirection="column" marginTop={1}>
          {tail.hiddenLines > 0 && <Text dimColor>  ↑ {tail.hiddenLines} lines above</Text>}
          <MarkdownRenderer content={tail.text} />
        </Box>
      );
    }

    case "tool_call":
      return (
        <Box>
          <Text dimColor>
            {"  ⎿ "}
          </Text>
          <Text color="cyan">{entry.content}</Text>
        </Box>
      );

    case "tool_result": {
      const icon = entry.success === false ? "✗" : "✓";
      const color = entry.success === false ? "red" : "green";
      const isDiff = entry.content.startsWith("DIFF:");
      const display = isDiff
        ? entry.content
        : entry.content.length > 200
          ? entry.content.slice(0, 200) + "..."
          : entry.content;

      if (isDiff) {
        const lines = display.split("\n");
        return (
          <Box flexDirection="column">
            <Box>
              <Text dimColor>{"    "}</Text>
              <Text color={color}>{icon} </Text>
              <Text dimColor>{lines[0]}</Text>
            </Box>
            {lines.slice(1).map((line, i) => {
              if (line.startsWith("+ ")) {
                return <Text key={i} color="green" backgroundColor="#002800">{"      "}{line}</Text>;
              }
              if (line.startsWith("- ")) {
                return <Text key={i} color="red" backgroundColor="#280000">{"      "}{line}</Text>;
              }
              return <Text key={i} dimColor>{"      "}{line}</Text>;
            })}
          </Box>
        );
      }

      return (
        <Box>
          <Text dimColor>{"    "}</Text>
          <Text color={color}>{icon} </Text>
          <Text dimColor>{display}</Text>
        </Box>
      );
    }

    case "tool_output": {
      const outputLines = compactToolOutput(entry.content);
      return (
        <Box flexDirection="column">
          {outputLines.map((line, i) => (
            <Box key={i}>
              <Text dimColor>{"    "}</Text>
              <Text color="gray">{line}</Text>
            </Box>
          ))}
        </Box>
      );
    }

    case "system": {
      const lines = entry.content.split("\n");
      // Pattern-match the first line to choose a severity. The agent emits
      // these prefixes consistently from auth.rs / install.sh / wizard text,
      // so this stays in sync with where they're written.
      const first = lines[0] ?? "";
      const lower = first.toLowerCase();

      // Detect a single bare URL line (frequently the OAuth auth_url) — make
      // it bold cyan so the user can find it instantly to copy-paste.
      const urlOnly = lines.length === 1 && /^\s*https?:\/\/\S+\s*$/.test(first);

      let color: string | undefined;
      let prefix = "";
      if (urlOnly) {
        color = "cyan";
      } else if (lower.startsWith("error:") || lower.startsWith("login failed")) {
        color = "red";
        prefix = "✗ ";
      } else if (
        lower.startsWith("warning:") ||
        lower.startsWith("note:") ||
        lower.startsWith("port ") && lower.includes("in use")
      ) {
        color = "yellow";
        prefix = "⚠ ";
      } else if (lower.startsWith("tip:") || lower.startsWith("hint:") || lower.startsWith("looks like")) {
        color = "cyan";
        prefix = "ℹ ";
      } else if (lower.startsWith("opening browser") || lower.startsWith("waiting for")) {
        color = "green";
        prefix = "▸ ";
      }

      if (urlOnly) {
        return (
          <Box>
            <Text color={color} bold>{lines[0]!.trim()}</Text>
          </Box>
        );
      }

      if (color) {
        return (
          <Box flexDirection="column">
            {lines.map((line, idx) => (
              <Text key={idx} color={color}>
                {idx === 0 ? prefix : "  "}
                {line}
              </Text>
            ))}
          </Box>
        );
      }

      // Default: plain dim. Multi-line entries still render correctly because
      // <Text> preserves embedded newlines.
      return (
        <Box>
          <Text dimColor>{entry.content}</Text>
        </Box>
      );
    }

    case "error":
      return (
        <Box marginTop={1} flexDirection="column">
          {entry.content.split("\n").map((line, idx) => (
            <Text key={idx} color="red" bold={idx === 0}>
              {idx === 0 ? "✗ " : "  "}
              {line}
            </Text>
          ))}
        </Box>
      );

    case "plan_status":
      return (
        <Box marginTop={1}>
          <Text color="yellow">{"◆ "}{entry.content}</Text>
        </Box>
      );

    case "plan_content":
      return (
        <Box flexDirection="column">
          <Text>{entry.content}</Text>
        </Box>
      );

    case "subagent_header":
      return (
        <Box>
          <Text dimColor>{"  ⎿ "}</Text>
          <Text color="magenta">{entry.content}</Text>
        </Box>
      );

    default:
      return <Text>{entry.content}</Text>;
  }
});
