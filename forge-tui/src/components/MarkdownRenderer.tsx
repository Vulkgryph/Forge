// SPDX-License-Identifier: Apache-2.0
import React, { useMemo } from "react";
import { Box, Text } from "ink";
import { parseMarkdownBlocks, renderInlinePlainText, type InlinePart, type MarkdownBlock } from "../utils/markdown.js";
import { highlight as cliHighlight, supportsLanguage } from "cli-highlight";

// ── Syntax highlighting (ANSI via cli-highlight, safe inside <Text>) ─

function highlightCode(code: string, lang?: string): string {
  if (!lang) return code;
  try {
    if (!supportsLanguage(lang.toLowerCase())) return code;
    return cliHighlight(code, { language: lang.toLowerCase(), ignoreIllegals: true });
  } catch {
    return code;
  }
}

// ── Inline renderer — produces Ink <Text> children ───────────────────

function InlineParts({ parts }: { parts: InlinePart[] }): React.ReactElement {
  const children: (string | React.ReactElement)[] = [];
  let key = 0;

  for (const part of parts) {
    switch (part.type) {
      case "text":
        children.push(part.content);
        break;
      case "bold":
        children.push(<Text key={key++} bold>{part.content}</Text>);
        break;
      case "italic":
        children.push(<Text key={key++} italic>{part.content}</Text>);
        break;
      case "code":
        children.push(<Text key={key++} color="cyan">{"`"}{part.content}{"`"}</Text>);
        break;
      case "link":
        children.push(
          <Text key={key++} color="blue" underline>{part.label}</Text>
        );
        if (part.href && part.href !== part.label) {
          children.push(<Text key={key++} dimColor>{" (" + part.href + ")"}</Text>);
        }
        break;
      case "del":
        children.push(<Text key={key++} strikethrough>{part.content}</Text>);
        break;
    }
  }

  return <Text>{children}</Text>;
}

// ── Block renderer ────────────────────────────────────────────────────

function BlockRenderer({
  block,
  depth = 0,
  columns = 80,
}: {
  block: MarkdownBlock;
  depth?: number;
  columns?: number;
}): React.ReactElement | null {
  const indent = "  ".repeat(depth);

  switch (block.type) {
    case "heading": {
      const prefix = block.level === 1 ? "▸ " : block.level === 2 ? "  ▸ " : "    › ";
      const color = block.level === 1 ? "cyan" : block.level === 2 ? "blue" : undefined;
      return (
        <Box marginTop={1}>
          <Text bold color={color}>{indent + prefix}</Text>
          <Text bold color={color}><InlineParts parts={block.parts} /></Text>
        </Box>
      );
    }

    case "paragraph":
      return (
        <Box>
          <Text>{indent}</Text>
          <InlineParts parts={block.parts} />
        </Box>
      );

    case "code": {
      const highlighted = highlightCode(block.text, block.lang);
      const lines = highlighted.split("\n");
      const border = "  " + indent;
      return (
        <Box flexDirection="column">
          <Text dimColor>{border}╭{block.lang ? `─ ${block.lang} ` : "─"}</Text>
          {lines.map((line, i) => (
            <Text key={i}>{border + "│ "}{line}</Text>
          ))}
          <Text dimColor>{border}╰─</Text>
        </Box>
      );
    }

    case "list":
      return (
        <Box flexDirection="column">
          {block.items.map((parts, i) => {
            const marker = block.ordered ? `${i + 1}. ` : "- ";
            const plain = renderInlinePlainText(parts);

            return (
              <Text key={i}>
                <Text dimColor>{indent}  </Text>
                <Text color="cyan">{marker}</Text>
                {plain}
              </Text>
            );
          })}
        </Box>
      );

    case "blockquote":
      return (
        <Box flexDirection="column">
          {block.blocks.map((b, i) => (
            <Box key={i}>
              <Text dimColor>{indent}│ </Text>
              <BlockRenderer block={b} depth={depth + 1} columns={columns} />
            </Box>
          ))}
        </Box>
      );

    case "table": {
      // Render as monospaced single-line strings, NOT a horizontal Ink Box of
      // many <Text> children. Multi-Text rows wider than the terminal wrap at
      // arbitrary boundaries and leave a multi-screen blank gap under the
      // message once Ink repaints the live region (the "finished reply then
      // huge empty space before Thought/Worked for" bug).
      return <TableBlock headers={block.headers} rows={block.rows} columns={columns} />;
    }

    case "hr":
      return <Text dimColor>{"  " + "─".repeat(40)}</Text>;

    case "space":
      return <Box marginTop={1} />;

    default:
      return null;
  }
}

function flatLength(parts: InlinePart[]): number {
  return parts.reduce((s, p) => s + inlinePartLength(p), 0);
}

function inlinePartLength(part: InlinePart): number {
  if (part.type === "link") {
    return part.href && part.href !== part.label
      ? part.label.length + part.href.length + 3
      : part.label.length;
  }
  return part.content.length;
}

