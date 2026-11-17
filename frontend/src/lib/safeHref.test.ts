import { describe, expect, it } from "vitest";
import { safeHref } from "./safeHref";

describe("safeHref", () => {
  it("passes absolute http(s) URLs through unchanged", () => {
    expect(safeHref("https://github.com/org/repo/pull/42#c1")).toBe(
      "https://github.com/org/repo/pull/42#c1",
    );
    expect(safeHref("http://example.com/x")).toBe("http://example.com/x");
  });

  it("neutralizes executable and non-http schemes", () => {
    expect(safeHref("javascript:alert(document.domain)")).toBe("#");
    expect(safeHref("JavaScript:alert(1)")).toBe("#");
    expect(safeHref("data:text/html,<script>1</script>")).toBe("#");
    expect(safeHref("vbscript:msgbox")).toBe("#");
  });

  it("neutralizes relative and malformed URLs", () => {
    expect(safeHref("/relative/path")).toBe("#");
    expect(safeHref("//evil.example/x")).toBe("#");
    expect(safeHref("")).toBe("#");
    expect(safeHref(undefined)).toBe("#");
    expect(safeHref(null)).toBe("#");
  });
});
