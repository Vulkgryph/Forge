// SPDX-License-Identifier: Apache-2.0
import React, { useState } from "react";
import { Box, Text, useInput } from "ink";

interface Props {
  command: string;
  prompt: string;
  onSubmit: (text: string) => void;
}

export function BgPromptDialog({ command, prompt, onSubmit }: Props) {
  const [buffer, setBuffer] = useState("");

  useInput((input, key) => {
    if (key.return) {
      onSubmit(buffer);
      return;
    }
    if (key.backspace || key.delete) {
      setBuffer((s) => s.slice(0, -1));
      return;
    }
    if (input && !key.ctrl && !key.meta) {
      setBuffer((s) => s + input);
    }
  });

  return (
    <Box flexDirection="column">
      <Box>
        <Text color="yellow">{"⚠ "}</Text>
        <Text bold>Background command needs input</Text>
      </Box>
      <Box>
        <Text color="gray">{"  $ "}</Text>
        <Text dimColor>{command}</Text>
      </Box>
      <Box>
        <Text color="cyan">{"  "}{prompt}</Text>
      </Box>
      <Box>
        <Text color="cyan">{"  > "}</Text>
        <Text>{buffer}</Text>
        <Text color="gray">{"█"}</Text>
      </Box>
    </Box>
  );
}
