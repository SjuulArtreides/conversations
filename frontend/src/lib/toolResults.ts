import type { Message } from "./types";

/**
 * Build a session-wide lookup from `tool_use.id` → the matching
 * `tool_result.content`. Tool results arrive in a later (user) message — and a
 * single assistant turn may issue several parallel tool calls whose results all
 * land in the next message — so we scan every block of every message.
 */
export function buildToolResultMap(messages: Message[]): Map<string, string> {
  const map = new Map<string, string>();
  for (const m of messages) {
    for (const block of m.blocks) {
      if (block.type === "tool_result") {
        map.set(block.tool_use_id, block.content ?? "");
      }
    }
  }
  return map;
}

/** True when every block in the message is a `tool_result`. */
export function isPureToolResultMessage(message: Message): boolean {
  return (
    message.blocks.length > 0 &&
    message.blocks.every((b) => b.type === "tool_result")
  );
}

/**
 * True when every block is a signature-only thinking block — i.e. extended
 * thinking that Claude Code stored with an encrypted signature but no plaintext.
 * `BlockRenderer` renders these as `null` (they carry nothing readable), so such
 * a message would otherwise show an empty ASSISTANT bubble. Drop it.
 */
export function isPureHiddenThinkingMessage(message: Message): boolean {
  return (
    message.blocks.length > 0 &&
    message.blocks.every(
      (b) => b.type === "thinking" && b.thinking.trim().length === 0,
    )
  );
}

/**
 * Drop messages that render to nothing: blocks ENTIRELY `tool_result` (folded
 * into the originating tool-call boxes), or entirely signature-only thinking
 * (hidden by `BlockRenderer`). Messages that mix such blocks with other content
 * are kept and render normally.
 */
export function filterRenderedMessages(messages: Message[]): Message[] {
  return messages.filter(
    (m) => !isPureToolResultMessage(m) && !isPureHiddenThinkingMessage(m),
  );
}

/** Whether a result string looks like a tool error. */
export function isErrorResult(content: string): boolean {
  return /^\s*Error\b/i.test(content);
}
