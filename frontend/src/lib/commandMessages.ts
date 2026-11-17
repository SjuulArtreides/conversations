import type { Message } from "./types";

/** A collapsed CLI command-meta message: a one-line label + the raw text. */
export interface CommandMessageInfo {
  label: string;
  raw: string;
}

/** The CLI command-meta tag prefixes Claude Code injects as `role:"user"` text. */
const META_PREFIXES = [
  "<local-command-caveat>",
  "<command-name>",
  "<command-message>",
  "<command-args>",
  "<local-command-stdout>",
  "<command-stdout>",
  "<command-stderr>",
  "<command-contents>",
] as const;

/** Join the trimmed text of a message's `text` blocks. */
function combinedText(message: Message): string {
  return message.blocks
    .filter((b): b is { type: "text"; text: string } => b.type === "text")
    .map((b) => b.text)
    .join("")
    .trim();
}

/**
 * Classify a message as a CLI command-meta message (caveat, slash-command
 * invocation, or command output wrapper) that should collapse to a compact
 * chip rather than a full bubble. Returns `null` for anything that isn't
 * entirely command-meta — real user prose, tool results, assistant turns.
 */
export function classifyCommandMessage(
  message: Message,
): CommandMessageInfo | null {
  if (message.role !== "user" || message.kind === "tool_result") return null;

  const raw = combinedText(message);
  if (!raw) return null;

  // Be conservative: only collapse when the text STARTS WITH a meta tag. Real
  // prose that merely mentions one of these tags renders normally.
  if (!META_PREFIXES.some((p) => raw.startsWith(p))) return null;

  let label = "⌘ command";
  const nameMatch = raw.match(/<command-name>\s*([^<]*)<\/command-name>/);
  if (nameMatch) {
    const name = nameMatch[1].trim();
    label = `⌘ ${name}`;
    const argsMatch = raw.match(/<command-args>\s*([^<]*)<\/command-args>/);
    const args = argsMatch?.[1].trim();
    if (args) label += ` ${args}`;
  } else if (raw.startsWith("<local-command-caveat>")) {
    label = "⌘ command context";
  } else if (/<(local-command-stdout|command-stdout|command-stderr)>/.test(raw)) {
    label = "⌘ command output";
  }

  return { label, raw };
}
