// SPDX-License-Identifier: Apache-2.0
import React, { useState, useEffect } from "react";
import { Box, Text, useInput, useStdout } from "ink";

export interface MenuOption {
  label: string;
  description: string;
  marker?: string;
  /** If true, this item is a non-selectable section header. */
  header?: boolean;
}

interface Props {
  title: string;
  items: MenuOption[];
  initialSelected?: number;
  onSelect: (index: number) => void;
  onCancel: () => void;
  onAction?: (key: string, selectedIndex: number) => void;
  footer?: string[];
  actionHint?: string;
  nested?: boolean;
}

function firstSelectable(items: MenuOption[], from = 0): number {
  if (items.length === 0) return 0;
  const start = Math.max(0, Math.min(from, items.length - 1));
  for (let i = start; i < items.length; i++) {
    if (!items[i]?.header) return i;
  }
  for (let i = 0; i < start; i++) {
    if (!items[i]?.header) return i;
  }
  return start;
}

function prevSelectable(items: MenuOption[], from: number): number {
  if (items.length === 0) return 0;
  const current = Math.max(0, Math.min(from, items.length - 1));
  for (let i = current - 1; i >= 0; i--) {
    if (!items[i]?.header) return i;
  }
  for (let i = items.length - 1; i > current; i--) {
    if (!items[i]?.header) return i;
  }
  return items[current]?.header ? firstSelectable(items, current) : current;
}

function nextSelectable(items: MenuOption[], from: number): number {
  if (items.length === 0) return 0;
  const current = Math.max(0, Math.min(from, items.length - 1));
  for (let i = current + 1; i < items.length; i++) {
    if (!items[i]?.header) return i;
  }
  for (let i = 0; i < current; i++) {
    if (!items[i]?.header) return i;
  }
  return items[current]?.header ? firstSelectable(items, current) : current;
}

function selectableNear(items: MenuOption[], index: number, direction: 1 | -1): number {
  if (items.length === 0) return 0;
  const clamped = Math.max(0, Math.min(index, items.length - 1));
  if (!items[clamped]?.header) return clamped;

  for (let i = clamped; i >= 0 && i < items.length; i += direction) {
    if (!items[i]?.header) return i;
  }
  for (let i = clamped; i >= 0 && i < items.length; i -= direction) {
    if (!items[i]?.header) return i;
  }
  return clamped;
}

function truncateText(value: string, maxWidth: number): string {
  if (maxWidth <= 0) return "";
  if (value.length <= maxWidth) return value;
  if (maxWidth <= 3) return value.slice(0, maxWidth);
  return `${value.slice(0, maxWidth - 3)}...`;
}

