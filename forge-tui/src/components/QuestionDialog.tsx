// SPDX-License-Identifier: Apache-2.0
import React, { useState } from "react";
import { Box, Text, useInput } from "ink";
import type { PendingQuestion } from "../hooks/useAgent.js";

interface Props {
  question: PendingQuestion;
  onAnswer: (answer: string) => void;
  onCancel: () => void;
}

export function QuestionDialog({ question, onAnswer, onCancel }: Props) {
  const item = question.items[0];
  const hasOther = item?.options.some((o) => o.label.toLowerCase() === "other") ?? false;
  const visibleOptions = item ? item.options.filter((o) => o.label.toLowerCase() !== "other") : [];
  const hasChoices = visibleOptions.length > 0;
  const options = hasChoices ? [...visibleOptions.map((o) => o.label), "Other"] : [];

  const [selected, setSelected] = useState(0);
  const [freeText, setFreeText] = useState(() => !hasChoices);
  const [textBuffer, setTextBuffer] = useState("");

  useInput((input, key) => {
    if (freeText) {
      if (key.return) {
        if (textBuffer.trim()) {
          onAnswer(textBuffer.trim());
        }
        return;
      }
      if (key.escape) {
        if (!hasChoices) {
          onCancel();
          return;
        }
        setFreeText(false);
        setTextBuffer("");
        return;
      }
      if (key.backspace || key.delete) {
        setTextBuffer((s) => s.slice(0, -1));
        return;
      }
      if (input && !key.ctrl && !key.meta) {
        setTextBuffer((s) => s + input);
      }
      return;
    }

    if (!hasChoices) {
      if (key.escape) {
        onCancel();
      }
      return;
    }

    if (key.upArrow) {
      setSelected((s) => (s > 0 ? s - 1 : options.length - 1));
      return;
    }
    if (key.downArrow) {
      setSelected((s) => (s < options.length - 1 ? s + 1 : 0));
      return;
    }
    if (key.return) {
      if (selected === options.length - 1) {
        // "Other" — switch to free text
        setFreeText(true);
        return;
      }
      onAnswer(options[selected]!);
      return;
    }
    if (key.escape) {
      onCancel();
    }
  });

  const questionText = item?.question ?? question.question;

  return (
    <Box flexDirection="column">
      <Box>
        <Text bold>
          <Text color="magenta">{"? "}</Text>
          {questionText}
        </Text>
      </Box>
      {visibleOptions.map((opt, i) => (
        <Box key={opt.label}>
          <Text>  </Text>
          <Text color={i === selected && !freeText ? "cyan" : undefined} bold={i === selected && !freeText}>
            {i === selected && !freeText ? "❯ " : "  "}
            {opt.label}
          </Text>
          {opt.description ? (
            <Text color="gray">{`  ${opt.description}`}</Text>
          ) : null}
        </Box>
      ))}
      {hasChoices ? (
        <Box>
          <Text>  </Text>
          <Text
            color={selected === options.length - 1 && !freeText ? "cyan" : undefined}
            bold={selected === options.length - 1 && !freeText}
          >
            {selected === options.length - 1 && !freeText ? "❯ " : "  "}
            Other
          </Text>
          {hasOther ? <Text color="gray">{"  Provide a custom response"}</Text> : null}
        </Box>
      ) : null}
      {freeText && (
        <Box>
          <Text color="cyan">{"  > "}</Text>
          <Text>{textBuffer}</Text>
          <Text color="gray">{"█"}</Text>
        </Box>
      )}
    </Box>
  );
}
