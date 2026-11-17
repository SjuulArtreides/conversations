import { describe, expect, it } from "vitest";
import { stripTerminalChrome } from "./terminalText";

describe("stripTerminalChrome", () => {
  it("drops full-width box-drawing border rules", () => {
    const input = ["❯ write a poem", "─".repeat(60), "", "more text"].join("\n");
    expect(stripTerminalChrome(input)).toBe(
      ["❯ write a poem", "", "more text"].join("\n"),
    );
  });

  it("drops underscore / dash / equals rules too", () => {
    const input = ["a", "________________", "b", "----------", "c", "======"].join(
      "\n",
    );
    expect(stripTerminalChrome(input)).toBe(["a", "b", "c"].join("\n"));
  });

  it("keeps lines that mix a border glyph with real text", () => {
    const input = ["└ Tip: name your conversations", "├─ branch: main"].join(
      "\n",
    );
    // Both lines carry letters, so neither is a pure border rule.
    expect(stripTerminalChrome(input)).toBe(input);
  });

  it("keeps blank lines and ordinary prose", () => {
    const input = ["Hi Alice!", "", "What can I help with?"].join("\n");
    expect(stripTerminalChrome(input)).toBe(input);
  });

  it("keeps a short '- ' bullet or prompt glyph line", () => {
    // The prompt caret on its own is not a border rule.
    expect(stripTerminalChrome("❯")).toBe("❯");
  });
});
