import { describe, expect, it } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
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
    // userEvent.setup installs a working clipboard stub on navigator that
    // resolves writeText, so clicking exercises the real onCopy path.
    const user = userEvent.setup();
    const { container } = render(
      <CodeBlock language="ts" code={"const x = 1;"} />,
    );

    const button = screen.getByRole("button", { name: "Copy code" });
    // Initially there is no checkmark icon.
    expect(container.querySelector("svg")).toBeNull();

    await user.click(button);

    // The copied text was written to the clipboard.
    await expect(navigator.clipboard.readText()).resolves.toBe("const x = 1;");

    // Confirmation state: accessible name flips and a checkmark renders.
    const confirmed = await screen.findByRole("button", { name: "Copied" });
    expect(confirmed).toHaveTextContent("Copied");
    expect(container.querySelector("svg")).not.toBeNull();

    // Reverts to the idle Copy state after the ~1.5s timeout.
    await waitFor(
      () => {
        expect(
          screen.getByRole("button", { name: "Copy code" }),
        ).toBeInTheDocument();
      },
      { timeout: 2000 },
    );
    expect(container.querySelector("svg")).toBeNull();
  });
});
