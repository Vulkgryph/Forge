// SPDX-License-Identifier: Apache-2.0
import { marked, type Token, type Tokens } from "marked";

// ── Public API ────────────────────────────────────────────────────────

export type InlinePart =
  | { type: "text"; content: string }
  | { type: "bold"; content: string }
  | { type: "italic"; content: string }
  | { type: "code"; content: string }
  | { type: "link"; label: string; href: string }
  | { type: "del"; content: string };

export type MarkdownBlock =
  | { type: "heading"; level: number; parts: InlinePart[] }
  | { type: "paragraph"; parts: InlinePart[] }
  | { type: "code"; text: string; lang?: string }
  | { type: "list"; ordered: boolean; items: InlinePart[][] }
  | { type: "blockquote"; blocks: MarkdownBlock[] }
  | { type: "table"; headers: InlinePart[][]; rows: InlinePart[][][] }
  | { type: "hr" }
  | { type: "space" };

export function parseMarkdownBlocks(text: string): MarkdownBlock[] {
  try {
    const tokens = marked.lexer(text);
    return tokens.flatMap(tokenToBlock).filter(Boolean) as MarkdownBlock[];
  } catch {
    return [{ type: "paragraph", parts: [{ type: "text", content: text }] }];
  }
}

// ── Token → Block ─────────────────────────────────────────────────────

function tokenToBlock(token: Token): MarkdownBlock[] {
  switch (token.type) {
    case "heading": {
      const t = token as Tokens.Heading;
      return [{ type: "heading", level: t.depth, parts: inlineTokens(t.tokens) }];
    }
    case "paragraph": {
      const t = token as Tokens.Paragraph;
      return [{ type: "paragraph", parts: inlineTokens(t.tokens) }];
    }
    case "code": {
      const t = token as Tokens.Code;
      return [{ type: "code", text: t.text, lang: t.lang || undefined }];
    }
    case "list": {
      const t = token as Tokens.List;
      const items = t.items.map((item) => {
        const parts: InlinePart[] = [];
        for (const child of item.tokens) {
          if (child.type === "text") {
            parts.push(...inlineTokens((child as Tokens.Text).tokens ?? [{ type: "text", raw: (child as any).text, text: (child as any).text }]));
          } else if (child.type === "paragraph") {
            parts.push(...inlineTokens((child as Tokens.Paragraph).tokens));
          }
        }
        return parts;
      });
      return [{ type: "list", ordered: t.ordered, items }];
    }
    case "blockquote": {
      const t = token as Tokens.Blockquote;
      return [{ type: "blockquote", blocks: t.tokens.flatMap(tokenToBlock).filter(Boolean) as MarkdownBlock[] }];
    }
    case "table": {
      const t = token as Tokens.Table;
      return [{
        type: "table",
        headers: t.header.map((c) => inlineTokens(c.tokens)),
        rows: t.rows.map((row) => row.map((c) => inlineTokens(c.tokens))),
      }];
    }
    case "hr":
      return [{ type: "hr" }];
    case "space":
      return [{ type: "space" }];
    default:
      return [];
  }
}

// ── Inline tokens → InlinePart[] ─────────────────────────────────────

function inlineTokens(tokens: Token[]): InlinePart[] {
  const parts: InlinePart[] = [];
  for (const t of tokens) {
    switch (t.type) {
      case "text": {
        const tt = t as Tokens.Text;
        if (tt.tokens?.length) {
          parts.push(...inlineTokens(tt.tokens));
        } else {
          parts.push({ type: "text", content: tt.text });
        }
        break;
      }
      case "strong":
        parts.push({ type: "bold", content: flatText((t as Tokens.Strong).tokens) });
        break;
      case "em":
        parts.push({ type: "italic", content: flatText((t as Tokens.Em).tokens) });
        break;
      case "codespan":
        parts.push({ type: "code", content: (t as Tokens.Codespan).text });
        break;
      case "link": {
        const lt = t as Tokens.Link;
        parts.push({ type: "link", label: flatText(lt.tokens), href: lt.href });
        break;
      }
      case "del":
        parts.push({ type: "del", content: flatText((t as Tokens.Del).tokens) });
        break;
      case "br":
        parts.push({ type: "text", content: "\n" });
        break;
      case "softbreak":
        // Single newline in markdown source = space in output
        parts.push({ type: "text", content: " " });
        break;
      case "escape":
        parts.push({ type: "text", content: (t as Tokens.Escape).text });
        break;
      case "html":
        // strip HTML tags, keep visible text
        parts.push({ type: "text", content: (t as any).text?.replace(/<[^>]*>/g, "") ?? "" });
        break;
      default: {
        // For unknown tokens, prefer .text over .raw to avoid emitting
        // raw newlines/control chars that corrupt word boundaries
        const content = (t as any).text ?? "";
        if (content) parts.push({ type: "text", content });
        break;
      }
    }
  }
  return parts;
}

function flatText(tokens?: Token[]): string {
  if (!tokens) return "";
  return tokens.map((t) => {
    if (t.type === "softbreak") return " ";
    if (t.type === "br") return " ";
    return (t as any).text ?? "";
  }).join("");
}

export function renderInlinePlainText(parts: InlinePart[]): string {
  return parts.map((p) => {
    if (p.type === "link") return p.href && p.href !== p.label ? `${p.label} (${p.href})` : p.label;
    if (p.type === "code") return `\`${p.content}\``;
    return p.content;
  }).join("");
}

// Legacy export — no longer used for rendering but kept for any imports
export interface MarkdownNode { type: "text"; content: string }
export function parseMarkdown(_text: string): MarkdownNode[] { return []; }
