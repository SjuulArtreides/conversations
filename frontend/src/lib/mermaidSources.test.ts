import { describe, expect, it } from "vitest";
import type { Message } from "./types";
import { mermaidSources } from "./mermaidSources";

const message = (
  uuid: string,
  text: string,
  role: Message["role"] = "assistant",
) =>
  ({
    uuid,
    role,
    kind: "text",
    blocks: [{ type: "text", text }],
    text,
  }) as Message;

describe("terminal Mermaid sources", () => {
  it("keeps the original diagram source rather than the terminal's ASCII layout", () => {
    const source = 'flowchart LR\n A["Original<br/>label"] -->|"Yes"| B';
    const found = mermaidSources([
      message("answer", `A diagram:\n\`\`\`mermaid\n${source}\n\`\`\``),
    ]);
    expect(found.map((entry) => entry.source)).toEqual([source]);
  });

  it("ignores user prompts, tool output, incomplete fences, and examples inside other fences", () => {
    const text = "```mermaid\ngraph LR\n A --> B\n```";
    const tool = {
      ...message("tool", text),
      kind: "tool_result",
      blocks: [{ type: "tool_result", content: text }],
    } as Message;
    expect(
      mermaidSources([
        message("prompt", text, "user"),
        tool,
        message("partial", "```mermaid\ngraph TD\n A -->"),
        message("example", "````markdown\n" + text + "\n````"),
      ]),
    ).toEqual([]);
  });

  it("offers complete diagrams across replies with stable identities as new replies arrive", () => {
    const first = message("one", "```\nsequenceDiagram\n A->>B: Hello\n```");
    const before = mermaidSources([first]);
    const after = mermaidSources([
      first,
      message("two", "~~~mermaid\ngraph TD\n B --> C\n~~~"),
    ]);
    expect(after).toHaveLength(2);
    expect(after[0]).toEqual(before[0]);
    expect(after[1].id).not.toEqual(after[0].id);
  });

  it("accepts Mermaid fence metadata without reinterpreting another language", () => {
    expect(
      mermaidSources([
        message(
          "metadata",
          '```mermaid title="Example"\ngraph LR\n A --> B\n```',
        ),
      ]),
    ).toHaveLength(1);
    expect(
      mermaidSources([message("plain", "```text\ngraph LR\n A --> B\n```")]),
    ).toEqual([]);
  });
});
