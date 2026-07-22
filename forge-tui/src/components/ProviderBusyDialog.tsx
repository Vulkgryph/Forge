// SPDX-License-Identifier: Apache-2.0
import React, { useState } from "react";
import { Box, Text, useInput } from "ink";
import type { PendingProviderBusy } from "../hooks/useAgent.js";

interface Props {
  busy: PendingProviderBusy;
  onSwitchToPriority: () => void;
  onDismiss: () => void;
}

const OPTIONS = ["Switch to priority tier (2x cost)", "Dismiss"];

/** Shown when forge-agent rejects a request because the provider is at
 * capacity (xAI's "at capacity" 429) and the affected endpoint is a genuine
 * xAI one not already on the priority tier — mirrors Forge IDE's
 * "Switch to Priority Tier" card action. */
export function ProviderBusyDialog({ busy, onSwitchToPriority, onDismiss }: Props) {
  const [selected, setSelected] = useState(0);

  useInput((input, key) => {
    if (key.upArrow) {
      setSelected((s) => (s > 0 ? s - 1 : OPTIONS.length - 1));
      return;
    }
    if (key.downArrow) {
      setSelected((s) => (s < OPTIONS.length - 1 ? s + 1 : 0));
      return;
    }
    if (key.return) {
      if (selected === 0) onSwitchToPriority();
      else onDismiss();
      return;
    }
    if (input === "p" || input === "P") {
      onSwitchToPriority();
      return;
    }
    if (key.escape || input === "d" || input === "D") {
      onDismiss();
    }
  });

  return (
    <Box flexDirection="column">
      <Box>
        <Text color="red">{"✗ "}</Text>
        <Text bold>{busy.message}</Text>
      </Box>
      <Box>
        <Text dimColor>
          Priority requests higher scheduling priority from xAI during high demand, at double
          the standard per-token price. This is a charge from xAI, not Forge.
        </Text>
      </Box>
      {OPTIONS.map((opt, i) => (
        <Box key={opt}>
          <Text>  </Text>
          <Text color={i === selected ? "cyan" : undefined} bold={i === selected}>
            {i === selected ? "❯ " : "  "}
            {opt}
          </Text>
        </Box>
      ))}
    </Box>
  );
}
