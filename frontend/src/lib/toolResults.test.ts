import { describe, expect, it } from "vitest";
import type { Message } from "./types";
import {
  buildToolResultMap,
  filterRenderedMessages,
  isErrorResult,
  isPureToolResultMessage,
} from "./toolResults";

function msg(partial: Partial<Message> & Pick<Message, "blocks">): Message {
  return {
    uuid: Math.random().toString(36).slice(2),
    parent_uuid: null,
    role: "assistant",
    kind: "mixed",
    model: null,
    is_sidechain: false,
    timestamp: null,
    text: "",
    usage: null,
    ...partial,
  };
}

describe("toolResults", () => {
  it("maps every tool_result by its tool_use_id, across messages", () => {
    const messages: Message[] = [
      msg({
        role: "assistant",
        blocks: [
          { type: "tool_use", id: "a", name: "Read", input: {} },
          { type: "tool_use", id: "b", name: "Read", input: {} },
        ],
      }),
      msg({
        role: "user",
        kind: "tool_result",
        blocks: [
          { type: "tool_result", tool_use_id: "a", content: "ra" },
          { type: "tool_result", tool_use_id: "b", content: "rb" },
        ],
      }),
    ];
    const map = buildToolResultMap(messages);
    expect(map.get("a")).toBe("ra");
    expect(map.get("b")).toBe("rb");
    expect(map.size).toBe(2);
  });

  it("detects pure-tool_result messages", () => {
    const pure = msg({
      kind: "tool_result",
      blocks: [{ type: "tool_result", tool_use_id: "a", content: "x" }],
    });
    const mixed = msg({
      blocks: [
        { type: "tool_result", tool_use_id: "a", content: "x" },
        { type: "text", text: "hi" },
      ],
    });
    const empty = msg({ blocks: [] });
    expect(isPureToolResultMessage(pure)).toBe(true);
    expect(isPureToolResultMessage(mixed)).toBe(false);
    expect(isPureToolResultMessage(empty)).toBe(false);
  });

  it("filters out pure-tool_result messages but keeps mixed ones", () => {
    const pure = msg({
      kind: "tool_result",
      blocks: [{ type: "tool_result", tool_use_id: "a", content: "x" }],
    });
    const mixed = msg({
      blocks: [
        { type: "tool_result", tool_use_id: "a", content: "x" },
        { type: "text", text: "hi" },
      ],
    });
    const out = filterRenderedMessages([pure, mixed]);
    expect(out).toEqual([mixed]);
  });

  it("flags error-looking results", () => {
    expect(isErrorResult("Error: boom")).toBe(true);
    expect(isErrorResult("  error happened")).toBe(true);
    expect(isErrorResult("File created successfully")).toBe(false);
  });
});
