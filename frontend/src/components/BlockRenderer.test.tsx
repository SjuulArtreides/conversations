import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { BlockRenderer, langForPath } from "./BlockRenderer";

/** Open a tool call's disclosure the way a reader does: click its summary. */
async function openToolCall(
  user: ReturnType<typeof userEvent.setup>,
  container: HTMLElement,
) {
  const details = container.querySelector("details")!;
  await user.click(details.querySelector("summary")!);
  expect(details).toHaveAttribute("open");
}

describe("BlockRenderer", () => {
  it("renders a text block's markdown, including a fenced code block", () => {
    const { container } = render(
      <BlockRenderer
        block={{
          type: "text",
          text: "Hello **world**\n\n```ts\nconst x = 1;\n```",
        }}
      />,
    );
    expect(screen.getByText("world").tagName.toLowerCase()).toBe("strong");
    // fenced code becomes a <pre><code> inside our CodeBlock chrome
    expect(container.querySelector("pre code")).not.toBeNull();
    expect(container.textContent).toContain("const x = 1;");
  });

  it("renders a GFM table from a text block", () => {
    const md = "| a | b |\n| - | - |\n| 1 | 2 |";
    const { container } = render(
      <BlockRenderer block={{ type: "text", text: md }} />,
    );
    expect(container.querySelector("table")).not.toBeNull();
  });

  it("detects a Mermaid fence and offers source Copy or the diagram preview icon", () => {
    render(
      <BlockRenderer
        block={{
          type: "text",
          text: "```mermaid\nflowchart LR\n  A --> B\n```",
        }}
      />,
    );
    expect(
      screen.getByRole("button", { name: "Preview Mermaid diagram" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Copy code" }),
    ).toBeInTheDocument();
  });

  it("offers the diagram preview for a Write of a Mermaid file once the call is opened", async () => {
    const user = userEvent.setup();
    const content = "sequenceDiagram\n  Alice->>Bob: Hi";
    for (const filePath of ["/repo/docs/login.mmd", "/repo/flow.mermaid"]) {
      const { unmount, container } = render(
        <BlockRenderer
          block={{
            type: "tool_use",
            id: `w-${filePath}`,
            name: "Write",
            input: { file_path: filePath, content },
          }}
        />,
      );
      await openToolCall(user, container);
      expect(
        screen.getByRole("button", { name: "Preview Mermaid diagram" }),
      ).toBeInTheDocument();
      expect(
        screen.getByText("Alice->>Bob: Hi", { exact: false }),
      ).toBeInTheDocument();
      unmount();
    }
  });

  it("does not offer a diagram preview for a truncated Mermaid file until it is expanded", async () => {
    const user = userEvent.setup();
    const content = `flowchart TD\n${"  A --> B\n".repeat(600)}`;
    const { container } = render(
      <BlockRenderer
        block={{
          type: "tool_use",
          id: "w-long",
          name: "Write",
          input: { file_path: "/repo/big.mmd", content },
        }}
      />,
    );
    await openToolCall(user, container);
    expect(
      screen.queryByRole("button", { name: "Preview Mermaid diagram" }),
    ).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: /Show more/ }));
    expect(
      screen.getByRole("button", { name: "Preview Mermaid diagram" }),
    ).toBeInTheDocument();
  });

  it("detects a Markdown fence and offers source Copy or rendered View", () => {
    render(
      <BlockRenderer
        block={{
          type: "text",
          text: "```markdown\n# Preview\n\n| A | B |\n| - | - |\n| 1 | 2 |\n```",
        }}
      />,
    );
    expect(
      screen.getByRole("button", { name: "View rendered Markdown" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Copy code" }),
    ).toBeInTheDocument();
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
  });

  it("renders a text-bearing thinking block as a collapsed Collapsible (not forced open)", () => {
    const { container } = render(
      <BlockRenderer
        block={{ type: "thinking", thinking: "secret reasoning" }}
      />,
    );
    // A <details> element is present (the Collapsible renders one).
    expect(container.querySelector("details")).not.toBeNull();
    // The label is visible.
    expect(screen.getByText("💭 Thinking")).toBeInTheDocument();
    // The content is in the DOM (Collapsible keeps content mounted even when
    // closed, so asserting presence is sufficient — do NOT assert it's visible).
    expect(screen.getByText("secret reasoning")).toBeInTheDocument();
    // The details element must NOT have the open attribute — i.e. collapsed by default.
    expect(container.querySelector("details")!.hasAttribute("open")).toBe(
      false,
    );
  });

  it("renders nothing for a signature-only (empty) thinking block", () => {
    const { container } = render(
      <BlockRenderer block={{ type: "thinking", thinking: "" }} />,
    );
    // The block renders null — no element, no chip, no disclosure.
    expect(container.firstChild).toBeNull();
    expect(container.textContent).toBe("");
  });

  it("renders nothing for a whitespace-only thinking block", () => {
    const { container } = render(
      <BlockRenderer block={{ type: "thinking", thinking: "   " }} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("renders a Bash tool_use as 'Bash: <description>' with the command as a code box (not JSON)", () => {
    const { container } = render(
      <BlockRenderer
        block={{
          type: "tool_use",
          id: "t1",
          name: "Bash",
          input: { command: "git status", description: "Check git status" },
        }}
      />,
    );
    expect(screen.getByText("Bash")).toBeInTheDocument();
    expect(screen.getByText(/: Check git status/)).toBeInTheDocument();
    // The command shows in a code box; the raw JSON does not.
    expect(container.querySelector("pre code")).not.toBeNull();
    expect(container.textContent).toContain("git status");
    expect(container.textContent).not.toContain('"command"');
  });

  it("falls back to JSON for an unknown tool_use", () => {
    const { container } = render(
      <BlockRenderer
        block={{
          type: "tool_use",
          id: "t2",
          name: "SomeMysteryTool",
          input: { file_path: "/tmp/x" },
        }}
      />,
    );
    expect(screen.getByText("SomeMysteryTool")).toBeInTheDocument();
    expect(container.textContent).toContain('"file_path": "/tmp/x"');
  });

  it("renders a Write tool_use as 'Write: <path>' with the content in a code box (not JSON)", () => {
    const { container } = render(
      <BlockRenderer
        block={{
          type: "tool_use",
          id: "t3",
          name: "Write",
          input: {
            file_path: "/tmp/hello.ts",
            content: "export const x = 1;",
          },
        }}
      />,
    );
    expect(screen.getByText("Write")).toBeInTheDocument();
    expect(screen.getByText(/: \/tmp\/hello\.ts/)).toBeInTheDocument();
    // Content shows in a code box; the raw JSON does not.
    expect(container.querySelector("pre code")).not.toBeNull();
    expect(container.textContent).toContain("export const x = 1;");
    expect(container.textContent).not.toContain('"content"');
  });

  it("renders a Skill tool_use as a 'Skill: <name>' chip with no JSON box", () => {
    const { container } = render(
      <BlockRenderer
        block={{
          type: "tool_use",
          id: "t4",
          name: "Skill",
          input: { skill: "deep-research" },
        }}
      />,
    );
    expect(screen.getByText("Skill")).toBeInTheDocument();
    expect(screen.getByText(/: deep-research/)).toBeInTheDocument();
    // Chip = no disclosure and no JSON box.
    expect(container.querySelector("details")).toBeNull();
    expect(container.querySelector("pre code")).toBeNull();
    expect(container.textContent).not.toContain('"skill"');
  });

  it("renders a Read tool_use as a 'Read: <path>' chip with no JSON box", () => {
    const { container } = render(
      <BlockRenderer
        block={{
          type: "tool_use",
          id: "t5",
          name: "Read",
          input: { file_path: "/tmp/x" },
        }}
      />,
    );
    expect(screen.getByText("Read")).toBeInTheDocument();
    expect(screen.getByText(/: \/tmp\/x/)).toBeInTheDocument();
    expect(container.querySelector("details")).toBeNull();
    expect(container.querySelector("pre code")).toBeNull();
    expect(container.textContent).not.toContain('"file_path"');
  });

  it("folds a paired result into the tool_use box when a results map is passed", () => {
    const results = new Map<string, string>([
      ["t6", "File created successfully at: /tmp/hello.ts"],
    ]);
    const { container } = render(
      <BlockRenderer
        block={{
          type: "tool_use",
          id: "t6",
          name: "Write",
          input: { file_path: "/tmp/hello.ts", content: "x" },
        }}
        results={results}
      />,
    );
    // The short result renders inline inside the same box.
    expect(
      screen.getByText(/File created successfully at: \/tmp\/hello\.ts/),
    ).toBeInTheDocument();
    // Still one rendered tool box, not a separate result bubble.
    expect(container.querySelectorAll("details").length).toBe(1);
  });

  it("renders an Edit tool_use as a red/green line diff", () => {
    const { container } = render(
      <BlockRenderer
        block={{
          type: "tool_use",
          id: "te1",
          name: "Edit",
          input: {
            file_path: "/tmp/x.ts",
            old_string: "const a = 1;\nshared line\n",
            new_string: "const a = 2;\nshared line\n",
          },
        }}
      />,
    );
    expect(screen.getByText("Edit")).toBeInTheDocument();
    // No more separate Old/New labelled boxes.
    expect(screen.queryByText("Old")).toBeNull();
    expect(screen.queryByText("New")).toBeNull();
    // Removed line tinted red, added line tinted green (tint on the row).
    const removed = screen.getByText("const a = 1;").closest("div")!;
    const added = screen.getByText("const a = 2;").closest("div")!;
    expect(removed.className).toContain("text-red-300");
    expect(added.className).toContain("text-green-300");
    // Shared context line appears exactly once and is neutral.
    const shared = screen.getAllByText("shared line");
    expect(shared.length).toBe(1);
    expect(shared[0].closest("div")!.className).toContain(
      "text-muted-foreground",
    );
    // +/- gutters are present.
    expect(container.textContent).toContain("-");
    expect(container.textContent).toContain("+");
  });

  it("renders each MultiEdit edit as its own diff", () => {
    render(
      <BlockRenderer
        block={{
          type: "tool_use",
          id: "te2",
          name: "MultiEdit",
          input: {
            file_path: "/tmp/x.ts",
            edits: [
              { old_string: "foo old", new_string: "foo new" },
              { old_string: "bar old", new_string: "bar new" },
            ],
          },
        }}
      />,
    );
    expect(screen.getByText("MultiEdit")).toBeInTheDocument();
    expect(screen.getByText("Edit #1")).toBeInTheDocument();
    expect(screen.getByText("Edit #2")).toBeInTheDocument();
    expect(screen.getByText("foo old").closest("div")!.className).toContain(
      "text-red-300",
    );
    expect(screen.getByText("foo new").closest("div")!.className).toContain(
      "text-green-300",
    );
    expect(screen.getByText("bar old").closest("div")!.className).toContain(
      "text-red-300",
    );
    expect(screen.getByText("bar new").closest("div")!.className).toContain(
      "text-green-300",
    );
  });

  it("reveals a chip tool's multi-line result directly (one disclosure, no nested second collapsible)", () => {
    const results = new Map<string, string>([
      ["tre1", "line one\nline two\nline three"],
    ]);
    const { container } = render(
      <BlockRenderer
        block={{
          type: "tool_use",
          id: "tre1",
          name: "Read",
          input: { file_path: "/tmp/x.ts" },
        }}
        results={results}
      />,
    );
    // The outer collapsible identifies the call…
    expect(screen.getByText("Read")).toBeInTheDocument();
    expect(screen.getByText(/: \/tmp\/x\.ts/)).toBeInTheDocument();
    // …and there is exactly ONE disclosure — the result content is not nested
    // inside a second "→ result" collapsible.
    expect(container.querySelectorAll("details").length).toBe(1);
    expect(screen.queryByText(/→ result/)).toBeNull();
    // The content itself is present (rendered via TruncatedPre).
    expect(container.textContent).toContain("line one");
    expect(container.textContent).toContain("line three");
  });

  it("renders a Bash tool_use open by default", () => {
    const { container } = render(
      <BlockRenderer
        block={{
          type: "tool_use",
          id: "tb1",
          name: "Bash",
          input: { command: "git status", description: "Check git status" },
        }}
      />,
    );
    // The outer Bash collapsible is open by default so the command is visible
    // without a click.
    expect(container.querySelector("details")!.hasAttribute("open")).toBe(true);
  });

  it("keeps non-Bash box tools closed by default", () => {
    const { container } = render(
      <BlockRenderer
        block={{
          type: "tool_use",
          id: "tw2",
          name: "Write",
          input: { file_path: "/tmp/hello.ts", content: "export const x = 1;" },
        }}
      />,
    );
    expect(container.querySelector("details")!.hasAttribute("open")).toBe(
      false,
    );
  });

  it("adds a line-count size hint to a folded (multi-line) tool result", () => {
    const results = new Map<string, string>([
      ["tr1", "line one\nline two\nline three"],
    ]);
    render(
      <BlockRenderer
        block={{
          type: "tool_use",
          id: "tr1",
          name: "Bash",
          input: { command: "echo hi" },
        }}
        results={results}
      />,
    );
    // Folded result label carries the line count.
    expect(screen.getByText(/3 lines/)).toBeInTheDocument();
  });

  it("soft-wraps Edit diff lines instead of forcing horizontal scroll", () => {
    const { container } = render(
      <BlockRenderer
        block={{
          type: "tool_use",
          id: "tw1",
          name: "Edit",
          input: {
            file_path: "/tmp/x.ts",
            old_string: "short old\n",
            new_string: "short new\n",
          },
        }}
      />,
    );
    // The wrapping span carries whitespace-pre-wrap (wrap), not whitespace-pre.
    const added = screen.getByText("short new");
    expect(added.className).toContain("whitespace-pre-wrap");
    // No horizontal-scroll container around the diff.
    expect(container.querySelector(".overflow-x-auto")).toBeNull();
  });

  it("shows a muted note for unknown/other blocks", () => {
    render(<BlockRenderer block={{ type: "other", weird: true }} />);
    expect(screen.getByText("(unsupported block)")).toBeInTheDocument();
  });
});

describe("langForPath", () => {
  it.each([
    ["main.go", "go"],
    ["flake.nix", "nix"],
    ["change.patch", "diff"],
    ["fix.diff", "diff"],
    ["schema.graphql", "graphql"],
    ["q.gql", "graphql"],
    ["main.rs", "rust"],
    ["app.py", "python"],
    ["pyproject.toml", "toml"],
    ["infra.tf", "hcl"],
    ["prod.tfvars", "hcl"],
    ["A.java", "java"],
    ["lib.c", "c"],
    ["lib.hpp", "cpp"],
    ["Program.cs", "csharp"],
    ["app.rb", "ruby"],
    ["Main.kt", "kotlin"],
    ["api.proto", "protobuf"],
  ])("maps %s → %s", (path, lang) => {
    expect(langForPath(path)).toBe(lang);
  });

  it("maps the bare Dockerfile / Makefile names", () => {
    expect(langForPath("services/api/Dockerfile")).toBe("dockerfile");
    expect(langForPath("Makefile")).toBe("makefile");
  });

  it("falls back to plain text for unknown extensions", () => {
    expect(langForPath("notes.xyz")).toBe("text");
    expect(langForPath("LICENSE")).toBe("text");
  });
});
