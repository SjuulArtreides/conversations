import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { CodeBlock } from "./CodeBlock";

describe("CodeBlock", () => {
  it("highlights a TOML block (no children) into hljs token spans", () => {
    const { container } = render(
      <CodeBlock
        language="toml"
        code={'[tool.example]\ntenant = "example-tenant"\nport = 5432'}
      />,
    );
    // highlight.js emits class names like `hljs-section`, `hljs-string`, etc.
    expect(container.querySelector('[class^="hljs-"]')).not.toBeNull();
    expect(container.querySelector("code.hljs")).not.toBeNull();
    expect(container.textContent).toContain("tool.example");
  });

  it("highlights a Rust block (no children) into hljs token spans", () => {
    const { container } = render(
      <CodeBlock language="rust" code={'fn main() {\n    let x = 1;\n}'} />,
    );
    expect(container.querySelector('[class^="hljs-"]')).not.toBeNull();
    expect(container.textContent).toContain("fn main");
  });

  it("highlights an HCL block (no children) into hljs token spans", () => {
    const { container } = render(
      <CodeBlock
        language="hcl"
        code={'resource "aws_s3_bucket" "b" {\n  bucket = "my-bucket"\n}'}
      />,
    );
    expect(container.querySelector('[class^="hljs-"]')).not.toBeNull();
    expect(container.textContent).toContain("aws_s3_bucket");
  });

  it("falls back to plain text under .hljs for an unregistered language", () => {
    const { container } = render(
      <CodeBlock language="brainfuck" code={"+++."} />,
    );
    // No token spans, but still carries the theme base class.
    expect(container.querySelector('[class^="hljs-"]')).toBeNull();
    expect(container.querySelector("code.hljs")).not.toBeNull();
    expect(container.textContent).toContain("+++.");
  });

  it("renders provided children unchanged (Markdown-highlighted path wins)", () => {
    const { container } = render(
      <CodeBlock language="json" code={'{"a":1}'}>
        <code className="hljs language-json" data-testid="prehighlighted">
          provided child
        </code>
      </CodeBlock>,
    );
    const child = container.querySelector('[data-testid="prehighlighted"]');
    expect(child).not.toBeNull();
    expect(child?.textContent).toBe("provided child");
    // The verbatim `code` prop is NOT rendered when children are present.
    expect(container.textContent).not.toContain('{"a":1}');
  });

  it("flashes a Copied confirmation with a checkmark after copying", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      userEvent.setup();
      const { container } = render(
        <CodeBlock language="ts" code={"const x = 1;"} />,
      );

      const button = screen.getByRole("button", { name: "Copy code" });
      expect(container.querySelector("svg")).toBeNull();

      await act(async () => {
        fireEvent.click(button);
      });
      await expect(navigator.clipboard.readText()).resolves.toBe("const x = 1;");

      expect(screen.getByRole("button", { name: "Copied" })).toHaveTextContent(
        "Copied",
      );
      expect(container.querySelector("svg")).not.toBeNull();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(1500);
      });
      expect(screen.getByRole("button", { name: "Copy code" })).toBe(button);
      expect(container.querySelector("svg")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });
});
