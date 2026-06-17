// SPDX-License-Identifier: Apache-2.0
import type { EndpointInfo } from "./protocol.js";

function toggleLabel(value: "provider_default" | "on" | "off"): string {
  return ({
    provider_default: "default",
    on: "on",
    off: "off",
  })[value];
}

function effortLabel(value: EndpointInfo["reasoning"]["chatgpt_codex"]["effort"]): string {
  return ({
    provider_default: "default",
    none: "none",
    minimal: "minimal",
    low: "low",
    medium: "medium",
    high: "high",
    xhigh: "xhigh",
  })[value];
}

function anthropicBudgetLabel(ep: EndpointInfo): string {
  const current = ep.reasoning.anthropic.budget_tokens;
  const maxBudget = Math.max(1024, ep.max_output_tokens - 1);
  const levels = [
    { label: "low", tokens: Math.min(1024, maxBudget) },
    { label: "medium", tokens: Math.min(4096, maxBudget) },
    { label: "high", tokens: Math.min(8192, maxBudget) },
    { label: "xhigh", tokens: Math.min(32768, maxBudget) },
  ].filter((level, index, list) => index === 0 || level.tokens !== list[index - 1]?.tokens);
  const exact = levels.find((level) => level.tokens === current);
  return exact ? `${exact.label} (${exact.tokens})` : `${current}`;
}

export function reasoningDisplay(ep?: EndpointInfo | null): string | null {
  if (!ep) return null;
  switch (ep.endpoint_type) {
    case "anthropic":
      return `thinking ${toggleLabel(ep.reasoning.anthropic.thinking)}, ${anthropicBudgetLabel(ep)}`;
    case "chatgpt_codex":
      return `reasoning ${effortLabel(ep.reasoning.chatgpt_codex.effort)}`;
    default: {
      const thinking = toggleLabel(ep.reasoning.open_ai_compatible.thinking);
      const preserve = toggleLabel(ep.reasoning.open_ai_compatible.preserve_thinking);
      return `thinking ${thinking}, preserve ${preserve}`;
    }
  }
}

export function thinkingIntensityDisplay(ep?: EndpointInfo | null): string | null {
  if (!ep) return null;
  switch (ep.endpoint_type) {
    case "anthropic":
      return ep.reasoning.anthropic.thinking === "on"
        ? `thinking ${anthropicBudgetLabel(ep)}`
        : null;
    case "chatgpt_codex": {
      const effort = ep.reasoning.chatgpt_codex.effort;
      return effort !== "provider_default" && effort !== "none"
        ? `thinking ${effortLabel(effort)}`
        : null;
    }
    default:
      return ep.reasoning.open_ai_compatible.thinking === "on"
        ? "thinking on"
        : null;
  }
}

export function activeEndpoint(
  endpoints: EndpointInfo[],
  modelName: string,
  modelId: string,
  maxContextTokens: number
): EndpointInfo | null {
  return endpoints.find((ep) => ep.name === modelName)
    ?? endpoints.find((ep) => ep.model_id === modelId && ep.max_context_tokens === maxContextTokens)
    ?? endpoints.find((ep) => ep.model_id === modelId)
    ?? null;
}
