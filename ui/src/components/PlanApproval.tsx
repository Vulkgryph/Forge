// SPDX-License-Identifier: Apache-2.0
import React, { useState } from "react";
import { Box, Text, useInput } from "ink";
import { MarkdownRenderer } from "./MarkdownRenderer.js";

interface Props {
  content: string;
  onClearAndApprove: () => void;
  onApprove: () => void;
  onReject: (feedback: string) => void;
}

const OPTIONS = [
  "Clear context + auto-approve edits",
  "Auto-approve edits",
  "Approve (manual edits)",
  "Discuss",
];

export function PlanApproval({ content, onClearAndApprove, onApprove, onReject }: Props) {
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
        case 0: onClearAndApprove(); break;
        case 1: onApprove(); break;
        case 2: onApprove(); break;
        case 3: onReject("DISCUSS"); break;
      }
      return;
    }
    if (key.escape) {
      onReject("");
    }
  });

  return (
    <Box flexDirection="column">
      {/* Plan content */}
      <Box flexDirection="column" marginBottom={1} borderStyle="single" borderColor="cyan" paddingX={1}>
        <Text bold color="cyan">Plan</Text>
        <MarkdownRenderer content={content || "(no plan content)"} />
      </Box>

      <Box flexDirection="column">
        <Box>
          <Text color="yellow">{"? "}</Text>
          <Text bold>Plan ready. How to proceed?</Text>
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
    </Box>
  );
}
