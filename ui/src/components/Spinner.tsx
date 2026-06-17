// SPDX-License-Identifier: Apache-2.0
import React, { useState, useEffect } from "react";
import { Text } from "ink";

const FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

interface Props {
  label?: string;
}

export function Spinner({ label = "Thinking" }: Props) {
  const [frame, setFrame] = useState(0);

  useEffect(() => {
    const timer = setInterval(() => {
      setFrame((f) => (f + 1) % FRAMES.length);
    }, 120);
    return () => clearInterval(timer);
  }, []);

  return (
    <Text>
      <Text color="yellow">{FRAMES[frame]}</Text>
      <Text dimColor> {label}...</Text>
    </Text>
  );
}