export function Menu({ title, items, initialSelected = 0, onSelect, onCancel, onAction, footer, actionHint, nested = false }: Props) {
  const { stdout } = useStdout();
  const terminalHeight = stdout?.rows ?? 24;
  const terminalWidth = stdout?.columns ?? 100;
  const contentWidth = Math.max(24, terminalWidth - 2);

  // Reserve rows for title, scroll indicators, footer, nav hint, and margins.
  // Menu item rows are kept to one terminal row so this stays predictable.
  const reservedRows = 1 + 2 + (footer?.length ?? 0) + 1 + 2;
  const maxVisible = Math.max(1, terminalHeight - reservedRows);

  const [selected, setSelected] = useState(() => {
    const item = items[initialSelected];
    return item?.header ? firstSelectable(items) : initialSelected;
  });

  // Scroll offset — keep selected item inside the visible window
  const [scrollOffset, setScrollOffset] = useState(() => {
    const idx = items[initialSelected]?.header ? firstSelectable(items) : initialSelected;
    return Math.max(0, idx - Math.floor(maxVisible / 2));
  });

  useEffect(() => {
    setSelected((current) => {
      if (items.length === 0) return 0;
      const clamped = Math.max(0, Math.min(current, items.length - 1));
      return items[clamped]?.header ? firstSelectable(items, clamped) : clamped;
    });
  }, [items]);

  // Whenever selected changes, adjust scroll to keep it visible
  useEffect(() => {
    setScrollOffset((offset) => {
      if (items.length === 0) return 0;
      const maxOffset = Math.max(0, items.length - maxVisible);
      const clampedOffset = Math.max(0, Math.min(offset, maxOffset));
      if (selected < clampedOffset) return selected;
      if (selected >= clampedOffset + maxVisible) {
        return Math.max(0, Math.min(selected - maxVisible + 1, maxOffset));
      }
      return clampedOffset;
    });
  }, [selected, maxVisible, items.length]);

  useInput((input, key) => {
    if (key.upArrow || input === "k") {
      setSelected((s) => prevSelectable(items, s));
      return;
    }
    if (key.downArrow || input === "j") {
      setSelected((s) => nextSelectable(items, s));
      return;
    }
    if (key.pageUp || (key.ctrl && input === "u")) {
      setSelected((s) => selectableNear(items, s - maxVisible, -1));
      return;
    }
    if (key.pageDown || (key.ctrl && input === "d")) {
      setSelected((s) => selectableNear(items, s + maxVisible, 1));
      return;
    }
    if (input === "g") {
      setSelected(firstSelectable(items));
      return;
    }
    if (input === "G") {
      setSelected(selectableNear(items, items.length - 1, -1));
      return;
    }
    if (key.return) {
      if (!items[selected]?.header) onSelect(selected);
      return;
    }
    if (key.escape) {
      onCancel();
      return;
    }
    if (onAction && input && !key.ctrl && !key.meta) {
      onAction(input, selected);
    }
  });

  const visibleItems = items.slice(scrollOffset, scrollOffset + maxVisible);
  const hiddenAbove = scrollOffset;
  const hiddenBelow = Math.max(0, items.length - scrollOffset - maxVisible);

  return (
    <Box flexDirection="column">
      <Box>
        <Text bold color="cyan">{title}</Text>
      </Box>

      {hiddenAbove > 0 && (
        <Text dimColor>  ↑ {hiddenAbove} more</Text>
      )}

      {visibleItems.map((item, vi) => {
        const i = vi + scrollOffset;
        if (item.header) {
          return (
            <Box key={i} marginTop={vi === 0 ? 0 : 1}>
              <Text dimColor wrap="truncate">  {truncateText(item.label, contentWidth - 2)}</Text>
            </Box>
          );
        }
        const prefix = `${i === selected ? "❯ " : "  "}${item.marker ? `${item.marker} ` : ""}`;
        const labelWidth = Math.max(8, Math.min(32, Math.floor(contentWidth * 0.4)));
        const descriptionWidth = Math.max(0, contentWidth - prefix.length - labelWidth - 2);
        return (
          <Box key={i}>
            <Text>  </Text>
            <Text color={i === selected ? "cyan" : undefined} bold={i === selected}>
              {prefix}
            </Text>
            <Text color={i === selected ? "cyan" : undefined} bold={i === selected} wrap="truncate">
              {truncateText(item.label, labelWidth)}
            </Text>
            <Text dimColor wrap="truncate">{"  "}{truncateText(item.description, descriptionWidth)}</Text>
          </Box>
        );
      })}

      {hiddenBelow > 0 && (
        <Text dimColor>  ↓ {hiddenBelow} more</Text>
      )}

      {footer?.map((line, i) => (
        <Box key={i}>
          <Text dimColor wrap="truncate">{truncateText(line, contentWidth)}</Text>
        </Box>
      ))}
      <Box marginTop={1}>
        <Text dimColor wrap="truncate">↑↓/jk navigate  PgUp/PgDn jump  g/G top/bottom  Enter select  Esc {nested ? "go back" : "close"}{actionHint ? `  ${actionHint}` : ""}</Text>
      </Box>
    </Box>
  );
}
