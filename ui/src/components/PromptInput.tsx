// SPDX-License-Identifier: Apache-2.0
import React, { useEffect, useMemo, useReducer, useRef, useState } from "react";
import { Box, Text, useInput } from "ink";

export interface SlashCommand {
  command: string;
  description: string;
}

interface Props {
  onSubmit: (text: string) => void;
  placeholder?: string;
  disabled?: boolean;
  allowEmpty?: boolean;
  hidden?: boolean;
  slashCommands?: SlashCommand[];
}

interface InputState {
  value: string;
  cursor: number;
}

type Action =
  | { type: "insert"; text: string }
  | { type: "newline" }
  | { type: "backspace" }
  | { type: "left" }
  | { type: "right" }
  | { type: "set"; value: string }
  | { type: "reset" };

function reducer(state: InputState, action: Action): InputState {
  const { value, cursor } = state;
  switch (action.type) {
    case "insert":
      return {
        value: value.slice(0, cursor) + action.text + value.slice(cursor),
        cursor: cursor + action.text.length,
      };
    case "newline":
      return {
        value: value.slice(0, cursor) + "\n" + value.slice(cursor),
        cursor: cursor + 1,
      };
    case "backspace":
      if (cursor === 0) return state;
      return {
        value: value.slice(0, cursor - 1) + value.slice(cursor),
        cursor: cursor - 1,
      };
    case "left":
      return { value, cursor: Math.max(0, cursor - 1) };
    case "right":
      return { value, cursor: Math.min(value.length, cursor + 1) };
    case "set":
      return { value: action.value, cursor: action.value.length };
    case "reset":
      return { value: "", cursor: 0 };
    default:
      return state;
  }
}

