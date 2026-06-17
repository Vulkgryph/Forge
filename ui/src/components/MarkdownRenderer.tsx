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

function BlockRenderer({ block, depth = 0 }: { block: MarkdownBlock; depth?: number }): React.ReactElement | null {
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
              <BlockRenderer block={b} depth={depth + 1} />
            </Box>
          ))}
        </Box>
      );

    case "table": {
      // Compute column widths
      const allRows = [block.headers, ...block.rows];
      const colWidths = block.headers.map((_, ci) =>
        Math.max(...allRows.map((row) =>
          row[ci]?.reduce((s, p) => s + inlinePartLength(p), 0) ?? 0
        ), 3)
      );
      const pad = (n: number) => " ".repeat(n);
      return (
        <Box flexDirection="column">
          <Text dimColor>{"  ┌" + colWidths.map((w) => "─".repeat(w + 2)).join("┬") + "┐"}</Text>
          <Box>
            <Text dimColor>{"  │"}</Text>
            {block.headers.map((parts, ci) => (
              <React.Fragment key={ci}>
                <Text> </Text><Text bold><InlineParts parts={parts} /></Text>
                <Text>{pad(colWidths[ci]! - flatLength(parts) + 1)}</Text>
                <Text dimColor>│</Text>
              </React.Fragment>
            ))}
          </Box>
          <Text dimColor>{"  ├" + colWidths.map((w) => "─".repeat(w + 2)).join("┼") + "┤"}</Text>
          {block.rows.map((row, ri) => (
            <Box key={ri}>
              <Text dimColor>{"  │"}</Text>
              {row.map((parts, ci) => (
                <React.Fragment key={ci}>
                  <Text> </Text><InlineParts parts={parts} />
                  <Text>{pad(colWidths[ci]! - flatLength(parts) + 1)}</Text>
                  <Text dimColor>│</Text>
                </React.Fragment>
              ))}
            </Box>
          ))}
          <Text dimColor>{"  └" + colWidths.map((w) => "─".repeat(w + 2)).join("┴") + "┘"}</Text>
        </Box>
      );
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

// ── Public component ─────────────────────────────────────────────────

interface Props {
  content: string;
}

export const MarkdownRenderer = React.memo(function MarkdownRenderer({ content }: Props) {
  const blocks = useMemo(() => parseMarkdownBlocks(content), [content]);
  return (
    <Box flexDirection="column">
      {blocks.map((block, i) => (
        <BlockRenderer key={i} block={block} />
      ))}
    </Box>
  );
});
