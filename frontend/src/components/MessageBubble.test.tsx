import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import type { Message, MessageKind, Role } from "@/lib/types";
import { MessageBubble } from "./MessageBubble";

function msg(overrides: Partial<Message> = {}): Message {
  return {
    uuid: "m1",
    parent_uuid: null,
    role: "user" as Role,
    kind: "text" as MessageKind,
    model: null,
    is_sidechain: false,
    timestamp: null,
    blocks: [{ type: "text", text: "hello" }],
    text: "hello",
    usage: null,
    ...overrides,
  };
}

describe("MessageBubble", () => {
  it("labels a real user message 'User'", () => {
    render(<MessageBubble message={msg()} />);
    expect(screen.getByText("User")).toBeInTheDocument();
  });

  it("labels a tool-result (role=user, kind=tool_result) as 'Tool result'", () => {
    render(
      <MessageBubble
        message={msg({
          kind: "tool_result",
          blocks: [
            { type: "tool_result", tool_use_id: "t1", content: "output" },
          ],
          text: "output",
        })}
      />,
    );
    expect(screen.getByText("Tool result")).toBeInTheDocument();
    expect(screen.queryByText("User")).not.toBeInTheDocument();
  });

  const assistant = (overrides: Partial<Message> = {}) =>
    msg({
      role: "assistant" as Role,
      model: "claude-opus-4",
      usage: { input_tokens: 100, output_tokens: 20 },
      ...overrides,
    });

  it("shows the assistant label + usage footer on a lone (single-bubble) run", () => {
    render(<MessageBubble message={assistant()} />);
    expect(screen.getByText("Assistant")).toBeInTheDocument();
    expect(screen.getByText("claude-opus-4")).toBeInTheDocument();
  });

  it("suppresses the label on a non-run-start assistant bubble", () => {
    render(<MessageBubble message={assistant()} runStart={false} runEnd={false} />);
    expect(screen.queryByText("Assistant")).not.toBeInTheDocument();
  });

  it("suppresses the usage footer on a non-run-end assistant bubble", () => {
    render(<MessageBubble message={assistant()} runStart runEnd={false} />);
    // Header still present at the start of the run…
    expect(screen.getByText("Assistant")).toBeInTheDocument();
    // …but the repeated usage footer is hidden until the last bubble.
    expect(screen.queryByText("claude-opus-4")).not.toBeInTheDocument();
  });

  it("shows the footer once at the end of a run (no label on the tail bubble)", () => {
    render(<MessageBubble message={assistant()} runStart={false} runEnd />);
    expect(screen.queryByText("Assistant")).not.toBeInTheDocument();
    expect(screen.getByText("claude-opus-4")).toBeInTheDocument();
  });
});
