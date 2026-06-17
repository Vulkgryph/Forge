// SPDX-License-Identifier: Apache-2.0
import React from "react";
import { Box, Text } from "ink";
import type { ActiveSubagent } from "../hooks/useAgent.js";

interface Props {
  subagents: Map<string, ActiveSubagent>;
}

function truncate(value: string, max: number): string {
  if (value.length <= max) return value;
  return `${value.slice(0, Math.max(0, max - 3))}...`;
}

export function SubagentStatus({ subagents }: Props) {
  if (subagents.size === 0) return null;

  const entries = Array.from(subagents.values());

  return (
    <Box flexDirection="column" marginTop={1}>
      <Box>
        <Text color="magenta">Subagents</Text>
        <Text dimColor>{` · ${entries.length} running`}</Text>
      </Box>
      {entries.map((agent) => {
        const status = agent.currentTool
          ? `${agent.currentTool}${agent.detail ? ` · ${agent.detail}` : ""}`
          : agent.detail || "starting";

        return (
          <Box key={agent.id} flexDirection="column" marginLeft={2}>
            <Box>
              <Text color="magenta">◆ </Text>
              <Text>{agent.agentType}</Text>
              <Text dimColor>{` · ${truncate(agent.prompt, 72)}`}</Text>
            </Box>
            <Box marginLeft={2}>
              <Text dimColor>{truncate(status, 96)}</Text>
            </Box>
          </Box>
        );
      })}
    </Box>
  );
}