function partsToPlain(parts: InlinePart[]): string {
  return renderInlinePlainText(parts);
}

function truncateCell(text: string, width: number): string {
  if (width <= 0) return "";
  if (text.length <= width) return text.padEnd(width, " ");
  if (width === 1) return "…";
  return text.slice(0, Math.max(0, width - 1)) + "…";
}

/**
 * Fit column widths into the terminal. Prefer shrinking the widest columns
 * first so short labels stay readable.
 */
function fitColWidths(raw: number[], maxTotal: number): number[] {
  const widths = raw.map((w) => Math.max(3, w));
  const total = () => widths.reduce((a, b) => a + b, 0) + widths.length * 3 + 1; // padding + borders
  if (total() <= maxTotal) return widths;

  // Hard floor per column so the frame still looks like a table.
  const floor = 4;
  while (total() > maxTotal) {
    let widest = 0;
    for (let i = 1; i < widths.length; i++) {
      if (widths[i]! > widths[widest]!) widest = i;
    }
    if (widths[widest]! <= floor) {
      // Still too wide — collapse evenly as best-effort.
      for (let i = 0; i < widths.length && total() > maxTotal; i++) {
        if (widths[i]! > 2) widths[i]! -= 1;
      }
      if (total() > maxTotal) break;
    } else {
      widths[widest]! -= 1;
    }
  }
  return widths;
}

function TableBlock({
  headers,
  rows,
  columns,
}: {
  headers: InlinePart[][];
  rows: InlinePart[][][];
  columns: number;
}): React.ReactElement {
  const maxTableWidth = Math.max(24, columns - 2);
  const allRows = [headers, ...rows];
  const rawWidths = headers.map((_, ci) =>
    Math.max(
      3,
      ...allRows.map((row) => flatLength(row[ci] ?? [{ type: "text", content: "" }]))
    )
  );
  const colWidths = fitColWidths(rawWidths, maxTableWidth);
  const frameWidth = colWidths.reduce((a, b) => a + b, 0) + colWidths.length * 3 + 1;

  // If even the floor widths overflow (tiny terminal / many columns), fall
  // back to a stacked key/value list — never emit a row longer than the
  // terminal, which is what creates the blank-gap repaint bug.
  if (frameWidth > maxTableWidth || colWidths.length > 6) {
    const headerPlain = headers.map(partsToPlain);
    return (
      <Box flexDirection="column">
        {rows.map((row, ri) => (
          <Box key={ri} flexDirection="column" marginTop={ri === 0 ? 0 : 1}>
            {row.map((parts, ci) => {
              const label = headerPlain[ci] || `col${ci + 1}`;
              const value = partsToPlain(parts);
              const line = `  ${label}: ${value}`;
              const clipped =
                line.length > maxTableWidth
                  ? line.slice(0, Math.max(0, maxTableWidth - 1)) + "…"
                  : line;
              return <Text key={ci}>{clipped}</Text>;
            })}
          </Box>
        ))}
      </Box>
    );
  }

  const top = "  ┌" + colWidths.map((w) => "─".repeat(w + 2)).join("┬") + "┐";
  const mid = "  ├" + colWidths.map((w) => "─".repeat(w + 2)).join("┼") + "┤";
  const bot = "  └" + colWidths.map((w) => "─".repeat(w + 2)).join("┴") + "┘";
  const fmtRow = (cells: InlinePart[][]): string =>
    "  │" +
    cells
      .map((parts, ci) => {
        const w = colWidths[ci] ?? 3;
        return " " + truncateCell(partsToPlain(parts), w) + " ";
      })
      .join("│") +
    "│";

  // Pad short rows so borders stay aligned.
  const normalize = (cells: InlinePart[][]): InlinePart[][] => {
    const out = cells.slice(0, colWidths.length);
    while (out.length < colWidths.length) out.push([{ type: "text", content: "" }]);
    return out;
  };

  return (
    <Box flexDirection="column">
      <Text dimColor>{top.slice(0, maxTableWidth)}</Text>
      <Text>{fmtRow(normalize(headers)).slice(0, maxTableWidth)}</Text>
      <Text dimColor>{mid.slice(0, maxTableWidth)}</Text>
      {rows.map((row, ri) => (
        <Text key={ri}>{fmtRow(normalize(row)).slice(0, maxTableWidth)}</Text>
      ))}
      <Text dimColor>{bot.slice(0, maxTableWidth)}</Text>
    </Box>
  );
}

// ── Public component ─────────────────────────────────────────────────

interface Props {
  content: string;
  /** Terminal width — used to clamp tables so they cannot overflow/wrap. */
  columns?: number;
}

export const MarkdownRenderer = React.memo(function MarkdownRenderer({ content, columns = 80 }: Props) {
  const blocks = useMemo(() => parseMarkdownBlocks(content), [content]);
  return (
    <Box flexDirection="column">
      {blocks.map((block, i) => (
        <BlockRenderer key={i} block={block} columns={columns} />
      ))}
    </Box>
  );
});
