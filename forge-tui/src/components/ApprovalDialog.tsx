// SPDX-License-Identifier: Apache-2.0
import React, { useState } from "react";
import { Box, Text, useInput } from "ink";
import type { PendingApproval } from "../hooks/useAgent.js";

interface Props {
  approval: PendingApproval;
  onApprove: (toolId: string) => void;
  onApproveAlways: (toolName: string, toolId: string) => void;
  onDeny: (toolId: string) => void;
}

const OPTIONS = ["Yes", "Yes, for this session", "No"];

export function ApprovalDialog({ approval, onApprove, onApproveAlways, onDeny }: Props) {
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
      switch (selected) {
        case 0:
          onApprove(approval.toolId);
          break;
        case 1:
          onApproveAlways(approval.toolName, approval.toolId);
          break;
        case 2:
          onDeny(approval.toolId);
          break;
      }
      return;
    }
    // Quick keys
    if (input === "y" || input === "Y") {
      onApprove(approval.toolId);
      return;
    }
    if (input === "n" || input === "N") {
      onDeny(approval.toolId);
      return;
    }
    if (key.escape) {
      onDeny(approval.toolId);
    }
  });

  return (
    <Box flexDirection="column">
      <Box>
        <Text color="yellow">
          {"? "}
        </Text>
        <Text bold>Allow {approval.toolName}?</Text>
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
