import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Markdown } from "./Markdown";

const SOURCE =
  "# Preview\n\n| Choice | Result |\n| --- | --- |\n| Copy | Exact source |";

describe("MarkdownBlock", () => {
  it("copies fenced Markdown exactly without rendering it as a table", async () => {
    const user = userEvent.setup();
    render(<Markdown>{`\`\`\`markdown\n${SOURCE}\n\`\`\``}</Markdown>);

    expect(screen.queryByRole("table")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Copy code" }));
    // react-markdown includes the newline immediately before the closing fence;
    // keep it because it is part of the fenced source, not table conversion.
    await expect(navigator.clipboard.readText()).resolves.toBe(`${SOURCE}\n`);
  });

  it("switches from source to rendered GFM and back", async () => {
    const user = userEvent.setup();
    render(<Markdown>{`\`\`\`md\n${SOURCE}\n\`\`\``}</Markdown>);

    await user.click(
      screen.getByRole("button", { name: "View rendered Markdown" }),
    );
    expect(
      screen.getByRole("heading", { name: "Preview" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("table")).toBeInTheDocument();
    expect(screen.getByTestId("markdown-preview")).toHaveTextContent(
      "Exact source",
    );

    await user.click(
      screen.getByRole("button", { name: "View Markdown source" }),
    );
    expect(
      screen.getByRole("button", { name: "View rendered Markdown" }),
    ).toBeInTheDocument();
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
  });

  it("keeps preview controls available to hover, keyboard, and touch users", async () => {
    const user = userEvent.setup();
    render(<Markdown>{`\`\`\`markdown\n${SOURCE}\n\`\`\``}</Markdown>);
    await user.click(
      screen.getByRole("button", { name: "View rendered Markdown" }),
    );

    const copy = screen.getByRole("button", { name: "Copy Markdown source" });
    const controls = copy.parentElement;
    expect(controls).toHaveClass("sm:group-hover/markdown:opacity-100");
    expect(controls).toHaveClass("sm:group-focus-within/markdown:opacity-100");
  });

  it("recognizes Markdown language aliases case-insensitively", () => {
    render(<Markdown>{`\`\`\`MD\n${SOURCE}\n\`\`\``}</Markdown>);
    expect(
      screen.getByRole("button", { name: "View rendered Markdown" }),
    ).toBeInTheDocument();
  });
});