export function PromptInput({ onSubmit, placeholder = "Type a message...  (\\+Enter or Ctrl+N for newline)", disabled = false, allowEmpty = false, hidden = false, slashCommands = [] }: Props) {
  const [{ value, cursor }, dispatch] = useReducer(reducer, { value: "", cursor: 0 });
  const [selectedSuggestion, setSelectedSuggestion] = useState(0);
  const lastWasEsc = useRef(false);

  const slashQuery = useMemo(() => {
    if (!value.startsWith("/") || value.includes("\n") || value.includes(" ")) return null;
    return value.slice(1).toLowerCase();
  }, [value]);

  const suggestions = useMemo(() => {
    if (slashQuery === null) return [];
    return slashCommands
      .filter((cmd) => cmd.command.slice(1).toLowerCase().includes(slashQuery))
      .slice(0, 8);
  }, [slashCommands, slashQuery]);

  useEffect(() => {
    setSelectedSuggestion(0);
  }, [slashQuery]);

  const completeSelectedSuggestion = () => {
    const selected = suggestions[selectedSuggestion];
    if (!selected) return false;
    dispatch({ type: "set", value: selected.command });
    return true;
  };

  useInput(
    (input, key) => {
      if (disabled) return;
      if (input === "c" && key.ctrl) return;
      if (key.tab && key.shift) return;

      // Ctrl+N → clean newline (reliable in all terminals)
      if (input === "n" && key.ctrl) {
        lastWasEsc.current = false;
        dispatch({ type: "newline" });
        return;
      }

      // Any other Ctrl-modified key is a global shortcut handled in App
      // (Ctrl+F copy mode, Ctrl+T expand reasoning, etc.). Ignore it here so the
      // bare letter isn't inserted into the prompt.
      if (key.ctrl) return;

      // ESC+CR as single raw sequence (Option+Enter on proper terminals)
      if (input === "\x1b\r" || input === "\x1b\n") {
        lastWasEsc.current = false;
        dispatch({ type: "newline" });
        return;
      }

      // ESC: may be start of two-event Alt+Enter sequence
      if (key.escape) {
        lastWasEsc.current = true;
        return;
      }

      if (suggestions.length > 0) {
        if (key.upArrow) {
          setSelectedSuggestion((idx) => (idx + suggestions.length - 1) % suggestions.length);
          return;
        }
        if (key.downArrow) {
          setSelectedSuggestion((idx) => (idx + 1) % suggestions.length);
          return;
        }
        if (key.tab) {
          completeSelectedSuggestion();
          return;
        }
      }

      if (key.return) {
        if (suggestions.length > 0) {
          const selected = suggestions[selectedSuggestion];
          if (selected && value !== selected.command) {
            completeSelectedSuggestion();
            return;
          }
        }
        // \+Enter → newline (primary multiline method, works in VS Code)
        if (value[cursor - 1] === "\\") {
          lastWasEsc.current = false;
          dispatch({ type: "backspace" });
          dispatch({ type: "newline" });
          return;
        }
        // VS Code Shift+Enter sends raw "\\\r\n" — caught above via the \ check.
        // key.meta / key.shift only work in proper terminals (not VS Code).
        if (key.meta || key.shift || lastWasEsc.current) {
          lastWasEsc.current = false;
          dispatch({ type: "newline" });
          return;
        }
        lastWasEsc.current = false;
        const trimmed = value.trim();
        if (trimmed || allowEmpty) {
          onSubmit(trimmed);
          dispatch({ type: "reset" });
        }
        return;
      }

      lastWasEsc.current = false;

      if (key.backspace || key.delete) {
        dispatch({ type: "backspace" });
        return;
      }
      if (key.leftArrow)  { dispatch({ type: "left" });  return; }
      if (key.rightArrow) { dispatch({ type: "right" }); return; }
      if (key.upArrow || key.downArrow) return;

      if (input) {
        // Convert lone \r to \n (Alt+Enter on some terminals sends raw \r)
        const text = input.replace(/\r/g, "\n");
        if (text) dispatch({ type: "insert", text });
      }
    },
    { isActive: !disabled && !hidden }
  );

  if (hidden) return null;

  const MAX_VISIBLE_LINES = 8;

  const displayValue = value || "";
  const before = displayValue.slice(0, cursor);
  const cursorChar = displayValue[cursor] ?? " ";
  const after = displayValue.slice(cursor + 1);
  const fullDisplay = before + cursorChar + after;
  const allLines = fullDisplay.split("\n");

  // Find which line the cursor is on
  let remaining = cursor;
  let cursorLine = 0;
  let cursorCol = 0;
  for (let i = 0; i < allLines.length; i++) {
    if (remaining <= (allLines[i]?.length ?? 0)) {
      cursorLine = i;
      cursorCol = remaining;
      break;
    }
    remaining -= (allLines[i]?.length ?? 0) + 1;
  }

  // Scroll the viewport to keep cursor visible
  const totalLines = allLines.length;
  let scrollOffset = 0;
  if (totalLines > MAX_VISIBLE_LINES) {
    // Keep cursor in view: scroll so cursor line is always visible
    scrollOffset = Math.max(0, cursorLine - MAX_VISIBLE_LINES + 1);
    // But don't scroll past the end
    scrollOffset = Math.min(scrollOffset, totalLines - MAX_VISIBLE_LINES);
  }

  const visibleLines = allLines.slice(scrollOffset, scrollOffset + MAX_VISIBLE_LINES);
  const hiddenAbove = scrollOffset;

  const prompt = allowEmpty ? "⎸ " : "❯ ";
  const promptColor = allowEmpty ? "yellow" : "blue";

  // Build all rows as a flat array — avoid fragments inside ternaries
  // which can confuse Ink's block layout engine.
  const rows: React.ReactElement[] = [];

  if (displayValue.length === 0 && !disabled) {
    rows.push(
      <Box key="placeholder">
        <Text bold color={promptColor}>{prompt}</Text>
        <Text dimColor>{placeholder}</Text>
      </Box>
    );
  } else {
    if (hiddenAbove > 0) {
      rows.push(
        <Text key="above" dimColor>  ↑ {hiddenAbove} line{hiddenAbove > 1 ? "s" : ""} above</Text>
      );
    }
    for (let vi = 0; vi < visibleLines.length; vi++) {
      const i = vi + scrollOffset;
      const line = visibleLines[vi] ?? "";
      const pfx = i === 0
        ? <Text bold color={promptColor}>{prompt}</Text>
        : <Text dimColor>{"· ".padEnd(prompt.length)}</Text>;
      if (i === cursorLine) {
        const lb = line.slice(0, cursorCol);
        const lc = line[cursorCol] ?? " ";
        const la = line.slice(cursorCol + 1);
        rows.push(
          <Box key={i}>
            {pfx}
            <Text>{lb}<Text inverse>{lc}</Text>{la}</Text>
          </Box>
        );
      } else {
        rows.push(<Box key={i}>{pfx}<Text>{line}</Text></Box>);
      }
    }
  }

  return (
    <Box marginTop={1} flexDirection="column">
      {suggestions.length > 0 && (
        <Box borderStyle="round" borderColor="gray" flexDirection="column" paddingX={1}>
          {suggestions.map((item, idx) => (
            <Box key={item.command}>
              <Text color={idx === selectedSuggestion ? "cyan" : undefined} bold={idx === selectedSuggestion}>
                {idx === selectedSuggestion ? "❯ " : "  "}
                {item.command}
              </Text>
              <Text dimColor>{"  "}{item.description}</Text>
            </Box>
          ))}
          <Text dimColor>↑↓ navigate  Tab complete  Enter select/run</Text>
        </Box>
      )}
      {rows}
    </Box>
  );
}
