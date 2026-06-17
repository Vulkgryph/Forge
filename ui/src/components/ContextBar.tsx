// SPDX-License-Identifier: Apache-2.0
import React from "react";
import { Box, Text } from "ink";
import type { UsageSnapshot } from "../protocol.js";
import type { PermissionMode } from "../hooks/useAgent.js";

interface Props {
  modelName: string;
  reasoningLabel?: string | null;
  usage: UsageSnapshot | null;
  planMode: boolean;
  isThinking: boolean;
  permissionMode: PermissionMode;
}

export function ContextBar({ modelName, reasoningLabel, usage, planMode, isThinking, permissionMode }: Props) {
  const parts: string[] = [];

  if (modelName) {
    parts.push(modelName);
  }

  if (reasoningLabel) {
    parts.push(reasoningLabel);
  }

  if (usage) {
    const pct =
      usage.max_context_tokens > 0
        ? ((usage.last_prompt_tokens / usage.max_context_tokens) * 100).toFixed(0)
        : "0";
    parts.push(`${pct}% ctx`);
  }

  if (planMode) {
    parts.push("PLAN");
  }

  // Permission mode indicator
  const modeLabel =
    permissionMode === "auto_accept" ? "\u23F5\u23F5 auto-accept edits"
    : permissionMode === "plan" ? "\u23F8 plan mode"
    : null;

  return (
    <Box>
      <Text dimColor>
        {parts.join(" \u00B7 ")}
      </Text>
      {modeLabel && (
        <Text color={permissionMode === "plan" ? "yellow" : "green"}>
          {" \u00B7 "}{modeLabel}
        </Text>
      )}
    </Box>
  );
}
