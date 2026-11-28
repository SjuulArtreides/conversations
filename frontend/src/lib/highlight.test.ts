import { describe, expect, it } from "vitest";
import { highlightCode, rehypeLanguages } from "./highlight";

describe("highlightCode", () => {
  // Spot-check the languages added beyond the original web/data set — these are
  // the stacks that show up in this monorepo's transcripts.
  it.each([
    ["go", "package main\n\nfunc main() {}"],
    ["nix", '{ pkgs }: pkgs.mkShell { buildInputs = [ pkgs.go ]; }'],
    ["diff", "@@ -1 +1 @@\n-old line\n+new line"],
    ["graphql", "query Q { field { id } }"],
    ["java", "class A { void m() {} }"],
    ["ruby", "def hello\n  puts 'hi'\nend"],
  ])("colorizes a %s block into hljs token spans", (lang, code) => {
    const html = highlightCode(code, lang);
    expect(html).not.toBeNull();
    expect(html).toContain("hljs-");
  });

  it("returns null for an unregistered language (caller renders plain)", () => {
    expect(highlightCode("+++.", "brainfuck")).toBeNull();
  });

  it("every language in the rehype map is also registered for the direct path", () => {
    // The two render paths share one `grammars` source of truth; this guards
    // against a future edit that adds a fence language but forgets to register
    // it (or vice versa) — every rehype key must highlight via `highlightCode`.
    for (const lang of Object.keys(rehypeLanguages)) {
      expect(highlightCode("x", lang), `${lang} should be registered`).not.toBeNull();
    }
  });
});
