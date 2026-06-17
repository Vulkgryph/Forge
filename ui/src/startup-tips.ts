// SPDX-License-Identifier: Apache-2.0
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { homedir } from "node:os";

const TIPS = [
  "Tip: Use /copy or Ctrl+F to freeze live updates while selecting text.",
  "Tip: Use /thinking to adjust reasoning for the active model.",
  "Tip: Use /revert to restore a previous message and its code snapshot.",
  "Tip: Type /model to switch models or configure the web helper model.",
  "Tip: Press Shift+Tab to cycle permission modes.",
  "Tip: Use /sessions to resume an earlier Forge conversation.",
  "Tip: Use /settings to change tool permissions and context behavior.",
  "Tip: Send a message while Forge is working to queue it for the next agent boundary.",
];

interface TipState {
  nextIndex?: number;
}

export function nextStartupTip(): string {
  const fallback = TIPS[Math.floor(Date.now() / 1000) % TIPS.length]!;
  try {
    const path = join(homedir(), ".config", "forge", "ui-state.json");
    let state: TipState = {};
    if (existsSync(path)) {
      state = JSON.parse(readFileSync(path, "utf8")) as TipState;
    }

    const index = Number.isInteger(state.nextIndex) ? state.nextIndex! : 0;
    const tip = TIPS[((index % TIPS.length) + TIPS.length) % TIPS.length]!;
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, JSON.stringify({ ...state, nextIndex: index + 1 }, null, 2));
    return tip;
  } catch {
    return fallback;
  }
}
